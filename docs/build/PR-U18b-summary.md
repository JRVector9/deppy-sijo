# PR-U18b Build Summary

## Input Findings

- `DbWriteWorker` and `DbWriteHandle` existed but runtime persistence hot paths
  still wrote status/log-offset metadata directly through `PersistPipe`.
- Layout persistence has read-after-write expectations and should stay
  synchronous unless a separate coalescing contract is defined.

## Scope

- Wire runtime persistence status and log-offset burst paths into the existing
  batched DB writer.
- Keep session spawn and mux layout save synchronous.
- Do not add UI-thread DB writes or change DB schema.

## Changes

- `PersistPipe` now owns a `DbWriteWorker` plus handle when persistence is
  enabled.
- Detector status updates and session exit status updates enqueue through
  `DbWriteHandle::try_update_session_status`.
- Redacted ANSI log progress is counted in runtime and enqueued through
  `DbWriteHandle::try_update_session_log_offset`.
- Enqueue failures fall back to the existing direct write path.
- Shutdown flushes pending batched writes before the worker exits.

## Tests

- `cargo test -p runtime persist_pipe` - pass

## Acceptance Criteria Check

- [x] `DbWriteWorker` foundation is connected to runtime persistence.
- [x] Status burst path no longer relies only on direct SQLite writes.
- [x] Log-offset burst path is coalesced through the batch handle.
- [x] Layout save remains synchronous.
- [x] Pending write flush is available on shutdown.

## Regression Risks

- Low to medium. Runtime persistence now opens a second SQLite worker
  connection, matching the existing WAL/multi-connection design.
- If the worker queue cannot accept a write, runtime falls back to direct
  persistence and logs a warning.

## Resource Impact

- Adds one DB writer thread per persisted runtime worker.
- Reduces hot-path SQLite transaction pressure for status and log-offset bursts
  by coalescing per session.

## Security Impact

- No new secret persistence. Log offset counts redacted ANSI bytes only.

## I18n/CJK Impact

- None.

## Rollback Plan

- Remove `DbWriteWorker` ownership from `PersistPipe`.
- Route `session_status`, `session_exited`, and `session_log_offset` back to
  direct `persist` calls only.

## Follow-up

- Keep layout save synchronous unless a future PR defines a clear coalescing and
  read-after-write contract.

