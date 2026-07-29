# Multi Cross-Workspace Pane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add bounded, horizontally reorderable cross-workspace panes for live and persisted canonical sessions without idle polling, duplicate terminal buffers, or bursty cold restore.

**Architecture:** The App owns ordered attachment slots and a one-in-flight cold restore coordinator. Runtime restores one requested persisted pane into a bounded lazy skeleton, WorkspaceUi prepares each runtime once and renders visible panes many times, and sidebar actions carry identifier-only targets.

**Tech Stack:** Rust, egui drag-and-drop/scroll areas, existing runtime/mux/persist/storage layers, repository i18n catalogs, Cargo tests and Clippy.

---

## File Ownership and Merge Order

### Execution adjustment after wave-one contracts

The typed persisted row is produced in App-owned background projection code,
while the current sidebar receives only live `SessionEntry` values. A standalone
MWP05 change would either break the App's exhaustive action match or fabricate
live IDs for cold rows. To preserve TDD and exact identities, Task 5 is merged
into the serial Task 7 owner: that worker exclusively owns `app.rs`,
`file_tree.rs`, and the new entry-action locale keys. Task 6 remains an
independent renderer worktree and runs in parallel with the final MWP04 review.
No production requirement is removed; this is an ownership/merge-order change.

1. `MWP01 state`: `crates/app/src/ui/cross_workspace.rs` only.
2. `MWP02 config`: `crates/app/src/config.rs`, `crates/app/src/ui/settings.rs`, and five `crates/i18n/locales/*/messages.txt` files.
3. `MWP03 storage`: `crates/storage/src/db.rs` only.
4. `MWP04 runtime`: `crates/runtime/src/command.rs`, `crates/runtime/src/in_process.rs`, `crates/persist/src/repo.rs`.
5. After MWP01/config merge, run `MWP06 renderer`; retain reviewed storage/runtime commits out of the UI base until their App consumer is ready.
6. `MWP05+MWP07 coordinator`: `crates/app/src/app.rs`, `crates/app/src/ui/file_tree.rs`, and entry-action locale keys, after storage/runtime/renderer contracts merge.
7. `MWP08 integration`: docs, resource evidence, direct Codex reviews, final gates.

No two simultaneous workers may edit the same file. Each worker uses an
isolated worktree branched from the same frozen plan commit.

### Task 1: Ordered Attachment State

**Files:**
- Modify: `crates/app/src/ui/cross_workspace.rs`

- [ ] **Step 1: Write failing state tests**

Add tests named:

```rust
attach_right_appends_and_duplicate_focuses_existing
attachment_focus_survives_reorder_by_stable_id
reorder_never_moves_primary_surface
capacity_trim_detaches_rightmost_views
detach_last_reference_releases_runtime_protection
non_finite_width_uses_or_retains_safe_value
reconcile_one_target_does_not_mutate_siblings
```

The tests must expect `AttachmentId`, `FocusedSurface::Attached(id)`, an ordered
slice of attachments, a hard cap of six, default width 420, and width clamp
320–960.

- [ ] **Step 2: Run tests and observe RED**

Run:

```bash
cargo test -p deppy-sijo --locked cross_workspace::tests -- --nocapture
```

Expected: compile/test failure because the ordered attachment API does not
exist and the current second attach replaces the first.

- [ ] **Step 3: Implement the bounded ordered model**

Replace the single optional attachment with these public contracts:

```rust
pub(crate) const HARD_MAX_CROSS_WORKSPACE_PANES: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AttachmentId(u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum FocusedSurface {
    #[default]
    Primary,
    Attached(AttachmentId),
}

pub(crate) enum AttachOutcome {
    Appended(AttachmentId),
    FocusedExisting(AttachmentId),
    CapacityReached,
}
```

Store a `Vec<AttachedPane>`, assign monotonic IDs, append right, compare exact
targets for duplicates, focus by ID, detach/reorder by ID, and return detached
targets without emitting runtime commands. Retain the existing exact runtime
relation reconciliation behavior per attachment.

- [ ] **Step 4: Run state tests GREEN**

Run the Step 2 command. Expected: all cross-workspace model tests pass.

- [ ] **Step 5: Run lane gates and commit**

```bash
cargo check -p deppy-sijo --all-targets --locked
cargo fmt --all -- --check
git diff --check
git add crates/app/src/ui/cross_workspace.rs
git commit -m "feat(app): model bounded cross-workspace panes"
```

### Task 2: Pane Limit Setting

