# Cross-Workspace Fluid Pane Strip Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the primary and attached workspace panes share a gutter-free resizable horizontal strip and display attached titles as `Project (Workspace)`.

**Architecture:** Keep the existing bounded attachment vector as the only persistent layout state. A lower state helper performs safe adjacent-width updates, WorkspaceUi renders a supplied precomputed title, and the App composition root derives the fluid primary/foreign geometry and divider interactions once per frame.

**Tech Stack:** Rust, egui/eframe, existing `CrossWorkspacePaneState`, existing `WorkspaceUi`, Cargo test/Clippy/fmt, macOS Developer ID packaging.

---

## File Map and Ownership

- `crates/app/src/ui/cross_workspace.rs`: worker A only; width bounds and adjacent attached-pane resizing.
- `crates/app/src/ui/workspace.rs`: worker B only; attached header title API and rendering.
- `crates/app/src/app.rs`: worker C only after A and B integration; fluid geometry, project/workspace title derivation, divider wiring, placeholder title.
- `docs/CODEX_HANDOFF.md`: orchestrator only; progress, review findings, tests, packaging, remaining physical checks.

Workers must not edit files outside their assigned scope. Tests are written during implementation but, per user instruction, are executed only once after all three production lanes are integrated.

### Task 1: Bounded Adjacent Width State

**Files:**
- Modify: `crates/app/src/ui/cross_workspace.rs:1`
- Test: `crates/app/src/ui/cross_workspace.rs:980`

- [ ] **Step 1: Expose the existing width bounds inside the app crate**

Change only visibility; do not change values:

```rust
pub(crate) const MIN_ATTACHED_WIDTH_PX: f32 = 320.0;
pub(crate) const MAX_ATTACHED_WIDTH_PX: f32 = 960.0;
const DEFAULT_ATTACHED_WIDTH_PX: f32 = 420.0;
```

- [ ] **Step 2: Add adjacent-width regressions before implementation**

Add tests covering exact adjacency, total-width preservation, both clamps, reversed/non-adjacent IDs, and non-finite requests:

```rust
#[test]
fn adjacent_resize_preserves_pair_total_and_bounds() {
    let mut state = CrossWorkspacePaneState::default();
    let left = state.attach_right("primary", target("foreign-a", 1), 420.0, 6)
        .appended_id().unwrap();
    let right = state.attach_right("primary", target("foreign-b", 2), 420.0, 6)
        .appended_id().unwrap();

    assert!(state.resize_adjacent(left, right, 500.0));
    assert_eq!(state.attachment(left).unwrap().width_px(), 500.0);
    assert_eq!(state.attachment(right).unwrap().width_px(), 340.0);

    assert!(state.resize_adjacent(left, right, 700.0));
    assert_eq!(state.attachment(left).unwrap().width_px(), 520.0);
    assert_eq!(state.attachment(right).unwrap().width_px(), 320.0);
}
```

- [ ] **Step 3: Implement one bounded adjacent mutation**

Add this API next to `set_width`:

```rust
pub(crate) fn resize_adjacent(
    &mut self,
    left_id: AttachmentId,
    right_id: AttachmentId,
    requested_left_width: f32,
) -> bool {
    if !requested_left_width.is_finite() {
        return false;
    }
    let Some(left_index) = self.attachments.iter().position(|pane| pane.id == left_id) else {
        return false;
    };
    if self.attachments.get(left_index + 1).map(|pane| pane.id) != Some(right_id) {
        return false;
    }
    let (left, right) = {
        let (before_right, from_right) = self.attachments.split_at_mut(left_index + 1);
        (&mut before_right[left_index], &mut from_right[0])
    };
    let pair_width = left.width_px + right.width_px;
    let minimum_left = MIN_ATTACHED_WIDTH_PX.max(pair_width - MAX_ATTACHED_WIDTH_PX);
    let maximum_left = MAX_ATTACHED_WIDTH_PX.min(pair_width - MIN_ATTACHED_WIDTH_PX);
    let next_left = requested_left_width.clamp(minimum_left, maximum_left);
    left.width_px = next_left;
    right.width_px = pair_width - next_left;
    true
}
```

- [ ] **Step 4: Defer execution and report the exact intended command**

Do not run tests yet. Report this command to the orchestrator:

```bash
cargo test -p deppy-sijo ui::cross_workspace::tests --locked -- --test-threads=1
```

Expected after integration: all state tests pass, including the new adjacent-resize cases.

### Task 2: Precomputed Attached Header Title

**Files:**
- Modify: `crates/app/src/ui/workspace.rs:2965`
- Test: `crates/app/src/ui/workspace.rs:6158`

- [ ] **Step 1: Add a title contract regression**

Add a source or render harness test proving the header uses only the supplied title and does not reconstruct the old source/session marker:

