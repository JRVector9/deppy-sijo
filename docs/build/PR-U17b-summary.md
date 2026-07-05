# PR-U17b Build Summary

## Input Findings

- `SessionStatus` was a simple enum consumed by runtime, workspace UI, and
  notifications.
- Confidence/source metadata and user override UX were deferred.
- Existing `SessionStatusChanged` consumers must remain compatible.

## Scope

- Add additive status view metadata.
- Add user override command and workspace context-menu controls.
- Preserve existing `SessionStatusChanged` semantics.
- Add i18n keys for all visible override controls.

## Changes

- Added `SessionStatusView`, `StatusSource`, `StatusConfidence`, and
  `UserStatusOverride`.
- `StatusDetector` tracks source and exposes a status view with confidence.
- Added `RuntimeCommand::SetUserStatusOverride`.
- Added `RuntimeEvent::SessionStatusViewChanged` while keeping
  `SessionStatusChanged`.
- Runtime emits view events for detector changes, user overrides, and process
  exit.
- Workspace UI stores the status view and exposes override controls in the pane
  context menu.
- Warm replay coalesces `SessionStatusViewChanged` per session.

## Tests

- `cargo test -p session status_view` - pass
- `cargo test -p runtime user_status_override` - pass
- `cargo test -p deppy-sijo coalesce_dedups_status_view` - pass
- `cargo run -p xtask -- i18n-check` - pass

## Acceptance Criteria Check

- [x] Existing `SessionStatus` and `SessionStatusChanged` remain available.
- [x] Status source/confidence is available through `SessionStatusView`.
- [x] User override is exposed through RuntimeClient command/event flow.
- [x] Workspace UI has localized override controls.
- [x] Warm replay coalesces status view churn.

## Regression Risks

- Medium. Runtime command/event enums gained append-only variants, so lockstep
  remote/client versions are still assumed.
- Manual overrides affect display view only; legacy detector events remain raw.

## Resource Impact

- Adds one bounded per-worker override map keyed by live session.

## Security Impact

- No command/env/secret values added to status view or override events.

## I18n/CJK Impact

- Added required locale keys for source labels and override controls.

## Rollback Plan

- Remove status view/override command and event variants.
- Keep existing `SessionStatusChanged` path unchanged.

## Follow-up

- Decide whether manual overrides should produce OS notifications. Current PR
  avoids duplicate notification side effects.

