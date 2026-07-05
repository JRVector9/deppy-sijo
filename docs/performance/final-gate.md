# PR-U20 Final Performance Gate

작성일: 2026-07-05
작성 범위: PR-U20b Scenario A-E release-gate report, PR-U20c baseline update

## Verdict

Release verdict: **Not approved yet**

Reason: automated performance smoke passes for the PR-U20c baseline, but this
Codex sandbox did not run the GUI/remote soak measurements required to claim
final RSS/CPU/frame-p95 approval for Scenarios A-E.

## Environment

- Repository: `/Users/jr/Desktop/Projects/deppy-sijo`
- Baseline commit under test: `52f1cd1 Document remaining parallel PR contracts`
- PR-U20c baseline adds measurement documentation only; no runtime behavior was
  changed by the baseline measurement commit.
- OS: macOS 26.4.1 build 25E253
- Kernel: Darwin 25.4.0 arm64
- Hardware details: `sysctl` hardware queries were denied in the sandbox.
- Build profile used for automated smoke: Cargo test/dev profile.

## Automated Evidence

Latest baseline run:

- `cargo run -p xtask -- perf-smoke` - pass

The smoke command covered:

- app performance harness unit tests
- runtime backpressure-filtered tests
- hidden-session status detector smoke

Related gates from the same working set:

- `cargo fmt --check` - pass
- `cargo clippy --workspace --all-targets` - pass
- `cargo check --workspace --all-targets` - pass
- `cargo test --workspace --no-run` - pass

## Scenario Results

| Scenario | Requirement | Automated Evidence | Measured Result | Gate |
|---|---|---|---|---|
| A | Empty app idle RSS/CPU and idle repaint | None beyond compile/smoke | Not measured in this sandbox | Pending |
| B | 5 workspaces, 20 panes, 10 sessions, 2 visible panes | Current mux/runtime tests compile; no full GUI load run | Not measured in this sandbox | Pending |
| C | 10 hidden sessions, 3 high-output sessions | `perf-smoke` validates hidden-session harness shape and hidden snapshot smoke | RSS/CPU/frame p95 not measured | Pending |
| D | Folder tree 100k files | Existing file-tree tests and PR-U05b ignore/listing hardening exist | 100k-file GUI/folder-tree run not measured | Pending |
| E | Remote slow consumer | `perf-smoke` exercises runtime backpressure tests | Long slow-client soak not measured | Pending |

Detailed baseline notes are recorded in
`docs/performance/release-hardware-measurements.md`.

## Required Manual Measurement Procedure

Run these on the release target machine, preferably with a release build and no
debugger attached.

1. Build release binary:
   `cargo build --release -p deppy-sijo`

2. Enable frame stats:
   `DEPPY_FRAME_STATS=1 target/release/deppy-sijo`

3. Scenario A:
   Start with an empty/default workspace and leave idle for 60 seconds. Record
   RSS, CPU, and whether frame stats continue appearing while idle.

4. Scenario B:
   Create or restore 5 workspaces, 20 panes, 10 sessions, with 2 visible panes.
   Interact for 60 seconds. Record RSS, CPU, and frame p95 logs.

5. Scenario C:
   Start with `DEPPY_PERF_HARNESS=1 DEPPY_FRAME_STATS=1` and run for at least
   120 seconds. Record RSS, CPU, frame p95, and whether hidden panes create
   viewport snapshots.

6. Scenario D:
   Open a generated workspace containing 100k files and exercise folder expand,
   collapse, refresh, and drag path insert. Record UI stalls, RSS, CPU, and
   watcher queue behavior.

7. Scenario E:
   Connect a remote client that intentionally stops or slows reads. Record
   server RSS/CPU, outbound queue behavior, disconnect/degraded behavior, and
   local runtime responsiveness.

## Acceptance Criteria

- RSS/CPU targets are explicitly recorded for the release machine.
- Idle app has no recurring repaint loop.
- Active pane frame time p95 satisfies the release target.
- Hidden panes do not create viewport snapshots.
- Output/remote queues do not grow without bound.
- Slow remote client cannot stall local runtime.
- CJK path DnD and terminal copy/paste remain covered by existing gates.

## Current Release Blockers

- Scenario A-E RSS/CPU/frame-p95 numbers are not captured in this environment.
- Scenario D still needs an actual 100k-file workspace run.
- Scenario E still needs a slow-client soak run.

## Follow-up

- Add an automated `xtask perf-report` harness when GUI scenario setup can be
  controlled headlessly.
- Re-run this document on release hardware and replace Pending rows with measured
  Pass/Fail values.
