//! Non-blocking pipeline creation. Translating a console shader takes milliseconds; compiling
//! the HLSL with DXC takes 0.2 s to seconds, and a title creates thousands of pipelines while
//! it loads. When a pipeline's DXIL is not in the disk cache yet, the title gets a stand-in
//! pipeline (shaders that draw nothing) at once and the real one is built on a worker; binding
//! (`SetPipelineState`, console list replay, the two-pass lookups) uses the real one as soon
//! as it is there. A draw waits for its graphics pipeline only so long (`resolve_for_draw`),
//! then draws nothing until it is built; a compute pipeline is waited
//! for at its first bind (a skipped dispatch would corrupt what it computes).
//! `XCL_ASYNC_PSO=0` builds every pipeline in the call, as before.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

struct Pending {
    /// the build, until a worker – or a bind that cannot wait for the queue – takes it
    job: Mutex<Option<Job>>,
    /// the build went to the workers (a lazy one goes on its first `resolve`)
    sent: std::sync::atomic::AtomicBool,
    /// the real pipeline state (raw pointer), 0 while building or when building failed
    real: AtomicUsize,
    done: Mutex<bool>,
    cv: Condvar,
    compute: bool,
    /// a bounded compute wait already timed out; subsequent binds skip waiting until `real` lands
    timed_out: std::sync::atomic::AtomicBool,
    /// the stand-in was released before the real pipeline was built
    orphan: std::sync::atomic::AtomicBool,
}

/// stand-in handed to the title → its real pipeline
static PENDING: Mutex<Option<HashMap<usize, Arc<Pending>>>> = Mutex::new(None);

/// The build is done (`real`: its pipeline, 0 = failed).
fn complete(p: &Pending, real: usize) {
    if real == 0 {
        FAILED.fetch_add(1, Ordering::Relaxed);
    } else {
        BUILT.fetch_add(1, Ordering::Relaxed);
    }
    let orphan = {
        let mut done = p.done.lock().unwrap();
        let orphan = p.orphan.load(Ordering::Acquire);
        if !orphan {
            p.real.store(real, Ordering::Release);
        }
        *done = true;
        p.cv.notify_all();
        orphan
    };
    // outside `done`: the Release override takes PENDING (`counts` takes PENDING, then `done`)
    if orphan {
        release(real);
    }
}
static BUILT: AtomicUsize = AtomicUsize::new(0);
static FAILED: AtomicUsize = AtomicUsize::new(0);

pub fn enabled() -> bool {
    std::env::var("XCL_ASYNC_PSO").map_or(true, |v| v != "0")
}

/// Builds the real pipeline: its raw pointer, 0 = failed.
pub type Job = Box<dyn FnOnce() -> usize + Send>;

fn queue() -> &'static Mutex<mpsc::Sender<Arc<Pending>>> {
    static Q: OnceLock<Mutex<mpsc::Sender<Arc<Pending>>>> = OnceLock::new();
    Q.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Arc<Pending>>();
        let rx = Arc::new(Mutex::new(rx));
        // translation recurses deeply on big programs; DXC itself runs on its own pool
        let n = crate::gpu::recomp::dxc::compile_threads();
        for k in 0..n {
            let rx = rx.clone();
            let _ = std::thread::Builder::new().name(format!("xcl-pso-{k}")).stack_size(64 << 20).spawn(move || loop {
                let next = rx.lock().unwrap().recv();
                let Ok(p) = next else { return };
                // (None: a bind took it and builds it itself)
                let job = p.job.lock().unwrap().take();
                if let Some(job) = job {
                    complete(&p, job());
                }
            });
        }
        Mutex::new(tx)
    })
}

/// Release a pipeline state XCL owns (raw pointer, 0 = none).
pub fn release(p: usize) {
    if p != 0 {
        unsafe { drop(<windows::Win32::Graphics::Direct3D12::ID3D12PipelineState as windows::core::Interface>::from_raw(p as *mut std::ffi::c_void)) };
    }
}

/// `standin` went to the title; `job` builds the real pipeline on a worker.
pub fn submit(standin: usize, compute: bool, job: Job) {
    submit_as(standin, compute, job, true);
}

