# Pane Drag-and-Drop Feedback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make session drag-and-drop visibly discoverable, insert a dropped session immediately after the exact hovered pane, and restrict foreign-workspace identity color to the pane header's top line.

**Architecture:** Three disjoint UI/state workers change `file_tree.rs`, `cross_workspace.rs`, and `workspace.rs` in parallel. After those patches pass focused RED/GREEN checks, one serial App integration worker changes only `app.rs` and wires exact pane anchors into the existing bounded restore/admission path. Broad regression, resource gates, direct Codex review, and release packaging run only after integration.

**Tech Stack:** Rust, egui 0.35 drag-and-drop APIs, existing DesignALL tokens, existing bounded cross-workspace attachment and restore coordinator.

---

## File Ownership

- `crates/app/src/ui/file_tree.rs`: context-menu width/non-wrap and exact-source drag elevation only.
- `crates/app/src/ui/cross_workspace.rs`: bounded insert anchors and ordered attachment insertion only.
- `crates/app/src/ui/workspace.rs`: foreign top-line paint and reusable pane drop-highlight painter only.
- `crates/app/src/app.rs`: serial integration after all three lower patches are frozen.
- `docs/CODEX_HANDOFF.md`: orchestrator-owned checkpoints only.

No two implementation workers may edit the same file. Locale files, runtime,
storage, terminal, persistence, Cargo manifests, and dependencies are out of
scope.

### Task 1: Session Menu and Drag Source Feedback

**Files:**
- Modify: `crates/app/src/ui/file_tree.rs:4261-4665`
- Test: inline `#[cfg(test)]` module in `crates/app/src/ui/file_tree.rs`

- [ ] **Step 1: Write failing style tests**

Add focused tests proving a small pure projection returns a minimum menu width
of 220 px with `TextWrapMode::Extend`, and that only the exact active
`SessionRowDragPayload` selects the elevated row style.

```rust
#[test]
fn inactive_session_menu_keeps_korean_labels_on_one_line() {
    let style = inactive_session_menu_style();
    assert!(style.min_width >= 220.0);
    assert_eq!(style.wrap_mode, egui::TextWrapMode::Extend);
}

#[test]
fn exact_session_drag_projects_elevated_source_style() {
    let style = session_drag_style(true, designall::DARK);
    assert!(style.fill.is_some());
    assert_eq!(style.stroke.width, 1.0);
    assert!(style.shadow.blur > 0);
    assert!(style.rail_multiplier > 1.0);
}
```

- [ ] **Step 2: Run RED tests**

Run:

```bash
cargo test -p deppy-sijo inactive_session_menu_keeps_korean_labels_on_one_line --locked -- --test-threads=1
cargo test -p deppy-sijo exact_session_drag_projects_elevated_source_style --locked -- --test-threads=1
```

Expected: compilation fails because the new style projections do not exist.

- [ ] **Step 3: Implement menu and source-row styling**

Set the inactive-session context menu's minimum width before creating buttons,
temporarily select `TextWrapMode::Extend`, and restore no global style. Detect
the exact active payload with `egui::DragAndDrop::payload::<SessionRowDragPayload>`
and target equality. Paint the stronger neutral fill, one-pixel accent stroke,
soft shadow, and boosted status rail behind the existing text without moving
the row rectangle. Expose `workspace_accent` as `pub(crate)` so App can pass the
same identity color to foreign pane headers.

- [ ] **Step 4: Run focused GREEN tests**

Run:

```bash
cargo test -p deppy-sijo inactive_session_menu --locked -- --test-threads=1
cargo test -p deppy-sijo session_drag --locked -- --test-threads=1
```

Expected: all selected tests pass with no warnings.

- [ ] **Step 5: Freeze the isolated patch**

The worker reports the exact diff and focused results. It may create one
isolated transport commit; the orchestrator does not push it.

### Task 2: Exact After-Pane Attachment Insertion

**Files:**
- Modify: `crates/app/src/ui/cross_workspace.rs:1-520`
- Test: inline `#[cfg(test)]` module in `crates/app/src/ui/cross_workspace.rs`

- [ ] **Step 1: Write failing ordering tests**

Add the bounded anchor type and tests for primary, exact attachment, end, stale
anchor, duplicate, and capacity behavior.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsertAnchor {
    Primary,
    Attached(AttachmentId),
    End,
}

#[test]
fn primary_anchor_inserts_before_all_foreign_panes() { /* exact order assertion */ }

#[test]
fn attached_anchor_inserts_immediately_after_exact_id() { /* exact order assertion */ }

#[test]
fn stale_attached_anchor_fails_closed() { /* no order or focus mutation */ }
```

- [ ] **Step 2: Run RED tests**

Run:

```bash
cargo test -p deppy-sijo cross_workspace_insert_anchor --locked -- --test-threads=1
```

Expected: compilation fails because `InsertAnchor` and anchored admission do
not exist.

- [ ] **Step 3: Implement bounded anchored admission**

Add anchored live and restoring admission methods. Resolve `Primary` to index
zero, `End` to `attachments.len()`, and `Attached(id)` to the exact following
index. Return a distinct fail-closed outcome for a stale anchor or leave the
existing outcome unchanged with a separate `Result`; do not silently append.
Duplicate focus happens before anchor resolution and does not reorder. Capacity
rejection remains non-mutating. Keep existing right-append wrappers delegating
to `InsertAnchor::End` for compatibility.

- [ ] **Step 4: Run focused GREEN tests**

Run:

```bash
cargo test -p deppy-sijo ui::cross_workspace::tests --locked -- --test-threads=1
```

Expected: all cross-workspace state tests pass.

- [ ] **Step 5: Freeze the isolated patch**

The worker reports the exact API names, diff, and focused results. It may create
one isolated transport commit; the orchestrator does not push it.

### Task 3: Pane Drop Highlight and Foreign Top Line

**Files:**
- Modify: `crates/app/src/ui/workspace.rs:360-455,580-700,2800-3045`
- Test: inline `#[cfg(test)]` module in `crates/app/src/ui/workspace.rs`

