# PR-U03 Build Summary

## Input Findings

- v2.8 requires migration smoke coverage before release.
- Existing storage tests already cover current migrations and version upgrades, but there was no `xtask` gate command matching the PR-U plan.

## Scope

- Add an executable migration smoke gate.
- Do not change DB schema or migrations.
- Do not refactor repositories.

## Changes

- Added `cargo run -p xtask -- smoke-db-migrations`.
- The gate runs storage migration smoke tests, including latest DB reopen, migration idempotence, backup creation, all-version prefix smoke, and v8/v9/v10 upgrade tests.

## Tests

- `cargo run -p xtask -- smoke-db-migrations` - pass

## Acceptance Criteria Check

- [x] Empty/current DB migration smoke runs.
- [x] Older version migration smoke runs.
- [x] Migration backup/idempotence tests run.
- [x] No DB schema change.
- [ ] Store-by-store CRUD gate remains incremental; existing tests cover current repositories but not a separate release report.

## Regression Risks

Low. This PR adds an orchestration command around existing tests.

## Resource Impact

None outside test execution.

## Security Impact

Keeps migration gate available before release.

## I18n/CJK Impact

None.

## Rollback Plan

Remove the `smoke-db-migrations` branch from `xtask`.

## Follow-up

- Add repository-specific smoke grouping if store crates are further split.