/// Lazy pipeline creation (`XCL_LAZY_PSO=0`: off): a title's pipelines with console shaders get
/// their stand-in when made and are built when first used – the first bind of a graphics one
/// sends it to the workers, a compute one is built at its first bind. A retail game makes
/// ~51 000 pipelines in its first 30 s and uses few; each one built stays in the driver
/// (~400 KB), and the process ran out of commit.
pub fn lazy() -> bool {
    static L: OnceLock<bool> = OnceLock::new();
    *L.get_or_init(|| enabled() && std::env::var("XCL_LAZY_PSO").map_or(true, |v| v != "0"))
}

/// `submit`, sent to the workers only on its first `resolve` (`lazy`).
pub fn submit_lazy(standin: usize, compute: bool, job: Job) {
    submit_as(standin, compute, job, false);
}

fn submit_as(standin: usize, compute: bool, job: Job, send: bool) {
    let p = Arc::new(Pending {
        job: Mutex::new(Some(job)),
        sent: std::sync::atomic::AtomicBool::new(send),
        real: AtomicUsize::new(0),
        done: Mutex::new(false),
        cv: Condvar::new(),
        compute,
        timed_out: Default::default(),
        orphan: Default::default(),
    });
    PENDING.lock().unwrap().get_or_insert_with(HashMap::new).insert(standin, p.clone());
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        crate::xlog_info!("D3D12.X: pipelines whose shaders are not compiled yet get a stand-in and are built in the background");
    });
    if send {
        let _ = queue().lock().unwrap().send(p);
    }
}

fn entry(pso: usize) -> Option<Arc<Pending>> {
    PENDING.lock().unwrap().as_ref()?.get(&pso).cloned()
}

/// The pipeline to use for `pso` now: the real one once built, else `pso` itself.
pub fn resolve(pso: usize) -> usize {
    let Some(p) = entry(pso) else { return pso };
    match p.real.load(Ordering::Acquire) {
        0 => {
            // a lazy one is wanted now: to the workers
            if !p.sent.swap(true, Ordering::AcqRel) {
                let _ = queue().lock().unwrap().send(p);
            }
            pso
        }
        r => r,
    }
}

/// `resolve`, waiting for a pipeline still being built – built here if no worker has started
/// it yet (a queue of a title's hundreds of pipelines must not hold up the one it binds now).
pub fn resolve_wait(pso: usize) -> usize {
    let Some(p) = entry(pso) else { return pso };
    let start = Instant::now();
    let job = p.job.lock().unwrap().take();
    let here = job.is_some();
    if let Some(job) = job {
        complete(&p, job());
    }
    let mut done = p.done.lock().unwrap();
    while !*done {
        done = p.cv.wait(done).unwrap();
    }
    let real = p.real.load(Ordering::Acquire);
    drop(done);
    note_long_wait(pso, p.compute, here, start.elapsed());
    match real {
        0 => pso,
        r => r,
    }
}

/// A title's thread held this long in one pipeline wait is logged: a retail game's watchdog
/// thread ends the process (`mov dword [0], 0` at title+0x2215c35, decoded in #232: it wakes
/// once a second and compares watched timestamps with a constant 300.0) when a watched thread
/// stops posting, and an NVIDIA driver takes 10 - 17 s for one big translated pixel shader,
/// minutes for a big compute program (#156).
const LONG_WAIT: Duration = Duration::from_secs(2);

fn note_long_wait(pso: usize, compute: bool, here: bool, took: Duration) {
    static N: AtomicUsize = AtomicUsize::new(0);
    if took >= LONG_WAIT && N.fetch_add(1, Ordering::Relaxed) < 32 {
        crate::xlog_warn!(
            "D3D12.X: thread '{}' waited {:.1} s for {} pipeline {pso:#x} ({})",
            std::thread::current().name().unwrap_or("title"),
            took.as_secs_f32(),
            if compute { "compute" } else { "graphics" },
            if here { "built in this thread" } else { "built on a worker" }
        );
    }
}

