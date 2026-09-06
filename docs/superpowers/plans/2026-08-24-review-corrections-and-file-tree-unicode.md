# Review Corrections and File-tree Unicode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (- [ ]) syntax for tracking.

**Goal:** Fix the four confirmed App/Workspace lifecycle defects and render macOS NFD Korean file names as composed Hangul without changing filesystem identity.

**Architecture:** Reuse the existing document save continuation, auxiliary-tab transitions, split transaction, resize-presentation fence, and file-tree flat cache. Three exclusive shared-worktree lanes own app.rs, workspace.rs, and file_tree.rs; each lane must produce observed RED evidence before production edits, while the root owns docs, integration, review, and commits.

**Tech Stack:** Rust workspace, egui 0.35, unicode-normalization 0.1, Cargo/libtest, Codex CLI review.

---

## File map and ownership

- Modify/test crates/app/src/app.rs: document close disposition and session navigation.
- Modify/test crates/app/src/ui/workspace.rs: split drag lifecycle and presentation settlement.
- Modify/test crates/app/src/ui/file_tree.rs: display-only NFC normalization.
- Modify docs/CODEX_HANDOFF.md: root-only evidence ledger.
- Create the Workstep Obsidian journal only after the final code commit.

Agents must not edit another lane, manifests, plan/spec files, or CODEX_HANDOFF.md.

### Task 1: App document-close disposition

**Files:**

- Modify crates/app/src/app.rs around document_close_requires_confirm and apply_document_tab_intent.
- Test in the existing app.rs document tests.

- [ ] **Step 1: Write the failing disposition test**

    #[test]
    fn document_close_disposition은_saving을_dirty보다_우선한다() {
        let mut document = stub_open_document("/tmp/a.md", "A", "A", false);
        assert_eq!(
            document_close_disposition(Some(&document)),
            DocumentCloseDisposition::CloseNow
        );
        document.dirty = true;
        assert_eq!(
            document_close_disposition(Some(&document)),
            DocumentCloseDisposition::ConfirmDirty
        );
        document.saving = true;
        document.dirty = false;
        document.saving_source = Some("B".to_owned());
        assert_eq!(
            document_close_disposition(Some(&document)),
            DocumentCloseDisposition::DeferUntilSave
        );
    }

- [ ] **Step 2: Run it and verify RED**

Run:

    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo document_close_disposition은_saving을_dirty보다_우선한다 --locked -- --test-threads=1

Expected: exit 101 because DocumentCloseDisposition and document_close_disposition do not exist. The selected test count must be one; discard any accidental zero-test run.

- [ ] **Step 3: Implement the minimal disposition**

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum DocumentCloseDisposition {
        CloseNow,
        ConfirmDirty,
        DeferUntilSave,
    }

    fn document_close_disposition(
        document: Option<&OpenDocument>,
    ) -> DocumentCloseDisposition {
        match document {
            Some(document) if document.saving => {
                DocumentCloseDisposition::DeferUntilSave
            }
            Some(document) if document.dirty => {
                DocumentCloseDisposition::ConfirmDirty
            }
            _ => DocumentCloseDisposition::CloseNow,
        }
    }

Replace the Close branch with a match. DeferUntilSave inserts id into document_close_after_save, ConfirmDirty enqueues CloseWithDirty, and CloseNow calls close_document_entry. Do not expose Discard while a write is already in flight.

- [ ] **Step 4: Add the save/revert regression**

Construct an OpenDocument with source A, saved_source A, saving_source B, saving true, and dirty false. Apply a Saved outcome through the existing save helper. Assert saved_source becomes B, source remains A, dirty becomes true, and a pending close does not complete silently. Use the existing revision fixture constructor; do not alter production types for the test.

- [ ] **Step 5: Run focused GREEN tests**

    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo document_close_disposition --locked -- --test-threads=1
    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo 저장중_close후 --locked -- --test-threads=1

Expected: each command selects at least one test and exits 0.

### Task 2: App session navigation demotes every auxiliary tab

**Files:**

- Modify crates/app/src/app.rs at reveal_terminal_session.
- Test in existing auxiliary-tab tests.