```rust
#[test]
fn attached_header_uses_only_precomputed_project_workspace_title() {
    let source = include_str!("workspace.rs");
    let header = source
        .split_once("fn render_attached_pane_header(").unwrap().1
        .split_once("fn render_attached_placeholder(").unwrap().0;
    assert!(header.contains("attached_display_title"));
    assert!(!header.contains("workspace.cross_pane.external_source"));
    assert!(!header.contains("pane_title"));
}
```

- [ ] **Step 2: Separate display title from runtime workspace label**

Add `attached_display_title: &str` immediately after `external_workspace_label: &str` in `show_prepared_attached_pane` and `render_attached_pane_header`. Keep `external_workspace_label` for placeholders and `PaneRenderMode::Attached.workspace_label`.

The compatibility wrapper supplies the existing workspace label for both values:

```rust
self.show_prepared_attached_pane(
    ui,
    config,
    catalog,
    target,
    external_workspace_label,
    external_workspace_label,
    availability,
    None,
)
```

- [ ] **Step 3: Render the supplied title only**

Replace the old `source · pane_title` construction with:

```rust
ui.painter().text(
    text_rect.left_center(),
    egui::Align2::LEFT_CENTER,
    attached_display_title,
    egui::FontId::proportional(12.0),
    tokens.muted_text,
);
```

Do not change identity top-line color, detach, reorder, focus, body, or placeholder behavior.

- [ ] **Step 4: Update WorkspaceUi-local test and wrapper call sites**

Every `show_prepared_attached_pane` call inside `workspace.rs` passes a deterministic test title such as `"Project (Workspace)"` after its workspace label. Do not edit `app.rs`.

- [ ] **Step 5: Defer execution and report the exact intended command**

```bash
cargo test -p deppy-sijo ui::workspace::tests --locked -- --test-threads=1
```

Expected after integration: all WorkspaceUi tests pass and the old header marker is absent from the header function.

### Task 3: Fluid App Geometry and Divider Wiring

**Files:**
- Modify: `crates/app/src/app.rs:1913`
- Test: `crates/app/src/app.rs:21900`

This task starts only after Tasks 1 and 2 are reviewed and integrated.

- [ ] **Step 1: Add pure geometry and title regressions**

Cover fitting, overflow, narrow viewport, no-right-gutter, alias/path/fallback titles, and primary-divider first-pane requests:

```rust
#[test]
fn fluid_strip_fits_foreign_content_to_right_edge() {
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1_200.0, 700.0));
    let layout = fluid_cross_workspace_layout(rect, &[420.0]);
    assert_eq!(layout.primary.width(), 776.0);
    assert_eq!(layout.divider.width(), ATTACHED_DIVIDER_WIDTH);
    assert_eq!(layout.foreign.right(), rect.right());
    assert_eq!(layout.foreign.width(), 420.0);
}

#[test]
fn attached_title_is_project_then_workspace_alias() {
    let row = workspace_row("workspace-id", "Deploy", "/repo/VisionAI2");
    assert_eq!(App::attached_workspace_title(&row), "VisionAI2 (Deploy)");
}
```

- [ ] **Step 2: Extend the render projection with a precomputed title**

```rust
struct CrossWorkspaceRenderPane {
    id: ui::cross_workspace::AttachmentId,
    target: Option<ui::cross_workspace::WorkspacePaneTarget>,
    width_px: f32,
    render_state: ui::cross_workspace::AttachedRenderState,
    workspace_label: String,
    display_title: String,
    identity_color: egui::Color32,
}
```

Derive both labels from the already loaded `storage::WorkspaceRow`:

```rust
fn workspace_project_name(row: &storage::WorkspaceRow) -> String {
    std::path::Path::new(row.path.trim())
        .file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| Self::workspace_display_name(row))
}

fn attached_workspace_title(row: &storage::WorkspaceRow) -> String {
    format!("{} ({})", Self::workspace_project_name(row), Self::workspace_display_name(row))
}
```

If the row is unexpectedly absent, use the bounded workspace ID for both parts.

- [ ] **Step 3: Add a pure fluid layout helper**

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
struct FluidCrossWorkspaceLayout {
    primary: egui::Rect,
    divider: egui::Rect,
    foreign: egui::Rect,
}

