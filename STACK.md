# pc-stack

Upstream `ivall/xcl` main plus open PRs picked for a Windows PC with an **NVIDIA RTX 40-series GPU
(32-lane waves) and an Intel CPU**. Rebuilt on newer upstream main as PRs land; each entry
says why it is here and drops out once upstream merges it.

| PR | What | Why on this machine |
|---|---|---|
| [#533](https://github.com/ivall/xcl/pull/533) | compute pipeline waits are bounded and built on workers | slow NVIDIA driver compiles tripped the title's watchdog (#232) |
| [#267](https://github.com/ivall/xcl/pull/267) | a draw waits at most 4 s for a graphics pipeline | same watchdog, graphics side |

Merge notes: #533 and #267 both edit `pso_async.rs`; `resolve_for_indirect` keeps #533's
bounded compute path and uses #267's `resolve_for_draw` for graphics. Both test modules kept.

Dropped once merged upstream: #300 (title timer, choppy audio).

## Checked

- `cargo test --release --workspace`: all pass (RTX 4070 Ti, driver 32.0.16.1062, DXC 1.8.2502).
- Runs: see [QA.md](QA.md).

## Setup notes for this kind of machine

- Page file: the title plus XCL commit 15–40 GB; with 32 GB RAM a 32–48 GB page file keeps it
  off the commit limit. `xcl shaders --precompile` also runs about twice as fast with the room.
- `xcl shaders <shaders.rpf> --precompile` (DXC cache) is worth running once. `--pipelines`
  is not on NVIDIA: the driver builds about one large pipeline at a time whatever
  `XCL_PIPELINE_THREADS` says (~325 in 65 min here).
- Always `XCL_NETWORK=0`.
