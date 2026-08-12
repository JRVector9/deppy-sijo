# Archived Agent Notice Bar Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render restored-agent interruption guidance in an opaque, dedicated bottom bar that cannot overlap terminal output while preserving every existing resume action.

**Architecture:** Extend the existing pure terminal-pane layout projection with an optional archived-notice rectangle. The same projection shrinks the terminal content used for PTY row calculation and supplies the exact rectangle for painting and interaction, preventing layout/render drift. Keep all resume-state mapping and dispatch inside `WorkspaceUi::render_pane`.

**Tech Stack:** Rust, egui 0.35, egui_kittest, Cargo.

---

### Task 1: Reserve and render the opaque restored-agent notice bar

**Files:**
- Modify: `crates/app/src/ui/workspace.rs:400-650`
- Modify: `crates/app/src/ui/workspace.rs:3833-4625`
- Test: `crates/app/src/ui/workspace.rs:6230-6490`
- Test: `crates/app/src/ui/workspace.rs:10576-10686`

- [x] **Step 1: Write the failing layout/style regression test**

Add a focused test that calls the wished-for `terminal_pane_layout_for_state(rect, true, true)` and `archived_agent_notice_style()` APIs. Assert that a normal `589×358pt` pane produces a `36pt` full-width notice rectangle at the bottom, that terminal content ends above it with the existing vertical padding, and that the approved fill, separator, text, button fill, button stroke, button height, and corner radius match the design spec. Add a compact-height case and assert all rectangles remain positive and non-overlapping.

```rust
#[test]
fn 복원_agent_안내바는_터미널과_겹치지_않는_불투명_차콜영역이다() {
    let rect = egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(589.0, 358.0),
    );
    let layout = terminal_pane_layout_for_state(rect, true, true);
    let notice = layout.archived_notice.expect("복원 안내 바가 있어야 한다");
    let style = archived_agent_notice_style();

    assert_eq!(notice.height(), ARCHIVED_AGENT_NOTICE_HEIGHT);
    assert_eq!(notice.left(), layout.surface.left());
    assert_eq!(notice.right(), layout.surface.right());
    assert_eq!(notice.bottom(), layout.surface.bottom());
    assert_eq!(layout.content.bottom(), notice.top() - TERMINAL_STREAM_VERTICAL_PADDING);
    assert!(!layout.content.intersects(notice));
    assert_eq!(style.fill, egui::Color32::from_rgb(0x1a, 0x1d, 0x23));
    assert_eq!(style.separator.color, egui::Color32::from_rgb(0x35, 0x3b, 0x45));
    assert_eq!(style.text, egui::Color32::from_rgb(0xb0, 0xb5, 0xbf));
    assert_eq!(style.button_fill, egui::Color32::from_rgb(0x29, 0x2e, 0x37));
    assert_eq!(style.button_stroke.color, egui::Color32::from_rgb(0x48, 0x50, 0x5d));
    assert_eq!(style.button_height, 26.0);
    assert_eq!(style.button_corner_radius, 4);

    let compact = terminal_pane_layout_for_state(
        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(80.0, 42.0)),
        true,
        true,
    );
    let compact_notice = compact.archived_notice.expect("낮은 pane도 안내 바를 유지한다");
    assert!(compact.surface.is_positive());
    assert!(compact.content.is_positive());
    assert!(compact_notice.is_positive());
    assert!(!compact.content.intersects(compact_notice));
}
```

- [x] **Step 2: Run the focused test and verify RED**

Run:

```bash
cargo test -p deppy-sijo --locked 복원_agent_안내바는_터미널과_겹치지_않는_불투명_차콜영역이다 -- --nocapture
```

Expected: compilation fails because `terminal_pane_layout_for_state`, `archived_agent_notice_style`, `ARCHIVED_AGENT_NOTICE_HEIGHT`, and `TerminalPaneLayout::archived_notice` do not exist.

- [x] **Step 3: Implement the minimal shared layout and visual contract**

In `workspace.rs`, add fixed constants for the approved bar geometry and a small `ArchivedAgentNoticeStyle` projection. Extend `TerminalPaneLayout` with `archived_notice: Option<egui::Rect>` and implement:

```rust
fn terminal_pane_layout_for_state(
    rect: egui::Rect,
    embedded_header: bool,
    show_archived_notice: bool,
) -> TerminalPaneLayout
```

Compute the header as today. Use the remaining surface height to clamp the notice height so the terminal area and notice are always positive. Compute terminal content padding against the terminal-only rectangle, not the entire surface. Keep `terminal_pane_layout_with_embedded_header` as the no-notice wrapper for existing callers and tests.

At the start of `WorkspaceUi::render_pane`, derive `show_archived_notice` only when the pane's session view has `restored_readonly == true` and a present `exit_code`. Call `terminal_pane_layout_for_state` with this flag before terminal sizing so `ui.available_size()` and `queue_terminal_resize` exclude the bar.

Replace the old 22pt transparent overlay with a child UI in `layout.archived_notice`. Paint the full rectangle with `#1A1D23` and a 1pt `#353B45` top separator. Use 14/12pt horizontal insets and right-to-left centered layout so the 26pt button is allocated first and preserved; add the one-line 14pt `#B0B5BF` message afterward with `egui::Label::truncate()`. Configure the button with the approved fill, stroke, 4pt corner radius, and minimum height. Preserve the current `mode.is_local() && action_enabled` gate and `respawn_archived_request` dispatch exactly.

- [x] **Step 4: Run focused layout and resume-action tests and verify GREEN**

Run:

```bash
cargo test -p deppy-sijo --locked 복원_agent_안내바는_터미널과_겹치지_않는_불투명_차콜영역이다 -- --nocapture
cargo test -p deppy-sijo --locked restored_archived_resume -- --nocapture
```

Expected: the layout/style test passes 1/1 and the existing restored-resume kittests pass 2/2, including exact/recent dispatch, unsupported new execution, and unavailable/checking suppression.

- [x] **Step 5: Run regression gates**

Run:

```bash
cargo test -p deppy-sijo --locked ui::workspace::tests:: -- --nocapture
cargo check -p deppy-sijo --all-targets --locked
cargo fmt --all -- --check
git diff --check
```

Expected: all commands exit 0. Do not package or launch the app.

- [x] **Step 6: Record results and hand off without mixing prior work**

Update `docs/CODEX_HANDOFF.md` with the exact RED/GREEN outputs, modified files, design decisions, and remaining explicit rebuild step. Keep the implementation uncommitted if an atomic commit would include the pre-existing sidebar current-work modifications already present in `workspace.rs`; do not stage or rewrite those unrelated changes.
