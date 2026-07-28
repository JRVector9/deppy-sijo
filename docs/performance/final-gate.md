# SF06 Final Stability Gate

작성일: 2026-07-28
검증 커밋: `ff8e486c44fd0bfc9e29df0ef718ae930b0a34e0`

## Verdict

Deterministic verdict: **Approved**

Release hardware verdict: **Pending**

SF01-SF05 focused integration tests and every frozen deterministic command pass at the validation commit. The required 30-minute release measurement was not run because a user-owned Deppy instance remained active, so physical resource stability is not yet approved. The deterministic prerequisite for design-only SSH00 is satisfied.

## Environment

- Repository: `/Users/jr/Desktop/projects/deppy-sijo`
- Integration worktree: `/tmp/deppy-sf06-integration`
- Branch: `codex/sf06-integration-evidence`
- OS: macOS 26.5.2 build 25F84
- Kernel: Darwin 25.5.0 arm64
- Automated profile: Cargo test/dev profile
- Release measurement profile: Not run

## Deterministic Evidence

| Gate | Result | Evidence |
|---|---|---|
| Workspace check | Pass | `cargo check --workspace --all-targets --locked` exit 0 |
| Workspace strict Clippy | Pass | `cargo clippy --workspace --all-targets --locked -- -D warnings` exit 0 |
| Dependency gate | Pass | `cargo run -p xtask --locked -- check-deps` exit 0 |
| Boundary gate | Pass | `cargo run -p xtask --locked -- check-boundary` exit 0 |
| Performance smoke | Pass | `cargo run -p xtask --locked -- perf-smoke`: 16 exact smoke tests |
| BG01 deterministic gate | Pass | Structural, security, failure, performance smoke, workspace regressions and doc-tests |
| Diff check | Pass | `git diff --check` exit 0 |

## Gate Remediation Evidence

- `e00967d`: applied the repository's exact rustfmt output and replaced the terminal test's manual repeat/take sequence with `std::iter::repeat_n`.
- `c39daae`: initialized the named sidebar font before the kittest assertion frame. Installing fonts after `Harness::new_ui` or inside its first-frame closure both failed because that frame had already resolved fonts.
- `ff8e486`: prevented dashboard shutdown wake loss by mutating the stop predicate while holding the condvar mutex. The original full BG01 attempt was interrupted only after a process sample proved `JoinHandle::join` waiting on a worker parked in `Condvar::wait`; the exact regression test then passed 100/100 and `web-remote` passed 153/153.

## Focused Results

| Area | Result |
|---|---|
| App replay and Git-label retention | Pass: 3 tests total, 0 failed |
| Web-push admission/retry/in-flight behavior | Pass: 42 tests, 0 failed |
| Persisted restore cwd projection | Pass: 1 test, 0 failed |
| Runtime restore atomicity | Pass: 8 tests, 0 failed |
| Remote protocol/deadline/liveness | Pass: 60 tests, 0 failed |
| Session log scan/GC | Pass: 25 tests, 0 failed |
| Scrollback archive scan/GC | Pass: 11 tests, 0 failed |

## Release Scenario Matrix

| Scenario | Requirement | Automated Evidence | Physical Measurement | Gate |
|---|---|---|---|---|
| A Empty app idle | RSS/CPU, idle repaint, frame p95 | Existing perf smoke only | Not run | Pending |
| B Multi-workspace load | 5 workspaces, 20 panes, 10 sessions | Workspace/replay focused tests | Not run | Pending |
| C Hidden high-output | Hidden/warm churn, queue caps, RSS slope | Replay cap and perf harness smoke | 30-minute run not started | Pending |
| D Folder tree 100k | UI latency, watcher pressure, RSS/CPU | No SF06 production change | Not run | Pending |
| E Remote slow consumer | Queue pressure, disconnect, local responsiveness | 60 remote tests and perf smoke | Long slow-consumer run not run | Pending |

## Why Release Measurement Is Pending

The existing benchmark procedure rejects concurrent Deppy instances. A user-owned debug `deppy-sijo` process was active when SF06 reached the measurement step, so the agent did not stop it or launch a competing release instance. No release build, 30-minute lifecycle run, or physical resource sample was claimed.

## Required Next Commands

Run only after other Deppy instances are closed:

```bash
scripts/render-bench.sh build
USE_HARNESS=1 SECS=1800 SAMPLE_INTERVAL=60 scripts/render-bench.sh run wgpu switch 5
```

Then record RSS, child RSS, threads, fd/socket counts, pending replay count, push job count, and post-shutdown return-to-baseline. If an existing measurement surface cannot expose a metric, leave that metric Pending rather than inferring it.

The exact deterministic commands are green without suppressions or waivers. Only the release hardware procedure remains.
