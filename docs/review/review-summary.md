# Review Summary

## Overall Verdict
- Review Track result: Block
- Critical: 0
- High: 16 merged findings
- Medium: 28 merged/backlog findings
- Low: 9 merged/backlog findings

`cargo check --workspace --all-targets` and `cargo test --workspace --no-run` passed. Dependency graph checks passed and no Cargo package cycle was reproduced. The main blockers are not compilation blockers; they are boundary, security, DnD/scalability, MCP audit/env, performance, and i18n readiness gaps against the v2.6/v2.8/v3.1 design documents.

## Files Produced
- `docs/review/PR-R00-current-implementation-inventory.md`
- `docs/review/PR-R01-boundary-violation-findings.md`
- `docs/review/PR-R02-dependency-graph-store-cycle-findings.md`
- `docs/review/PR-R03-pane-mux-visibility-findings.md`
- `docs/review/PR-R04-folder-tree-dnd-findings.md`
- `docs/review/PR-R05-terminal-clipboard-paste-cjk-findings.md`
- `docs/review/PR-R06-redaction-secret-env-leak-findings.md`
- `docs/review/PR-R07-mcp-permission-audit-findings.md`
- `docs/review/PR-R08-resource-performance-findings.md`
- `docs/review/PR-R09-i18n-readiness-findings.md`

## Commands Run
- `cargo check --workspace --all-targets` - pass
- `cargo test --workspace --no-run` - pass
- `cargo tree --workspace --edges normal,build` - pass
- `cargo tree --workspace --edges normal,build,dev` - pass
- `cargo metadata --format-version 1 > target/cargo-metadata.json` - pass
- `cargo run -p xtask -- check-deps` - pass, no forbidden edge/cycle
- `cargo test -p runtime --lib` - pass after sandbox loopback bind failure was retried outside the sandbox
- `cargo test -p terminal -p storage` - pass
- `cargo test -p mcp -p audit -p mcp-proxy -p mux` - pass
- Subagents additionally ran focused `rg` commands and targeted tests for dependency graph, mux/runtime visibility, terminal, mcp/audit/proxy/storage.

## Second Pass Update
- PR-R00 baseline corrected: implemented DnD paste is folder-tree/sidebar `PathBuf` insertion into terminal, not terminal-selected-text internal drag/drop paste.
- PR-R01 broadened: Connector UI directly constructs/runs local MCP manager flows, evaluates policy, writes audit, and passes secret-backed audit encryption from UI leaf modules. This is tracked under the existing High boundary blocker.
- PR-R01 `InProcessRuntimeClient` usage in `crates/app/src/app.rs` is downgraded to Low/Open Policy if that file is the composition root; leaf UI still uses `&dyn RuntimeClient`.
- PR-R02 storage facade status is downgraded to Low policy alignment because v2.8 §17.3 permits a maintained composition/app-level storage facade and the graph remains a legal DAG.
- PR-R03 active-pane wording is normalized: active workspace/window active tab visible panes may render/snapshot; focused pane is for input/IME/scroll. A focused-only fix would regress split panes.
- PR-R06/PR-R07 `input_encrypted_blob` default-on is one cross-cutting High finding, not two separate findings.
- PR-R07 schema reapproval risk is narrowed to `deppy-mcp-proxy` session cache; Connector Center direct calls rediscover tools before policy evaluation.
- PR-R08 backpressure wording corrected: PTY output is already bounded; the remaining gap is local command/status/input queue policy and user-visible backpressure signaling.
- PR-R09 dynamic domain values are correctly preserved raw. Terminal output, paths, commands, env keys, and MCP server/tool names should remain unlocalized; surrounding UI prose and generated default titles still need i18n work.

## Build Track Handoff: Critical/High

### B00 Boundary Hardening
Sources: PR-R00, PR-R01
Severity: High
Scope:
- Remove direct `SecretStore`/`KeyringSecretStore` usage from `crates/app/src/ui/*`.
- Move Connector UI MCP manager execution, permission policy, audit logging, storage calls, and secret-backed audit encryption behind a runtime/app-service boundary.
- Decide whether `crates/app/src/app.rs` is a composition root exception.
- Add `xtask check-boundary` for UI/runtime/terminal/pty/secret boundaries.
Acceptance criteria:
- UI leaf modules do not directly call secret store or storage repos unless explicitly allowlisted.
- Terminal/runtime app UI still sends terminal actions through `RuntimeClient`.
- Existing folder tree, DnD, copy/paste UX remains intact.

### B03 Folder Tree DnD Scalability And Quoting
Sources: PR-R04, PR-R08, PR-R00
Severity: High
Scope:
- Shell-aware quoting for POSIX, fish, PowerShell, cmd.
- Async/chunked directory listing for large flat directories.
- Multi-path payload support.
- Clarify terminal path insert vs tree-internal move contract.
- Clarify empty workspace path behavior before enabling tree DnD/move from fallback roots.
Acceptance criteria:
- No auto Enter on terminal drop.
- 100k direct-child directory does not block UI thread beyond agreed budget.
- Path fixtures with spaces, apostrophe, Japanese, Chinese, Korean, emoji pass shell-specific tests.
- Empty workspace path either shows the path-required state or explicitly documents and tests any Desktop fallback behavior.

