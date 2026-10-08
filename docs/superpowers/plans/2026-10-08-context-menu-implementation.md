# Compact context menus Implementation Plan

> **For agentic workers:** Execute inline in the user-authorized task. The user approved the existing HTML proposal with menu headers removed and slightly smaller spacing/width. No subagents or app restart are needed.

**Goal:** Implement all nine proposed context-menu locations, preserving their actions and state guards, with no redundant root title.

**Architecture:** A leaf `ui/context_menu.rs` owns scoped popup/menu styling, icon painting and danger rows. Callers retain target identity and side effects. Existing callbacks move into selection/split/folder/new-session/path submenus. No runtime, PTY or persistence changes.

**Tech Stack:** Rust, egui0.36.1, egui_kittest, five i18n catalogs, standalone HTML.

### Task1: Regression and shared presentation

- [x] Add real egui tests to existing Workspace/FileTree tests: folder request retains session7; nested split issues SplitPane for pane `pa`; a short session menu stays within205pt and on one line without leaking style.
- [x] Run RED through `python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test --offline --locked -q -p deppy-sijo compact_context_menu -- --nocapture`; verify missing grouping/current220pt width causes failure.
- [x] Add `crates/app/src/ui/context_menu.rs` and register it in `ui/mod.rs`. Use `Popup::context_menu(response).style(compact_style).show(...)`,192pt minimum width,4pt frame margin,1pt item Y spacing and Extend wrapping. Draw14pt vector icons via a reserved egui Atom without altering accessible labels. Submenus use the same scoped style; dangerous rows use existing design tokens.

### Task2: Migrate callers with unchanged target/action contracts

- [x] Workspace: common popup for local/remote; copy/paste direct; selected-text actions under selection; split directions under split; session folder under folder; environment settings before final close.
- [x] FileTree: active session name/diff/resume direct, creation and folder submenus, worktree-delete/close last. Keep inactive open-beside only. Workspace remains3actions with final separated close. File/directory retain exact MenuAction path payloads; path operations grouped, trash last. Header keeps only existing new-folder.
- [x] Fleet: schedule and optional separated cancel, keep PTY/target-key guards. Notes: preserve cut/copy/paste/delete selection guards and separate select-all. App env path: choose/rescan/separated clear, keep path presence guards.
- [x] Add seven translated group/direction keys to all five catalogs; use consistent Korean rename/trash/workspace-end labels where already localized.

### Task3: Approved artifact and validation

- [x] HTML: remove root menu-context title block; root width284→256px, row minimum32→28px, padding7×10→5×8px, outer padding5→4px. Keep accessible menu label and source evidence outside popup.
- [x] Run focused menu regressions, relevant FileTree/Workspace/Fleet/Notes suites, full App+i18n, strict Clippy, fmt, boundary and diff checks using the existing cargo gate; adapt old tests that directly click now-grouped items to open the real submenu first.
- [x] Inspect the updated HTML in native Chrome with CUA, review final Rust diff and update handoff. No local changed app is delivered, so no release version increase or app launch is part of this task.
