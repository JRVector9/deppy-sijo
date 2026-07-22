# PR-ST01 — Storage/Audit Transactions

## 2026-07-22 pending-approval startup ownership amendment

Startup reconciliation now requires a lifetime-held `ActivePendingApprovalOwner` bound to the
exact physical database. Acquisition is nonblocking and uses one `owner.lock` in a hashed physical
DB namespace that is completely separate from the 256 authorization-executor stripes. The token
is non-Clone, its Debug is redacted, and cross-DB use fails before transaction entry.

Tests prove same-DB exclusion, independent-DB concurrency, drop release, hardlink-alias
competition, atomic path-replacement identity separation, cross-DB no-mutation, one-file
cardinality after 1,024 acquisitions, sanitized errors, 256-row bounded/idempotent reconciliation,
and injected-failure rollback. Root verification passes all 138 storage tests plus doc-tests,
all-target check, strict Clippy, package fmt, and scoped diff-check. The unowned API remains only as
temporary app-cutover compatibility and must be removed when IN01 adopts the owned call.

## 2026-07-22 bounded approval-inbox amendment

The durable approval inbox now fails closed at 256 live rows globally and per runtime session, with
a 1 MiB aggregate retained-text ceiling and fixed byte ceilings for every field. All text is
validated for size and NUL before a transaction. Batch duplicates and aggregate bytes are checked
before mutation; an IMMEDIATE transaction then serializes the same-snapshot count/byte/session
preflight with insertion, so concurrent writers cannot overshoot the bound.

Reads preflight `COUNT` and SQLite byte lengths before materializing `String` values and use
`LIMIT n+1`; corrupt legacy overflow returns an error rather than a partial approval list. Startup
reconciliation atomically denies at most 256 session-scoped pending rows and changes zero rows on
overflow or injected failure. The public row/insert/page `Debug` output contains only presence
booleans, row count, and `has_more`; operation/server/tool/preview/session/time values are excluded.

Root verification for the amendment:

- `cargo test -p mcp-store -- --test-threads=1`: 40/40 plus doc-tests passed.
- `cargo test -p storage -- --test-threads=1`: 133/133 plus doc-tests passed.
- The 255-row concurrent-writer regression admits exactly one writer and retains 256 rows.
- All-target check, strict Clippy, rustfmt/diff-check, and the workspace security scan passed.
- Dependency law passed and the boundary count remains the same 53 existing exceptions.

## Outcome

PR-ST01 adds the durable transaction and audit primitives needed by the Connector service cutover.
It does not move UI callers or execute external MCP calls; PR-SV01/PR-AU01/PR-IN01 consume these
APIs. No concrete secret value is accepted by or persisted through the new storage APIs.

## Schema and migration

- Appends forward-only storage migration v25, owned by `audit::MIGRATION_AUDIT_LIFECYCLE`.
- Adds nullable `operation_id`, non-null `lifecycle`, nullable `outcome_error_code`, and nullable
  `completed_at` to `tool_audit_logs`.
- Adds a partial unique index for non-null `operation_id`, preventing reuse after every lifecycle,
  including `Unknown`.
- Existing audit rows are backfilled as `Succeeded` with no operation ID. A data-preservation test
  covers the legacy-row upgrade.
- There is no down migration. Existing storage-core backup-before-migration and forward-only
  migration recovery remain the rollback mechanism.

## Public APIs

### `audit`

- `AuditLifecycle::{Prepared,Succeeded,Failed,Unknown,Denied}`
- `AuditOperation`
- `prepare_audit_operation`
- `complete_audit_operation`
- `audit_lifecycle`
- `reconcile_prepared_audits`

Preflight rejects invalid JSON and duplicate/invalid operation IDs. Only `Prepared` can transition
to `Succeeded` or `Failed`. Startup reconciliation changes every remaining `Prepared` row to
`Unknown`; the unique operation index prevents automatic retry with the same operation ID.
Persisted error codes are constrained to a 64-byte low-cardinality identifier, not raw errors.

### `mcp-store`

- `update_server_url` now atomically updates the URL, removes Allow rules, and removes cached tools.
  Deny rules remain because retaining them cannot widen authority. An unchanged URL is a no-op.
- `replace_tools_for_server` validates every row belongs to the target server before deletion and
  remains all-or-nothing.
- `ensure_server_by_url` performs an IMMEDIATE transaction and canonical URL lookup for idempotent
  built-in-provider registration (Slack consumer).
- `insert_servers_batch` validates first and inserts an import batch in one transaction.

### `storage::Db`

