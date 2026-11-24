# QA runs: the newer retail title on pc-stack

Machine: RTX 4070 Ti 12 GB (driver 32.0.16.1062, 32-lane waves), i5-13600K (no SSE4a / CLZERO:
emulated), 32 GB RAM + 32 GB page file, Windows 11 26300, DXC 1.8.2502 (SDK 10.0.26100).
Package 1.0.2.1, `XCL_NETWORK=0`, `XCL_ACTIVITY=1`, frame dumps on (no NVIDIA driver crash from
`XCL_FRAME_DUMP` in any run, cf. #499). Input: `XCL_PRESS_A_AT` / `XCL_PRESS_AT` only.

DXIL cache filled beforehand with `xcl shaders --precompile` (22 800 / 26 503 programs; the last
~3 700 are the largest and took ~18 min per 200 on this CPU, so the run was stopped there).

| run | build | length | input | furthest screen | fps | end |
|---|---|---|---|---|---|---|
| 1 | `e8c33a2` | 300 s | A at 14, 17, 21 s | account prompt (presses came before it was up) | ~9 (first run: 512 MB coherent window) | `XCL_RUN_SECONDS`, no crash, 2 783 frames |
| 2 | `e8c33a2` | 360 s | A at 30–130 s | first-boot settings (UI text size, subtitles) | 7 in the artwork intro, then 30 | clean, 8 887 frames |
| 3 | `e8c33a2` | 600 s | A every 4 s from 120 s | legal notices (A alone cannot reach "Next") | 30 | clean, 16 106 frames |
| 4 | `e8c33a2` | ~380 s | DOWN, DOWN, A every 6 s from 30 s | legal terms & policies (privacy policy / terms of service checkboxes) | 30 | stopped by hand before the agreement screen was answered |
| 5 | `e8c33a2` | 368 s | as 4 until 147 s; terms ticked and confirmed at 175 s; A every 5 s | **world loading** (spinner, DirectStorage streaming 60–350 MB / 2 s) | 30 → ~1 → 0 | **title watchdog** `+0x23cf683` (null write via `+0x2215c35`) at 363 s |
| 6 | `e8c33a2`, waits 1 ms | 347 s | as 5 (terms already saved) | world loading from ~150 s | 30 → mostly 0 | watchdog at 343 s, VRAM 14.4 GB |
| 7 | `diag/list-101` | 215 s | as 6 | world loading | 30 → 0 | watchdog at 215 s, VRAM 13.7 GB |
| 8 | `exp/series-s` (+ #242), `device_type = "lockhart"` | 40 s | as 6 | account prompt | 30 | title assert `0x80000003` at `+0xb18de7` ~3 s after the account is picked |
| 9 | as 8, fresh `XCL_DATA_ROOT` | 39 s | as 5 | account prompt | 30 | same assert, same address: not the saved settings |
| 10 | `d44e8e8` (main with #574, #636, in-place detiling + #533 / #267) | 132 s | as 6 | world loading from ~95 s (earlier than before) | 30 → 0 | **stack overflow** `0xc00000fd` at `+0x43c042` at 118 s, not the watchdog; just before: `Wait64BitValueX timed out … want 0x800 … memory holds Some(0)` (the never-written counter of #505); VRAM 14.1 GB, target pages 9.9 GB |
| 11 | `d44e8e8` | 124 s (then hung on exit) | as 6 | world loading | 30 → 0 | watchdog `+0x23cf683` again (run 10's stack overflow did not repeat) |
| 12 | `6d67c35` (+ #634: driver compute blobs on disk) | 138 s | as 6 | story loading screen (artwork) at ~90 s | 30 → 0 | **no watchdog**: `Wait64BitValueX … want 0x802 … memory holds Some(0)`, then read of `0xffffffffffffffff` at `+0x81cb67` – the title reading data the GPU never wrote (the out-of-order wait the XCL devs describe) |
| 13 | `exp/wait-order` (`08c4eb7` + #708) | 102 s | as 6 | world loading from ~85 s | 30 → 0 | **0 `Wait64BitValueX` timeouts**; null writes at 90.6 s (`vcruntime140+0x14d3` write 0x20, then two more) ~2.5 s after the first `#101`; VRAM 14.6 GB |
| 14 | same | 120 s | as 6 | world loading from ~85 s | 30 → 0 | **0 `Wait64` timeouts** again; watchdog `+0x23cf683` at 116 s; VRAM 14.2 GB, target pages 9.7 GB. Run 13's null writes did not repeat |

| 15 | `exp/page-budget` (#708 + #640), `XCL_PAGE_BUDGET_MB=6144` | 130 s | as 6 | world loading from ~80 s | 30 → 0 | **video memory 10.3 GB (under the card)**, target pages 5.5 GB, 7 316 textures evicted, 42 mapped again; 0 `Wait64` timeouts; watchdog at 127 s with 10 compute pipelines "still building after 4000 ms" from 101 s |
| 16 | same | 132 s | as 6 | world loading from ~75 s | 30 → 0 | 546 shader sets built ahead (run 15: 525); 13 compute pipelines still building at ~103 s; VRAM 10.4 GB; watchdog at 129 s |
| 17 | `fa6e820` (pc-stack e915150 + #708 + #640), budget 6 GB, 99 driver blobs precompiled offline (`feat/precompile-driver-blobs`) | 148 s | as 6 | world loading from ~75 s | 30 → 0 | **32 compute pipelines from kept driver blobs**, 12 still building past 4 s (run 16: 13); VRAM 10.3 GB; watchdog at ~145 s (runs 15–16: ~128 s) |

#708 removes the never-written-counter waits on this machine (runs 13–16; run 12 had them), and #640's
budget keeps video memory under the 12 GB card (runs 15–16). What is left on an RTX 4070 Ti is the
first build of the world's large compute pipelines: #690 builds a pipeline ahead only once the title
creates it, which here is seconds before its first use, and the slowest take the NVIDIA driver longer
than the title's ~25 s stall allowance, so they never finish, never reach the driver cache (#634), and
the same ones stall every run.

Series S profile (runs 8–9): at the menus video memory is 7.3 GB with 1.9 GB of target pages
(Series X: ~6 GB / ~5 GB later), but the title asserts at `+0xb18de7` right after the account
prompt, every time, on current main + #242. #242's own Series S run was on an older base with
#203; not followed further here.

## Run 5: world loading and the watchdog

- Terms confirmed → world loading from ~215 s. Local video memory climbs from 8.7 GB (224 s) past the
  card's 12 GB (12.3 GB at 284 s; target pages 5.1 → 8.4 GB), commit 22 → 33 GB: the jump #560 describes.
- 322.5 s: first call of `ID3D12GraphicsCommandList10::#101` (stub, not on PC). From here no
  present, submit or draw until ~340 s (as in #505: the watchdog after the ListX #101 stub).
- 342–364 s: #533's bounded compute waits fire: 14 × "still building after 4000 ms", **five of them
  back to back on one thread** (30908: 347, 351, 355, 359, 364 s). Each wait is bounded, the
  sum is not: that thread made no progress for 20 s+, on top of the 18 s gap before it.
- 363.2 s: the title's watchdog.

So on NVIDIA the per-pipeline bound of #533 is not enough by itself: consecutive first binds on one
thread add up (the N × limit trade-off noted in #267).

## Run 6: no pipeline waits — same watchdog

`XCL_COMPUTE_PSO_WAIT_MS=1 XCL_PSO_WAIT_MS=1`. With the terms saved, world loading starts by ~150 s.
Zero-present gaps remain (156–168, 176–186, 200–210, 228 s …) with no pipeline wait in them; local video
memory 10 → 14.4 GB on the 12 GB card (target pages 9.8 GB), commit 26 → 42 GB. Watchdog at 343 s,
~95 s after the first `#101`. Pipeline waits are not the only cause of the stall on this machine.

## Run 7: what command list #101 takes (branch `diag/list-101`)

A logging stand-in for the newer layout's #101 (answers 0 like the stub). Calls come in pairs on one list:

```
#101 call 1: 0x2 0x1 0x1 0xbf00011d 0x0
#101 call 2: 0x2 0x0 0x0 0x0 0x0
#101 call 5: 0x2 0x1 0xffffffff00000000 0x11d 0x0
#101 call 6: 0x2 0x0 0x0 0x0 0x0
```

Only the first two arguments are stable: `2` (the compute list type's value) and on / off. The rest
change from call to call (leftover registers). It writes no memory the title then waits on, so it is
an unlikely source of the never-written counter #505 reports; a toggle answered 0 is probably
harmless. First call at 90 s here (during loading, long before the stall). Watchdog at 215 s after
~27 s without a present; local video memory 13.7 GB (target pages 9.6 GB) on the 12 GB card, 41 GB commit.

## Where it stands

Furthest point: world loading, about 2–4 min in. Every run that reaches the world ends at the title's
watchdog after 20–30 s without a present, with local video memory 1.5–2.5 GB past the card. The likely
causes on a 12 GB NVIDIA card are the video-memory jump at world start (#560, #320) together with slow
first pipeline builds (#156, #533, #267). `#101` is not it.

Notes
- Second and later runs: the coherent window is the learned 4 095 MB (#284); the account prompt is
  up within ~25 s and the menus hold 30 fps.
- The artwork intro after the account prompt runs at ~7 fps for ~60–90 s (as reported in #300's
  thread), then 30 fps.
- No watchdog (`+0x2215c35`, #232) and no device removal in ~27 minutes of runs; no pipeline wait
  warnings from #533 / #267 yet (no world loading reached).
- The window title reports `57 806 pipelines queued` (lazy pipelines, built at first use).

## Precompiling the driver's blobs (branch `feat/precompile-driver-blobs`, same idea as #744)

`xcl shaders <shaders.rpf> --precompile --only cs --pipelines --pipeline-min-kb 256` now keeps each
build's driver blob where a title's compute pipeline looks for it (#634's `driver\`). Blobs the
title wrote in earlier runs are found under the same names ("kept already"), so the DXIL matches.
After 99 blobs (75 builds, ~25 per 6–10 min on the 4070 Ti), run 17 made 32 pipelines from them;
the world's stalling programs were not among them yet, so the precompile is left to finish.
