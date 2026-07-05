# Release Hardware Measurements

작성일: 2026-07-05

## Scope

This document records PR-U20c release-hardware Scenario A-E measurement state.
It intentionally separates the baseline run from final release approval.

## Baseline

- Baseline commit: `52f1cd1 Document remaining parallel PR contracts`
- Repository: `/Users/jr/Desktop/Projects/deppy-sijo`
- OS: macOS 26.4.1 build 25E253
- Kernel: Darwin 25.4.0 arm64
- Hardware model: unavailable; `sysctl -n hw.model` returned `Operation not
  permitted` in the sandbox.
- Hardware memory: unavailable; `sysctl -n hw.memsize` returned `Operation not
  permitted` in the sandbox.
- Build profile for automated baseline: Cargo test/dev profile.

## Baseline Commands

| Command | Result | Notes |
|---|---|---|
| `cargo run -p xtask -- perf-smoke` | Pass | Covered app perf harness unit tests, runtime backpressure-filtered tests, and hidden-session status detector smoke. |
| `sw_vers` | Pass | Reported macOS 26.4.1 build 25E253. |
| `uname -a` | Pass | Reported Darwin 25.4.0 arm64. |
| `sysctl -n hw.model` | Denied | Sandbox denied hardware query. |
| `sysctl -n hw.memsize` | Denied | Sandbox denied hardware query. |

## Scenario A-E Baseline Matrix

| Scenario | Baseline State | Final Measurement Requirement |
|---|---|---|
| A Empty app idle | Not measured in GUI; automated smoke passed only. | Record release-build RSS, CPU, idle repaint, and frame p95 for 60 seconds. |
| B 5 workspaces / 20 panes / 10 sessions | Not measured in GUI; compile/smoke only. | Record RSS, CPU, frame p95, and responsiveness under multi-workspace load. |
| C Hidden sessions and high output | Hidden-session smoke passed; RSS/CPU/frame p95 not measured. | Record hidden snapshot count, queue pressure, RSS, CPU, and active-pane frame p95. |
| D Folder tree 100k files | Not measured with generated 100k-file workspace. | Record folder tree expand/refresh latency, watcher queue behavior, RSS, and CPU. |
| E Remote slow consumer | Runtime backpressure tests included in smoke; long slow-client soak not run. | Record server RSS/CPU, outbound queue pressure, disconnect/degraded behavior, and local responsiveness. |

## Baseline Verdict

Baseline automated evidence is healthy, but final release approval remains
pending. `perf-smoke` success is not treated as Scenario A-E release approval.

## Final Measurement Slot

PR-U12c, PR-U18b, PR-U15c, and PR-U17b have landed.

- Final implementation commit under test:
  `8d18295 Add status confidence and user override flow`
- Final automated gate profile: Cargo test/dev profile.
- Final automated gate date: 2026-07-05.

## Final Automated Commands

| Command | Result | Notes |
|---|---|---|
| `cargo run -p xtask -- security-scan` | Pass | Boundary, dependency, secret-redaction, MCP audit/permission, and security unit gates passed. |
| `cargo run -p xtask -- perf-smoke` | Pass | App perf harness, runtime backpressure-filtered tests, and hidden-session status detector smoke passed. |
| `cargo run -p xtask -- i18n-check` | Pass | Required locale completeness, CJK path paste fixture, and notification message-id gates passed. |

## Scenario A-E Final Matrix

| Scenario | RSS | CPU | Frame p95 | Queue/Pressure | Gate |
|---|---:|---:|---:|---|---|
| A Empty app idle | Pending | Pending | Pending | Pending | Pending |
| B 5 workspaces / 20 panes / 10 sessions | Pending | Pending | Pending | Pending | Pending |
| C Hidden sessions and high output | Pending | Pending | Pending | Automated smoke passed; release soak pending | Pending |
| D Folder tree 100k files | Pending | Pending | Pending | Pending | Pending |
| E Remote slow consumer | Pending | Pending | Pending | Automated backpressure smoke passed; release soak pending | Pending |

## Final Verdict

Automated security, performance smoke, and i18n gates pass after the remaining
implementation PRs. Release approval remains pending because this sandbox did
not run the required GUI release-build Scenario A-E measurements on release
hardware. `perf-smoke` success is still not treated as full release approval.
