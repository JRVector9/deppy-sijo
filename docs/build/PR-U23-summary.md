# PR-U23 Build Summary

## Input Findings

- PR-R09 required runtime-facing user messages to use `message_id + args` rather than pre-rendered localized strings.
- PR-U22 migrated core UI labels but intentionally deferred runtime/event message payloads.

## Scope

- Structure runtime failure messages crossing the `RuntimeEvent` boundary.
- Keep terminal output, paths, command names, env keys, MCP names, and session titles untranslated.
- Avoid a broad notification service or DB schema change.

## Changes

- Added `runtime::MessagePayload` and `runtime::MessageArg`.
- Changed `RuntimeEvent::SpawnFailed` from `message: String` to `message: MessagePayload`.
- Runtime now emits stable IDs for shell/agent spawn failures, agent secret resolve failures, and split target failures.
- UI renders runtime payloads through `i18n::Catalog` at display time.
- Notification status items now store stable `notification.session.*` message IDs and render them through the catalog for in-app and OS notifications.
- Added required locale keys for runtime failure and notification status messages.

## Tests

- `cargo test -p runtime spawn_failed_payload -- --nocapture`
- `cargo test -p deppy-sijo running_복귀는_알림_아님 -- --nocapture`
- `cargo run -p xtask -- i18n-check`

## Acceptance Criteria Check

- [x] Runtime failure events carry `message_id + args`.
- [x] Runtime no longer emits Korean rendered prose in `SpawnFailed`.
- [x] Remote/postcard event roundtrip covers `MessagePayload`.
- [x] Notification status labels use stable message IDs.
- [x] Terminal output, paths, command names, env keys, MCP server/tool names, and session titles remain raw values.
- [x] No DB schema change or broad notification persistence rewrite.

## Regression Risks

- `RuntimeEvent::SpawnFailed` wire shape changes, so mixed old/new remote clients are not compatible for that event. Current app/runtime deployment is expected to be lockstep.
- Diagnostics may contain lower-level non-localized OS or library text; the stable UI message remains the message ID.

## Resource Impact

- Message payloads allocate small vectors only on failure/status notification paths.

## Security Impact

- Secret values remain excluded from failure payloads.
- Agent secret resolve failure includes credential id and diagnostic only.

## I18n/CJK Impact

- Runtime and notification status messages now participate in required locale completeness checks.

## Rollback Plan

- Revert `RuntimeEvent::SpawnFailed` to `String` and restore UI direct message display. Locale catalog additions can remain harmlessly unused.

## Follow-up

- PR-U24 should run layout/pseudo-locale/CJK gate checks against the UI and runtime message path.
