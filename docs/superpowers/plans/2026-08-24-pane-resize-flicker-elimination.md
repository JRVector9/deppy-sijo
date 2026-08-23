# Pane Split/Resize Flicker Elimination — Implementation Plan

> **Execution rule:** follow RED → GREEN → focused regression → commit for every production task. Do not send pane-drag resize commands from an egui sizing/discarded pass.

**Goal:** Keep terminal content visually stable while a pane divider moves, issue exactly one final PTY resize after the matching mux layout acknowledgement, and suppress transient blank snapshots during both final reflow and new split startup.

**Approved behavior:** A — the divider follows the pointer immediately. Existing terminal cells remain unchanged during the drag and are clipped on shrink or surrounded by the stable pane background on grow. Reflow happens once after release. Existing terminal contents and scrollback are never cleared or stretched.

**Architecture:** `WorkspaceUi` owns a tab/path-scoped split transaction. While it is active or awaiting acknowledgement, visible panes do not enqueue `RuntimeCommand::Resize`. A matching `MuxUpdated` ends the preview and arms one final resize for visible sessions. Each affected `SessionView` keeps its last stable snapshot while target-size viewports settle. A newly split shell similarly withholds its output-free seed viewport for a bounded interval.

**Scope:** `crates/app/src/ui/workspace.rs`, the final-pass flush call in `crates/app/src/app.rs`, this plan, and `docs/CODEX_HANDOFF.md`. Runtime/session/wire formats remain unchanged. OS window-border live resize remains outside this pane-divider task.

---

## Task 1: Make divider resize an acknowledged transaction

**Files:**

- Modify: `crates/app/src/ui/workspace.rs`
- Modify: `crates/app/src/app.rs`
- Test: `crates/app/src/ui/workspace.rs` (`#[cfg(test)] mod tests`)
- Test: `crates/app/src/app.rs` (`#[cfg(test)] mod tests`)

### Step 1: Add RED transaction tests

Add focused tests using the existing `mux`, `tab`, `pane`, `drain_protocol`, and `egui::Context` fixtures:

1. `pane_drag_stable_state_does_not_enqueue_resize_until_matching_ack`
   - Seed `sent_sizes` for two visible sessions and leave `pending_resize_target` empty (the real stable-state precondition the old debounce test missed).
   - Start a root-path horizontal split transaction and offer several desired grid sizes.
   - Assert zero `Resize` intents while active and while committed-but-unacknowledged.
   - Apply an unrelated/old `MuxUpdated`; assert preview and suppression remain.
   - Apply the same tab/path/final ratio; assert the transaction clears and both visible sessions are armed for final sizing.

2. `committed_split_preview_survives_until_matching_mux_ack`
   - Commit one `ResizeSplit` and drain it.
   - Assert render ratio lookup still returns the final preview before ACK.
   - Assert an ACK for another tab/path or old ratio does not clear it.
   - Assert the matching ACK clears it.

3. `late_first_ack_does_not_cancel_a_new_active_drag`
   - Create a second `Active` transaction before applying the first command's late ACK.
   - Assert the second tab/path/ratio remains active.

4. `discarded_or_sizing_pass_does_not_admit_split_protocol`
   - Stage a candidate with `cumulative_pass_nr()` and exercise the flush gate: sizing does not stage; a discarded or different pass does not admit; the matching final pass admits once.

5. `app_flushes_workspace_render_effects_after_the_last_widget`
   - Source-contract test the host boundary: active and warm workspace flush calls occur after the agent launcher (the final UI-producing widget) and before frame statistics finish.

Run:

```bash
cargo test -p deppy-sijo --locked ui::workspace::tests::pane_drag_stable_state_does_not_enqueue_resize_until_matching_ack -- --exact --test-threads=1
cargo test -p deppy-sijo --locked ui::workspace::tests::committed_split_preview_survives_until_matching_mux_ack -- --exact --test-threads=1
cargo test -p deppy-sijo --locked ui::workspace::tests::late_first_ack_does_not_cancel_a_new_active_drag -- --exact --test-threads=1
cargo test -p deppy-sijo --locked ui::workspace::tests::discarded_or_sizing_pass_does_not_admit_split_protocol -- --exact --test-threads=1
cargo test -p deppy-sijo --locked app_flushes_workspace_render_effects_after_the_last_widget -- --exact --test-threads=1
```