/// The longest one draw holds the title's thread for a graphics pipeline still being built
/// (`XCL_PSO_WAIT_MS`, default 4000; 0: no limit, as before). Past it the draw goes on with the
/// stand-in (draws nothing) and the pipeline is built on a worker. 4 s is a safety margin under
/// the watchdog's one-call trigger seen in #156 (23 s), not a measured threshold.
fn draw_wait_limit() -> Option<Duration> {
    static L: OnceLock<Option<Duration>> = OnceLock::new();
    *L.get_or_init(|| match std::env::var("XCL_PSO_WAIT_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(Duration::from_secs(4)),
    })
}

/// draws issued with a stand-in because their pipeline was not built within the limit
static STANDIN_DRAWS: AtomicUsize = AtomicUsize::new(0);

/// Draws that went to the GPU with a stand-in (drew nothing) because `resolve_for_draw` gave up
/// on their pipeline (`report.json → d3d12.standin_draws`). Lost, not replayed.
pub fn standin_draws() -> usize {
    STANDIN_DRAWS.load(Ordering::Relaxed)
}

/// For a draw (or ExecuteIndirect) of console code: a compute pipeline is waited for
/// (`resolve_wait`); a graphics one at most `draw_wait_limit`, each draw on its own, and never
/// built in the title's thread, where a driver compile cannot be cut short. A draw past the
/// limit goes on with the stand-in, counted and logged.
pub fn resolve_for_draw(pso: usize) -> usize {
    resolve_within(pso, draw_wait_limit())
}

fn resolve_within(pso: usize, limit: Option<Duration>) -> usize {
    let Some(p) = entry(pso) else { return pso };
    let Some(limit) = limit.filter(|_| !p.compute) else { return resolve_wait(pso) };
    // to the workers (a lazy one has not gone yet)
    if !p.sent.swap(true, Ordering::AcqRel) {
        let _ = queue().lock().unwrap().send(p.clone());
    }
    let start = Instant::now();
    let deadline = start + limit;
    let mut done = p.done.lock().unwrap();
    while !*done {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        done = p.cv.wait_timeout(done, deadline - now).unwrap().0;
    }
    let (finished, real) = (*done, p.real.load(Ordering::Acquire));
    drop(done);
    let waited = start.elapsed();
    if !finished {
        let n = STANDIN_DRAWS.fetch_add(1, Ordering::Relaxed);
        if n < 16 || n.is_power_of_two() {
            crate::xlog_warn!(
                "D3D12.X: graphics pipeline {pso:#x} not built after {:.1} s (limit {} ms, XCL_PSO_WAIT_MS) – the draw goes with its stand-in and is lost, the pipeline still building on a worker ({} such draws)",
                waited.as_secs_f32(),
                limit.as_millis(),
                n + 1
            );
        }
    } else {
        note_long_wait(pso, false, false, waited);
    }
    match real {
        0 => pso,
        r => r,
    }
}

/// Maximum time a title thread waits for a compute pipeline on its first bind/dispatch
/// (`XCL_COMPUTE_PSO_WAIT_MS`, default 4000 ms; `0` = unbounded). Huge compute shaders can take
/// 20+ s to compile in DXC / the driver, which trips a retail title's render-thread watchdog.
fn compute_wait_timeout() -> Option<std::time::Duration> {
    static LIMIT: OnceLock<Option<std::time::Duration>> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        let ms = std::env::var("XCL_COMPUTE_PSO_WAIT_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(4000);
        if ms == 0 { None } else { Some(std::time::Duration::from_millis(ms)) }
    })
}

