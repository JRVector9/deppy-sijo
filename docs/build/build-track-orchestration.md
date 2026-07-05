# Build Track Orchestration

## Source Inputs
- `docs/review/review-summary.md`
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
- `ai_agent_workspace_final_architecture_v2_6_FOLDER_TREE.md`
- `ai_agent_workspace_v2_8_persistence_store_improvement_FINAL.md`
- `ai_agent_workspace_v3_1_review_build_split_pr_plan.md`
- `ai_agent_workspace_review_build_prompt_pack.md`

## Global Build Rules
- Implement only what is needed to resolve Review findings.
- Do not perform unrelated refactors.
- Preserve existing pane, folder tree, drag-and-drop, copy, paste, CJK, and IME UX.
- Keep raw plaintext logs disabled by default.
- Do not introduce crate cycles across `mcp`, `audit`, `persist`, and `storage`.
- Do not expose terminal backend implementation types to app UI.
- Do not let session directly know the secret store.
- Preserve the normalized visibility policy: active workspace/window active tab visible panes may render/snapshot; hidden tabs/workspaces must not create viewport snapshots.

## First Build Wave

### PR-B06a Audit Encrypted Blob Default-Off
Worker: `019f3034-d08d-7c03-996e-3b8aea976385`

Status: Complete

Sources:
- PR-R06
- PR-R07

Owned scope:
- `crates/app/src/ui/connectors.rs`
- `crates/mcp-proxy/src/hook.rs`
- `crates/mcp-proxy/src/main.rs` / `cli.rs` only if needed
- `crates/mcp/src/transport.rs`
- `crates/audit/src/*` tests only if needed
- `docs/build/PR-B06a-summary.md`

Acceptance criteria:
- Default Connector Center audit rows store redacted input only and leave `input_encrypted_blob IS NULL`.
- Default MCP proxy audit rows leave `input_encrypted_blob IS NULL`.
- Invalid JSON or non-object Connector Center input does not create approval, permission, audit, or tool-call side effects.
- MCP debug logging does not log full unsolicited JSON values.

Verification:
- `cargo test -p audit -p mcp -p mcp-proxy` - pass
- `cargo test -p deppy-sijo` - pass

### PR-B04a Terminal CJK Selection Normalization
Worker: `019f3035-1585-7e62-89be-601c87cb652f`

Status: Complete

Sources:
- PR-R00
- PR-R05

Owned scope:
- `crates/terminal/src/*`
- `crates/app/src/ui/workspace.rs` only if endpoint normalization cannot be kept in terminal code
- `docs/build/PR-B04a-summary.md`

Acceptance criteria:
- Selection starting or ending on wide-char spacer cells copies the full CJK character.
- Existing ASCII selection remains unchanged.
- Required fixture tests cover English, Japanese, Simplified Chinese, Traditional Chinese, Korean, and the rocket emoji path where feasible.
- `cargo test -p terminal` passes.

Verification:
- `cargo test -p terminal` - pass

### PR-B08a Idle Approval Repaint And Output Batch Lower Bound
Worker: `019f3035-4d17-7d62-b559-5849fff369d3`

Status: Complete

Sources:
- PR-R08

Owned scope:
- `crates/app/src/app.rs`
- `crates/app/src/config.rs`
- `crates/app/src/ui/settings.rs` only if display/slider lower bound needs alignment
- `docs/build/PR-B08a-summary.md`

Acceptance criteria:
- Empty app does not schedule periodic repaint solely for approval polling.
- Pending approval still wakes or polls reliably.
- `output_batch_ms = 0` or `1` normalizes to at least 16ms.

Verification:
- `cargo test -p deppy-sijo` - pass

### PR-B01b MCP Dependency Hygiene
Worker: `019f3035-8108-7711-a61d-714c6173db9b`

Status: Complete

Sources:
- PR-R02

Owned scope:
- `crates/mcp/Cargo.toml`
- `Cargo.lock` only if cargo updates it
- `docs/build/PR-B01b-summary.md`

Acceptance criteria:
- Direct `mcp -> rusqlite` dependency is removed.
- Dependency checks still pass.
- No DB schema or migration changes.

Verification:
- `cargo run -p xtask -- check-deps` - pass
- `cargo tree -p mcp --edges normal,build` - pass; no `rusqlite` under `mcp`

## Integrated Verification
- `cargo fmt` - pass
- `cargo test -p terminal` - pass
- `cargo test -p deppy-sijo` - pass
- `cargo test -p audit -p mcp -p mcp-proxy` - pass
- `cargo run -p xtask -- check-deps` - pass
- `cargo check --workspace --all-targets` - pass
- `cargo test --workspace --no-run` - pass
- `cargo tree --workspace --edges normal,build` - pass
- `cargo tree --workspace --edges normal,build,dev` - pass

## Deferred Build Waves
- PR-B00 Boundary Hardening: wait until PR-B06a lands because Connector UI ownership overlaps.
- PR-B03 Folder Tree Scalability & DnD Hardening: start after terminal paste/CJK and security audit changes are integrated.
- PR-B04b Unified Paste/DnD Byte Helper: start after PR-B04a and PR-B03 quoting contract are known.
- PR-B11-B13 I18n: dedicated wave because it changes broad UI/runtime event contracts.
- PR-B02 Pane Resource Guard: can run after current wave; it should not change the normalized visible-pane policy.

## Build PR Review Requests
- PR-B06a: security review against PR-R06/PR-R07, including DB audit rows and logs.
- PR-B04a: terminal/CJK review against PR-R05 and PR-R00 baseline.
- PR-B08a: performance review against PR-R08, especially idle repaint and pending approval wake behavior.
- PR-B01b: dependency review against PR-R02 and `xtask check-deps`.