- [ ] **Step 1: Write a failing pure transition test**

    #[test]
    fn 세션이동은_모든_보조탭을_inactive로_보존한다() {
        let (history, git, document, reset_search) =
            reveal_session_aux_tabs(
                PaneAuxTabState::OpenInactive,
                PaneAuxTabState::OpenInactive,
                PaneAuxTabState::OpenActive,
            );
        assert_eq!(history, PaneAuxTabState::OpenInactive);
        assert_eq!(git, PaneAuxTabState::OpenInactive);
        assert_eq!(document, PaneAuxTabState::OpenInactive);
        assert!(reset_search);

        let (history, git, document, reset_search) =
            reveal_session_aux_tabs(
                PaneAuxTabState::Closed,
                PaneAuxTabState::Closed,
                PaneAuxTabState::Closed,
            );
        assert_eq!(history, PaneAuxTabState::Closed);
        assert_eq!(git, PaneAuxTabState::Closed);
        assert_eq!(document, PaneAuxTabState::Closed);
        assert!(!reset_search);
    }

The test module already imports `PaneAuxTabState`; use that exact existing type.

- [ ] **Step 2: Run it and verify RED**

    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo 세션이동은_모든_보조탭을_inactive로_보존한다 --locked -- --test-threads=1

Expected: exit 101 because reveal_session_aux_tabs is missing.

- [ ] **Step 3: Implement and wire the transition**

The helper returns each state after on_session_tab_click plus a boolean recording whether any input state was active. reveal_terminal_session assigns all three states and resets aux_search only when that boolean is true. It must not clear documents, active_document, or Git data.

- [ ] **Step 4: Run focused and document GREEN tests**

    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo 세션이동은_모든_보조탭을_inactive로_보존한다 --locked -- --test-threads=1
    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo document --locked -- --test-threads=1

Expected: focused and document filters exit 0; only previously documented measurement tests may be ignored.

### Task 3: Workspace defers old presentation fences during split lifecycle

**Files:**

- Modify crates/app/src/ui/workspace.rs at settle_session_resize_presentation, render_node Split, and split_handle.
- Test beside the existing resize-presentation tests.

- [ ] **Step 1: Write the failing settlement regression**

    #[test]
    fn active_split_drag_defers_prior_resize_presentation_fence() {
        let started = std::time::Instant::now();
        let session = SessionId(62);
        let mut ui = WorkspaceUi::new();
        let view = ui.sessions.entry(session).or_default();
        view.install_snapshot(shaped_snapshot(80, 24, "stable"));
        view.arm_resize_presentation(100, 30, started);
        assert!(view.buffer_resize_snapshot(
            shaped_snapshot(100, 30, "old target"),
            started + std::time::Duration::from_millis(1),
        ));
        ui.begin_split_drag(tab_id("t"), Vec::new(), 0.72);
        let repaint = ui.settle_session_resize_presentation(
            session,
            started + std::time::Duration::from_millis(250),
        );
        assert_eq!(repaint, None);
        assert_eq!(
            ui.sessions[&session].snapshot.as_ref().unwrap().cols,
            80
        );
        assert!(ui.sessions[&session].resize_presentation.is_some());
    }

- [ ] **Step 2: Run it and verify RED**

    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo active_split_drag_defers_prior_resize_presentation_fence --locked -- --test-threads=1

Expected: one test fails because the old target is promoted and its fence cleared.

- [ ] **Step 3: Add the settlement gate**

    fn resize_presentation_settlement_deferred(
        &self,
        session: SessionId,
    ) -> bool {
        self.split_drag.is_some()
            || self.split_final_resize_sessions.contains(&session)
            || self.split_final_resize_pending.values().any(
                |(pending, _, _)| *pending == session,
            )
    }

settle_session_resize_presentation returns None before borrowing the SessionView when this is true. It must not schedule a drag-period timer.

- [ ] **Step 4: Activate drag state before child pane rendering**

Add one shared helper:

    fn split_handle_id(
        tab: &runtime::MuxTabId,
        path: &[u8],
    ) -> egui::Id {
        egui::Id::new(("split_handle", tab, path))
    }

