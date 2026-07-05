# Build PR Summary

## Input Findings
- Wave 2 Build PR Review Finding 2: agent/MCP args scanners missed one-line `--api-key value`, `--database-url=...`, and secret-like `KEY=VALUE` payloads.

## Scope
- Narrow follow-up to the PR-B05a persistence guard.
- No DB schema change.
- No UI flow redesign.

## Changes
- `storage` agent args validation now scans assignment forms and token-like fragments inside each arg string.
- `mcp-store` server args validation now applies the same checks.
- Added regression tests proving one-line secret flags, database URL flags, and secret-like assignments are rejected before rows are inserted.

## Tests
- `cargo test -p storage -p mcp-store` - pass
- `cargo test -p deppy-sijo agents` - covered by full app test
- `cargo test -p deppy-sijo connectors` - pass
- `cargo test -p deppy-sijo` - pass

## Risk Notes
- Scanner remains intentionally high-confidence and may not detect every possible secret representation.
- The scanner remains duplicated in `storage` and `mcp-store` to avoid adding a new cross-store dependency in this wave.

## Rollback Plan
- Revert the scanner helper changes and added tests in `crates/storage/src/db.rs` and `crates/mcp-store/src/lib.rs`.
- No migration rollback is required.

## Follow-up Review Requests
- Review false-positive/false-negative boundaries for `--flag value`, `--database-url=...`, and `KEY=VALUE` args.
- Consider moving shared secret-like scanning into an acyclic lower-level crate in a later dependency-focused PR.
