# Measurement evidence (2026-09-27)

Latest product source: `78f054b9`, feature worktree. Product source was not modified.

- `live-debug-summary.json`: 31 samples/30.23s of the already-running debug PID71086; not idle, not latest release. Includes binary source uncertainty and vmmap units.
- `sample-path-summary.json`: selected inclusive stack counts from5s OS sampling. Main thread1977 observations; no CPU attribution percentages. Nested Context::tessellate closure counts are excluded from the180 call-entry count.
- `latest-backend-renderer.log`: production backend/renderer release probe rerun. Allocation request traffic includes realloc and uses System counting allocator; not app RSS.
- `durable-probe-replay.log`: copied probe successfully rerun from this directory with `--locked`, exit0; timings differ, correctness/allocation/shape counts agree.
- `probe/`: standalone workspace, exact lockfile and linked product dependencies. No app launch.
- `release-build.log`: latest release build exit0; no GUI launched.
- `prepared_gui_runner.py`: eight isolated normal-mimalloc GUI scenarios,18s each. **Prepared only; not executed. Requires explicit app-launch permission.** Current app is never stopped. Do not use old render-bench script which overrides HOME and refuses/kills another app.

Reproduce the headless probe from the feature worktree:

```sh
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-cloud-agent-mcp/target cargo run --release --locked --manifest-path docs/reviews/measurements/2026-09-27-render-memory-ime/probe/Cargo.toml
```

Original raw OS logs: `/tmp/deppy-live-sample-20260927.txt`, `/tmp/deppy-live-vmmap-20260927.txt`. Full stack logs were kept outside Git to avoid an oversized report. JXA read-only window inventory attempt failed because its CFArray bridge was not a JS array; no GUI state or native IME results were inferred from that failed attempt.