Before requested_ratio is computed in LayoutNode::Split, if the mode owns input and ctx.is_being_dragged(split_handle_id(tab_id, path)), read pointer.interact_pos, calculate the same clamped ratio used by split_handle, and call begin_split_drag. Retain the post-child interact registration for hit-test priority and replace its inline ID with split_handle_id.

- [ ] **Step 5: Add the first-drag-frame wiring regression and run GREEN**

Use the existing split kittest fixture to arm a promote-ready fence, press/drag the real handle, and assert the stable generation is unchanged on the first dragged frame. Name it split_drag_is_activated_before_child_pane_settlement.

    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo split_drag_is_activated_before_child_pane_settlement --locked -- --test-threads=1
    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo active_split_drag_defers_prior_resize_presentation_fence --locked -- --test-threads=1

Expected: both commands select one test and exit 0.

### Task 4: Workspace cancels orphaned active split drag

**Files:**

- Modify crates/app/src/ui/workspace.rs at split transaction helpers, update_hidden_with_native_input, and show_with_input.
- Test beside existing split transaction tests.

- [ ] **Step 1: Write the failing cancellation regression**

    #[test]
    fn hidden_input_cancels_active_split_drag_and_unblocks_resize() {
        let mut ui = WorkspaceUi::new();
        let ctx = egui::Context::default();
        ui.begin_split_drag(tab_id("t"), Vec::new(), 0.72);
        ui.cancel_active_split_drag();
        assert!(ui.split_drag.is_none());
        ui.stage_terminal_resize_for_pass(
            45,
            false,
            SessionId(41),
            100,
            24,
        );
        ui.flush_render_side_effects_for_pass(&ctx, 45, false);
        assert!(matches!(
            drain_protocol(&mut ui).as_slice(),
            [RuntimeCommand::Resize {
                session,
                cols: 100,
                rows: 24
            }] if *session == SessionId(41)
        ));
    }

- [ ] **Step 2: Run it and verify RED**

    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo hidden_input_cancels_active_split_drag_and_unblocks_resize --locked -- --test-threads=1

Expected: exit 101 because cancel_active_split_drag is missing.

- [ ] **Step 3: Implement cancellation**

    fn cancel_active_split_drag(&mut self) {
        if self.split_drag.as_ref().is_some_and(|transaction| {
            transaction.phase == SplitDragPhase::Active
        }) {
            self.split_drag = None;
            self.staged_split_commit_pass = None;
            self.staged_terminal_resizes.clear();
            self.pending_resize_target.clear();
            self.split_final_resize_sessions.clear();
        }
    }

- [ ] **Step 4: Reconcile active drag ownership**

Add a helper taking ctx, input_enabled, and Option active tab. For an Active transaction: commit when input is enabled, the tab matches, and ctx.drag_stopped_id equals the shared split ID; cancel when input ownership or tab is lost; also cancel when neither ctx.is_being_dragged nor pointer.primary_down remains. Committed transactions are untouched.

Call it after prepare_frame and active-tab resolution but before render_node. update_hidden_with_native_input calls cancel_active_split_drag after preparing the hidden frame.

- [ ] **Step 5: Add absent-widget behavior and run GREEN**

With the existing kittest split fixture: start a real drag, render input_enabled false, release, render active again, then assert split_drag is None, ResizeSplit was not sent, and a later terminal Resize stages. Name it split_drag_release_while_widget_absent_cancels_preview_and_unblocks_resize.

    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo hidden_input_cancels_active_split_drag_and_unblocks_resize --locked -- --test-threads=1
    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo split_drag_release_while_widget_absent_cancels_preview_and_unblocks_resize --locked -- --test-threads=1
    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo ui::workspace::tests --locked -- --test-threads=1

Expected: focused tests and the Workspace group exit 0.

### Task 5: File-tree display-only NFC normalization

**Files:**

- Modify crates/app/src/ui/file_tree.rs imports, FlatRow, render consumers, flatten, and FlatRow tests.

