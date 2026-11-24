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

Notes
- Second and later runs: the coherent window is the learned 4 095 MB (#284); the account prompt is
  up within ~25 s and the menus hold 30 fps.
- The artwork intro after the account prompt runs at ~7 fps for ~60–90 s (as reported in #300's
  thread), then 30 fps.
- No watchdog (`+0x2215c35`, #232) and no device removal in ~27 minutes of runs; no pipeline wait
  warnings from #533 / #267 yet (no world loading reached).
- The window title reports `57 806 pipelines queued` (lazy pipelines, built at first use).
