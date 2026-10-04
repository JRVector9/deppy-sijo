# Popup confirmations and inline workspace rename

**Goal:** restore workspace-row name editing and migrate approved HTML cases 07, 08, 10, 11, 12, 13 to shared confirmation components.

**Architecture:** add one confirmation presentation component under `ui/popup/`, using the existing shell/body/notice/footer and a Danger action role. Preserve target identity, runtime generation and IO admission in each caller. FileTreeUi owns the inline workspace-name draft; App receives a commit action and retains the existing asynchronous rename worker. Execute inline and preserve the earlier uncommitted 0.4.5 changes. No restart authorized.

**Tech Stack:** Rust, egui 0.36.1, egui_kittest/wgpu, five locale catalogs.

## Steps

- [x] Add a failing real UI assertion that resource/port confirmations use a centered modal rather than panel-local controls. Add an inline rename test that places TextInput inside the workspace row and emits the exact workspace ID/name on Enter, with Escape discarding.
- [x] `ui/popup/{mod,actions,confirmation}.rs`: add Danger palette/action and `ConfirmationSpec` + Confirm/Cancel choice. Confirmation uses existing shared shell/body/notices/footer, wraps long targets, closes by Cancel/X/backdrop/Esc and does not submit an unfocused destructive action on bare Enter.
- [x] Migrate `ui/workspace.rs::close_confirm_dialog` (07), `app.rs` workspace close block (08), `ui/resource_manager.rs` session/unattached confirmation (10/11), `ui/ports.rs` termination confirmation (12), and `ui/file_tree.rs::permanent_delete_confirm` (13). Keep ClosePane/CloseWorkspace/runtime_instance/socket bind/start identity and delete target/queue admission behavior intact.
- [x] Account for Resource/Ports popover lifecycle: confirmations must remain visible after the source popover closes, Escape must cancel the front modal, and stale targets must retain existing host validation. Verify through actual production host or a popup-backed harness.
- [x] `ui/file_tree.rs`: add a bounded inline name draft and a reusable workspace-row editor, focus once on entry, Enter commit, Escape/outside focus/navigation cancel (same behavior as the existing session editor); keep workspace ID and actual filesystem path unchanged. Route the existing RenameWorkspace menu into this editor and emit a CommitWorkspaceName action. `app.rs`: queue the existing RenameWorkspace settings worker from this action; remove obsolete sidebar rename Window/state.
- [x] Add four confirmation title keys and a distinct close-dialog accessibility label to all locales. Update `docs/design/popup-components.md` and the numbered HTML cases, retaining numbering for the inline rename inventory entry.
- [x] Run affected interaction/target/queue tests, offscreen visual render tests, full app/i18n suite, strict Clippy/fmt/diff checks and source review. Address findings and record actual results in CODEX_HANDOFF.
- [x] Bump 0.4.5→0.4.6 for the corrected UI, regenerate lock entries, release-build app/proxy, stage/sign/verify a separate 0.4.6 bundle/ZIP. Leave live 0.4.5 PID45213 untouched.

## Exact checks

`cargo test --locked -q -p deppy-sijo --bin deppy-sijo confirmation -- --test-threads=1`

`cargo test --locked -q -p deppy-sijo --bin deppy-sijo inline_workspace_rename -- --test-threads=1`

`cargo test --locked -q -p deppy-sijo --bin deppy-sijo ui::file_tree::tests -- --test-threads=1`

`cargo test --locked -q -p deppy-sijo --bin deppy-sijo -- --test-threads=1`

`cargo test --locked -q -p i18n`

`cargo clippy --locked -q -p deppy-sijo --bin deppy-sijo -- -D warnings`

`cargo fmt --all -- --check` and `git diff --check`

`cargo build --release --locked -p deppy-sijo -p mcp-proxy`

No destructive operation is executed by confirmation rendering; harnesses assert the existing intent/command paths instead.