/// Bounded wait for a compute pipeline: always builds on `xcl-pso-*` workers (never stalling the
/// title's thread inside DXC/driver), waits at most `limit` on the first bind/dispatch, and
/// returns the stand-in immediately on subsequent calls until the worker finishes.
fn resolve_compute_within(pso: usize, limit: Option<std::time::Duration>) -> usize {
    let Some(limit) = limit else {
        return resolve_wait(pso);
    };
    let Some(p) = entry(pso) else { return pso };
    if let r @ 1.. = p.real.load(Ordering::Acquire) {
        return r;
    }
    if !p.sent.swap(true, Ordering::AcqRel) {
        let _ = queue().lock().unwrap().send(p.clone());
    }
    if p.timed_out.load(Ordering::Acquire) {
        return match p.real.load(Ordering::Acquire) {
            0 => pso,
            r => r,
        };
    }
    let deadline = std::time::Instant::now() + limit;
    let mut done = p.done.lock().unwrap();
    while !*done {
        let rem = deadline.saturating_duration_since(std::time::Instant::now());
        if rem.is_zero() {
            break;
        }
        let (g, res) = p.cv.wait_timeout(done, rem).unwrap();
        done = g;
        if res.timed_out() {
            break;
        }
    }
    if !*done {
        if !p.timed_out.swap(true, Ordering::AcqRel) {
            static LOGGED: AtomicUsize = AtomicUsize::new(0);
            if LOGGED.fetch_add(1, Ordering::Relaxed) < 16 {
                crate::xlog_warn!(
                    "D3D12.X: compute pipeline {pso:#x} still building after {} ms – proceeding with stand-in (continues in background)",
                    limit.as_millis()
                );
            }
        }
        return pso;
    }
    match p.real.load(Ordering::Acquire) {
        0 => pso,
        r => r,
    }
}

/// Resolve a compute pipeline with `XCL_COMPUTE_PSO_WAIT_MS` bounding.
pub fn resolve_compute(pso: usize) -> usize {
    resolve_compute_within(pso, compute_wait_timeout())
}

/// For binding: compute pipelines wait up to `XCL_COMPUTE_PSO_WAIT_MS` on background workers;
/// graphics pipelines are queued without blocking.
pub fn resolve_for_bind(pso: usize) -> usize {
    match entry(pso) {
        Some(p) if p.compute => resolve_compute(pso),
        Some(_) => resolve(pso),
        None => pso,
    }
}

/// For `ExecuteIndirect`: compute pipelines use bounded waits, graphics pipelines keep `resolve_wait`.
pub fn resolve_for_indirect(pso: usize) -> usize {
    match entry(pso) {
        Some(p) if p.compute => resolve_compute(pso),
        // graphics: at most XCL_PSO_WAIT_MS (#267)
        Some(_) => resolve_for_draw(pso),
        None => pso,
    }
}

/// The title released stand-in `pso` for good: forget it (its address may come back for
/// another object) and return its real pipeline to release (0: none, or still being built –
/// the worker's result is then released when it arrives).
pub fn released(pso: usize) -> usize {
    let Some(p) = PENDING.lock().unwrap().as_mut().and_then(|m| m.remove(&pso)) else { return 0 };
    let done = p.done.lock().unwrap();
    if *done {
        p.real.swap(0, Ordering::AcqRel)
    } else {
        // the worker stores its result into a Pending nobody will read: mark it orphaned
        p.orphan.store(true, Ordering::Release);
        0
    }
}

/// `pso` is a stand-in whose real pipeline is still being built.
pub fn is_pending(pso: usize) -> bool {
    entry(pso).is_some_and(|p| !*p.done.lock().unwrap())
}

