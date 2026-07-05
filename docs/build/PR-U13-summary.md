# PR-U13 Build Summary

## Input Findings

- Phase D requires hidden workspaces to stop creating terminal snapshots and eventually release renderer/cache/PTY resources.
- Existing worker-per-workspace code already supported Active/Warm transitions and max-warm eviction, but the update summary still marked PR-U13 pending.

## Scope

- Keep the existing worker-per-workspace architecture.
- Add time-based Warm -> Suspended eviction for background workspaces.
- Preserve warm workspace notification drain and replay behavior.

## Changes

- Added `WorkspaceRuntime::backgrounded_at`.
- Active workspace reuse clears `backgrounded_at`; workspace switch records the old active workspace background time.
- Added `WARM_AUTO_SUSPEND_AFTER` and `evict_idle_warm`.
- Warm workspaces older than the timeout are drained for final notifications, transient notifications are pruned, and the runtime worker is shutdown as Suspended.
- Existing hidden snapshot prevention remains in runtime via `SetWorkspaceState(Warm)`.

## Tests

- `cargo test -p deppy-sijo warm_auto_suspend_candidates_respect_timeout_and_order`

## Acceptance Criteria Check

- [x] Hidden workspace enters Warm on workspace switch.
- [x] Warm workspace produces no visible pane snapshots because runtime render is inactive.
- [x] Warm workspace events are drained for status/exit notifications.
- [x] Warm workspace is auto-suspended after timeout.
- [x] Suspended workspace releases runtime worker resources.
- [x] Reopening a suspended workspace creates a fresh worker from DB metadata.

## Regression Risks

- Suspended means worker shutdown, so running PTY sessions in that workspace are terminated after the timeout. This matches the existing MAX_WARM eviction semantics.
- If users need long-running background agents, the timeout should later become a setting.

## Resource Impact

- Reduces steady-state CPU/RAM by bounding how long hidden workspace workers stay alive.
- No new threads are introduced outside the existing background shutdown join path.

## Security Impact

- No secret/env/log policy changes.

## I18n/CJK Impact

- No user-facing strings are added.

## Rollback Plan

- Remove `backgrounded_at`, `WARM_AUTO_SUSPEND_AFTER`, and the `evict_idle_warm` call. MAX_WARM eviction remains available as the previous resource guard.

## Follow-up

- Expose the auto-suspend timeout in settings if user control is required.
- Surface suspended workspace state in PR-U25 Global Activity View.