Expected RED: tests fail to compile because the transaction state/helpers do not exist, or fail because current `split_drag.take()` removes the preview and the first stable mismatch sends `Resize` immediately.

### Step 2: Implement the minimal transaction

Replace `Option<(Vec<u8>, f32)>` with a bounded state carrying `tab`, `path`, `ratio`, and phase:

```rust
enum SplitDragPhase {
    Active,
    Committed { admitted: bool },
}

struct SplitDragTransaction {
    tab: runtime::MuxTabId,
    path: Vec<u8>,
    ratio: f32,
    phase: SplitDragPhase,
}
```

Required behavior:

- Starting/updating a drag clears stale pending resize targets and records the current tab/path.
- `drag_stopped` changes the transaction to `Committed`; it does not remove the preview.
- `try_admit_committed_split` uses `send_keep_selection` and flips `admitted` only on successful queue admission. If the queue is full, retain the transaction and retry only on a naturally occurring final UI pass.
- `split_preview_ratio(tab, path)` applies a preview only to the exact split.
- `MuxUpdated` clears only an admitted committed transaction whose tab/path ratio matches within a small float tolerance. A new active transaction is never cleared by an older ACK.
- Matching ACK records the new snapshot's visible sessions in `split_final_resize_sessions` and removes their stale debounce targets.
- `render_pane` skips all automatic resizing while a transaction is Active or Committed. After ACK, each armed visible session stages its desired final size once.
- `render_pane` and `split_handle` stage candidates tagged with `ui.ctx().cumulative_pass_nr()` only when `!ui.is_sizing_pass()`; they do not enqueue protocol or mutate the resize debounce directly.
- Add `WorkspaceUi::flush_render_side_effects(ctx)`. It executes candidates only when their pass tag equals `ctx.cumulative_pass_nr()` and `!ctx.will_discard()`. A discarded pass retains final arms/transactions for the correction pass.
- Call that flush for the active workspace and rendered warm/attached workspaces at the end of `App::ui`, after the final UI-producing widget. Checking `will_discard()` inside `workspace.rs` during rendering is insufficient because a later widget can request discard.

Do not add a wire revision, runtime layout generation, or texture scaling.

### Step 3: Verify GREEN and regressions

Run the four focused tests above, then:

```bash
cargo test -p deppy-sijo --locked ui::workspace::tests -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

### Step 4: Commit

```bash
git add crates/app/src/ui/workspace.rs crates/app/src/app.rs docs/CODEX_HANDOFF.md
git commit -m "fix(app): transact pane divider resize"
```

---

## Task 2: Fence transient viewports during the single final reflow

**Files:**

- Modify: `crates/app/src/ui/workspace.rs`
- Test: `crates/app/src/ui/workspace.rs`

### Step 1: Add RED presentation tests

Add deterministic tests that pass explicit `Instant` values; do not sleep:

1. `final_resize_keeps_stable_snapshot_until_target_viewport_settles`
   - Give a session a nonblank 80×24 stable snapshot.
   - Arm a 100×30 resize fence.
   - Feed non-target and target snapshots.
   - Assert the visible snapshot pointer/generation stays stable before the 32ms quiet window and promotes only the latest target afterward.

2. `blank_target_viewport_waits_for_nonblank_or_hard_deadline`
   - Start with nonblank content, feed an all-whitespace target snapshot, advance beyond 32ms but below 250ms, and assert no promotion.
   - Feed a nonblank target and assert promotion after its quiet window.
   - In a second case, assert an unavoidable blank promotes at the 250ms hard deadline.

3. `final_resize_clears_selection_only_for_changed_grid`
   - If desired size equals the sent size, selection remains and no fence is armed.
   - If size changes and the resize intent is admitted, selection and frozen pending snapshot are cleared and a fence is armed.

4. `one_matching_ack_produces_at_most_one_distinct_resize_per_session`
   - Arm two sessions through one matching ACK, offer their final dimensions twice, and assert one `Resize` per distinct session/size.

Run:

```bash
cargo test -p deppy-sijo --locked ui::workspace::tests::final_resize_keeps_stable_snapshot_until_target_viewport_settles -- --exact --test-threads=1
cargo test -p deppy-sijo --locked ui::workspace::tests::blank_target_viewport_waits_for_nonblank_or_hard_deadline -- --exact --test-threads=1
cargo test -p deppy-sijo --locked ui::workspace::tests::final_resize_clears_selection_only_for_changed_grid -- --exact --test-threads=1
cargo test -p deppy-sijo --locked ui::workspace::tests::one_matching_ack_produces_at_most_one_distinct_resize_per_session -- --exact --test-threads=1
```

Expected RED: missing fence state/helpers, immediate viewport promotion, or selection retained across an actual grid change.

### Step 2: Implement the bounded presentation fence

Add constants:

```rust
const RESIZE_VIEWPORT_QUIET: Duration = Duration::from_millis(32);
const RESIZE_VIEWPORT_HARD_DEADLINE: Duration = Duration::from_millis(250);
```

Add a `ResizePresentationFence` to `SessionView` containing the target `(cols, rows)`, start/last-target times, latest target snapshot, and whether the stable snapshot had visible text.

Required behavior:

- Make `queue_terminal_resize` report whether a distinct resize was actually admitted.
- Only an admitted, changed final resize arms the fence and clears that session's coordinate-based selection/pending frozen snapshot.
- While fenced, non-target viewports never replace the visible stable snapshot. Target viewports replace only the buffered candidate.
- Promote the newest target after 32ms without a newer target.
- If the stable snapshot contains text and the candidate is entirely whitespace, keep the stable snapshot until a nonblank candidate settles or 250ms elapses.
- At 250ms promote the latest target if present; otherwise release the fence while retaining the old stable snapshot.
- Increment `snapshot_gen` and update `summary` exactly once at promotion. Request only the next quiet/deadline repaint; do not create an idle repaint loop.

Use a small `snapshot_has_visible_text` helper based on existing `cell_has_content` semantics. Preserve scrollback, `scroll_offset`, `is_alt_screen`, and the intentional focus accent.

### Step 3: Verify GREEN and regressions

Run the four focused tests, then:

```bash
cargo test -p deppy-sijo --locked ui::workspace::tests -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

### Step 4: Commit

```bash
git add crates/app/src/ui/workspace.rs docs/CODEX_HANDOFF.md
git commit -m "fix(app): fence terminal resize snapshots"
```

---

## Task 3: Hide the new split shell's blank seed and close pass-boundary gaps

**Files:**

- Modify: `crates/app/src/ui/workspace.rs`
- Test: `crates/app/src/ui/workspace.rs`

### Step 1: Add RED startup tests

1. `split_shell_blank_seed_is_withheld_until_first_nonblank_viewport`
   - Apply `MuxUpdated` for a split, then `ShellSpawned` for the new visible session, then a blank viewport.
   - Assert the session remains in connecting/stable presentation state.
   - Feed a nonblank viewport before 250ms and assert immediate promotion.

2. `split_shell_blank_seed_is_bounded_by_250ms`
   - With no nonblank output, advance to the deadline and assert the latest blank seed is promoted so the pane cannot remain stuck forever.

3. `ordinary_existing_session_blank_output_is_not_misclassified_as_seed`
   - A blank viewport without the causal `ShellSpawned` split marker follows the normal path.

4. `discarded_pass_does_not_consume_final_resize_arm`
   - Offering final dimensions from a sizing/discarded pass leaves the per-session final arm intact; the following final pass sends it once.

Run:

```bash
cargo test -p deppy-sijo --locked ui::workspace::tests::split_shell_blank_seed_is_withheld_until_first_nonblank_viewport -- --exact --test-threads=1
cargo test -p deppy-sijo --locked ui::workspace::tests::split_shell_blank_seed_is_bounded_by_250ms -- --exact --test-threads=1
cargo test -p deppy-sijo --locked ui::workspace::tests::ordinary_existing_session_blank_output_is_not_misclassified_as_seed -- --exact --test-threads=1
cargo test -p deppy-sijo --locked ui::workspace::tests::discarded_pass_does_not_consume_final_resize_arm -- --exact --test-threads=1
```