/// (built in the background, failed, still building)
pub fn counts() -> (usize, usize, usize) {
    let pending = PENDING.lock().unwrap().as_ref().map_or(0, |m| m.values().filter(|p| !*p.done.lock().unwrap()).count());
    (BUILT.load(Ordering::Relaxed), FAILED.load(Ordering::Relaxed), pending)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn bounded_compute_wait_times_out_then_resolves_once_worker_finishes() {
        let standin = 0x7300_0001usize;
        let real_pso = 0x7300_0002usize;
        let (tx, rx) = mpsc::channel::<()>();
        submit_lazy(
            standin,
            true,
            Box::new(move || {
                let _ = rx.recv_timeout(Duration::from_secs(5));
                real_pso
            }),
        );

        // First call sends the lazy build to the worker pool and returns the stand-in after ~limit.
        let t0 = Instant::now();
        assert_eq!(resolve_compute_within(standin, Some(Duration::from_millis(60))), standin);
        assert!(t0.elapsed() < Duration::from_secs(1), "bounded wait must not block indefinitely");

        // Subsequent calls while still building return immediately (no repeated timeout stall).
        let t1 = Instant::now();
        assert_eq!(resolve_compute_within(standin, Some(Duration::from_secs(2))), standin);
        assert!(t1.elapsed() < Duration::from_millis(200), "post-timeout calls must return immediately");

        // Unblock the background worker; once complete, resolution immediately returns the real PSO.
        tx.send(()).unwrap();
        let wait = Instant::now();
        while resolve_compute_within(standin, Some(Duration::from_millis(50))) != real_pso {
            assert!(wait.elapsed() < Duration::from_secs(5), "background compute PSO never completed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(released(standin), real_pso);
    }

    #[test]
    fn bounded_compute_wait_returns_real_when_build_finishes_in_time() {
        let standin = 0x7300_0003usize;
        let real_pso = 0x7300_0004usize;
        submit_lazy(standin, true, Box::new(move || real_pso));
        assert_eq!(resolve_compute_within(standin, Some(Duration::from_secs(4))), real_pso);
        assert_eq!(released(standin), real_pso);
    }
}

#[cfg(test)]
mod draw_wait_tests {
    use super::*;

    /// A graphics build that does not finish (an NVIDIA driver compile of minutes, #156) holds
    /// each draw only up to the limit and runs on a worker. With `resolve_wait` the draw waited
    /// for it without limit (building it itself when no worker had taken it), which held the
    /// title past its hang watchdog.
    #[test]
    fn each_draw_waits_for_a_graphics_build_only_within_the_limit_and_never_builds_it_itself() {
        let (standin, real) = (0x7410_0001usize, 0x7410_0002usize);
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (who_tx, who_rx) = mpsc::channel::<Option<String>>();
        submit_lazy(
            standin,
            false,
            Box::new(move || {
                let _ = who_tx.send(std::thread::current().name().map(str::to_string));
                let _ = go_rx.recv();
                real
            }),
        );
        let limit = Some(Duration::from_millis(300));
        let lost = standin_draws();
        for draw in 0..2 {
            // every draw gets the whole limit, not what an earlier draw left of it
            let t = Instant::now();
            assert_eq!(resolve_within(standin, limit), standin, "draw {draw} goes with the stand-in");
            let took = t.elapsed();
            assert!(took >= Duration::from_millis(290) && took < Duration::from_secs(5), "draw {draw} waited {took:?}");
        }
        assert!(standin_draws() >= lost + 2, "both draws counted as lost");
        let who = who_rx.recv_timeout(Duration::from_secs(5)).expect("the build went to a worker");
        assert!(who.as_deref().is_some_and(|n| n.starts_with("xcl-pso")), "built on {who:?}");
        assert!(is_pending(standin));
        // once built: the real pipeline
        go_tx.send(()).unwrap();
        assert_eq!(resolve_within(standin, Some(Duration::from_secs(10))), real);
        assert!(!is_pending(standin));
        let _ = released(standin);
    }

    /// Compute pipelines are still waited for (a skipped dispatch corrupts its results), and
    /// `XCL_PSO_WAIT_MS=0` (no limit) waits for graphics ones as before.
    #[test]
    fn compute_pipelines_and_no_limit_wait_as_before() {
        let (c, c_real) = (0x7410_0011usize, 0x7410_0012usize);
        submit_lazy(
            c,
            true,
            Box::new(move || {
                std::thread::sleep(Duration::from_millis(100));
                c_real
            }),
        );
        assert_eq!(resolve_within(c, Some(Duration::ZERO)), c_real);
        let (g, g_real) = (0x7410_0021usize, 0x7410_0022usize);
        submit_lazy(g, false, Box::new(move || g_real));
        assert_eq!(resolve_within(g, None), g_real);
        assert_eq!(resolve_within(0x7410_0031, Some(Duration::ZERO)), 0x7410_0031, "not a stand-in");
        let _ = released(c);
        let _ = released(g);
    }
}
