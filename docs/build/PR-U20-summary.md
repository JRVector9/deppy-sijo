# PR-U20 Build Summary

## Input Findings

- PR-R08 requires a final performance gate before release.
- Existing code had env-based frame stats and perf harness tests, but no `xtask perf-smoke` command.

## Scope

- Add an automated smoke command for currently testable performance/backpressure invariants.
- Do not claim full RSS/CPU final gate completion yet.

## Changes

- Added `cargo run -p xtask -- perf-smoke`.
- The smoke command runs:
  - app perf harness tests
  - runtime backpressure-filtered tests
  - runtime hidden snapshot/status detector smoke

## Tests

- `cargo run -p xtask -- perf-smoke` - pass

## Acceptance Criteria Check

- [x] Automated performance smoke command exists.
- [x] Hidden status detector snapshot smoke runs.
- [x] Existing remote/runtime backpressure tests are callable through the gate.
- [ ] Full Scenario A-E RSS/CPU/p95 measurements are not completed.
- [x] `docs/performance/final-gate.md` exists via PR-U20b and records pending
  release-hardware measurements.

## Regression Risks

Low. This PR adds a gate runner around existing tests.

## Resource Impact

No runtime impact. The command compiles and runs targeted tests.

## Security Impact

None.

## I18n/CJK Impact

None.

## Rollback Plan

Remove the `perf-smoke` branch from `xtask`.

## Follow-up

- Implement PR-U12/U13/U14/U15/U19 and PR-U26 before using PR-U20 as a release approval gate.
- Execute the `docs/performance/final-gate.md` manual Scenario A-E measurements
  on release hardware and replace Pending rows with measured values.
