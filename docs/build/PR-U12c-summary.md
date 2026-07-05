# PR-U12c Build Summary

## Input Findings

- PR-U12 foundation sampled only the app process RSS/CPU.
- `PtySession` did not expose stable child process identity.
- Global Activity showed app process resource samples but not per-session child
  process usage.

## Scope

- Add redacted PTY child process identity.
- Propagate identity through `Session`.
- Aggregate per-session child process CPU/RSS in runtime.
- Surface runtime-provided child usage in Global Activity.
- Do not change PTY input queue policy or expose command/env values.

## Changes

- Added `pty::ProcessIdentity` and `ProcessIdentitySource`.
- `PtySession::process_identity()` now returns PID/process-group metadata when
  portable-pty provides it.
- `Session` stores process identity at spawn time.
- `ProcessResourceMonitor` now samples per-session child process usage using a
  process-group first, PID-descendant fallback strategy.
- `RuntimeEvent::ResourceUsage` carries `session_usage`.
- Global Activity displays aggregate child CPU/RSS using localized text.

## Tests

- `cargo test -p pty process_identity` - pass
- `cargo test -p runtime session_usage` - pass
- `cargo test -p runtime resource_usage는_session_child_usage` - pass
- `cargo test -p deppy-sijo resource_label_includes_child_usage` - pass

## Acceptance Criteria Check

- [x] Session child process identity is available through runtime/session, not UI.
- [x] Per-session child process CPU/RSS aggregation is implemented.
- [x] Workspace activity can show child resource usage.
- [x] Process identity Debug output does not include command/env data.
- [x] Unavailable process identity falls back gracefully to zero child usage.

## Regression Risks

- Low to medium. Runtime now invokes `ps` during the low-cadence resource sample
  to aggregate child process usage on Unix platforms.
- Remote transport still filters `ResourceUsage`; resource telemetry remains
  local UI telemetry.

## Resource Impact

- Adds one low-cadence process table scan per resource sample when sessions are
  present.
- No per-frame or UI-owned process polling was added.

## Security Impact

- PID/process-group metadata only. Full command, args, env, and secrets are not
  exposed.

## I18n/CJK Impact

- Added `activity.child_resource_label` for all required locales.

## Rollback Plan

- Remove `ProcessIdentity` propagation and restore `RuntimeEvent::ResourceUsage`
  to app-process-only payload.
- Remove child resource label from Global Activity.

## Follow-up

- Use PR-U20c final measurements to confirm the low-cadence process scan cost on
  release hardware.

