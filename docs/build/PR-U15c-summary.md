# PR-U15c Build Summary

## Input Findings

- PTY output queue was bounded, but PTY input used an unbounded channel.
- Adding a naive blocking cap could reintroduce full-duplex deadlock.
- Users needed a visible pressure signal when input could not be accepted.

## Scope

- Add deadlock-safe byte/message input queue policy.
- Return explicit accepted/backpressured/rejected enqueue results.
- Surface PTY input pressure through `RuntimeEvent` and UI state.
- Preserve bracketed paste, terminal DnD paste, and CJK input paths.

## Changes

- Added `pty::PtyInputQueuePolicy`, `PtyInputEnqueueResult`,
  `PtyInputPressure`, and `PtyInputRejectReason`.
- Replaced unbounded PTY input channel with a bounded `sync_channel` using
  non-blocking `try_send` plus byte accounting.
- Writer thread owns blocking PTY writes and releases queue budget after each
  write finishes.
- `Session::write_input` returns enqueue results.
- Runtime emits `RuntimeEvent::PtyInputPressure` for backpressured/rejected
  input and only calls `StatusDetector::on_input()` after accepted input.
- Workspace UI shows a localized visible pressure warning.
- Global Activity queue column shows the last input pressure signal.

## Tests

- `cargo test -p pty queue` - pass
- `cargo test -p runtime oversized_input` - pass
- `cargo test -p deppy-sijo queue_label_includes_input_pressure` - pass
- `cargo test -p terminal bracketed_paste` - pass

## Acceptance Criteria Check

- [x] PTY input queue has bounded byte/message policy.
- [x] Queue full or oversized input is not silently dropped.
- [x] Runtime emits visible pressure signal.
- [x] Status detector input-clear only happens after accepted input.
- [x] Bracketed paste tests still pass.

## Regression Risks

- Medium. Very large single paste payloads over the queue byte budget are now
  rejected instead of accumulating without bound.
- The policy is all-or-nothing for each `WriteInput` payload to preserve
  bracketed paste framing.

## Resource Impact

- Prevents unbounded PTY input memory growth.
- Adds small mutex-protected accounting around PTY input enqueue/dequeue.

## Security Impact

- No input payload content is stored in pressure events; only byte counts and
  reason metadata are surfaced.

## I18n/CJK Impact

- Added localized pressure labels.
- Existing bracketed paste and terminal paste paths remain unchanged.

## Rollback Plan

- Restore unbounded PTY input channel and `write_input` result type.
- Remove `RuntimeEvent::PtyInputPressure` handling from runtime/app UI.

## Follow-up

- Revisit queue budget values using PR-U20c release-hardware Scenario C/E
  measurements.

