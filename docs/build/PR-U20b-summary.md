# PR-U20b Build Summary

## Input Findings

- `docs/build/PR-U20-summary.md` marked full Scenario A-E RSS/CPU/frame-p95
  measurements and `docs/performance/final-gate.md` as pending.
- `docs/update/partial-backlog-code-triage.md` classified PR-U20b as a
  release-gate measurement/report task, not app-code hardening.

## Scope

- Create the final performance gate report.
- Run currently automated performance smoke before documenting results.
- Do not add broad resource-heavy app code or change runtime behavior.

## Changes

- Added `docs/performance/final-gate.md`.
- Recorded the PR-U20 Scenario A-E requirements, automated evidence, current
  gaps, and release-gate verdict.
- Documented manual measurement procedure for empty app, multi-workspace/pane
  load, hidden high-output sessions, 100k-file folder tree, and remote slow
  consumer.

## Tests

- `cargo run -p xtask -- perf-smoke` - pass

## Acceptance Criteria Check

- [x] `docs/performance/final-gate.md` exists.
- [x] Scenario A-E are covered in the report.
- [x] Automated evidence is recorded.
- [x] The report does not falsely approve unmeasured GUI/remote soak scenarios.
- [ ] Final release approval remains pending until release-hardware measurements
  replace the Pending rows.

## Regression Risks

Low. Documentation-only PR.

## Resource Impact

No runtime impact. `perf-smoke` compiles and runs targeted tests only.

## Security Impact

None.

## I18n/CJK Impact

No UI string changes.

## Rollback Plan

Remove `docs/performance/final-gate.md` and this build summary if a different
release-gate format is adopted.

## Follow-up

- Run the documented Scenario A-E measurements on release hardware.
- Add an automated `xtask perf-report` if headless GUI setup becomes available.