- `rotate_credential_secret_slot` atomically publishes `credentials.keyring_username`, non-secret
  OAuth metadata, and a masked hint after the caller has written a new physical keyring slot.
- `credential_secret_location` resolves a logical credential ID to service/physical-slot metadata
  without reading a secret.
- `ensure_mcp_server_by_url` and `insert_mcp_servers_batch` expose the MCP transaction primitives.
- `commit_tool_authorization_preflight` commits an optional AllowAlways/DenyAlways rule and its
  audit preflight in one transaction.
- `complete_tool_audit`, `tool_audit_lifecycle`, and `reconcile_prepared_tool_audits` expose the
  lifecycle to the app adapter.

## Failure injection and transaction semantics

Deterministic SQLite triggers and constraint failures verify rollback for:

- URL update after permission mutation but before tool deletion;
- tool replacement after delete and the first insert;
- multi-server import after the first insert;
- credential slot pointer/OAuth metadata publication;
- permission update followed by audit preflight insert failure.

Additional tests cover invalid JSON producing no preflight row, legacy audit migration,
Prepared-to-Unknown crash recovery, duplicate operation-ID rejection, redacted persistence, and
concrete OAuth token-field rejection.

## Verification

- `cargo test -p audit -p mcp-store -p storage --no-fail-fast` — pass (`37 + 18 + 79`, 134 tests).
- `cargo check -p audit -p mcp-store -p storage` — pass.
- `cargo clippy -p audit -p mcp-store -p storage --all-targets -- -D warnings` — pass.
- `cargo check -p deppy-sijo` — pass (current integrated app tree).
- `cargo run -q -p xtask -- smoke-db-migrations` — pass.
- `cargo run -q -p xtask -- security-scan` — boundary/dependency, storage secret,
  mcp-store secret, and all 37 audit tests pass; the combined command cannot finish in the managed
  sandbox because 29 MCP HTTP mock tests are denied while binding a localhost listener.
- Scoped rustfmt and `git diff --check` — pass.

## Integration requirements

- PR-SC01 must write the complete access/refresh/DCR bundle to a versioned physical slot first,
  call `rotate_credential_secret_slot`, delete the old slot only after commit, and reconcile orphan
  slots at startup.
- PR-AU01 must call `commit_tool_authorization_preflight` before every allowed external call, then
  complete the same operation ID exactly once. A preflight error means external-call count zero.
- App bootstrap must call `reconcile_prepared_tool_audits` before Connector/proxy workers accept
  work.
- PR-IN01 must replace the old UI sequence (permission deletes, tool clear, URL update) with the
  single repository call and switch Slack/import/OAuth flows to the new atomic APIs.
- The current legacy Connector UI still performs the old decomposed calls until atomic cutover;
  keeping both production paths is not intended.

## Localhost/external constraints

All ST01 tests are in-memory/local SQLite tests. No external network, OAuth account, keyring, or
localhost listener is required or exercised. The broader `xtask security-scan` also runs MCP HTTP
tests outside this lane; 29 of those fail at `crates/mcp/src/http.rs`'s local `TcpListener::bind`
with sandbox `Operation not permitted`. Re-run that combined gate in a localhost-capable runner;
do not treat it as an ST01 storage/audit failure.

## Pre-IN01 request-target aggregate

- `Db::mcp_request_target_versioned` returns a redacted `McpRequestTargetRecord` from one
  `read_connector_config` transaction/revision: server plus ordered stdio physical credential
  locations, or server plus zero-to-two HTTP OAuth candidates.
- Stdio parsing performs SQL shape, duplicate, item, row-byte, and aggregate-byte preflight before
  materialization. It enforces 4,096 retained items, 1 KiB per coordinate, and 1 MiB aggregate;
  missing, wrong-service, legacy, corrupt, and cross-owned pointers fail closed.
- The aggregate and coordinate `Debug` implementations exclude service, username, server ID, URL,
  metadata, and logical/physical identifiers. New errors are static marker-free messages.
- Root verification: storage tests passed 142/142 plus doc-tests; all-target check, strict Clippy,
  package rustfmt, and scoped diff-check passed.

## Bounded app projections and hook-state retention

Storage now exposes SQL-preflighted bounded projections for environment profiles/variables,
dotenv-owned credential IDs, environment project counts, hook/status/turn prefixes, global
waiting rows, and per-workspace agent restore. Selection uses deterministic LIMIT+1 windows,
validates SQLite types and row/aggregate bytes before String allocation, and captures one snapshot
epoch for every TTL preflight/select pair. App production callers no longer full-materialize these
tables and retain the last complete UI snapshot when a bounded read fails. A failed agent-restore
projection leaves the workspace unloaded and retries from cached bindings on the next bounded
agent-state refresh, so detector outcome deduplication cannot suppress recovery.

