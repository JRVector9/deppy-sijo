# PR-U18 Build Summary

## Input Findings
- PR-R08 Finding 6: session/status/log offset/layout 계열 SQLite write가 단건 execute 중심이라 burst 상황에서 write 비용이 커질 수 있음.
- PR-U18 plan: status update debounce, notification insert batch, log offset update batch, prepared statement reuse, background DB worker 기반 필요.
- v2.8 persistence rule: store/facade 계층에서 crate cycle을 만들지 않고 WAL/foreign_keys/busy_timeout 연결 규약을 유지해야 함.

## Scope
- DB schema 변경 없음.
- `crates/storage/src/*`에 bounded/debounced background write worker 추가.
- `crates/mcp-store/src/*`에는 pending approval batch insert만 추가하고 기존 secret-like args guard는 유지.
- Runtime/app UI call-site rewiring은 이번 소유 범위 밖이라 건드리지 않음.

## Changes
- `storage::DbWriteWorker` / `DbWriteHandle` 추가.
- Session status update는 session id별 최신값으로 coalesce한 뒤 debounce window 또는 explicit flush에서 batch update.
- Session log offset update는 session id별 최대 offset으로 coalesce해 offset 역행 없이 batch update.
- Durable notification-like insert 경로로 현재 schema에 존재하는 `pending_approvals` insert를 bounded queue + batch flush로 처리.
- Worker 연결은 `storage_core::open_with_migrations`를 사용해 WAL, foreign_keys, busy_timeout, migration gate를 그대로 유지.
- Batch flush는 단일 transaction 안에서 prepared statement를 반복 재사용하고, `mcp-store` pending approval batch는 `prepare_cached`를 사용.
- `DbWriteStatsSnapshot`으로 queued/coalesced/flushed/missing/error/queue_full/transaction 통계를 테스트 가능하게 노출.

## Tests
- `cargo fmt --check` - pass
- `cargo test -p storage -p mcp-store` - pass
- `cargo run -p xtask -- check-deps` - pass
- `cargo check --workspace --all-targets` - pass

## Acceptance Criteria Check
- High output/status burst write path: added bounded coalescing path for status and log offset writes.
- WAL 유지: worker connection test verifies `PRAGMA journal_mode=wal`.
- UI thread direct repeated write: nonblocking handle API added; existing UI/runtime call-site migration remains follow-up because this PR's owned scope excludes app/runtime files.
- Write batch stats: `DbWriteStatsSnapshot` covered by storage tests.
- Existing security tests: storage/mcp-store secret/env/API key persistence guards still pass.

## Regression Risks
- Worker writes are best-effort asynchronous; callers that require immediate read-after-write must call `flush()`.
- Missing session ids are counted as `*_missing` instead of failing the whole worker loop.
- Pending approval queue overflow returns `NotificationQueueFull`; callers must surface or retry rather than silently ignore it.

## Resource Impact
- Reduces repeated SQLite writes by collapsing status/log offset churn to one row update per session per batch.
- Bounded pending maps/queue prevent unbounded memory growth under write bursts.
- Adds one background thread per `DbWriteWorker` instance.

## Security Impact
- No schema change and no new plaintext secret storage.
- Existing `EnvValue::Plain`, agent args, and MCP args guards remain covered by tests.
- Pending approval `arguments_preview` contract remains redacted-display-only; raw tool input is not added.

## I18n/CJK Impact
- No user-facing string or terminal text handling changes.
- Stored domain values such as status strings and pending approval preview remain raw data.

## Rollback Plan
- Remove `storage::write_worker` exports and `crates/storage/src/write_worker.rs`.
- Revert `mcp_store::PendingApprovalInsert` and `insert_pending_approval_batch`; restore single-row `insert_pending_approval` execute.
- No rollback migration needed because DB schema is unchanged.

## Follow-up
- Wire runtime persistence hot paths to `DbWriteHandle` once runtime scope is available.
- Add layout save coalescing in runtime/persist owner scope.
- Add UI/service boundary wiring so direct UI storage exceptions can use background writes where read-after-write is not required.
