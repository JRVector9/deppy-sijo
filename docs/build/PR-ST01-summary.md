# PR-ST01 — Storage/Audit Transactions

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
