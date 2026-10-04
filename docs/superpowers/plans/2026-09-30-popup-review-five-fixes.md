# Popup review five fixes implementation plan

> **For agentic workers:** Execute inline task by task with TDD and review checkpoints. User approved the five fixes in the full-popup-code-review report by requesting code correction. Preserve dirty work. Never launch Deppy. Do not commit/push without a current request.

**Goal:** Fix all five verified popup findings and deliver a versioned rebuilt app without restart.

**Architecture:** Keep presentation in common popup components and target/operation state in App. Use a bounded per-pass pending/current-modal fence, document-scoped widget IDs with transition focus reset, and a wrapped footer with measured height. Register folders through descriptor-based stable volume identity and an atomic identity cache bound to the stored path/dev/inode; unidentified device changes fail closed.

**Tech stack:** Rust, egui/egui_kittest, SQLite/rusqlite, macOS libc fgetattrlist, existing Cargo signing/package scripts.

## Task 1 · Document target focus

Files: `ui/document_dialogs.rs`, App26/27 callers, actual widget tests.

- [x] Add actual consecutive dirty/conflict tests: focus dangerous action, confirm first target, change target (including same basename), bare Enter must not act on second.
- [x] Execute `cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo popup_review -- --test-threads=1` and record assertion RED.
- [x] Pass a target ID separately from constant Area ID. Clear prior focus on target transition and scope footer widgets by target; retain only one previous target value per case.
- [x] Execute the focused tests and document_popups suite; preserve explicit click choices and disabled save.

## Task 2 · Modal/input ownership

Files: new `ui/popup/input.rs`, `popup/mod.rs`/shell, `workspace.rs`, `file_tree.rs`, `shortcuts.rs`, App logic/UI input calls, launcher.

- [x] Add actual search Area Enter/Shift+Enter/Esc tests and closed-menu→inline editor regression; execute to observe RED.
- [x] Add actual WorkspaceUi+conflict first-pass test supplying Text+Enter in one RawInput batch; existing queue state must block all WriteInput before modal rendering. Test early global shortcut too.
- [x] Store `{pass, pending_or_rendered_modal}` once per viewport/pass; publish pending App confirmations before shortcut dispatch and UI, register common Modal/launcher when rendered. Do not use arbitrary Foreground layers as blocking modal evidence.
- [x] Keep Middle legacy forms blocking, preserve non-modal Agents/diff exceptions, and use shared guard for search, file/name keys, terminal input, shortcuts, and Window Esc ownership.
- [x] Test focused popup_review, popup_audit, search, inline rename, IME ownership; ensure App primary and attached surfaces share the guard.

## Task 3 · Folder identity

Files: new `app/src/folder_identity.rs`, App registration worker, storage migration/registration and tests.

- [x] Add storage regression: unknown/different device with same inode must reject and leave original row unchanged; run for RED.
- [x] Read UUID from an opened directory descriptor on macOS using libc `fgetattrlist(ATTR_VOL_INFO|ATTR_VOL_UUID)`; absent/zero/unsupported ID is None. Preserve existing path/UTF8/admission rules. Compare path metadata with opened descriptor before use.
- [x] Add bounded per-workspace volume identity storage bound to exact stored path/dev/inode. Legacy API without UUID is strict `(dev,ino)`; stable UUID+inode may authorize device renumbering. Different UUID must reject even at equal inode, and stale identity must not authorize rebind. Save/update within the same transaction; no render I/O.
- [x] Test stable remount, distinct volume, legacy unknown, changed inode, stale cache, rollback, real native UUID query, and App worker registration.
- [x] Keep legacy device mismatch fail closed; report this conservative fallback clearly rather than silently importing old workspace state.

## Task 4 · Responsive common footer

Files: `popup/actions.rs`, `popup/shell.rs`, actual document/confirmation geometry tests.

- [x] Reproduce en-US27 at280pt, assert every action and area remains inside viewport; add short-height/long-path case and observe RED.
- [x] Wrap RTL action rows with 8pt row spacing and clamp long single buttons to content width. Reserve measured footer height for scrollable body and repaint only on changed dimensions; constant cache IDs.
- [x] Test all five locales and unchanged normal-width button height/padding/hint alignment; inspect actual narrow-render PNG if necessary.

## Task 5 · Review and release

- [x] Update design contract/numbered HTML for target safety, real modal fence, responsive footer, and folder volume matching; maintain handoff after meaningful steps/tests/failures.
- [x] Run full affected App/storage/Connector/i18n suites, strict all-target Clippy, fmt/diff, boundary and HTML syntax gates. Launch no native app or user shell/file operations.
- [x] Independent Codex CLI review; resolve confirmed findings and rerun affected gates. Record exact results, not historical counts.
- [x] Check versions across bundles/worktrees, bump patch above last shipped0.4.10, update workspace lock versions, build app/proxy, stage a separate bundle, verify compiled version + both plist fields + signature/archive using existing local package verifier. No restart.
- [x] Final report: five corrected behaviors, tests, artifact version/location, review limits. No commit/push requested.
