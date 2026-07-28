# DesignALL Workspace Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the approved card-free, line-separated three-column Deppy workspace on `DesignALL`, preserving current project/session/file/terminal/input/status behavior and presenting a real signed debug app after each D1–D4 checkpoint.

**Architecture:** Keep `App`, `SidebarSnapshot`, `FileTreeUi`, `WorkspaceUi`, and the existing runtime actions as state owners. Add one presentation-only token module, split the combined sidebar into a fixed navigation rail plus the existing resizable project/file panel, and migrate structural fills to pixel-snapped separators without touching PTY or storage code. Each checkpoint is implemented test-first, committed independently, validated with the closest kittests, then built and launched for user review before the next checkpoint.

**Tech Stack:** Rust 2024, egui/eframe 0.35, egui_kittest, existing Deppy i18n/font/theme helpers, Cargo, macOS `codesign`, `scripts/dev-run.sh`.

---

## Execution Rules

- Work only in `/tmp/deppy-sf06-integration` on local branch `DesignALL`.
- Starting commits: `ab7e5a7` (stability integration), `75267ec` (project-row simplification), `e0b3f5c` (approved design spec).
- Do not merge, rebase, push, or modify `main` during D1–D4.
- Do not change runtime commands, PTY rendering, terminal input mapping, persistence, status polling, or resource accounting.
- Do not add render-path file, Git, process, or network I/O.
- Do not start the next checkpoint until the user accepts the current signed debug app.
- If a visual change appears to require a behavioral fix outside this plan's file map, stop and record a separate defect.
- Use `apply_patch` for all edits and record only tests actually run.

## File Map

**Create**

- `crates/app/src/ui/designall.rs` — colors, geometry, interaction-state styles, structural separators, pure tests.

**Modify**

- `crates/app/src/ui/mod.rs` — export `designall` without changing unrelated shared UI styling.
- `crates/app/src/ui/file_tree.rs` — navigation rail, project/file panel, flat rows, sidebar tool tabs.
- `crates/app/src/app.rs` — shell frame, existing Git/Terminal/MCP shortcut dispatch, composer dock boundary.
- `crates/app/src/ui/workspace.rs` — terminal chrome only; no renderer/input changes.
- `crates/app/src/ui/composer.rs` — flat input frame only; no input semantics changes.
- `crates/app/src/ui/agent_terminal.rs` — status separators and content-contract test.
- `crates/i18n/locales/*/messages.txt` — localized Files/Search/Git/Terminal/MCP tab labels.
- `docs/CODEX_HANDOFF.md` — actual checkpoint evidence.

**Do not modify**

- `crates/terminal/**`, `crates/runtime/**`, `crates/storage/**`, `crates/persist/**`
- `crates/app/src/status_feed.rs`, `crates/app/src/claude_usage.rs`
- Cargo manifests and `Cargo.lock`

---

## Task 1: Add DesignALL Presentation Tokens

**Files:**
- Create: `crates/app/src/ui/designall.rs`
- Modify: `crates/app/src/ui/mod.rs:1-45`
- Test: `crates/app/src/ui/designall.rs`

- [ ] **Step 1: Write failing token and structural-state tests**

