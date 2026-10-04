# Session and workspace close popups Implementation Plan

**Goal:** Apply and verify the approved shared popup style for session and workspace termination without restarting Deppy.

**Architecture:** Keep the captured pane/workspace and runtime commands in their callers. Put the two localized presentation specifications in `ui/session_close_dialogs.rs`, reusing `popup::confirmation`. Reuse the document popup's target identity/focus protection in the common popup shell; keep fixed modal case IDs and scope action widgets to the captured target. Preserve the existing workspace confirmation setting and ended-session immediate cleanup policy.

**Tech Stack:** Rust, egui/egui_kittest, existing i18n catalog and popup components.

The existing design cases07/08 and the user's application request authorize this implementation. Execute inline, preserve prior dirty work, no new agents/commit/push/restart.

## Steps

- [x] Reproduce sequential same-name session confirmations carrying Danger button focus to a different pane with the real WorkspaceUi caller; verify only the explicitly confirmed pane closes.
- [x] Reproduce equivalent workspace target focus transfer with the current shared specification. Keep bare Enter, cancel, Esc and the explicit button behavior covered.
- [x] Extract existing target-change focus reset from document dialog into shared popup shell (`prepare_target`) and add target-aware confirmation (`confirmation_for_target`). Keep fixed Area/footer keys and existing document action scope.
- [x] Add `ui/session_close_dialogs.rs` presentation helpers for07/08 and wire `WorkspaceUi::close_confirm_dialog` / App workspace close confirmation through them. Pass pane/workspace identity, not display names. Update the actual08 offscreen renderer to call the helper used by App.
- [x] Run the regression and existing confirmation/document/input ownership tests; inspect07/08 offscreen screenshots without launching Deppy.
- [x] Update popup design and numbered HTML07/08. Run full App suite, strict App Clippy, fmt/diff/UI boundary; review only this task's source changes against the preserved0.5.1 snapshot and fix verified findings.
- [x] Increase patch version0.5.1→0.5.2 (after verifying local maximum), regenerate all inherited lock versions; release-build App/proxy, separately sign/package0.5.2 and verify compiled/plist/signature/archive versions without launch.
- [x] Update handoff/report with actual commands/results, source hash, remaining work and unchanged running PID.

## Exact checks

```sh
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo close_popup -- --test-threads=1
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo document_popups -- --test-threads=1
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo popup_parity_render_session_close -- --ignored --test-threads=1
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo popup_parity_render_workspace_close -- --ignored --test-threads=1
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo
cargo clippy --offline --locked -p deppy-sijo --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
cargo run --offline --locked -q -p xtask -- check-boundary
```

## Completion

Completed0.5.2 without launching/stopping Deppy. Actual App2514/i18n8 and both explicit offscreen renderers pass; independent scoped source review has no confirmed unresolved finding. Report: `docs/reviews/2026-10-01-session-workspace-close-popups.md`.