**Files:**
- Modify: `crates/app/src/config.rs`
- Modify: `crates/app/src/ui/settings.rs`
- Modify: `crates/i18n/locales/de/messages.txt`
- Modify: `crates/i18n/locales/en/messages.txt`
- Modify: `crates/i18n/locales/ja/messages.txt`
- Modify: `crates/i18n/locales/ko/messages.txt`
- Modify: `crates/i18n/locales/zh/messages.txt`

- [ ] **Step 1: Write failing config tests**

Add tests proving:

```rust
PerformanceConfig::default().max_cross_workspace_panes == 2
0 normalizes to 1
7 and u32::MAX normalize to 6
1, 2, and 6 round-trip through TOML
```

- [ ] **Step 2: Observe RED**

```bash
cargo test -p deppy-sijo --locked max_cross_workspace_panes -- --nocapture
```

Expected: compile failure because the field is absent.

- [ ] **Step 3: Add config and Performance stepper**

Add:

```rust
#[serde(default = "default_max_cross_workspace_panes")]
pub max_cross_workspace_panes: u32,

fn default_max_cross_workspace_panes() -> u32 { 2 }
```

Normalize with `.clamp(1, 6)`. Add a Performance-page stepper with range 1–6
and localized title/hint explaining that reducing the value closes rightmost
foreign views without terminating their sessions.

- [ ] **Step 4: Run config and i18n tests GREEN**

```bash
cargo test -p deppy-sijo --locked max_cross_workspace_panes -- --nocapture
cargo test -p deppy-sijo --locked i18n -- --nocapture
```

- [ ] **Step 5: Run lane gates and commit**

```bash
cargo check -p deppy-sijo --all-targets --locked
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
git add crates/app/src/config.rs crates/app/src/ui/settings.rs crates/i18n/locales
git commit -m "feat(settings): cap cross-workspace panes"
```

### Task 3: Exact Persisted Pane Projection

**Files:**
- Modify: `crates/storage/src/db.rs`

- [ ] **Step 1: Write failing bounded projection tests**

Extend the existing activity-pane tests to expect a typed row containing
`workspace_id`, `pane_id`, `title`, and `cwd`. Prove row-count, individual
field-byte, row-byte, aggregate-byte, corrupt-text, and `limit + 1` rejection
still fail closed without partial results.

- [ ] **Step 2: Observe RED**

```bash
cargo test -p storage --locked list_persisted_activity_panes_bounded -- --nocapture
```

Expected: compile/assertion failure because `pane_id` is not projected.

- [ ] **Step 3: Implement typed bounded rows**