Every hook/status/needs/turn writer validates a 1-KiB control-free workspace/session key and, in
its IMMEDIATE transaction, keeps at most 256 deterministic newest rows for the literal workspace
prefix and 4,096 rows globally. The protected write survives eviction; clear on a missing key keeps
the full 256. Exact/plus-one, `%`/`_` isolation, cross-workspace retention, corrupt row/type/byte
preflight, stable TTL cutoff, and injected eviction rollback regressions are included.

Root verification passes storage 183/183 plus doc tests, app 790/790 with five explicit ignores,
logging policy 15/15, app/storage all-target checks, dependency-inclusive strict Clippy, zero-
allowlist boundary, the 23-crate dependency DAG, full workspace fmt, and diff-check.

Persisted activity panes and web-push targets now use the same allocation-before-admission rule.
Activity is bounded to the UI's 256-workspace by 256-item product and 4 MiB retained bytes;
web-push is bounded to 8 subscriptions and 64 KiB. Both use deterministic LIMIT+1 windows and
reject corrupt SQLite types, oversized fields/rows, and aggregate overflow before returning a
String. Root switched the app adapters to the bounded APIs and preserves the previous activity
snapshot on error. Storage passes 189/189 plus doc tests, app passes 792/792 with five explicit
ignores, and logging policy passes 15/15. App/storage all-target check, strict Clippy, zero-allowlist
boundary, the clean dependency DAG, full workspace fmt, diff-check, and security scan pass.

## Agent-state aggregate transaction

Agent-state persistence and projection now have one bounded IMMEDIATE transaction seam. The job
applies generation-aware turn acknowledgements, identity-CAS stale binding deletes, a 256-pane
authoritative reconcile, and at most 16 structured mutations retaining at most 512 KiB. It then
preflights hook, status, waiting, turn, binding, and structured projections before materialization;
the complete snapshot retains at most 4 MiB. Any mutation, corrupt projection, aggregate overflow,
or commit failure returns no snapshot and rolls the whole job back. Live but temporarily
undetected pane bindings remain recoverable and unchanged desired bindings do not churn timestamps.

The job preserves the exact 1,024-byte workspace-ID bound and rejects +1. All new public DTO Debug
implementations expose only variant/count/presence information; hostile-marker tests cover pane,
session, path, and structured-row content. Root verification passes focused 6/6, storage 195/195
plus doc tests, strict all-target Clippy, scoped Rust-2024 fmt/check, and diff-check. The change adds
no schema migration, dependency, or boundary allowlist.

### Authoritative multi-workspace catalog amendment

`AgentStateJob` now carries 1..=256 unique bounded workspace IDs for the structured catalog. One
bound-value CTE applies a deterministic favorite/updated/workspace/local/rowid order across the
complete request, then preflights and materializes at most 500 rows and 4 MiB in the same IMMEDIATE
transaction as any exact mutation. Empty, duplicate, control-bearing, 257th, oversized, and
SQL-hostile IDs fail closed or remain literal values. Exact 500 rows and exact 4 MiB pass; both +1
boundaries fail before materialization and roll back an earlier exact mutation.

Root verification passes focused AgentState 10/10, storage 199/199 plus doc tests, all-target
check, strict all-feature Clippy, package fmt-check, and scoped diff-check. The full security scan
also passes with the zero-allowlist boundary and 23-crate dependency laws unchanged. An initial
`cargo xtask security-scan` shorthand failed because this checkout defines no Cargo alias; the
canonical `cargo run -p xtask -- security-scan` command passed without a product workaround.

### Caller-selected snapshot output budget amendment

`AgentStateJob::snapshot_bytes_max` closes the gap between committing an exact mutation and only
then discovering that app-owned resume/project-name output would exceed the worker's 4 MiB result
budget. Each job must request 1 byte through the existing 4 MiB hard maximum; `projection()` keeps
4 MiB as its default. The same IMMEDIATE transaction compares all six projected sections against
that requested ceiling before materialization or commit, while the existing separately bounded
job input is intentionally not charged twice as output.

Exact custom-byte and 4 MiB requests pass. Zero and 4 MiB+1 fail before mutation, and a
one-byte-short custom ceiling rolls back an earlier structured exact mutation. Root independently
reran focused AgentState 11/11 and diff-check; the lane passed storage 200/200 plus doc tests,
all-target check, strict all-feature Clippy, package fmt, and the canonical security scan.