fn fluid_cross_workspace_layout(
    rect: egui::Rect,
    attached_widths: &[f32],
) -> FluidCrossWorkspaceLayout {
    let content_width = foreign_strip_rects(egui::Pos2::ZERO, rect.height(), attached_widths)
        .last()
        .map_or(0.0, egui::Rect::right);
    let available_after_divider = (rect.width() - ATTACHED_DIVIDER_WIDTH).max(0.0);
    let primary_min = ui::cross_workspace::MIN_ATTACHED_WIDTH_PX.min(available_after_divider);
    let foreign_viewport_width = content_width.min((available_after_divider - primary_min).max(0.0));
    let primary_right = rect.right() - ATTACHED_DIVIDER_WIDTH - foreign_viewport_width;
    let primary = egui::Rect::from_min_max(rect.min, egui::pos2(primary_right, rect.bottom()));
    let divider = egui::Rect::from_min_max(
        egui::pos2(primary.right(), rect.top()),
        egui::pos2(primary.right() + ATTACHED_DIVIDER_WIDTH, rect.bottom()),
    );
    let foreign = egui::Rect::from_min_max(egui::pos2(divider.right(), rect.top()), rect.max);
    FluidCrossWorkspaceLayout { primary, divider, foreign }
}
```

- [ ] **Step 4: Replace the fixed 50/50 geometry**

Build the bounded width vector once, call `fluid_cross_workspace_layout`, and use its three rects. The attached `ScrollArea` keeps `content_width` as its minimum width; because the viewport width equals the fitting content width when content fits, no blank area remains to the right.

- [ ] **Step 5: Make the primary boundary draggable**

Interact with the full-height divider using `Sense::drag()`. Convert its absolute pointer position into the requested first attachment width by subtracting the fixed tail width of later attached panes and internal dividers. Store only `Some((first_attachment_id, requested_width))` and apply it through existing `set_width` after rendering.

```rust
let tail_width = content_width - first.width_px;
let requested_first = rect.right()
    - (pointer.x + ATTACHED_DIVIDER_WIDTH * 0.5)
    - tail_width;
attached_width_requested = Some((first.id, requested_first));
```

The existing state clamp protects 320..=960px. Do not request periodic repaint; use the existing drag repaint path.

- [ ] **Step 6: Make internal dividers resize adjacent panes and remove the outer divider**

Render an attached divider only when `index + 1 < render_panes.len()`. Its pointer position requests the left width, then defer this mutation until after rendering:

```rust
adjacent_width_requested = Some((
    pane.id,
    render_panes[index + 1].id,
    pointer.x - pane_rect.left(),
));
```

Apply with `cross_workspace_pane.resize_adjacent(left, right, width)`. The final pane has no trailing divider or right gutter.

- [ ] **Step 7: Wire the exact title into live and placeholder headers**

Pass `&pane.display_title` to `show_prepared_attached_pane` after `&pane.workspace_label`. Change `show_app_attached_placeholder` to accept both `workspace_name` and `display_title`; render `display_title` in its header while retaining `workspace_name` for body translations and ID salt.

- [ ] **Step 8: Preserve existing ownership and drop paths**

Do not change `FrameTerminalOwner`, visibility sets, offscreen selection, prepare-once grouping, drop anchors, restore coordination, capacity, or runtime commands. Add source-law assertions that the production render still uses `visible_attachment_indices`, `frame_terminal_owner`, and `InsertAnchor::Attached(pane.id)`.

- [ ] **Step 9: Defer execution and report the exact intended commands**

```bash
cargo test -p deppy-sijo cross_workspace_app --locked -- --test-threads=1
cargo test -p deppy-sijo ui::workspace::tests --locked -- --test-threads=1
cargo test -p deppy-sijo ui::cross_workspace::tests --locked -- --test-threads=1
```

Expected after integration: all new and existing cross-workspace tests pass.

### Task 4: Orchestrated Review and Batch Validation

**Files:**
- Modify: `docs/CODEX_HANDOFF.md:1`

- [ ] **Step 1: Review each worker patch before integration**

Reject overlap, new I/O, timers, threads, raw project probes, unbounded loops, or changes to runtime ownership. Confirm Task 1 changes only `ui/cross_workspace.rs`, Task 2 only `ui/workspace.rs`, and Task 3 only `app.rs`.

- [ ] **Step 2: Run focused tests once after full integration**

```bash
cargo test -p deppy-sijo ui::cross_workspace::tests --locked -- --test-threads=1
cargo test -p deppy-sijo ui::workspace::tests --locked -- --test-threads=1
cargo test -p deppy-sijo cross_workspace_app --locked -- --test-threads=1
```

Expected: zero failures.

- [ ] **Step 3: Run complete regression and resource gates sequentially**

```bash
cargo test -p deppy-sijo --locked -- --test-threads=1
cargo test -p runtime --lib --locked -- --test-threads=1
cargo run -p xtask --locked -- perf-smoke
cargo run -p xtask --locked -- check-boundary
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Expected: every command exits zero. Run serially to avoid CPU/RAM bursts.

- [ ] **Step 4: Package without relaunching**

```bash
DEPPY_SIGN_IDENTITY='Developer ID Application: VectorNine INC (ZDTU5LS35K)' \
  CARGO_NET_OFFLINE=true sh scripts/package-macos.sh
codesign --verify --deep --strict --verbose=2 'target/bundle/Deppy Sijo.app'
```

Expected: trusted bundle/archive verification and independent codesign verification pass. Do not relaunch or push unless explicitly requested.

- [ ] **Step 5: Update the handoff with actual evidence**

Record commands, actual counts, failures and corrections, review conclusions, artifact hashes, unchanged ownership/resource invariants, and the remaining visual/RSS checks. Never report a deferred or unexecuted test as passed.
