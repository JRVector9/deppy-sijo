# PR-U09b Build Summary

## Input Findings

- PR-U09 remained partial: secret-like plain env persistence was blocked, but a
  production env profile still only showed an inline warning.
- Code check confirmed `AgentsUi::run` sent `RuntimeCommand::SpawnAgent`
  directly after the run button click.

## Scope

- Add a production profile confirmation gate before agent spawn.
- Do not change env persistence, secret resolution, MCP proxy routing, or runtime
  command shape.

## Changes

- Added pending production-run state to `AgentsUi`.
- A production run now opens a confirmation window before `SpawnAgent` is sent.
- Confirmation text includes the agent and profile names but no env values or
  credential contents.
- Non-production profiles and no-profile runs keep the existing direct run path.
- Added localized confirmation strings for required locales and ko-KR.

## Tests

- `cargo fmt --check` - pass
- `cargo check --workspace --all-targets` - pass
- `cargo clippy --workspace --all-targets` - pass
- `cargo test -p deppy-sijo production_profile은_실행전_confirm_대상이다` - pass
- `cargo run -p xtask -- i18n-check` - pass

## Acceptance Criteria Check

- [x] Production env profile execution requires explicit confirmation.
- [x] Cancel path leaves runtime untouched.
- [x] Confirm path sends the same runtime command path as before.
- [x] No secret values are displayed in the confirmation UI.
- [x] Locale key completeness remains intact.

## Regression Risks

- Low. The change is isolated to agent launch UI state.
- Main risk is an extra click for intentionally selected production profiles.

## Resource Impact

No background worker, watcher, or rendering policy changes.

## Security Impact

Improves safety by preventing accidental production profile launches. Secret
values remain credential ids until runtime spawn-time resolution.

## I18n/CJK Impact

New UI strings are routed through `i18n::Catalog` and required locale
completeness still passes.

## Rollback Plan

Revert the `pending_production_run` state, confirmation window, locale keys, and
test from this PR.

## Follow-up

- PR-U10b should handle scoped MCP env injection separately.
