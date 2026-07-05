# Build PR Summary

## Input Findings
- PR-R00/PR-R01 High: `CredentialsUi` directly received `Db` and `SecretStore`, while Connector UI directly named `KeyringSecretStore`, `LocalMcpManager`, and `record_tool_audit`.
- Review summary B00: add an `xtask check-boundary` guard for leaf UI boundary drift and keep remaining exceptions explicit.
- v2.6 invariant: UI leaf modules must not call secret store APIs directly; `crates/app/src/app.rs` remains the composition root exception.

## Scope
- Narrow B00 hardening only.
- Moved credential add/delete/list and OAuth token keyring writes behind app-owned adapters.
- Did not redesign Connector Center MCP execution, permission policy, audit recording, or storage orchestration.

## Changes
- Added `CredentialService` boundary to `crates/app/src/ui/credentials.rs`; the UI now works with list/add/delete methods and no longer imports `Db`, `SecretStore`, `SecretString`, or keyring types.
- Added app-owned credential adapters in `crates/app/src/app.rs` for DB metadata, keyring writes/deletes, OAuth token storage, and redaction seeding.
- Replaced Connector UI direct `KeyringSecretStore`/`auth::store_token` usage with an `OAuthCredentialStore` adapter. Connector OAuth metadata insertion remains in Connector UI as a deferred storage exception.
- Added `cargo run -p xtask -- check-boundary`. It blocks leaf UI `KeyringSecretStore`, direct `SecretStore`, direct secret set/get/delete, concrete runtime/terminal/PTY types, and session-secret coupling.
- `check-boundary` also freezes current UI `db.`, `LocalMcpManager`, and `record_tool_audit` occurrences by file, snippet, and count so new side effects fail the guard unless intentionally reviewed.

## Tests
- `cargo fmt --check` - pass
- `cargo run -p xtask -- check-boundary` - pass
- `cargo run -p xtask -- check-deps` - pass
- `cargo test -p deppy-sijo credentials` - pass
- `cargo test -p deppy-sijo connectors` - pass
- `cargo check --workspace --all-targets` - pass

## Risk Notes
- `crates/app/src/app.rs` is treated as the composition root and still owns concrete `Db`, `KeyringSecretStore`, and in-process runtime wiring.
- Remaining explicit UI exceptions:
  - `crates/app/src/ui/connectors.rs`: `LocalMcpManager` import plus three discover/prepare/call constructions.
  - `crates/app/src/ui/connectors.rs`: direct DB calls for permission rules, MCP server/tool storage, OAuth credential metadata, and redacted-only tool audit.
  - `crates/app/src/ui/agents.rs` and `crates/app/src/ui/env_profiles.rs`: pre-existing settings DB calls, frozen by the new guard but not moved in this PR.
- Connector audit still records through `db.record_tool_audit(..., None)` from UI. Encrypted raw input remains default-off.

## Rollback Plan
- Revert `xtask/src/main.rs`, `crates/app/src/ui/credentials.rs`, `crates/app/src/ui/connectors.rs`, `crates/app/src/app.rs`, and this summary.
- Credential/OAuth behavior returns to the previous direct UI secret-store path; no DB schema or migration rollback is needed.

## Follow-up Review Requests
- Review PR-B00 against PR-R01 with focus on the new `check-boundary` allowlist and app composition-root exception.
- Follow-up Build PR should move Connector UI MCP execution, permission evaluation, audit recording, and MCP/credential metadata storage behind a runtime or app-service boundary.
- Separate follow-up should decide whether agent/env profile DB calls remain accepted settings exceptions or get service adapters like credentials.
