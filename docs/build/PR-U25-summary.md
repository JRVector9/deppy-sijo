# PR-U25 Build Summary

## Input Findings

- PR-U12 added low-cadence process resource samples but left UI display for PR-U25.
- PR-U13 added Active/Warm/Suspended workspace behavior and noted suspended state should surface in Global Activity View.
- PR-U22/PR-U23 established i18n UI/runtime message paths.

## Scope

- Add a global activity window using existing App/runtime state.
- Do not add kill/restart controls, process tree aggregation, or new runtime commands.
- Preserve RuntimeClient boundary and existing pane/folder tree/DnD/copy-paste UX.

## Changes

- Added `ui::activity` with `ActivityUi`, activity rows, workspace states, resource formatting, and switch action.
- Added a top-bar Activity button.
- App now records `RuntimeEvent::ResourceUsage` per workspace runtime for display.
- Activity view lists Active/Warm/Suspended workspaces, session count, pending event count, CPU/RSS sample, high resource flag, and warm auto-suspend countdown.
- Activity view can switch to another workspace through the existing App `switch_workspace` path.
- Added localized activity strings to required and optional locale catalogs.

## Tests

- `cargo test -p deppy-sijo activity -- --nocapture`
- `cargo run -p xtask -- i18n-check`
- `cargo check --workspace --all-targets`

## Acceptance Criteria Check

- [x] Activity view is global and spans all workspaces known to the App.
- [x] Active/Warm/Suspended states are visible.
- [x] Resource samples from PR-U12 are surfaced without new polling.
- [x] Warm auto-suspend timing is visible without creating hidden terminal snapshots.
- [x] Switching workspaces uses the existing App switch path.
- [x] UI strings use i18n catalog keys.

## Regression Risks

- Resource samples are process-level snapshots from PR-U12, not per-session child process aggregation.
- Activity view requests a 1s repaint only while open and only when warm countdown data exists.

## Resource Impact

- No new background threads or runtime commands.
- Small per-frame view model allocation while the egui frame renders.

## Security Impact

- No secret/env/API key values are displayed.
- Workspace names and resource counters only.

## I18n/CJK Impact

- Activity labels are included in required locale completeness checks.

## Rollback Plan

- Remove `ui::activity`, the App activity field/calls, activity resource snapshot recording, top-bar Activity button, and activity locale keys.

## Follow-up

- Add per-session child process resource aggregation when PR-U12 follow-up exposes child process data.
- Add explicit suspend/resume controls only after UX and safety rules are defined.