Add:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedActivityPane {
    pub workspace_id: String,
    pub pane_id: String,
    pub title: String,
    pub cwd: String,
}
```

Update preflight and select SQL to include `p.id`, include its bytes in row and
aggregate budgets, and materialize only after the preflight succeeds. Preserve
the single read transaction and one-query behavior.

- [ ] **Step 4: Run storage tests GREEN**

```bash
cargo test -p storage --locked list_persisted_activity_panes_bounded -- --nocapture
```

- [ ] **Step 5: Run lane gates and commit**

```bash
cargo check -p storage --all-targets --locked
cargo clippy -p storage --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
git add crates/storage/src/db.rs
git commit -m "feat(storage): project exact persisted pane ids"
```

### Task 4: One-Pane Lazy Runtime Restore

**Files:**
- Modify: `crates/runtime/src/command.rs`
- Modify: `crates/runtime/src/in_process.rs`
- Modify: `crates/persist/src/repo.rs`

- [ ] **Step 1: Write failing restore tests**

Add tests proving:

```rust
RestoreWorkspacePane { pane } loads the bounded layout once
only the requested pane receives a runtime session
unrequested pane skeletons keep their original IDs and layout
a second request materializes only its requested pane
an archived agent pane restores read-only and sends no agent start command
an unknown pane creates no PTY and leaves no partial session
normal RestoreWorkspace after partial restore materializes remaining panes once
```

- [ ] **Step 2: Observe RED**

```bash
cargo test -p runtime --locked restore_workspace_pane -- --nocapture
```

Expected: compile failure because `RuntimeCommand::RestoreWorkspacePane` and
the lazy restore catalog do not exist.

- [ ] **Step 3: Add command admission**

Add the bounded command variant:

```rust
RestoreWorkspacePane { pane: MuxPaneId },
```

Include it in debug redaction, command validation, remote deny/allow policy,
and command-name source-law tests. Reject empty or oversized IDs using the
existing mux-ID boundary.

- [ ] **Step 4: Build a lazy restore skeleton**

On the first one-pane restore, load `WorkspaceRestore` through the existing
bounded persistence loader. Install tabs, layouts, and pane shells without
starting unrequested sessions. Store unmaterialized `PaneState` records in a
bounded map keyed by pane ID. Materialize exactly the requested entry with the
existing shell/archive restoration paths, then remove it from the map.

When regular workspace activation requests full restore, materialize remaining
entries sequentially in the runtime worker and drop the catalog. Never invoke
agent launch or auto-resume for archived agent panes.

- [ ] **Step 5: Run runtime tests GREEN**

```bash
cargo test -p runtime --locked restore_workspace_pane -- --nocapture
cargo test -p runtime --locked restore_workspace -- --nocapture
```

- [ ] **Step 6: Run lane gates and commit**

```bash
cargo check -p runtime --all-targets --locked
cargo clippy -p runtime --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
git add crates/runtime/src/command.rs crates/runtime/src/in_process.rs crates/persist/src/repo.rs
git commit -m "feat(runtime): lazily restore requested panes"
```

### Task 5: Session Row Drag and Hover Entry

**Files:**
- Modify: `crates/app/src/ui/file_tree.rs`
- Modify: `crates/i18n/locales/de/messages.txt`
- Modify: `crates/i18n/locales/en/messages.txt`
- Modify: `crates/i18n/locales/ja/messages.txt`
- Modify: `crates/i18n/locales/ko/messages.txt`
- Modify: `crates/i18n/locales/zh/messages.txt`

This task starts only after Task 2 is merged; it owns only new entry-action
locale keys not already added by Task 2.

- [ ] **Step 1: Write failing sidebar tests**

Add tests for:

```rust
live rows emit SessionRowTarget::Live
idle canonical rows emit SessionRowTarget::PersistedPane
payload debug output contains IDs but no title cwd or scrollback
hover button and context menu emit the same OpenBeside target
drag threshold suppresses the ordinary workspace-switch click
active-workspace rows do not expose cross-workspace attach
```

- [ ] **Step 2: Observe RED**

```bash
cargo test -p deppy-sijo --locked file_tree::tests -- --nocapture
```

Expected: compile/assertion failure because rows are live-only and no drag or
hover action exists.

- [ ] **Step 3: Implement identifier-only entry actions**

Change session rows to carry:

```rust
pub(crate) enum SessionRowTarget { Live { /* exact IDs */ }, PersistedPane { workspace_id: String, pane: MuxPaneId } }
pub(crate) enum WorkspaceControllerAction { OpenBeside(SessionRowTarget), /* existing actions */ }
```

Use `Sense::click_and_drag()`, `dnd_set_drag_payload`, and the existing egui
drag threshold. Reveal a localized icon button only while hovering. Keep the
right-click item and route both controls to the same action constructor.

- [ ] **Step 4: Run sidebar and i18n tests GREEN**

```bash
cargo test -p deppy-sijo --locked file_tree::tests -- --nocapture
cargo test -p deppy-sijo --locked i18n -- --nocapture
```

- [ ] **Step 5: Run lane gates and commit**

```bash
cargo check -p deppy-sijo --all-targets --locked
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
git add crates/app/src/ui/file_tree.rs crates/i18n/locales
git commit -m "feat(sidebar): open any canonical session beside"
```

### Task 6: Prepare Once and Render Many

**Files:**
- Modify: `crates/app/src/ui/workspace.rs`

- [ ] **Step 1: Write failing renderer tests**

Add tests proving one runtime preparation can render two exact pane targets,
events are applied once, native input has one owner, offscreen pane rectangles
are excluded, and foreign header reorder output contains only `AttachmentId`
plus destination index.

- [ ] **Step 2: Observe RED**

```bash
cargo test -p deppy-sijo --locked workspace::tests -- --nocapture
```

Expected: failure because current `show_attached_pane` prepares and drains per
pane and the header only emits close/focus.

- [ ] **Step 3: Separate preparation from pane rendering**

Add a runtime-scoped preparation method that applies pending events and resets
per-frame state once. Make the pane renderer consume prepared caches and exact
targets without draining global native input. Add a pure visibility helper:

```rust
fn visible_attachment_indices(viewport: egui::Rect, rects: &[egui::Rect]) -> Vec<usize>
```

Add header drag output carrying only attachment ID and bounded destination.
Do not send runtime pane move, split, close, or kill commands.

- [ ] **Step 4: Run renderer tests GREEN**

```bash
cargo test -p deppy-sijo --locked workspace::tests -- --nocapture
```

- [ ] **Step 5: Run lane gates and commit**

```bash
cargo check -p deppy-sijo --all-targets --locked
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
git add crates/app/src/ui/workspace.rs
git commit -m "feat(ui): render prepared cross-workspace panes"
```

### Task 7: App Restore Coordinator and Horizontal Strip

**Files:**
- Modify: `crates/app/src/app.rs`

- [ ] **Step 1: Write failing coordinator tests**

Add deterministic tests named:

```rust
cross_workspace_restore_queue_has_one_in_flight
cross_workspace_restore_queue_deduplicates_workspace_and_pane
cross_workspace_restore_cancel_releases_pending_and_protection
cross_workspace_restore_timeout_releases_all_bookkeeping
cross_workspace_multiple_runtime_visibility_is_set_based
cross_workspace_last_detach_only_releases_runtime_visibility
cross_workspace_attached_runtimes_still_count_toward_live_budget
cross_workspace_offscreen_panes_are_not_rendered
cross_workspace_focus_routes_host_io_clipboard_ime_and_composer_exactly_once
cross_workspace_capacity_reduction_detaches_rightmost
```

- [ ] **Step 2: Observe RED**

```bash
cargo test -p deppy-sijo --locked cross_workspace_app_ -- --test-threads=1
```

Expected: compile/assertion failure because the App supports one live target
and no cold restore queue.

- [ ] **Step 3: Implement bounded restore coordinator**

Add an App-owned queue of at most six persisted requests, one optional in-flight
request with a finite deadline, and no timer when idle. Reuse an existing warm
runtime or create one within the normal live-runtime budget. Send exactly one
`RestoreWorkspacePane` at a time. Promote the placeholder only after exact mux
relation validation; stale or cancelled completions are ignored and released.

- [ ] **Step 4: Implement set-based leases and capacity**

Replace single-target visibility/protection helpers with unique runtime
identity sets. Count attached runtimes in the resident/live budget while
protecting them from eviction. On the last detach, send Warm visibility and
refresh the existing idle deadline. Lowered pane-limit excess detaches from the
right without closing source sessions.

- [ ] **Step 5: Implement deterministic horizontal layout**

Render A followed by ordered foreign panes inside a horizontal scroll area.
Use a 320 px primary minimum, bounded foreign widths, append-right behavior,
and foreign-only header reorder. Compute pane rects first and call the renderer
only for indices intersecting the viewport.

- [ ] **Step 6: Route exclusive input and host I/O**

Resolve exact focus by `AttachmentId`, workspace ID, runtime incarnation, tab,
pane, and session. Route keyboard, IME, clipboard suppression, composer, and
host I/O only to that surface. Drain/discard native input once when no live
surface is eligible. Reject stale completions without fallback to A.

- [ ] **Step 7: Run coordinator tests GREEN**

```bash
cargo test -p deppy-sijo --locked cross_workspace_app_ -- --test-threads=1
```

- [ ] **Step 8: Run app lane gates and commit**

```bash
cargo check -p deppy-sijo --all-targets --locked
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
git add crates/app/src/app.rs
git commit -m "feat(app): coordinate bounded foreign pane strip"
```

### Task 8: Reviews, Full Gates, and Resource Evidence

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`
- Modify: `docs/mockups/cross-workspace-pane-entry-scenarios.html`
- Create: `docs/build/PR-MWP-summary.md`

