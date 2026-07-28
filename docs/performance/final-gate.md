# SF06 Final Stability Gate

작성일: 2026-07-28
검증 커밋: `3cb6f38187465123f82ad648395e1b3e25aae9d1`

## Verdict

Release verdict: **Not approved**

SF01-SF05 focused integration tests, workspace check, dependency/boundary gates, performance smoke, and changed-package strict Clippy pass. The frozen workspace Clippy and BG01 commands remain red on pre-existing repository baseline findings, and the required 30-minute release measurement was not run. SF06 must not be reported as passed and SSH00 must not start yet.

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
| Workspace strict Clippy | Fail | Existing terminal test lint at `crates/terminal/src/alacritty_backend.rs:993`; reproduced on `main` |
| Changed-package strict Clippy | Pass | `deppy-sijo`, `web-remote`, `persist`, `runtime`, `storage` exit 0 |
| Dependency gate | Pass | `cargo run -p xtask --locked -- check-deps` exit 0 |
| Boundary gate | Pass | `cargo run -p xtask --locked -- check-boundary` exit 0 |
| Performance smoke | Pass | `cargo run -p xtask --locked -- perf-smoke` exit 0 |
| BG01 deterministic gate | Fail | Existing repo-wide rustfmt drift; `cargo fmt --all -- --check` also fails on `main` |
| Diff check | Pass | `git diff --check` exit 0 |

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

Before SSH00, rerun and pass the exact deterministic commands listed in the SF06 design specification without suppressions or waivers.
