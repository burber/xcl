//! Write32/64BitValueBottomOfPipeX into memory the GPU does not see: done by the CPU once the GPU
//! has finished the work before them.
//!
//! A console title marks its GPU progress with bottom-of-pipe writes into its own memory and
//! waits on them (a retail game: plain `VirtualAlloc` memory, outside XCL's unified memory).
//! The PC GPU cannot write there; dropped, the title waited for ever after its second frame.
//! Each queue gets its own fence and worker (adapted from https://github.com/ivall/xcl/pull/562).
//! After a submission with such writes, the queue signals its fence and the worker writes in
//! submission order. A stalled queue cannot block completed writes from another queue.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};

use windows::core::{Interface, Owned};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Threading::CreateEventW;

/// A bottom-of-pipe write into memory outside unified memory, applied by the CPU once the
/// queue's fence reaches the submission that recorded it.
#[derive(Clone)]
pub(super) enum Write {
    /// Immediate dwords to store starting at `va`.
    Immediate(usize, Vec<u32>),
    /// An immediate progress value, stored atomically after confirmed fence completion.
    ImmediateU64(usize, u64),
    /// A 64-bit value resolved by the GPU into a persistently mapped READBACK buffer slot
    /// (`src`), copied atomically to `va` once the queue's fence completes. Holding `_buf` keeps
    /// the mapping alive until the worker finishes.
    ReadbackU64 { va: usize, src: usize, _buf: ID3D12Resource },
}

struct Job {
    value: u64,
    writes: Vec<Write>,
}

/// A queue's fence, last signalled value, and dedicated completion worker.
struct QueueFence {
    fence: ID3D12Fence,
    value: u64,
    tx: mpsc::Sender<Job>,
}

static FENCES: Mutex<Option<HashMap<usize, QueueFence>>> = Mutex::new(None);

/// how long the worker waits for one submission before writing anyway (a removed device, a
/// queue that never gets there)
const WAIT_MS: u32 = 10_000;

fn spawn_queue_worker(dev: ID3D12Device, fence: ID3D12Fence) -> mpsc::Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    let spawned = std::thread::Builder::new().name("xcl-cpu-writes".into()).spawn(move || unsafe {
        let event = CreateEventW(None, false, false, None).ok().map(|event| Owned::new(event));
        let handle = event.as_deref().copied();
        for job in rx {
            let completed = crate::gpu::fence::wait(&fence, job.value, &dev, handle, WAIT_MS);
            for w in &job.writes {
                match w {
                    Write::Immediate(va, words) => write_now(*va, words),
                    Write::ImmediateU64(va, value) => match &completed {
                        Ok(()) => write_u64_now(*va, *value),
                        Err(e) => crate::xlog_warn!("D3D12.X: deferred 64-bit write at {va:#x} did not complete: {e:?}"),
                    },
                    Write::ReadbackU64 { va, src, .. } => {
                        let value = (*(*src as *const AtomicU64)).load(Ordering::Acquire);
                        write_u64_now(*va, value);
                    }
                }
            }
        }
    });
    if let Err(e) = spawned {
        crate::xlog_error!("D3D12.X: no thread for the writes after the GPU: {e}");
    }
    tx
}

/// Drop the queue's sender; its worker drains outstanding writes and closes its event.
pub(super) fn forget_queue(queue: usize) {
    if let Some(m) = FENCES.lock().unwrap().as_mut() {
        m.remove(&queue);
    }
}

/// The dwords at `va`, if the memory is still there and writable.
unsafe fn write_now(va: usize, words: &[u32]) {
    use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_EXECUTE_READWRITE, PAGE_READWRITE};
    let mut m: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
    if VirtualQuery(va as *const c_void, &mut m, std::mem::size_of_val(&m)) == 0 || m.State != MEM_COMMIT {
        return;
    }
    if m.Protect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE) == 0 || (m.BaseAddress as usize + m.RegionSize) < va + 4 * words.len() {
        return;
    }
    for (i, &w) in words.iter().enumerate() {
        (*((va + 4 * i) as *const AtomicU32)).store(w, Ordering::Release);
    }
}