Expected RED: `ShellSpawned` currently only resolves the cwd queue, every visible viewport promotes immediately, and the final-size arm/pass gate does not exist.

### Step 2: Implement the startup fence

- On `ShellSpawned`, mark the session only when it belongs to a visible tab that now has a split layout (two or more panes). Do not infer from blank content alone.
- Add an `InitialSnapshotFence` to `SessionView` with `started_at` and latest blank candidate.
- A nonblank viewport clears the initial fence and promotes immediately.
- A blank viewport is buffered until a nonblank viewport or the 250ms deadline.
- At the deadline, promote the latest blank candidate once; if none exists, release the fence while retaining the current display.
- Arm the same one-shot deadline from `apply_warm_events` when a split shell is observed, but continue to discard warm viewport payloads. If that workspace becomes active within 250ms, replay must not expose its seed blank; reapplying `ShellSpawned` must not extend the original deadline.
- Hidden-session cleanup and session removal clear both presentation fences naturally through existing `sessions.retain`/cache lifecycle.
- Never extend the seed deadline when a later blank candidate arrives. Cursor visibility alone, background colour, or SGR attributes do not make a seed nonblank.

### Step 3: Verify GREEN and all pane regressions

Run the four exact tests, then:

```bash
cargo test -p deppy-sijo --locked ui::workspace::tests -- --test-threads=1
cargo test -p mux --locked -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

### Step 4: Commit

```bash
git add crates/app/src/ui/workspace.rs docs/CODEX_HANDOFF.md
git commit -m "fix(app): suppress split seed blank frames"
```

---

## Task 4: Measure, review, rebuild, and relaunch

**Files:**

- Modify: `docs/CODEX_HANDOFF.md`
- Optionally modify production/tests only for concrete review findings, with a new RED first

### Step 1: Run complete gates without parallel Cargo jobs

```bash
cargo fmt --all
cargo test -p deppy-sijo --locked ui::workspace::tests -- --test-threads=1
cargo test -p mux --locked -- --test-threads=1
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo run -p xtask --locked -- i18n-check
cargo run -p xtask --locked -- check-boundary
cargo fmt --all -- --check
git diff --check
```

Record exact counts and exit codes in `docs/CODEX_HANDOFF.md`. Do not claim a pass for any command not executed.

### Step 2: Independent review

- Run three disjoint read-only subagent reviews: transaction/ACK, viewport fence/lifecycle, tests/performance.
- Run Codex CLI review at high reasoning. Try the requested model first; if the account rejects it, record the failure and use the available `gpt-5.6-sol` fallback:

```bash
codex review -m gpt-5.6 -c model_reasoning_effort=high --uncommitted
codex review -m gpt-5.6-sol -c model_reasoning_effort=high --uncommitted
```

- Root reviews the complete cumulative diff against `c2b85b0` for state leaks, duplicate resize admission, unbounded repaint, blank false positives, selection corruption, and protocol-queue retry behavior.
- Fix only reliable findings. Every production fix starts with a RED regression test.

### Step 3: Commit final evidence

Update `docs/CODEX_HANDOFF.md` with objective, completed work, modified files, decisions, exact tests/results, failed approaches, remaining work, and exact next commands.

```bash
git add docs/CODEX_HANDOFF.md
git commit -m "docs: record pane flicker verification"
```

### Step 4: Rebuild, sign, relaunch, and verify the exact executable

Build the requested debug application and helper, then use the repository launch script so signing matches the existing workflow:

```bash
cargo build -p deppy-sijo -p mcp-proxy --locked
scripts/dev-run.sh
```

Before terminating anything, resolve the existing Deppy PID and exact executable path. Send `TERM` only to that verified process, wait boundedly for exit, launch the newly built executable, then record new PID, executable, and signing evidence. Never kill by broad process-name match.

### Step 5: Runtime smoke and final report

- In both horizontal and vertical splits, drag continuously, pause while held, release, and repeat quickly.
- Verify the divider follows the pointer, existing text never blanks, release does not snap back, and each affected pane settles once.
- Verify a newly split pane shows connecting/stable content until its first prompt instead of an output-free blank seed.
- Report automated evidence separately from manual/visual evidence. Do not push unless the user explicitly asks.
