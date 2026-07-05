# Build PR Summary

## Input Findings
- PR-R06 Finding 3 / PR-R07 Finding 1: Connector Center and `deppy-mcp-proxy` passed a keyring encryptor by default, so `input_encrypted_blob` could retain raw tool input without explicit opt-in.
- PR-R07 Finding 4: Connector Center direct calls could reach policy/approval/audit before proving the user input was a JSON object.
- PR-R06 Finding 4: MCP unsolicited server messages could log full JSON values at debug level.

## Scope
- Changed only the PR-B06a paths for audit default-off, Connector Center validation, proxy hook validation, and MCP transport debug redaction.
- No raw/encrypted audit input opt-in plumbing was added in this PR.
- No boundary refactor, store crate restructuring, folder tree, DnD, pane, or terminal UX changes were made by this PR.

## Changes
- Connector Center now validates tool input as a size-bounded JSON object before policy evaluation, approval display, permission rule persistence, audit insertion, or tool execution.
- Connector Center audit writes now call `record_tool_audit(..., None)`, so default rows keep `input_redacted_json` and leave `input_encrypted_blob` NULL.
- `deppy-mcp-proxy` `DbPermissionHook` no longer owns or passes a keyring encryptor for audit writes. Keyring remains only for startup redaction seeding.
- Proxy hook defensively rejects non-object arguments before approval/audit logic if called directly with an invalid `Value`.
- MCP transport unsolicited/debug logging now records only the server method name, not full JSON params/value.
- Added regression tests for default NULL audit blobs, invalid input with no audit row, proxy non-object rejection, and debug log field capture.

## Tests
- `cargo test -p audit -p mcp -p mcp-proxy` - pass
- `cargo test -p deppy-sijo connectors` - pass
- Focused pre-checks also passed:
  - `cargo test -p mcp unsolicited_debug_log -- --nocapture`
  - `cargo test -p mcp-proxy 기본_proxy_audit -- --nocapture`
  - `cargo test -p mcp-proxy hook_non_object -- --nocapture`
  - `cargo test -p deppy-sijo connector_ -- --nocapture`
  - `cargo test -p deppy-sijo tool_arguments -- --nocapture`

## Risk Notes
- Existing encrypted raw audit blob support remains available in `audit::record_audit` for future explicit opt-in callers; this PR only changes default app/proxy callers.
- Connector Center validation happens in both submit and run paths so stale/impossible invalid states cannot persist permission rules before failing.
- The proxy server loop already rejects non-object `arguments`; the hook guard is an extra defense for future direct callers.

## Rollback Plan
- Revert the Connector Center call sites to pass an encryptor only if a future explicit user/config opt-in exists.
- Revert the proxy hook audit call to pass an encryptor only behind equivalent explicit opt-in plumbing.
- Revert the transport logging helper only if a replacement redaction-safe structured logger is added.

## Follow-up Review Requests
- Review PR-B06a against PR-R06 Findings 3/4 and PR-R07 Findings 1/4.
- Verify default Connector Center and proxy audit rows contain redacted input and `input_encrypted_blob IS NULL`.
- Confirm invalid Connector Center inputs do not create approval dialogs, permission rules, audit rows, or backend tool calls.