/// An 8-byte aligned 64-bit value at `va`, if the memory is still there and writable.
unsafe fn write_u64_now(va: usize, value: u64) {
    use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_EXECUTE_READWRITE, PAGE_GUARD, PAGE_READWRITE};
    let Some(end) = va.checked_add(8) else { return };
    let mut m: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
    if VirtualQuery(va as *const c_void, &mut m, std::mem::size_of_val(&m)) == 0 || m.State != MEM_COMMIT {
        return;
    }
    if m.Protect & PAGE_GUARD != 0 || m.Protect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE) == 0 || (m.BaseAddress as usize).saturating_add(m.RegionSize) < end {
        return;
    }
    (*(va as *const AtomicU64)).store(value, Ordering::Release);
}

/// Whether `[va, va + bytes)` is memory the CPU can write for the GPU later.
pub(super) fn writable(va: usize, bytes: usize) -> bool {
    crate::hle::kernelx::lazy::ensure(va, bytes);
    use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_EXECUTE_READWRITE, PAGE_READWRITE};
    let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let found = unsafe { VirtualQuery(va as *const c_void, &mut m, std::mem::size_of_val(&m)) } != 0;
    found
        && m.State == MEM_COMMIT
        && m.Protect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE) != 0
        && m.BaseAddress as usize + m.RegionSize >= va + bytes
}

/// `writes` of lists just submitted to `queue`: done when the GPU has finished them.
pub(super) unsafe fn after(queue: *mut c_void, writes: Vec<Write>) {
    if writes.is_empty() || queue.is_null() {
        return;
    }
    let Some(q) = ID3D12CommandQueue::from_raw_borrowed(&queue) else { return };
    enqueue(q, writes, |fence, value| q.Signal(fence, value));
}