Create the new module with tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_tokens_match_the_approved_design_freeze() {
        assert_eq!(DARK.app_background, egui::Color32::from_rgb(0x0b, 0x10, 0x15));
        assert_eq!(DARK.input_background, egui::Color32::from_rgb(0x11, 0x18, 0x20));
        assert_eq!(DARK.separator, egui::Color32::from_rgb(0x26, 0x30, 0x3a));
        assert_eq!(DARK.accent, egui::Color32::from_rgb(0x39, 0xb8, 0xe8));
        assert_eq!(STRUCTURAL_CORNER_RADIUS, 0);
        assert_eq!(NAV_RAIL_WIDTH, 88.0);
    }

    #[test]
    fn inactive_structure_has_no_fill_but_interactions_may_have_one() {
        assert_eq!(row_fill(DARK, false, false), None);
        assert_eq!(row_fill(DARK, true, false), Some(DARK.selected_background));
        assert_eq!(row_fill(DARK, false, true), Some(DARK.hover_background));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p deppy-sijo ui::designall::tests --locked -- --test-threads=1
```

Expected: FAIL because `ui::designall` is not exported.

- [ ] **Step 3: Implement the minimal token module**

Export it from `ui/mod.rs` with `pub mod designall;`, then implement:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tokens {
    pub app_background: egui::Color32,
    pub input_background: egui::Color32,
    pub separator: egui::Color32,
    pub text: egui::Color32,
    pub muted_text: egui::Color32,
    pub accent: egui::Color32,
    pub success: egui::Color32,
    pub warning: egui::Color32,
    pub error: egui::Color32,
    pub selected_background: egui::Color32,
    pub hover_background: egui::Color32,
}

pub const STRUCTURAL_CORNER_RADIUS: u8 = 0;
pub const INTERACTION_CORNER_RADIUS: u8 = 4;
pub const NAV_RAIL_WIDTH: f32 = 88.0;
pub const SEPARATOR_WIDTH: f32 = 1.0;

pub const DARK: Tokens = Tokens {
    app_background: egui::Color32::from_rgb(0x0b, 0x10, 0x15),
    input_background: egui::Color32::from_rgb(0x11, 0x18, 0x20),
    separator: egui::Color32::from_rgb(0x26, 0x30, 0x3a),
    text: egui::Color32::from_rgb(0xd8, 0xde, 0xe6),
    muted_text: egui::Color32::from_rgb(0x7f, 0x89, 0x96),
    accent: egui::Color32::from_rgb(0x39, 0xb8, 0xe8),
    success: egui::Color32::from_rgb(0x4a, 0xcb, 0x82),
    warning: egui::Color32::from_rgb(0xe0, 0xa4, 0x3a),
    error: egui::Color32::from_rgb(0xef, 0x66, 0x71),
    selected_background: egui::Color32::from_rgb(0x10, 0x21, 0x2a),
    hover_background: egui::Color32::from_rgb(0x10, 0x18, 0x20),
};

pub const LIGHT: Tokens = Tokens {
    app_background: egui::Color32::from_rgb(0xfb, 0xfc, 0xfd),
    input_background: egui::Color32::from_rgb(0xf1, 0xf3, 0xf5),
    separator: egui::Color32::from_rgb(0xd5, 0xd9, 0xdf),
    text: egui::Color32::from_rgb(0x23, 0x26, 0x2c),
    muted_text: egui::Color32::from_rgb(0x65, 0x6b, 0x74),
    accent: egui::Color32::from_rgb(0x1c, 0x93, 0xaa),
    success: egui::Color32::from_rgb(0x27, 0x91, 0x5b),
    warning: egui::Color32::from_rgb(0xb2, 0x70, 0x16),
    error: egui::Color32::from_rgb(0xc8, 0x3d, 0x49),
    selected_background: egui::Color32::from_rgb(0xe7, 0xf4, 0xf8),
    hover_background: egui::Color32::from_rgb(0xf1, 0xf5, 0xf7),
};

pub fn tokens(visuals: &egui::Visuals) -> Tokens {
    if visuals.dark_mode { DARK } else { LIGHT }
}

pub fn row_fill(tokens: Tokens, selected: bool, hovered: bool) -> Option<egui::Color32> {
    selected
        .then_some(tokens.selected_background)
        .or_else(|| hovered.then_some(tokens.hover_background))
}

pub fn separator_stroke(visuals: &egui::Visuals) -> egui::Stroke {
    egui::Stroke::new(SEPARATOR_WIDTH, tokens(visuals).separator)
}

pub fn structural_frame(visuals: &egui::Visuals) -> egui::Frame {
    egui::Frame::NONE
        .fill(tokens(visuals).app_background)
        .inner_margin(egui::Margin::ZERO)
        .corner_radius(egui::CornerRadius::same(STRUCTURAL_CORNER_RADIUS))
}
```

- [ ] **Step 4: Add a workspace-local visuals helper**

Do not modify `theme.rs`; that would restyle settings and modals outside the approved scope. Add a helper used only inside the main workspace panels:

```rust
pub fn apply_workspace_visuals(ui: &mut egui::Ui) {
    let t = tokens(ui.visuals());
    let visuals = ui.visuals_mut();
    visuals.override_text_color = Some(t.text);
    visuals.panel_fill = t.app_background;
    visuals.window_fill = t.app_background;
    visuals.extreme_bg_color = t.input_background;
    visuals.faint_bg_color = t.hover_background;
    visuals.hyperlink_color = t.accent;
    visuals.selection.bg_fill = t.accent;
    visuals.selection.stroke = egui::Stroke::new(1.0, t.accent);
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, t.separator);
}
```

Call this helper only inside the navigation rail, project/file panel, central terminal surface,
composer dock, and status bar closures introduced by this plan.

- [ ] **Step 5: Run focused tests and compilation**

```bash
cargo test -p deppy-sijo ui::designall::tests --locked -- --test-threads=1
cargo check -p deppy-sijo --all-targets --locked
```

Expected: PASS and exit 0.

- [ ] **Step 6: Commit**

```bash
git add crates/app/src/ui/designall.rs crates/app/src/ui/mod.rs
git commit -m "feat(ui): add DesignALL presentation tokens"
```

---

## Task 2: Split the Navigation Rail from the Project/File Panel

**Files:**
- Modify: `crates/app/src/ui/file_tree.rs:735-860`
- Modify: `crates/app/src/ui/file_tree.rs:1405-1548`
- Modify: `crates/app/src/ui/file_tree.rs:2968-3030`
- Test: `crates/app/src/ui/file_tree.rs:7890-8050`

- [ ] **Step 1: Replace obsolete lower-navigation tests with a failing shell test**

Delete tests tied to `navigation_section_height` and add a harness that records the remaining width after `tree.panel(...)`. Assert that it consumes at least `designall::NAV_RAIL_WIDTH + 40.0`, while the existing four-action navigation test remains unchanged.

```rust
#[test]
fn kittest_designall은_내비게이션레일과_프로젝트패널을_분리한다() {
    let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
    let snapshot = SidebarSnapshot {
        active_workspace_id: "ws-test",
        workspaces: &[],
        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
        home_notice_count: 0,
        inbox_count: 0,
        fleet_count: 0,
        agents_open: false,
    };
    let ctx = egui::Context::default();
    install_sidebar_test_fonts(&ctx);
    let mut tree = FileTreeUi::new(ctx.clone());
    let consumed = ctx.run_ui(egui::RawInput::default(), |ctx| {
        egui::CentralPanel::default()
            .show(ctx, |ui| {
                let before = ui.available_width();
                let _ = tree.panel(
                    ui,
                    &std::collections::HashMap::new(),
                    &snapshot,
                    &catalog,
                );
                before - ui.available_width()
            })
            .inner
    });
    assert!(consumed.inner >= crate::ui::designall::NAV_RAIL_WIDTH + 40.0);
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p deppy-sijo kittest_designall은_내비게이션레일과_프로젝트패널을_분리한다 --locked -- --test-threads=1
```

Expected: FAIL because navigation is still inside one panel.

- [ ] **Step 3: Remove lower-navigation height state**

Remove `navigation_section_height`, its initializer, `navigation_split_handle`, `sidebar_vertical_section_heights`, and `SIDEBAR_NAV_MIN_HEIGHT`, `SIDEBAR_NAV_DEFAULT_HEIGHT`, `SIDEBAR_NAV_SPLIT_HANDLE_HEIGHT`, `SIDEBAR_BODY_MIN_HEIGHT`. Keep `workspace_section_height`.

- [ ] **Step 4: Render two left panels with one existing action stream**

Refactor `panel`:

```rust
pub fn panel(
    &mut self,
    ui: &mut egui::Ui,
    sessions_by_workspace: &HashMap<String, Vec<SessionEntry>>,
    sidebar: &SidebarSnapshot<'_>,
    catalog: &i18n::Catalog,
) -> Option<SidebarAction> {
    let navigation_action = self.navigation_rail_panel(ui, sidebar, catalog);
    let project_action = self.project_file_panel(ui, sessions_by_workspace, sidebar, catalog);
    project_action.or(navigation_action)
}
```

Implement the fixed rail:

```rust
fn navigation_rail_panel(
    &mut self,
    ui: &mut egui::Ui,
    sidebar: &SidebarSnapshot<'_>,
    catalog: &i18n::Catalog,
) -> Option<SidebarAction> {
    egui::Panel::left("designall_navigation_rail")
        .resizable(false)
        .exact_size(crate::ui::designall::NAV_RAIL_WIDTH)
        .show_separator_line(false)
        .frame(crate::ui::designall::structural_frame(&ui.ctx().global_style().visuals))
        .show(ui, |ui| {
            crate::ui::designall::apply_workspace_visuals(ui);
            crate::fonts::apply_sidebar_text_styles(ui);
            egui::ScrollArea::vertical()
                .id_salt("designall_navigation_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| self.navigation(ui, sidebar, catalog))
                .inner
        })
        .inner
}
```

`project_file_panel` is the current resizable panel without the navigation allocation. Preserve collapsed width, horizontal resize, `contents`, and all shortcut-consumption behavior.

- [ ] **Step 5: Flatten navigation selection geometry**

Replace the rounded `pill` in `nav_row` with a full flat row, interaction-only fill, and a 2px selected rail. Keep icon painter, labels, badge, accessibility, and click rect.

```rust
let row = rect.shrink2(egui::vec2(4.0, 0.0));
let tokens = crate::ui::designall::tokens(ui.visuals());
if let Some(fill) = crate::ui::designall::row_fill(tokens, selected, response.hovered()) {
    ui.painter().rect_filled(row, 0.0, fill);
}
if selected {
    let rail = egui::Rect::from_min_max(row.left_top(), egui::pos2(row.left() + 2.0, row.bottom()));
    ui.painter().rect_filled(rail, 0.0, tokens.accent);
}
```

- [ ] **Step 6: Run file-tree tests**

```bash
cargo test -p deppy-sijo ui::file_tree::tests --locked -- --test-threads=1
```

Expected: all file-tree tests PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/app/src/ui/file_tree.rs
git commit -m "feat(ui): split DesignALL navigation rail"
```

---

## Task 3: Validate and Present D1

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`

- [ ] **Step 1: Run the D1 gate**

```bash
cargo test -p deppy-sijo ui::designall::tests --locked -- --test-threads=1
cargo test -p deppy-sijo ui::file_tree::tests --locked -- --test-threads=1
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
git diff --check
```

Expected: every command exits 0.

- [ ] **Step 2: Build and launch the signed debug app**

Identify only the exact worktree process:

```bash
pgrep -fl '^/tmp/deppy-sf06-integration/target/debug/deppy-sijo$|^\./target/debug/deppy-sijo$' || true
```

If found, send `SIGTERM`, wait up to 10 seconds, and confirm exit. Then run:

```bash
DEPPY_SIGN_IDENTITY='Developer ID Application: VectorNine INC (ZDTU5LS35K)' scripts/dev-run.sh
```

Expected: build succeeds, identifier is `app.vector9.deppy-sijo`, and the visible app opens without a new keychain prompt.

- [ ] **Step 3: Stop for user review**

Ask the user to inspect the continuous background, dedicated navigation rail, resizable project/file panel, 1px boundaries, unchanged nav actions, and unchanged status information. Do not start D2 before approval.

- [ ] **Step 4: Record and commit actual evidence**

Update `docs/CODEX_HANDOFF.md` with commands, counts, PID, signature, failures, feedback, and exact next action.

```bash
git add docs/CODEX_HANDOFF.md
git commit -m "docs(design): record DesignALL D1 checkpoint"
```

---

## Task 4: Remove Project and Session Structural Cards

**Files:**
- Modify: `crates/app/src/ui/file_tree.rs:1660-2105`
- Modify: `crates/app/src/ui/file_tree.rs:3440-3710`
- Modify: `crates/app/src/ui/file_tree.rs:4060-4235`
- Test: `crates/app/src/ui/file_tree.rs:5720-5960`

- [ ] **Step 1: Write the failing flat-row style test**

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WorkspaceRowStyle {
    fill: Option<egui::Color32>,
    accent: Option<egui::Color32>,
}

#[test]
fn designall_워크스페이스행은_상태가_없으면_구조배경이_없다() {
    let tokens = crate::ui::designall::DARK;
    assert_eq!(workspace_row_style(tokens, false, false), WorkspaceRowStyle {
        fill: None,
        accent: None,
    });
    assert_eq!(workspace_row_style(tokens, true, false), WorkspaceRowStyle {
        fill: Some(tokens.selected_background),
        accent: Some(tokens.accent),
    });
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p deppy-sijo designall_워크스페이스행은_상태가_없으면_구조배경이_없다 --locked -- --test-threads=1
```

Expected: FAIL because `workspace_row_style` does not exist.

- [ ] **Step 3: Implement interaction-only row style**

```rust
fn workspace_row_style(
    tokens: crate::ui::designall::Tokens,
    selected: bool,
    hovered: bool,
) -> WorkspaceRowStyle {
    WorkspaceRowStyle {
        fill: crate::ui::designall::row_fill(tokens, selected, hovered),
        accent: selected.then_some(tokens.accent),
    }
}
```

Use it in `workspace_row`. Inactive/non-hovered rows paint no fill. Selected rows may paint one flat fill and a maximum 2px left rail. Preserve avatar, name, count, disclosure, click rect, widget info, and actions.

- [ ] **Step 4: Delete structural card painters**

Delete all of these symbols and all three before/active/after call sites:

```text
WORKSPACE_LIST_BACKGROUND_TOP
WORKSPACE_LIST_BACKGROUND_BOTTOM
WORKSPACE_GROUP_FILL
WORKSPACE_GROUP_TOP_FILL
WORKSPACE_GROUP_BORDER
WORKSPACE_GROUP_SHADOW
WORKSPACE_SESSION_INSET_FILL
WORKSPACE_SESSION_INSET_BORDER
vertical_gradient_rect
workspace_list_background_gradient
workspace_group_gradient
paint_workspace_group_wrap
paint_workspace_session_inset
workspace_session_inset_rect
session_inset_fill_rect
```

Remove `Shape::Noop` reserves. Keep scopes, session rendering, scroll bounds, and action dispatch. Paint at most one pixel-snapped separator after each workspace group.

- [ ] **Step 5: Flatten session fills without changing interactions**

Keep row height, `session_highlight_rect`, `session_focus_fill_rect`, semantic status rail, context menu, edit mode, focus/click actions, and accessibility. Apply only these fills: idle `None`, hover `tokens.hover_background`, focus `tokens.selected_background`; corner radius 0; separator DesignALL 1px.

- [ ] **Step 6: Run tests**

```bash
cargo test -p deppy-sijo designall_워크스페이스행은_상태가_없으면_구조배경이_없다 --locked -- --test-threads=1
cargo test -p deppy-sijo ui::file_tree::tests --locked -- --test-threads=1
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/app/src/ui/file_tree.rs
git commit -m "feat(ui): flatten DesignALL workspace rows"
```

---

## Task 5: Preserve the Project/File Split with a 50px File Minimum

**Files:**
- Modify: `crates/app/src/ui/file_tree.rs:1550-1670`
- Test: `crates/app/src/ui/file_tree.rs:5890-5965`

- [ ] **Step 1: Write the failing bounded-height test**

```rust
#[test]
fn designall_프로젝트파일분할은_파일영역_50px를_보존한다() {
    assert_eq!(project_file_section_heights(700.0, 900.0), (644.0, 50.0));
    assert_eq!(project_file_section_heights(700.0, 270.0), (270.0, 424.0));
    assert_eq!(project_file_section_heights(140.0, 0.0), (84.0, 50.0));
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p deppy-sijo designall_프로젝트파일분할은_파일영역_50px를_보존한다 --locked -- --test-threads=1
```

Expected: FAIL because the helper is absent and the current file reserve is 160px.

- [ ] **Step 3: Implement one bounded helper**

```rust
const PROJECT_SECTION_MIN_HEIGHT: f32 = 84.0;
const FILE_SECTION_MIN_HEIGHT: f32 = 50.0;
const PROJECT_FILE_SPLIT_HEIGHT: f32 = 6.0;

fn project_file_section_heights(available: f32, requested_project: f32) -> (f32, f32) {
    let usable = (available - PROJECT_FILE_SPLIT_HEIGHT).max(0.0);
    let max_project = (usable - FILE_SECTION_MIN_HEIGHT).max(PROJECT_SECTION_MIN_HEIGHT);
    let project = requested_project.clamp(PROJECT_SECTION_MIN_HEIGHT, max_project);
    let files = (usable - project).max(FILE_SECTION_MIN_HEIGHT);
    (project, files)
}
```

Allocate both sections from this helper. Keep the project `ScrollArea`, file `show_rows`, split drag, and independent scroll IDs. Use DesignALL separator/accent for the split line.

- [ ] **Step 4: Run split and overflow regressions**

```bash
cargo test -p deppy-sijo designall_프로젝트파일분할은_파일영역_50px를_보존한다 --locked -- --test-threads=1
cargo test -p deppy-sijo kittest_활성_워크스페이스가_생성순_끝이어도_파일트리가_보인다 --locked -- --test-threads=1
cargo test -p deppy-sijo kittest_사이드바_조작_전반에_widget_id_충돌이_없다 --locked -- --test-threads=1
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/app/src/ui/file_tree.rs
git commit -m "fix(ui): preserve DesignALL sidebar split bounds"
```

---

## Task 6: Validate and Present D2

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`

- [ ] **Step 1: Run the D2 gate**

```bash
cargo test -p deppy-sijo ui::file_tree::tests --locked -- --test-threads=1
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
git diff --check
```

Expected: every command exits 0.

- [ ] **Step 2: Rebuild and relaunch**

Terminate only the prior exact DesignALL debug PID and run the signed `scripts/dev-run.sh` command from Task 3.

- [ ] **Step 3: Stop for user review**

Ask the user to inspect flat project/session hierarchy, interaction-only fills, 1px separators, the 50px file minimum, independent scrolling, and unchanged project/session actions. Do not start D3 before approval.

- [ ] **Step 4: Record and commit evidence**

```bash
git add docs/CODEX_HANDOFF.md
git commit -m "docs(design): record DesignALL D2 checkpoint"
```

---

## Task 7: Add Functional Line-Based Sidebar Tool Tabs

**Files:**
- Modify: `crates/app/src/ui/file_tree.rs:110-175`
- Modify: `crates/app/src/ui/file_tree.rs:2100-2260`
- Modify: `crates/app/src/app.rs:16940-17240`
- Modify: `crates/i18n/locales/en-US/messages.txt`
- Modify: `crates/i18n/locales/ko-KR/messages.txt`
- Modify: `crates/i18n/locales/ja-JP/messages.txt`
- Modify: `crates/i18n/locales/zh-Hans/messages.txt`
- Modify: `crates/i18n/locales/zh-Hant/messages.txt`
- Test: `crates/app/src/ui/file_tree.rs`

- [ ] **Step 1: Write a failing action-mapping test**

```rust
#[test]
fn designall_사이드바도구는_기존기능으로만_연결된다() {
    assert!(sidebar_tool_action(SidebarTool::Files).is_none());
    assert!(matches!(
        sidebar_tool_action(SidebarTool::Git),
        Some(SidebarAction::ShowFocusedDiff)
    ));
    assert!(matches!(
        sidebar_tool_action(SidebarTool::Terminal),
        Some(SidebarAction::ShowTerminal)
    ));
    assert!(matches!(
        sidebar_tool_action(SidebarTool::Mcp),
        Some(SidebarAction::OpenConnectors)
    ));
}
```

Search remains local `FileTreeUi` state and must not emit a host action.

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p deppy-sijo designall_사이드바도구는_기존기능으로만_연결된다 --locked -- --test-threads=1
```

Expected: FAIL because the tool enum and actions do not exist.

- [ ] **Step 3: Add presentation enum and bounded actions**

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarTool {
    Files,
    Search,
    Git,
    Terminal,
    Mcp,
}

// Add to the existing SidebarAction enum without changing existing variants.
ShowFocusedDiff,
ShowTerminal,
OpenConnectors,

fn sidebar_tool_action(tool: SidebarTool) -> Option<SidebarAction> {
    match tool {
        SidebarTool::Files | SidebarTool::Search => None,
        SidebarTool::Git => Some(SidebarAction::ShowFocusedDiff),
        SidebarTool::Terminal => Some(SidebarAction::ShowTerminal),
        SidebarTool::Mcp => Some(SidebarAction::OpenConnectors),
    }
}
```

Do not add persistence or runtime commands.

Add these keys to every locale in the repository's existing key order:

```text
sidebar.tool.files
sidebar.tool.search
sidebar.tool.git
sidebar.tool.terminal
sidebar.tool.mcp
```

Use `Files / Search / Git / Terminal / MCP` for `en-US`, `파일 / 검색 / Git / 터미널 / MCP`
for `ko-KR`, `ファイル / 検索 / Git / ターミナル / MCP` for `ja-JP`,
`文件 / 搜索 / Git / 终端 / MCP` for `zh-Hans`, and
`檔案 / 搜尋 / Git / 終端 / MCP` for `zh-Hant`.

- [ ] **Step 4: Render the flat tool row**

Replace the standalone 38px path/tool header with `Files | Search | Git | Terminal | MCP`.

- no enclosing frame or pill;
- active Files/Search uses text color plus a 2px bottom accent;
- inactive tabs use muted text;
- one DesignALL separator ends the row;
- Files closes `file_search_open`;
- Search opens and focuses the existing bounded search;
- Git/Terminal/MCP emit the new actions;
- new-folder, new-file, refresh, and hidden-file line icons remain right-aligned when width permits;
- keep the existing narrow-width priority.

- [ ] **Step 5: Dispatch through existing App paths**

Extract the current `ShowDiff { session }` body into:

```rust
fn open_session_diff(&mut self, ctx: &egui::Context, session: runtime::SessionId) {
    let cwd = self.cached_session_cwd(session);
    let workspace_name = self
        .workspaces
        .iter()
        .find(|workspace| workspace.id == self.active.id)
        .map(Self::workspace_display_name);
    let session_label = self.inbox_session_label(&self.active.id, session);
    let title = match (workspace_name, session_label) {
        (Some(workspace), Some(session)) => format!("{workspace} · {session}"),
        (Some(workspace), None) => workspace,
        (None, Some(session)) => session,
        (None, None) => String::new(),
    };
    self.diff_panel_ui
        .open_for(ctx, self.active.id.clone(), session, cwd, title);
}
```

Dispatch:

```rust
Some(SidebarAction::ShowFocusedDiff) => {
    if let Some(session) = self.active.workspace_ui.focused_session() {
        self.open_session_diff(ui.ctx(), session);
    }
}
Some(SidebarAction::ShowTerminal) => {
    self.agent_terminal_ui
        .set_view(ui::agent_terminal::AgentTerminalView::Terminal);
}
Some(SidebarAction::OpenConnectors) => {
    self.settings_category = ui::settings::Category::Connectors;
    self.settings_open = true;
}
```

- [ ] **Step 6: Run tests and check**

```bash
cargo test -p deppy-sijo designall_사이드바도구는_기존기능으로만_연결된다 --locked -- --test-threads=1
cargo test -p deppy-sijo ui::file_tree::tests --locked -- --test-threads=1
cargo check -p deppy-sijo --all-targets --locked
cargo run -p xtask --locked -- i18n-check
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/app/src/ui/file_tree.rs crates/app/src/app.rs crates/i18n/locales
git commit -m "feat(ui): add DesignALL workspace tool tabs"
```

---

## Task 8: Unify Terminal and Main-Panel Chrome

**Files:**
- Modify: `crates/app/src/app.rs:17275-17315`
- Modify: `crates/app/src/ui/workspace.rs:2260-2760`
- Test: `crates/app/src/ui/workspace.rs:4910-5055`

- [ ] **Step 1: Write a failing pane-header style test**

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PaneHeaderStyle {
    background: egui::Color32,
    selection_fill: Option<egui::Color32>,
    active_line: Option<egui::Color32>,
}

#[test]
fn designall_pane_header는_배경카드없이_활성선만_쓴다() {
    let style = pane_header_style(crate::ui::designall::DARK, true);
    assert_eq!(style.background, crate::ui::designall::DARK.app_background);
    assert_eq!(style.selection_fill, None);
    assert_eq!(style.active_line, Some(crate::ui::designall::DARK.accent));
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p deppy-sijo designall_pane_header는_배경카드없이_활성선만_쓴다 --locked -- --test-threads=1
```

Expected: FAIL because the projection is absent.

- [ ] **Step 3: Replace hard-coded chrome colors**

```rust
fn pane_header_style(tokens: crate::ui::designall::Tokens, focused: bool) -> PaneHeaderStyle {
    PaneHeaderStyle {
        background: tokens.app_background,
        selection_fill: None,
        active_line: focused.then_some(tokens.accent),
    }
}
```

In `App`, replace the hard-coded central frame fill with `designall::structural_frame`.

In `WorkspaceUi::render_pane_header`:

- paint one continuous header background;
- remove the focused tab rectangle fill;
- paint a 2px active line on the focused header only;
- use the DesignALL separator at the header bottom;
- retain title clipping, status dot, close, toolbar, context menu, and focus action.

In pane rendering, replace only the outer pane fill with `tokens.app_background`. Do not change `renderer_egui::draw` arguments or terminal ANSI colors.

- [ ] **Step 4: Normalize split-handle colors**

Keep geometry, hit rect, and runtime command unchanged. Use DesignALL separator when idle and accent when hovered/dragged.

- [ ] **Step 5: Run workspace regressions**

```bash
cargo test -p deppy-sijo designall_pane_header는_배경카드없이_활성선만_쓴다 --locked -- --test-threads=1
cargo test -p deppy-sijo 가로와_세로_split의_모든_leaf가_같은_밀착형_layout을_갖는다 --locked -- --test-threads=1
cargo test -p deppy-sijo 지원되는_모든_좁은_split에서_도구가_닫기를_덮지_않는다 --locked -- --test-threads=1
cargo test -p deppy-sijo ui::workspace::tests --locked -- --test-threads=1
```

Expected: PASS; no terminal crate files changed.

- [ ] **Step 6: Commit**

```bash
git add crates/app/src/app.rs crates/app/src/ui/workspace.rs
git commit -m "feat(ui): unify DesignALL terminal chrome"
```

---

## Task 9: Validate and Present D3

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`

- [ ] **Step 1: Run the D3 gate**

```bash
cargo test -p deppy-sijo ui::file_tree::tests --locked -- --test-threads=1
cargo test -p deppy-sijo ui::workspace::tests --locked -- --test-threads=1
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
git diff --check
```

Expected: every command exits 0.

- [ ] **Step 2: Rebuild and relaunch**

Use the exact-process termination and signed `scripts/dev-run.sh` procedure from Task 3.

- [ ] **Step 3: Stop for user review**

Ask the user to inspect Files/Search/Git/Terminal/MCP actions, flat pane headers, focused active line, unchanged search/split/new/close tools, terminal typing, selection, Cmd+C, and Korean IME. Do not start D4 before approval.

- [ ] **Step 4: Record and commit evidence**

```bash
git add docs/CODEX_HANDOFF.md
git commit -m "docs(design): record DesignALL D3 checkpoint"
```

---

## Task 10: Flatten the Composer Input Surface

**Files:**
- Modify: `crates/app/src/app.rs:15290-15345`
- Modify: `crates/app/src/ui/composer.rs:500-620`
- Test: `crates/app/src/ui/composer.rs:2450-2900`

- [ ] **Step 1: Write a failing composer-frame test**

```rust
#[test]
fn designall_composer는_그림자없는_입력표면이다() {
    let frame = composer_frame(&egui::Visuals::dark());
    assert_eq!(frame.shadow, egui::epaint::Shadow::NONE);
    assert_eq!(frame.corner_radius, egui::CornerRadius::same(4));
    assert_eq!(frame.fill, crate::ui::designall::DARK.input_background);
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p deppy-sijo designall_composer는_그림자없는_입력표면이다 --locked -- --test-threads=1
```

Expected: FAIL because `composer_frame` is absent.

- [ ] **Step 3: Implement the allowed input surface**

```rust
fn composer_frame(visuals: &egui::Visuals) -> egui::Frame {
    let tokens = crate::ui::designall::tokens(visuals);
    egui::Frame::NONE
        .fill(tokens.input_background)
        .stroke(crate::ui::designall::separator_stroke(visuals))
        .corner_radius(egui::CornerRadius::same(
            crate::ui::designall::INTERACTION_CORNER_RADIUS,
        ))
        .inner_margin(egui::Margin::symmetric(12, 10))
}
```

Replace the shadowed card with this frame. Do not change TextEdit IDs, focus, IME guard, send shortcuts, history, attachments, MCP tools, or draft storage.

In `App::render_composer_dock`, use one top DesignALL separator and a zero-radius structural frame. Retain outer spacing and composer behavior.

- [ ] **Step 4: Run composer regressions**

```bash
cargo test -p deppy-sijo designall_composer는_그림자없는_입력표면이다 --locked -- --test-threads=1
cargo test -p deppy-sijo ui::composer::tests --locked -- --test-threads=1
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/app/src/app.rs crates/app/src/ui/composer.rs
git commit -m "feat(ui): flatten DesignALL composer input"
```

---

## Task 11: Preserve and Restyle the Bottom Status Bar

**Files:**
- Modify: `crates/app/src/ui/designall.rs`
- Modify: `crates/app/src/ui/agent_terminal.rs:148-245`
- Test: `crates/app/src/ui/agent_terminal.rs:960-1050`

- [ ] **Step 1: Strengthen the existing content-contract kittest**

Render with fallback English, `waiting = 2`, `mcp_count = 5`, and a harness width of 1400px
so the existing non-compact memory form is visible, then query:

```rust
harness.get_by_label("Sessions 0");
harness.get_by_label("MCP 5");
harness.get_by_label("Claude");
harness.get_by_label("OpenAI");
harness.get_by_label("GitHub");
harness.get_by_label("Waiting for input 2");
harness.get_by_label("Terminal");
harness.get_by_label("CPU —");
harness.get_by_label("App 0 B · Sessions 0 B");
```

Keep `memory_label_은_좁은_폭에서_앱_값만_남긴다` unchanged.

- [ ] **Step 2: Run the characterization test before styling**

```bash
cargo test -p deppy-sijo kittest_하단상태바에_claude_openai_github가_함께_표시된다 --locked -- --test-threads=1
```

Expected: PASS. If an accessibility label differs, inspect current catalog output and adjust only the assertion.

- [ ] **Step 3: Add a pixel-snapped vertical separator helper**

```rust
pub fn vertical_separator(ui: &mut egui::Ui, height: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, height), egui::Sense::hover());
    let x = ui.painter().round_to_pixel_center(rect.center().x);
    ui.painter().vline(x, rect.y_range(), separator_stroke(ui.visuals()));
}
```

Replace only status-bar `ui.separator()` calls with this helper. Preserve exact order:

```text
usage | sessions | MCP | Claude OpenAI GitHub | conditional waiting
right: current view | CPU | app/session memory
```

Do not change totals, memory formatting, provider URLs, polling, usage, waiting, or right-to-left layout.

- [ ] **Step 4: Run status tests**

```bash
cargo test -p deppy-sijo ui::agent_terminal::tests --locked -- --test-threads=1
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/app/src/ui/designall.rs crates/app/src/ui/agent_terminal.rs
git commit -m "feat(ui): finish DesignALL status chrome"
```

---

## Task 12: Run Final D4 Gates and Present the App

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`

- [ ] **Step 1: Run focused UI suites serially**

```bash
cargo test -p deppy-sijo ui::designall::tests --locked -- --test-threads=1
cargo test -p deppy-sijo ui::file_tree::tests --locked -- --test-threads=1
cargo test -p deppy-sijo ui::workspace::tests --locked -- --test-threads=1
cargo test -p deppy-sijo ui::composer::tests --locked -- --test-threads=1
cargo test -p deppy-sijo ui::agent_terminal::tests --locked -- --test-threads=1
```

Expected: PASS.

- [ ] **Step 2: Run full app gates**

```bash
cargo test -p deppy-sijo --locked -- --test-threads=1
cargo check -p deppy-sijo --all-targets --locked
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo run -p xtask --locked -- i18n-check
git diff --check
```

Expected: every command exits 0. Do not fix unrelated failures; test whether they reproduce at `e0b3f5c` and record them.

- [ ] **Step 3: Verify scope and render-I/O boundaries**

```bash
git diff e0b3f5c --name-only
rg -n "std::fs|Command::new|TcpStream|reqwest|run_git" crates/app/src/ui/designall.rs crates/app/src/ui/file_tree.rs crates/app/src/ui/workspace.rs crates/app/src/ui/composer.rs crates/app/src/ui/agent_terminal.rs
```

Expected: only File Map files changed; new module has no I/O. Verify any existing old-file matches with `git diff -U0 e0b3f5c -- <file>`.

- [ ] **Step 4: Build and launch final signed debug app**

Use Task 3's exact-process termination and signed `scripts/dev-run.sh` procedure. Ask the user to verify the full three-column layout, no structural cards, all project/session/file/tool actions, resizing/scrolling, composer typing/send/Cmd+C/Korean IME, and unchanged status contents/order/hover/live updates.

- [ ] **Step 5: Record and commit final evidence**

Update handoff with exact counts, failed attempts, PID, signature, feedback, remaining physical checks, and disposition.

```bash
git add docs/CODEX_HANDOFF.md
git commit -m "docs(design): record DesignALL final validation"
```

- [ ] **Step 6: Stop before integration**

Do not merge or push. Report clean `DesignALL` HEAD and wait for an explicit integration, push, release package, or settings/modal redesign request.

---

## Acceptance Checklist

- [ ] Three columns: navigation rail, project/file sidebar, main work area.
- [ ] One continuous background with 1px structural lines.
- [ ] No project cards, session cards, group shadows, or active pills.
- [ ] Only interaction states use bounded fills.
- [ ] Git repository/branch subtitles remain removed.
- [ ] Project/session ordering, focus, rename, close, menus, and status semantics remain intact.
- [ ] File selection, expansion, search, DnD, clipboard, menus, and scrolling remain intact.
- [ ] Project/file split preserves a 50px file minimum.
- [ ] Terminal render, split, typing, Cmd+C, and Korean IME remain intact.
- [ ] Composer semantics and shortcuts remain intact.
- [ ] Status contents, order, waiting item, compact memory, hover, and sources remain intact.
- [ ] D1–D4 were each built, launched, shown, and accepted before proceeding.
- [ ] No runtime/storage/terminal crate or dependency file changed.
