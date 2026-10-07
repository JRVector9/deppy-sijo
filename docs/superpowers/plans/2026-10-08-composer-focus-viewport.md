# Composer focus and retained terminal viewport Implementation Plan

> **For agentic workers:** Execute inline in the current authorized task. Use the existing systematic-debugging and test-driven-development workflow; no subagents or app restart are authorized.

**Goal:** Restore compact unfocused input, expand on focus, and prevent expansion from deleting terminal output; investigate live nomorevibe admission using observed evidence.

**Architecture:** Composer reports its one-row compact card height; the host measures actual panel space consumed above that baseline. Workspace walks both visible and compact-height split rectangles with existing split geometry, using the latter only for PTY sizing. The renderer pans a retained grid inside the actual clipped pane, keeping cursor coordinates and hit testing aligned. Actual window/font/split geometry changes retain normal resize handling.

**Tech Stack:** Rust, egui0.36, AlacrittyBackend, in-process PTY runtime, egui_kittest.

### Task1: Verify and restore focus growth

Files: `crates/app/src/ui/composer.rs` (UI and real egui/backend regression).

- [x] Add a real UI test that measures compact vs focused height. Use the existing composer_harness and `ComposerUi::text_id(TEST_WS)`; the concrete assertion is `assert!(focused_height > compact_height)` and blur must return to the compact height.
- [x] Run RED: `python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test --offline --locked -q -p deppy-sijo composer_focus_expands -- --nocapture`.
- [x] Replace fixed3 text rows with `let rows = if self.expanded { buffer.lines().count().clamp(3, 8) } else { 1 };`, restore conditional toolbar, report `compact_height = row_height + card.total_margin().sum().y`. Set ScrollArea.min_scrolled_height(0.0) to allow the actual one-row viewport. Host credit is `available_before - available_after - dock_margin - prompt_prefix_height - compact_height`; preserve its sign to account for the panel allocation pass. Using current card height as credit failed and was replaced.
- [x] Run the focused regression with backend resize computed from available height plus Composer expansion. Assert both different visible heights and unchanged backend dimensions/output after focus,40line paste,blur and clear.

### Task2: Keep logical PTY rows during expansion

Files: `crates/app/src/app.rs`, `crates/app/src/ui/workspace.rs`, `crates/terminal/src/renderer_egui.rs`.

- [x] Host supplies zero credit when Composer is hidden and the measured expansion otherwise, each frame, to active and warm WorkspaceUi instances.
- [x] Workspace adds a `composer_height_expansion: f32` default0. Root logical rectangle is `Rect::from_min_max(rect.min, rect.max + vec2(0.0, self.composer_height_expansion))`. Pass visible and logical rectangles through render_node; call existing `terminal_split_ratio`/`terminal_split_rects` independently for each, preserving their minimum constraints. Leaf sizing uses the content-height difference after applying the same header/archived-notice layout to both rectangles; preserve signed differences. Attached leaves use the full host expansion.
- [x] Size backend rows from `grid_rows_for_available(avail.y + extra_height, cell.y)`. Render with a viewport variant of draw_with_preedit accepting the transient clipping flag. Compute overflow `max(snapshot.rows as f32 * cell.y * scale - content_rect.height(), 0.0)`; limit pan by the visible cursor's row, use zero pan in scrollback; subtract pan from returned/render origin. Existing renderer API delegates with clipping disabled.
- [x] Test nested vertical/horizontal split dimensions for both geometries and actual renderer origin/hit-test/clip correctness. Run RED before implementation where a regression can compile against existing API, then GREEN.

### Task3: Admission evidence and final validation

Files: `crates/session/src/status.rs`, `crates/runtime/src/in_process.rs` (regression); `docs/CODEX_HANDOFF.md`.

- [x] Compare short and multiline Composer sends in live nomorevibe with focused input and native editor contents. Record exact observed accepted/denied state. Live short and461byte/2line sends succeeded; native Ctrl+T followed by Composer send produced a fresh accepted_input_draft denial. Add0x14 to non-text controls, preserve prior erase evidence across this display key, and retain genuine draft/paste/history protection.
- [x] Protect native drafts/questions/foreground identity. Add a failing test for any established parser/admission defect before correction; do not bypass genuine draft protection to make sends pass.
- [x] Run Composer, Workspace, runtime admission and terminal renderer suites followed by App/terminal/session/runtime tests, strict Clippy, fmt and diff checks through cargo_gate.py. Record actual commands and results. Full gate exited0: Session76, Runtime355, Terminal120, App2784 and integrations39 PASS; existing ignored cases remained ignored. Strict Clippy, fmt and boundary checks PASS.
- [x] Review the final diff and update handoff with completed work, remaining input uncertainty, failed approaches, changed files and next commands. No release/version bump or app restart is part of this source-only task. Fixed-source live app verification awaits a separately authorized release/restart.