- [ ] **Step 1: Write failing visual-contract tests**

Add pure geometry/style tests proving the drop target uses a two-pixel inset
outline plus a three-pixel right insertion marker, and the foreign identity
color is consumed only by a one-pixel top-line painter.

```rust
#[test]
fn pane_drop_feedback_marks_surface_and_right_insertion_edge() {
    let style = pane_drop_feedback_style(designall::DARK);
    assert_eq!(style.outline.width, 2.0);
    assert_eq!(style.insertion_width, 3.0);
}

#[test]
fn foreign_identity_color_is_top_line_only() {
    let style = attached_identity_style(designall::DARK, egui::Color32::LIGHT_BLUE);
    assert_eq!(style.top_line.width, 1.0);
    assert_eq!(style.header_fill, designall::DARK.app_background);
    assert_eq!(style.body_fill, designall::DARK.app_background);
}
```

- [ ] **Step 2: Run RED tests**

Run:

```bash
cargo test -p deppy-sijo pane_drop_feedback --locked -- --test-threads=1
cargo test -p deppy-sijo foreign_identity_color_is_top_line_only --locked -- --test-threads=1
```

Expected: compilation fails because the style/painter helpers do not exist.

- [ ] **Step 3: Implement reusable transient painters**

Extend `AttachedPaneHeaderContext` with `identity_color`. Paint that color only
as a pixel-snapped one-pixel line at `header.top()`. Keep header fill, bottom
separator, placeholder body, terminal body, and resize handle neutral. Add a
`paint_session_pane_drop_feedback(ui, rect, label)` helper that paints an inset
outline, right-edge insertion marker, and compact neutral label; the helper has
no input handling or retained state.

- [ ] **Step 4: Run focused GREEN tests**

Run:

```bash
cargo test -p deppy-sijo ui::workspace::tests --locked -- --test-threads=1
```

Expected: all WorkspaceUi tests pass.

- [ ] **Step 5: Freeze the isolated patch**

The worker reports the exact signature changes, diff, and focused results. It
may create one isolated transport commit; the orchestrator does not push it.

### Task 4: Serial App Integration

**Files:**
- Modify: `crates/app/src/app.rs:1860-2040,7340-7410,11620-12670,19040-19425`
- Test: inline `#[cfg(test)]` module in `crates/app/src/app.rs`

This task starts only after Tasks 1-3 are integrated and reviewed.

- [ ] **Step 1: Write failing App integration tests**

Add tests proving the DnD controller action carries `Primary` or exact
`Attached(id)`, hover/context actions carry `End`, stale anchors produce no
attachment, primary inserts first, middle foreign inserts immediately after,
and only visible pane rectangles are registered.

- [ ] **Step 2: Run RED tests**

Run:

```bash
cargo test -p deppy-sijo cross_workspace_app_drop_anchor --locked -- --test-threads=1
```

Expected: compilation fails on the missing anchored controller/action path.

- [ ] **Step 3: Wire exact visible pane drop surfaces**

Replace the central all-surface release with one primary interaction and one
interaction per visible foreign rectangle. Accept only
`SessionRowDragPayload`. While matching payload hover is active, call the new
workspace painter. On release, stage the existing open flow with the exact
anchor. Resolve workspace identity color from the same sidebar palette and pass
it through `AttachedPaneHeaderContext`. Hover-button and context-menu paths use
`InsertAnchor::End`.

- [ ] **Step 4: Preserve admission and restore safety**

Route both live and persisted targets through anchored state admission while
retaining the six-pane cap, one-global cold restore coordinator, durable
barrier, duplicate focus, runtime budget, exact visibility, and non-destructive
detach behavior. A stale anchor returns without spawning or queueing anything.

- [ ] **Step 5: Run focused GREEN tests**

Run:

```bash
cargo test -p deppy-sijo cross_workspace_app_ --locked -- --test-threads=1
```

Expected: all App cross-workspace tests pass.

### Task 5: Integration Review and Deferred Broad Gates

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`

- [ ] **Step 1: Run direct Codex review**

The orchestrator runs `codex exec` directly against the complete implementation
diff. The review must trace exact drop geometry to exact insertion order,
stale-anchor fail-closed behavior, cold restore admission, payload bounds, and
foreign-color paint scope. Accept only `CONCLUSION: OK`.

- [ ] **Step 2: Apply at most two correction rounds**

Return findings to the owning worker with the same exclusive file scope, then
rerun direct review.

- [ ] **Step 3: Run broad tests after implementation**

Run serially only after all code is integrated:

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

- [ ] **Step 4: Rebuild only after green gates**

```bash
DEPPY_SIGN_IDENTITY='Developer ID Application: VectorNine INC (ZDTU5LS35K)' \
  CARGO_NET_OFFLINE=true sh scripts/package-macos.sh
```

Do not relaunch unless the user explicitly requests it.
