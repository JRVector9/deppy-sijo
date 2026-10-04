# Document popups and terminal tab strip Implementation Plan

> **For agentic workers:** Execute inline in the existing performance worktree. The user has explicitly approved implementing HTML cases 26–29 and the tab strip interaction. No delegation, commit, push, or app restart.

**Goal:** Apply the approved shared popup design to cases 26–29, review prior popup changes, and make the empty tab strip open the session launcher with a visible boundary after X.

**Architecture:** Presentation lives in `ui/document_dialogs.rs`, reusing `ui/popup` shell/body/footer/notices. App retains document queues and asynchronous settings actions. Folder-move prompts capture their workspace, old path, new path and inode anchor; a completed older request must not clear a newer prompt. Tab strip behavior reuses `WorkspaceUi::new_session_requested`, without spawning a shell.

**Tech Stack:** Rust, egui, egui_kittest, existing i18n catalogs and settings worker.

## Approved behavior

- 26: 480pt; filename, warning, queued count and save-limit notice; Discard/Danger, Save/Secondary (domain eligibility), Cancel/Ghost. X/backdrop/Esc cancel. No implicit destructive Enter.
- 27: 400pt; filename and queued count; Reload/Danger or Cancel/Ghost. Dismiss cancels.
- 28: 420pt; Info notice and Close/Primary. Dismiss acknowledges only this notice.
- 29: 480pt; old/new paths, Info notice, Update/Primary and Ignore/Ghost. Disable Update during another settings operation; keep the prompt to retry. Capture target identity and clear only the matching prompt after admission. Retain worker CAS.
- Active terminal tab click keeps selecting that tab. Empty space after all visible tabs and before toolbar opens the existing launcher. Close, auxiliary tabs and toolbar keep independent hit boxes. Inactive input surfaces only request focus. Separator uses existing pixel-snapped `paint_tab_divider` even with zero auxiliary tabs.

## Tasks

- [x] Review affected source, old diff, settings admission and completion ownership. Record confirmed findings.
- [x] Add and execute failing actual-header harness: click the blank strip → `take_new_session_requested()` true, no RuntimeCommand; inspect separator shape.
- [x] Create presentation-only `document_dialogs.rs`: `DirtyChoice::{Save,Discard,Cancel}`, `dirty(ctx,id,name,queued,can_save,too_large,catalog)`, `conflict(...) -> Option<bool>`, `cap(...) -> bool`, `moved(ctx,id,old,new,can_update,catalog) -> Option<bool>`.
- [x] Replace four Window blocks with these functions. Map DirtyChoice to existing App choices and preserve front-of-queue resolution. Capture folder prompt identity; clear after accepted queue only and never unconditionally on an unrelated outcome.
- [x] Verify buttons/disabled Save, each queue action, X/Esc/backdrop, pending path update, long paths and narrow screens with offscreen harness. Render cases 26–29 for visual inspection.
- [x] Add blank-strip interaction and unconditional session divider to both active and session-less headers, respecting input ownership and existing auxiliary tab actions.
- [x] Update design/HTML/review/handoff. Run `cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo -- --test-threads=1`, affected connector/i18n tests, strict Clippy, fmt, diff and HTML syntax checks.
- [x] Increase 0.4.9 → 0.4.10 for this behavior correction; build app/proxy, stage a separate signed bundle, verify both plist fields and compiled version marker. Never execute the app binary.

## Self review

All four numbered cases, divider, blank click, review, reusable code, docs, tests and rebuild have a task. Existing queue APIs remain in App; no filesystem or runtime operations in presentation. Existing user authorization covers inline execution and build; it does not cover launch or commit.
