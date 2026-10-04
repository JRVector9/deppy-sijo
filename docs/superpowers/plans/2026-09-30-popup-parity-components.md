# Popup Parity Components Implementation Plan

**Goal:** apply all seven approved popup review corrections to shared components and cases 01–03.

**Architecture:** the approved contract is `docs/design/popup-components.md`, with the correction list in `docs/reviews/2026-09-30-popup-mockup-parity.md`. Keep rendering in `ui/popup/`, draft state and operations in WorkspaceAddUi/FileTreeUi. Execute inline; do not restart Deppy or change unrelated dialogs.

**Tech Stack:** Rust, egui 0.36.1, egui_kittest, five locale catalogs.

## Components and sequence

- [x] Reproduce the real GitHub form's thin TextEdit controls and undersized footer buttons with harness rectangle assertions. Run `cargo test --locked -q -p deppy-sijo --bin deppy-sijo popup_parity -- --test-threads=1` and record expected failures.
- [x] `ui/popup/fields.rs`: add shared `text_input(ui, draft, hint)` and `path_input(ui, draft, browse_label)` controls; explicitly size the outer input to 36pt with 8/10pt inner padding, font 13pt and full width. Allocate the path remainder after the button's measured width and 7pt gap. Set label gap 7pt, hint gap 6pt, field spacing 16pt. Style segmented choices without inheriting global selected/hover fills.
- [x] `ui/popup/actions.rs`: retain Primary/Secondary, add Ghost, apply 34pt height, 6/13pt padding, explicit hover/focus/disabled colors, 8pt footer gap and optional localized left footnote. Preserve right-aligned actions and return responses.
- [x] `ui/popup/{mod.rs,notice.rs,shell.rs}`: consolidate dimensions/colors, make notices fill available width and use separate info/error colors; place Close independently of the title height, define subtitle/field line heights and header/body/footer spacing. Preserve modal close and narrow viewport behavior.
- [x] `workspace_add.rs` and `ui/file_tree.rs`: use the shared inputs, path row and Ghost cancel; add localized URL-format and supported-key footnotes. Keep Workspace Add's existing click-only submit behavior and show Esc guidance only; keep create Enter/Esc behavior. Add all new keys to all five locales.
- [x] Rerun real-dialog rectangle tests and component tests. Cover path-row right edge, input/browse vertical alignment, notice width, selected color, keyboard/focus behavior and narrow viewport overflow. Render actual form harnesses to PNG without launching Deppy and inspect images.
- [x] Update design rules and HTML cases 01–03 to the same final dimensions, component APIs and supported keys. Mark the seven review findings resolved with actual test evidence.
- [x] Run focused popup/Workspace Add/File Tree/i18n tests, fmt, strict Clippy and final review. Bump 0.4.4→0.4.5, update workspace lock entries, build release app/proxy and separately stage/sign/verify the 0.4.5 bundle. Leave the running 0.4.4 bundle untouched. Record results and exact continuation commands in CODEX_HANDOFF.

## Acceptance assertions

```rust
assert!((input.rect().height() - 36.0).abs() < 0.5);
assert!((primary.rect().left() - cancel.rect().right() - 8.0).abs() < 0.5);
assert!((browse.rect().right() - url.rect().right()).abs() < 0.5);
assert!((browse.rect().center().y - path.rect().center().y).abs() < 0.5);
```

Header controls must stay inside the modal, field rows must not grow past available width, and errors/long paths must wrap or clip inside the scroll body. No operation is started by render-only component tests.