- [ ] **Step 1: Write the failing raw-identity/display test**

    #[test]
    fn 평탄화는_nfd_경로를_보존하고_표시명만_nfc로_합성한다() {
        let raw = concat!(
            "\u{1112}\u{116A}\u{1106}\u{1167}\u{11AB} ",
            "\u{1103}\u{1175}\u{110C}\u{1161}\u{110B}",
            "\u{1175}\u{11AB}"
        );
        let nodes = vec![file(raw)];
        let mut out = Vec::new();
        flatten(&nodes, Path::new("/r"), 0, false, &mut out);
        assert_eq!(nodes[0].name, raw);
        assert_eq!(out[0].display_name, "화면 디자인");
        assert_eq!(out[0].path, Path::new("/r").join(raw));
        assert_ne!(out[0].path, Path::new("/r/화면 디자인"));
    }

- [ ] **Step 2: Run it and verify RED**

    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo 평탄화는_nfd_경로를_보존하고_표시명만_nfc로_합성한다 --locked -- --test-threads=1

Expected: exit 101 because FlatRow has no display_name field.

- [ ] **Step 3: Implement cached display normalization**

Import unicode_normalization::UnicodeNormalization. Rename FlatRow.name to display_name. In flatten, construct raw path first, then assign display_name with node.name.nfc().collect(). Replace only render/color consumers and FlatRow test assertions. Do not change TreeNode.name, sorting, hidden filtering, node lookup, or rename buffers.

- [ ] **Step 4: Run focused and file-tree GREEN tests**

    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo 평탄화는_nfd_경로를_보존하고_표시명만_nfc로_합성한다 --locked -- --test-threads=1
    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo ui::file_tree::tests --locked -- --test-threads=1

Expected: focused test passes and the file-tree group exits 0.

### Task 6: Root integration, review, corrections, and delivery

**Files:**

- Modify only code implicated by actual review findings.
- Modify docs/CODEX_HANDOFF.md.
- Create the Workstep journal after the code commit.

- [ ] **Step 1: Inspect and format the integrated diff**

    git status --short --branch
    git diff -- crates/app/src/app.rs crates/app/src/ui/workspace.rs crates/app/src/ui/file_tree.rs
    git diff --check
    cargo fmt --all -- --check

Expected: only three owned code files plus root-owned docs are changed; all checks exit 0.

- [ ] **Step 2: Run focused regression groups serially**

    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo document --locked -- --test-threads=1
    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo ui::workspace::tests --locked -- --test-threads=1
    CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo ui::file_tree::tests --locked -- --test-threads=1

Expected: all selected tests pass; only previously documented measurement tests may remain ignored.

- [ ] **Step 3: Run static project gates**

    CARGO_BUILD_JOBS=2 cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
    cargo run -p xtask -- i18n-check
    cargo run -p xtask -- check-boundary
    cargo fmt --all -- --check
    git diff --check

Expected: all commands exit 0 with no Clippy warning.

- [ ] **Step 4: Review the actual code diff**

    codex review --uncommitted

Expected: preserve severity and location. Fix every Critical/High and each in-scope Medium, then rerun affected focused tests and Steps 2-3.

- [ ] **Step 5: Update handoff and commit**

Record objective, modified files, actual RED/GREEN commands and counts, failed approaches, review corrections, remaining manual checks, and exact next commands in CODEX_HANDOFF.md.

    git add crates/app/src/app.rs crates/app/src/ui/workspace.rs crates/app/src/ui/file_tree.rs docs/CODEX_HANDOFF.md
    git commit -m "fix(app): 분할 수명주기와 한글 파일명 표시 교정"

- [ ] **Step 6: Write the Workstep journal**

Create 2026-08-24 분할 수명주기와 한글 파일명 표시 교정.md under the configured 프로젝트 일지/deppy-sijo directory. Include branch, final SHA, behavior, exact review result, accepted/rejected findings, test evidence, and remaining manual visual checks.

- [ ] **Step 7: Report without claiming unrun visual checks**

Report the commit, test results, review result, and journal path. Label live macOS screenshot verification as pending unless actually performed after rebuilding and relaunching the signed bundle.
