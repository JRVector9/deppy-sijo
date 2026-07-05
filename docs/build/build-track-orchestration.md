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

## Second Build Wave

### PR-B02a Pane Resource Guard
Worker: `019f3045-607e-7922-99c8-f13226473393`

Status: Complete

Sources:
- PR-R03

Owned scope:
- `crates/app/src/ui/workspace.rs`
- `crates/runtime/src/remote.rs`
- `crates/session/src/session.rs` only if needed
- `docs/build/PR-B02a-summary.md`

Acceptance criteria:
- Hidden stale `Viewport` events do not rehydrate UI snapshot cache after a `MuxUpdated` makes their session invisible.
- Remote server `last_sent` baselines are pruned by the visible set from `MuxUpdated`.
- Remote client `recon` baselines are pruned by the visible set from `MuxUpdated`.
- Active tab visible split panes continue to render; focused-only rendering is forbidden.

Verification:
- `cargo test -p deppy-sijo workspace` - pass
- `cargo test -p runtime -p mux` - pass

### PR-B03a Folder Tree Shell Quoting And Empty Root
Worker: `019f3045-e80e-7372-ad03-5655f2fc27a2`

Status: Complete

Sources:
- PR-R00
- PR-R04

Owned scope:
- `crates/app/src/ui/file_tree.rs`
- `crates/app/src/app.rs`
- `docs/build/PR-B03a-summary.md`

Acceptance criteria:
- Shell-specific quoting helper covers POSIX, fish, PowerShell, and cmd.
- Existing `shell_quote(path)` remains available for current call sites.
- Empty workspace path does not silently expose Desktop as file tree root.
- Terminal path insertion remains no-auto-Enter.

Verification:
- `cargo test -p deppy-sijo file_tree` - pass
- `cargo test -p deppy-sijo workspace_path_to_tree_root` - pass
- `cargo test -p deppy-sijo` - pass

### PR-B05a Secret-like Env And Args Persistence Guard
Worker: `019f3046-329c-7811-96c1-bb2506a26a42`

Status: Complete

Sources:
- PR-R06

Owned scope:
- `crates/app/src/ui/env_profiles.rs`
- `crates/app/src/ui/agents.rs`
- `crates/app/src/ui/connectors.rs`
- `crates/storage/src/db.rs`
- `crates/mcp-store/src/lib.rs`
- `docs/build/PR-B05a-summary.md`

Acceptance criteria:
- Secret-like env keys cannot be saved as `EnvValue::Plain` by default.
- Secret-backed env values still save successfully.
- Agent and MCP args with high-confidence secret-like payloads are rejected before SQLite persistence.
- Rejected values do not appear in DB plain columns or args JSON.

Verification:
- `cargo test -p storage -p mcp-store` - pass
- `cargo test -p deppy-sijo agents` - pass
- `cargo test -p deppy-sijo connectors` - pass
- `cargo test -p deppy-sijo` - pass

## Second Wave Integrated Verification
- `cargo fmt` - pass
- `cargo check --workspace --all-targets` - pass
- `cargo test --workspace --no-run` - pass
- `cargo test -p deppy-sijo file_tree` - pass
- `cargo test -p deppy-sijo workspace_path_to_tree_root` - pass
- `cargo test -p deppy-sijo workspace` - pass
- `cargo test -p runtime -p mux` - pass
- `cargo test -p storage -p mcp-store` - pass
- `cargo test -p deppy-sijo agents` - pass
- `cargo test -p deppy-sijo connectors` - pass
- `cargo test -p deppy-sijo` - pass
- `cargo run -p xtask -- check-deps` - pass
- `cargo tree --workspace --edges normal,build` - pass
- `cargo tree --workspace --edges normal,build,dev` - pass
- `git diff --check` - pass

## Deferred Build Waves
- PR-B00 Boundary Hardening: ready for a focused follow-up now that PR-B06a and PR-B05a have landed.
- PR-B03b Folder Tree Async Listing: ready for a focused follow-up now that PR-B03a quoting/root contract is integrated.
- PR-B04b Unified Paste/DnD Byte Helper: ready for a focused follow-up now that PR-B04a and PR-B03a have landed.
- PR-B11-B13 I18n: dedicated wave because it changes broad UI/runtime event contracts.
- PR-B10 Output Pipeline Backpressure and PR-B15 SQLite batching: defer until pane/security/folder tree wave lands.

## Build PR Review Requests
- PR-B06a: security review against PR-R06/PR-R07, including DB audit rows and logs.
- PR-B04a: terminal/CJK review against PR-R05 and PR-R00 baseline.
- PR-B08a: performance review against PR-R08, especially idle repaint and pending approval wake behavior.
- PR-B01b: dependency review against PR-R02 and `xtask check-deps`.
- PR-B02a: pane/resource review against PR-R03, especially hidden `Viewport` handling and split-pane visibility.
- PR-B03a: folder tree/DnD review against PR-R04, especially shell quoting fixtures and empty root handling.
- PR-B05a: security review against PR-R06, especially rejected secret-like env and args persistence.