- [ ] **Step 1: Run direct lane reviews**

For each integrated lane, run `codex exec` directly from the orchestrator with
read-only sandbox and require the last line to be `CONCLUSION: OK`. Accepted
findings return to the original owner for no more than two correction rounds.

- [ ] **Step 2: Run focused and workspace gates**

```bash
cargo test -p storage --locked list_persisted_activity_panes_bounded -- --nocapture
cargo test -p runtime --locked restore_workspace_pane -- --nocapture
cargo test -p deppy-sijo --locked cross_workspace -- --test-threads=1
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo test -p deppy-sijo --locked -- --test-threads=1
git diff --check
```

- [ ] **Step 3: Measure bounded resource behavior**

Use the existing render benchmark harness on the same machine and record:

```text
baseline idle RSS and repaint rate
six-pane idle RSS and repaint rate
peak concurrent cold materializations
RSS after detaching all panes and waiting through normal eviction grace
```

The evidence passes only if idle remains event-driven, materialization peak is
one, and detached runtimes become evictable. Do not infer memory reclamation
from static review.

- [ ] **Step 4: Update mockup and handoff**

Remove command-palette/header-picker scenarios from the mockup. Show session-row
drag, hover `Open beside`, right-click, append-right multi-pane layout, foreign
header reorder, horizontal overflow, and the 1–6 setting. Record exact commands
and real results in the summary and handoff.

- [ ] **Step 5: Commit final evidence**

```bash
git add docs/CODEX_HANDOFF.md docs/mockups/cross-workspace-pane-entry-scenarios.html docs/build/PR-MWP-summary.md
git commit -m "docs: record multi-pane delivery evidence"
```