unsafe fn enqueue(
    q: &ID3D12CommandQueue, writes: Vec<Write>,
    signal: impl FnOnce(&ID3D12Fence, u64) -> windows::core::Result<()>,
) {
    let count = writes.len();
    {
        let mut g = FENCES.lock().unwrap();
        let m = g.get_or_insert_with(HashMap::new);
        let entry = match m.entry(q.as_raw() as usize) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let mut dev: Option<ID3D12Device> = None;
                if q.GetDevice(&mut dev).is_err() {
                    return;
                }
                let Some(dev) = dev else { return };
                let Ok(fence) = dev.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE) else { return };
                let tx = spawn_queue_worker(dev, fence.clone());
                v.insert(QueueFence { fence, value: 0, tx })
            }
        };
        entry.value += 1;
        if signal(&entry.fence, entry.value).is_err() {
            return;
        }
        // Enqueue before unlocking so concurrent producers keep fence signal order.
        let _ = entry.tx.send(Job { value: entry.value, writes });
    }
    static FIRST: std::sync::Once = std::sync::Once::new();
    FIRST.call_once(|| crate::xlog_info!("D3D12.X: bottom-of-pipe writes outside unified memory are done by the CPU after the GPU ({count} in the first submission)"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
    use windows::Win32::Graphics::Dxgi::*;

    unsafe fn test_device() -> Option<ID3D12Device> {
        let mut dev = None;
        if D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut dev).is_err() {
            let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
            let warp: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
            D3D12CreateDevice(&warp, D3D_FEATURE_LEVEL_11_0, &mut dev).ok()?;
        }
        dev
    }

    /// The value lands after the queue has done what came before it.
    #[test]
    fn writes_land_after_the_queue() {
        unsafe {
            let dev = test_device().expect("D3D12 device (hardware or WARP)");
            let q: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).unwrap();
            let cell = Box::new([0u32; 2]);
            let va = cell.as_ptr() as usize;
            assert!(writable(va, 8));
            after(q.as_raw(), vec![Write::Immediate(va, vec![7, 9])]);
            let t = std::time::Instant::now();
            while (*(va as *const AtomicU32)).load(Ordering::Acquire) != 7 {
                assert!(t.elapsed().as_secs() < 5, "never written");
                std::thread::yield_now();
            }
            forget_queue(q.as_raw() as usize);
            assert_eq!(*cell, [7, 9]);
        }
    }

    /// A ReadbackU64 write copies the 64-bit value from the mapped readback buffer after the queue finishes.
    #[test]
    fn readback_u64_lands_after_the_queue() {
        unsafe {
            let Some(dev) = test_device() else {
                crate::gpu::recomp::runner::skip_test("no GPU");
                return;
            };
            let q: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).unwrap();
            let hp = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
            let rd = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 8,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut rb: Option<ID3D12Resource> = None;
            dev.CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &rd, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut rb).unwrap();
            let rb = rb.unwrap();
            let mut ptr: *mut c_void = std::ptr::null_mut();
            rb.Map(0, None, Some(&mut ptr)).unwrap();
            let expected = 0x1234_5678_9ABC_DEF0u64;
            (*(ptr as *const AtomicU64)).store(expected, Ordering::Release);

            let dst = Box::new(0u64);
            let va = dst.as_ref() as *const u64 as usize;
            assert!(writable(va, 8));
            after(q.as_raw(), vec![Write::ReadbackU64 { va, src: ptr as usize, _buf: rb }]);
            let t = std::time::Instant::now();
            while (*(va as *const AtomicU64)).load(Ordering::Acquire) != expected {
                assert!(t.elapsed().as_secs() < 5, "readback value never written");
                std::thread::yield_now();
            }
            forget_queue(q.as_raw() as usize);
            assert_eq!(*dst, expected);
        }
    }

    #[test]
    fn blocked_queue_does_not_stall_another_queues_writes() {
        use std::time::{Duration, Instant};

        // Heap pages are writable, and the destinations outlive even a failure to drain.
        static CELLS: std::sync::LazyLock<Box<[AtomicU32; 2]>> =
            std::sync::LazyLock::new(|| Box::new([AtomicU32::new(0), AtomicU32::new(0)]));
        let wait_for_write = |cell: &AtomicU32, expected| {
            // Detect a shared worker before its 10 s fence timeout could drain both writes.
            let deadline = Instant::now() + Duration::from_secs(5);
            while cell.load(Ordering::Acquire) != expected {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            true
        };

        unsafe {
            let Some(dev) = test_device() else {
                crate::gpu::recomp::runner::skip_test("no GPU");
                return;
            };
            let desc = D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() };
            let a: ID3D12CommandQueue = dev.CreateCommandQueue(&desc).unwrap();
            let b: ID3D12CommandQueue = dev.CreateCommandQueue(&desc).unwrap();
            let [a_cell, b_cell] = CELLS.as_ref();

            // The real queue keys and completion fences go through the same enqueue path as
            // after(). Control the fences from the CPU so no GPU queue is held waiting.
            let mut blocked = None;
            enqueue(&a, vec![Write::Immediate(a_cell.as_ptr() as usize, vec![11])], |fence, value| {
                blocked = Some((fence.clone(), value));
                Ok(())
            });
            let (fence_a, value_a) = blocked.expect("queue A completion fence");
            enqueue(&b, vec![Write::Immediate(b_cell.as_ptr() as usize, vec![22])], |fence, value| {
                fence.Signal(value)
            });

            let b_written = wait_for_write(b_cell, 22);
            let a_pending = fence_a.GetCompletedValue() < value_a && a_cell.load(Ordering::Acquire) == 0;
            // Release A and disconnect both senders even when B was stalled.
            let unblocked = fence_a.Signal(value_a);
            forget_queue(a.as_raw() as usize);
            forget_queue(b.as_raw() as usize);
            unblocked.expect("signal queue A completion fence");

            assert!(b_written, "queue B's completed write stalled behind queue A");
            assert!(a_pending, "queue A wrote before its completion fence was signaled");
            assert!(wait_for_write(a_cell, 11), "queue A did not write after its completion fence was signaled");
        }
    }
}
