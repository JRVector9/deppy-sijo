# PR-U10b Build Summary

## Input Findings

- PR-R07 and backlog triage reported that MCP stdio backends had no scoped env
  contract.
- `McpServerConfig` only carried command/args, and `StdioClient::spawn` inherited
  the parent process environment by default.
- `deppy-mcp-proxy` rebuilt backend configs without any per-server env metadata.

## Scope

- Add scoped env metadata for MCP server rows.
- Store safe plain env values and credential ids only; never persist secret env
  values.
- Resolve credential-backed env values immediately before MCP backend spawn and
  register resolved values for redaction.
- Preserve existing Connector Center UX; no broad env editor UI is added in this
  PR.

## Changes

- Added `mcp_servers.env_json`, `env_credentials_json`, and `inherit_env`
  migration.
- Extended `McpServerRow` with `env_plain`, `env_secrets`, and `inherit_env`.
- Added env persistence validation that rejects duplicate keys, invalid keys,
  and secret-like plain values.
- Extended `McpServerConfig` with scoped env and parent-env inheritance policy.
- Updated stdio MCP spawning to support `env_clear` plus explicit scoped env.
- Updated Connector Center direct discover/call paths to resolve scoped env
  through the app secret store only at schema-check/tool-run time.
- Updated `deppy-mcp-proxy` backend config creation to resolve credential env
  after keyring initialization and seed redaction before spawning.
- Extended credential deletion guards so MCP scoped env credential references
  prevent metadata deletion.
- Implemented custom `Debug` for `McpServerConfig` so env values are not printed.

## Tests

- `cargo fmt` - pass
- `cargo check --workspace --all-targets` - pass
- `cargo fmt --check` - pass
- `cargo clippy --workspace --all-targets` - pass
- `cargo test -p mcp-store` - pass
- `cargo test -p mcp scoped_env` - pass
- `cargo test -p storage mcp_scoped_env` - pass
- `cargo test -p deppy-sijo scoped_mcp_env_config` - pass
- `cargo run -p xtask -- check-deps` - pass
- `cargo run -p xtask -- smoke-db-migrations` - pass
- `cargo run -p xtask -- security-scan` - pass
- `cargo test --workspace --no-run` - pass

## Acceptance Criteria Check

- [x] MCP server rows can persist safe scoped env metadata.
- [x] Secret env values are stored only as credential ids.
- [x] Plain secret-like env values are rejected before persistence.
- [x] MCP stdio backend receives explicit scoped env values.
- [x] Strict mode can clear inherited parent env before injecting scoped env.
- [x] Credential-backed env values are resolved immediately before backend spawn.
- [x] Resolved secret env values are registered for redaction and hidden from `Debug`.

## Regression Risks

- Existing MCP server rows default to `inherit_env=true` with empty scoped env,
  preserving prior behavior unless metadata is explicitly populated.
- Strict scoped env mode may require users to add baseline variables needed by a
  backend command.

## Resource Impact

No persistent worker or rendering changes. Secret resolution happens only on
MCP schema-check/tool-run/proxy backend spawn paths.

## Security Impact

Improves MCP env isolation and prevents accidental plaintext secret persistence.
Secret values are still held only in process memory long enough to spawn the
backend subprocess.

## I18n/CJK Impact

No new user-facing strings or layout changes.

## Rollback Plan

Revert the migration, `McpServerRow`/`McpServerConfig` env fields, spawn env
handling, proxy/app resolver wiring, and added tests.

## Follow-up

- Add a dedicated Connector Center env binding editor if product scope requires
  user-managed MCP env bindings.
- Extend release security gate coverage when more MCP env UX is added.
