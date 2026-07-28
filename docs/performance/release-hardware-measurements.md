# Release Hardware Measurements

작성일: 2026-07-28
대상 커밋: `3cb6f38187465123f82ad648395e1b3e25aae9d1`

## Scope

This document records the latest SF06 release-hardware measurement state. Automated tests prove bounded contracts but do not substitute for a release-build wall-clock resource slope.

## Host

- OS: macOS 26.5.2 build 25F84
- Kernel: Darwin 25.5.0 arm64
- Repository: `/Users/jr/Desktop/projects/deppy-sijo`
- Integration worktree: `/tmp/deppy-sf06-integration`
- Release binary: Not built for this measurement

## Measurement Attempt

| Step | Result | Notes |
|---|---|---|
| Detect running Deppy instances | Blocked | An existing user-owned debug `deppy-sijo` instance was active |
| `scripts/render-bench.sh build` | Not run | Avoided loading the machine for a measurement that could not pass preflight |
| 30-minute `wgpu switch` with `USE_HARNESS=1` | Not run | Repository preflight rejects concurrent instances to prevent lock and CPU contamination |
| Stop existing process | Not performed | User process ownership was preserved |

## SF06 Resource Matrix

| Metric | Baseline | Peak | End | Post-shutdown | Trend | Gate |
|---|---:|---:|---:|---:|---|---|
| App RSS | Pending | Pending | Pending | Pending | Pending | Pending |
| Child RSS | Pending | Pending | Pending | Pending | Pending | Pending |
| CPU | Pending | Pending | Pending | Pending | Pending | Pending |
| Threads | Pending | Pending | Pending | Pending | Pending | Pending |
| Open file descriptors | Pending | Pending | Pending | Pending | Pending | Pending |
| Open sockets | Pending | Pending | Pending | Pending | Pending | Pending |
| Pending replay items | Pending | Pending | Pending | Pending | Static cap 1,024/workspace only | Pending |
| Push jobs | Pending | Pending | Pending | Pending | Static fresh+retry cap 256 only | Pending |
| Frame p95 | Pending | Pending | Pending | N/A | Pending | Pending |

No number in this matrix is inferred from unit tests, allocator behavior, or earlier benchmark runs.

## Scenario A-E Matrix

| Scenario | RSS | CPU | Frame p95 | Queue/Pressure | Gate |
|---|---:|---:|---:|---|---|
| A Empty app idle | Pending | Pending | Pending | Pending | Pending |
| B 5 workspaces / 20 panes / 10 sessions | Pending | Pending | Pending | Pending | Pending |
| C Hidden sessions and high output | Pending | Pending | Pending | Replay cap tests pass; physical slope pending | Pending |
| D Folder tree 100k files | Pending | Pending | Pending | Pending | Pending |
| E Remote slow consumer | Pending | Pending | Pending | Remote tests pass; long soak pending | Pending |

## Automated Context

- Focused SF01-SF05 integration tests: 150 passed, 0 failed across the eight recorded commands.
- Workspace check, changed-package strict Clippy, dependency gate, boundary gate, performance smoke, and diff check passed.
- Exact workspace strict Clippy and BG01 deterministic gate did not pass because of reproduced pre-existing lint/format baseline failures.
- These results establish deterministic bounded behavior only; they do not establish physical memory release or long-run socket/thread stability.

## Required Procedure

After all other Deppy instances are closed:

```bash
scripts/render-bench.sh build
USE_HARNESS=1 SECS=1800 SAMPLE_INTERVAL=60 scripts/render-bench.sh run wgpu switch 5
```

Use the benchmark CSV/JSONL plus host process tools to record one-minute samples. Record unavailable internal queue metrics as Pending unless an existing approved measurement surface exposes them. Verify worker, thread, fd, and socket counts return to baseline after normal shutdown.

## Verdict

Release hardware approval is **Pending**. No 30-minute run was executed, and SF06 deterministic approval is also blocked until the exact workspace gates pass.
