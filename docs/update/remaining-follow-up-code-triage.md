# Remaining Follow-up Code Triage

작성일: 2026-07-05

## Summary

PR-U05b, PR-U06b, PR-U09b, PR-U10b, PR-U20b, and PR-U08b are implemented and
committed. The remaining follow-ups below were checked against code and should
not be mixed into the current completed PRs.

## PR-U12 Child Process Tree CPU/RSS Aggregation

Status: implemented in PR-U12c.

Code checked:

- `crates/runtime/src/resource_monitor.rs`
- `crates/pty/src/lib.rs`
- `crates/session/src/session.rs`
- `crates/runtime/src/in_process.rs`

Reason:

- Current `PtySession` does not expose child pid/process group metadata.
- `portable_pty::MasterPty::process_group_leader()` is used internally during
  Drop, but the runtime/session boundary has no stable API for child process
  tree sampling.
- Implementing this correctly needs pty trait API changes, session metadata
  propagation, and platform-specific process tree aggregation.

Implemented scope:

- Added redacted `ProcessIdentity { pid, process_group, source }` at the pty
  boundary.
- Runtime aggregates per-session child tree CPU/RSS using process group first
  and PID-descendant fallback.
- Global Activity displays runtime-provided child CPU/RSS without owning process
  sampling.
- Added process identity, resource aggregation, runtime smoke, and activity
  label tests.

## PR-U18 Runtime/App Hot-path DB Write Batching Wiring

Status: implemented in PR-U18b.

Code checked:

- `crates/storage/src/write_worker.rs`
- `crates/runtime/src/persistence.rs`
- `crates/runtime/src/in_process.rs`
- `crates/persist/src/repo.rs`

Reason:

- `DbWriteWorker` and `DbWriteHandle` exist and are tested.
- Runtime persistence still uses `PersistPipe` with a direct rusqlite
  connection for session/layout writes.
- Wiring requires lifecycle ownership, flush/read-after-write policy, and
  rollback behavior for runtime persistence. It is implementable, but it touches
  runtime persistence contracts and should stay isolated.

Implemented scope:

- `PersistPipe` owns a `DbWriteWorker` and handle when persistence is enabled.
- Runtime detector status/session-exit status and redacted log-offset progress
  route through the batched handle.
- Enqueue failures fall back to direct persistence.
- Layout save remains synchronous.

## PR-U15 PTY Input Queue Policy / Visible Backpressure Badge

Status: defer pending deadlock-safe design.

Code checked:

- `crates/pty/src/lib.rs`
- `crates/runtime/src/in_process.rs`
- `crates/app/src/ui/workspace.rs`
- `crates/app/src/ui/activity.rs`

Reason:

- PTY output is bounded by a `sync_channel(64)`.
- PTY input currently uses an unbounded channel intentionally to avoid
  full-duplex deadlock while output is backpressured.
- A visible badge requires a new runtime/UI pressure signal; adding a cap without
  a deadlock-safe write policy can regress paste/input reliability.

Recommended scope:

- Design a byte-budgeted PTY input queue with explicit overflow result.
- Surface pressure through runtime events or existing activity view.
- Preserve bracketed paste, terminal DnD paste, and CJK input tests.

## PR-U17 Status Detector Confidence / User Override

Status: defer to a broader status UX PR.

Code checked:

- `crates/session/src/status.rs`
- `crates/runtime/src/in_process.rs`
- `crates/app/src/ui/workspace.rs`
- `crates/app/src/ui/notifications.rs`

Reason:

- `SessionStatus` is currently a simple enum consumed by runtime, workspace UI,
  notifications, remote events, and i18n message IDs.
- Adding confidence and manual override would change event semantics and UI
  behavior, not just detector cost control.

Recommended scope:

- Add a separate status model that carries status, confidence/source, and
  optional user override.
- Define notification semantics before changing runtime events.
- Add i18n keys and pseudo-locale layout coverage if override controls are UI
  visible.

## PR-U08 Debug Redaction

Status: implemented in PR-U08b.

Code changed:

- `runtime::RuntimeCommand` manual Debug redacts args/env/input/credential ids.
- `storage::EnvValue` manual Debug redacts plain values and credential ids.

Validation:

- `cargo test -p runtime runtime_command_debug`
- `cargo test -p storage env_value_debug`
- `cargo run -p xtask -- security-scan`