### B04 Terminal Clipboard/Paste/CJK
Sources: PR-R05
Severity: High
Scope:
- Normalize wide-char selection endpoints.
- Unify clipboard paste, path insert, and DnD paste byte generation with bracketed paste support.
- Add clipboard failure abstraction/notification.
- Disambiguate terminal-selected-text internal DnD paste from file-tree `PathBuf` DnD.
Acceptance criteria:
- Selection starting or ending on wide-char spacer copies the full CJK character.
- DnD path insertion respects bracketed paste mode and still does not auto-execute.
- Required PR-R05 language/emoji fixtures are covered.
- Fixture coverage includes `src/main.rs`, Japanese, Simplified Chinese, Traditional Chinese, Korean, and `project/🚀-deploy/config.json` across selection/copy and paste/DnD byte generation.

### B05/B06 Redaction, Secret, MCP Audit, MCP Env
Sources: PR-R06, PR-R07
Severity: High
Scope:
- Reject or explicitly warn on secret-like env keys stored as `EnvValue::Plain`.
- Reject/redact secret-like agent/MCP args before SQLite persistence and UI display.
- Make `input_encrypted_blob` explicit opt-in and default-off for app and proxy paths.
- Remove sensitive full JSON logging from MCP unsolicited messages.
- Add MCP server scoped env injection; prevent backend MCP process from inheriting full agent env.
- Scope schema reapproval work to the MCP proxy session cache; direct Connector Center calls already rediscover tools before policy evaluation.
Acceptance criteria:
- API keys/tokens are not saved in SQLite plain env or args fields by default.
- Default MCP audit rows have redacted input only and `input_encrypted_blob IS NULL`.
- MCP backend receives only controlled baseline env + scoped MCP env.
- Debug/crash dump hardening covers `RuntimeCommand`, `EnvValue`, `AgentConfigRow`, `McpServerRow`, and `McpServerConfig`.

### B08/B10/B15 Performance And Backpressure
Sources: PR-R08
Severity: High/Medium mix
Scope:
- Remove idle approval polling repaint loop.
- Align `output_batch_ms` lower bound with UI/documented 16ms.
- Keep PTY output bounded and add local command/status/input queue policy plus user-visible backpressure signal.
- Add SQLite write batching/debounce.
Acceptance criteria:
- Empty app does not schedule periodic repaint solely for approval polling.
- Pending approval still wakes the UI reliably.
- Queue policies are bounded/coalesced and covered by tests.

### B11-B13 I18n Infrastructure And Message Boundary
Sources: PR-R09
Severity: High
Scope:
- Add `crates/i18n`, required catalogs for `en-US`, `ja-JP`, `zh-Hans`, `zh-Hant`, fallback, pseudo-locale.
- Replace hardcoded UI strings in high-risk dialogs first.
- Convert RuntimeEvent/NotificationEvent user-facing messages to `message_id + args`.
- Represent runtime-generated default mux/session titles as stable kind/ordinal or message ids; persist user-supplied titles literally.
Acceptance criteria:
- `cargo xtask i18n-check` validates required locales.
- Runtime/notification payloads do not carry rendered UI prose.
- Terminal output, paths, commands, env keys, MCP tool names remain unlocalized.

## Medium / Low Backlog
- PR-R02: Keep storage facade policy aligned with v2.8 §17.3; remove unused `mcp -> rusqlite`.
- PR-R03: Prevent stale hidden `Viewport` from rehydrating UI cache; prune remote server `last_sent` and remote client `recon`; centralize hidden snapshot guard.
- PR-R04: Add gitignore/global ignore rules; resolve tree-internal move contract; settle empty workspace path fallback.
- PR-R05: Preserve grapheme clusters for copy or document v0 limits; define terminal internal DnD paste; add clipboard mock failure and required CJK/emoji fixture tests.
- PR-R06: Manual `Debug` redaction for `RuntimeCommand`, `EnvValue`, `AgentConfigRow`, `McpServerRow`, and `McpServerConfig`.
- PR-R07: MCP proxy schema hash cache invalidation; Connector Center validate JSON before approval/audit.
- PR-R08: terminal dirty ranges, byte/RSS cache budget, process resource monitor, plain remote slow-consumer gate.
- PR-R09: pseudo-locale visual smoke, CJK overflow tests, generated default mux/session title localization.

## Recommended Operating Order
1. Build Track starts with security and boundary blockers: B05/B06, then B00.
2. In parallel or immediately after, address user-visible UX blockers: B03 and B04.
3. Then address performance blockers: idle repaint and folder tree scalability, followed by cache/backpressure/SQLite batching.
4. I18n B11-B13 should be planned as a dedicated track because it touches broad UI/runtime event contracts.
5. Every Build PR must receive a Build PR Review using the prompt pack and run `cargo check --workspace --all-targets`, `cargo test --workspace --no-run`, and relevant extra gates.

## Build PR Review Requests
- Boundary/security PRs: request review against PR-R01, PR-R06, PR-R07 findings plus secret/env/API key invariant.
- Folder tree/DnD PRs: request review against PR-R04, PR-R08, PR-R05 bracketed paste overlap.
- Terminal/CJK PRs: request review against PR-R05 and PR-R00 baseline.
- I18n PRs: request review against PR-R09 and verify terminal output/path/command values are not localized.
- Performance PRs: request review against PR-R08 and run release-mode perf smoke before merge.

## Open Policy Decisions
- Confirm normalized wording: active workspace/window active tab visible panes render/snapshot; focused pane is input/IME/scroll target.
- Is `crates/app/src/app.rs` a composition root exception for concrete runtime/secret construction?
- Should secret-like env plain values be hard-blocked or allowed behind explicit unsafe override?
- Is encrypted raw audit input ever default-on, or always explicit opt-in?
- Should MCP backend processes inherit agent env, or only scoped MCP env?
- Is `ko-KR` the source/default locale or optional locale?
