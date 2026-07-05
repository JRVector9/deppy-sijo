# Remaining Parallel PR Map

작성일: 2026-07-05

## Purpose

This document freezes ownership and shared naming for the remaining follow-up
PRs. It is a coordination artifact only; it does not authorize behavior changes.

## Non-Regression Baseline

- Pane-level workspace/session operation remains unchanged.
- Folder tree rendering and terminal path insertion remain enabled.
- Path insertion, folder tree drag/drop, and terminal drag/drop paste must insert
  text only; they must not auto-execute commands.
- Terminal selection/copy/paste and bracketed paste behavior must remain intact.
- Required CJK/emoji path fixtures remain covered.
- UI terminal actions continue through `RuntimeClient`.
- Hidden panes/workspaces must not create `TerminalViewportSnapshot`.
- Raw plaintext logs remain disabled by default.
- Secret/env/API key values must not be stored in DB/config/log/export as plain
  text.
- `mcp`/`audit`/`persist`/`storage` crate cycles remain forbidden.

## Shared Names

| Area | Frozen Name | Owning PR | Notes |
|---|---|---|---|
| Process identity | `ProcessIdentity` | PR-U12c | Redacted PID/process-group metadata exposed by `pty`, owned by runtime/session after spawn. |
| Resource source | `ProcessIdentitySource` | PR-U12c | Distinguishes portable-pty, platform fallback, and unavailable identity. |
| Session resources | `SessionResourceUsage` | PR-U12c | Per-session child process CPU/RSS aggregation. |
| Input queue policy | `PtyInputQueuePolicy` | PR-U15c | Byte/message budget for PTY input. |
| Input enqueue result | `PtyInputEnqueueResult` | PR-U15c | Explicit accepted/backpressured/rejected result. |
| Input pressure event | `PtyInputPressure` | PR-U15c | Runtime/UI pressure signal; no silent input drop. |
| Status view | `SessionStatusView` | PR-U17b | Compatibility wrapper around existing `SessionStatus`. |
| Status source | `StatusSource` | PR-U17b | Process, stream, screen, idle, or user override source. |
| Status override | `UserStatusOverride` | PR-U17b | User-driven status mark/clear action. |

## RuntimeEvent Extension Points

Existing events remain compatible. New events must be additive:

- PR-U12c may extend `RuntimeEvent::ResourceUsage` with optional per-session
  child usage, or add a resource-specific payload type used by that event.
- PR-U15c may add `RuntimeEvent::PtyInputPressure { session, pressure }`.
- PR-U17b may add `RuntimeEvent::SessionStatusViewChanged { session, view }`
  or keep `SessionStatusChanged` and provide `SessionStatusView` through
  runtime state. Existing `SessionStatusChanged` consumers must keep compiling.

No PR may replace `SessionStatus`, expose terminal backend implementation types
to UI, or make UI read PTY/process handles directly.

## File Ownership

| PR | Primary Files | Allowed Shared Touches | Forbidden Touches |
|---|---|---|---|
| PR-U18b | `crates/storage/src/write_worker.rs`, `crates/runtime/src/persistence.rs`, `crates/runtime/src/in_process.rs`, `crates/persist/src/repo.rs` | Build summary and focused tests | No layout save async rewrite; no UI DB writes. |
| PR-U12c | `crates/pty/src/process_identity.rs`, `crates/session/src/session.rs`, `crates/runtime/src/resource_monitor.rs`, `crates/runtime/src/in_process.rs`, `crates/app/src/ui/activity.rs` | `crates/pty/src/lib.rs` export only | No PTY input queue policy changes; no command/env exposure. |
| PR-U15c | `crates/pty/src/input_queue.rs`, `crates/runtime/src/in_process.rs`, `crates/app/src/ui/workspace.rs`, `crates/app/src/ui/activity.rs` | `crates/pty/src/lib.rs` export only | No blocking single-loop `write_all`; no silent input drop. |
| PR-U17b | `crates/session/src/status.rs`, `crates/session/src/status_detector.rs`, `crates/runtime/src/in_process.rs`, `crates/app/src/ui/workspace.rs`, `crates/app/src/ui/notifications.rs`, `crates/i18n/`, `locales/*/` | Additive runtime event/status fields only | No breaking replacement of `SessionStatus`; no UI text without i18n keys. |
| PR-U20c | `docs/performance/final-gate.md`, `docs/performance/release-hardware-measurements.md` | Measurement script only if needed | Do not mark pending measurements passed without evidence. |

## Merge Order

1. PR-PAR-00 must land before code PRs.
2. PR-U20c baseline may land after PR-PAR-00 and before implementation PRs.
3. PR-U18b may land independently.
4. PR-U12c and PR-U15c must check shared `in_process.rs` and
   `activity.rs` diffs before merge.
5. PR-U17b must check runtime event, notification, and i18n changes before
   merge.
6. PR-U20c final measurement runs after implementation PRs.

