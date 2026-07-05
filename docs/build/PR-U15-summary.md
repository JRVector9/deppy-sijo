# PR-U15 Build Summary

## Input Findings

- PR-R08 noted that local command/status/input queue policy and user-visible backpressure signaling were still incomplete.
- PTY output was already bounded and viewport delivery already used latest-wins slots.
- Remote slow consumer backpressure is handled separately by PR-U19.

## Scope

- Add a bounded local runtime command queue.
- Surface queue overflow to `RuntimeCommandSink::send_command` callers.
- Do not change PTY input writer threading or add UI badges in this slice.

## Changes

- Replaced the in-process runtime command channel with `sync_channel(IN_PROCESS_CMD_QUEUE_CAP)`.
- `InProcessRuntimeClient::send_command` now uses `try_send`.
- A full local runtime command queue returns an explicit backpressure error instead of blocking or growing unbounded.
- Added a deterministic unit test that fills a one-slot queue without relying on timing.

## Tests

- `cargo fmt --check` - pass
- `cargo test -p runtime in_process_command_queue_full --lib` - pass
- `cargo test -p runtime remote` - pass

## Acceptance Criteria Check

- [x] Local runtime command queue is bounded.
- [x] Overflow is surfaced to the caller.
- [x] Existing UI error path can display `send_command` failures.
- [x] PTY output bounded policy remains unchanged.
- [ ] PTY input writer queue policy remains deferred because the current unbounded channel intentionally avoids full-duplex deadlock.
- [ ] User-visible backpressure badge remains deferred.

## Regression Risks

- Very large command bursts can now fail fast instead of enqueueing indefinitely.
- Callers that ignored `send_command` errors may miss a command under extreme pressure; existing workspace send path already surfaces errors.

## Resource Impact

Prevents unbounded local command queue growth during resize/paste/command bursts.

## Security Impact

No secret, env, DB, audit, or log behavior changes.

## I18n/CJK Impact

No terminal text or path handling changes.

## Rollback Plan

Restore the unbounded `channel()` command queue and `send()` behavior in `InProcessRuntimeClient`.

## Follow-up

- Add a user-visible runtime pressure indicator when notification/message contracts are ready.
- Revisit PTY input writer queue with a deadlock-safe bounded design.
