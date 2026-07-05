# PR-U20c Build Summary

## Input Findings

- PR-U20 required full Scenario A-E release-hardware RSS/CPU/frame-p95
  measurements.
- PR-U20b created the final-gate report but left release-hardware measurements
  pending.

## Scope

- Run the immediate baseline measurement available in the current environment.
- Record baseline environment, command results, and final-measurement slots.
- Do not mark unmeasured GUI/remote soak scenarios as passed.

## Changes

- Updated `docs/performance/final-gate.md` to reference the PR-U20c baseline.
- Added `docs/performance/release-hardware-measurements.md`.

## Tests

- `cargo run -p xtask -- perf-smoke` - pass
- `sw_vers` - pass
- `uname -a` - pass
- `sysctl -n hw.model` - denied by sandbox
- `sysctl -n hw.memsize` - denied by sandbox

## Acceptance Criteria Check

- [x] Baseline measurement was executed before implementation PRs.
- [x] Scenario A-E final measurement slots are documented.
- [x] `perf-smoke` is not treated as release approval.
- [x] Pending measurement reasons are explicit.
- [ ] Final Scenario A-E measurements remain pending until implementation PRs
  land and release hardware GUI/remote soak can be run.

## Regression Risks

None. Documentation-only baseline update.

## Resource Impact

No runtime impact. Baseline ran targeted smoke tests only.

## Security Impact

None.

## I18n/CJK Impact

None.

## Rollback Plan

Revert this documentation update and rerun PR-U20c baseline with a different
measurement format.

## Follow-up

- Re-run final Scenario A-E after PR-U12c, PR-U18b, PR-U15c, and PR-U17b.

