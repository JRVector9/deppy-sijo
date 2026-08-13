# First Session Activation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore the persisted terminal selected immediately after app launch and transfer keyboard focus to it without a second click.

**Architecture:** AgentState Catalog completion becomes the authoritative one-shot startup restore trigger instead of consulting an empty async projection during `App::new`. Persisted session clicks retain their exact pane identity; a primary-pane dotenv continuation sends `RestoreWorkspacePane`, `FocusPane`, then `RestoreWorkspace`, while an App-owned runtime-fenced pending focus waits for the first real terminal surface. Existing runtime commands, dotenv verification, warm limits, and archived-agent restoration remain unchanged.

**Tech Stack:** Rust 2024, eframe/egui 0.35, existing in-process runtime and lazy dotenv worker, egui_kittest, Cargo.

---

## File map and isolation

- `crates/app/src/ui/file_tree.rs`: emit an exact persisted-session activation action.
- `crates/app/src/ui/workspace.rs`: expose the existing one-shot pane focus mechanism to App.
- `crates/app/src/app.rs`: own Catalog restore admission, exact-pane activation, ordered dotenv continuation, and runtime-fenced focus.
- `docs/CODEX_HANDOFF.md`: record actual RED/GREEN evidence and remaining work.

`app.rs` and `workspace.rs` already contain unrelated user work. Make and commit production changes in a temporary worktree created from the current `HEAD`, then cherry-pick the isolated commit onto the dirty main worktree. Do not stage, discard, or rewrite the existing changes.

### Task 1: Preserve persisted pane identity at the sidebar boundary

**Files:**
- Modify: `crates/app/src/ui/file_tree.rs:286-310`
- Modify: `crates/app/src/ui/file_tree.rs:4437-4447`
- Test: `crates/app/src/ui/file_tree.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn persisted_session_row_click_activates_its_exact_pane() {
    let target = SessionRowTarget::persisted(
        "workspace-b",
        runtime::MuxPaneId("pane-exact".to_owned()),
    );
    assert!(matches!(
        session_row_activation(&target),
        SidebarAction::ActivatePersistedSession { workspace_id, pane }
            if workspace_id == "workspace-b" && pane.0 == "pane-exact"
    ));
}
```

- [ ] **Step 2: Run RED**

Run `cargo test -p deppy-sijo --locked persisted_session_row_click_activates_its_exact_pane -- --nocapture`.

Expected: compilation fails because the action and helper do not exist.

- [ ] **Step 3: Implement the minimal typed mapping**

```rust
pub enum SidebarAction {
    SwitchWorkspace(String),
    ActivatePersistedSession {
        workspace_id: String,
        pane: runtime::MuxPaneId,
    },
    // existing variants remain
}

fn session_row_activation(target: &SessionRowTarget) -> SidebarAction {
    match target {
        SessionRowTarget::Live { workspace_id, tab, pane, .. } => {
            SidebarAction::FocusSession {
                workspace_id: workspace_id.clone(),
                tab: tab.clone(),
                pane: pane.clone(),
            }
        }
        SessionRowTarget::PersistedPane { workspace_id, pane } => {
            SidebarAction::ActivatePersistedSession {
                workspace_id: workspace_id.clone(),
                pane: pane.clone(),
            }
        }
    }
}
```

Use the helper for inactive session-row clicks. Workspace-header clicks remain `SwitchWorkspace`.

- [ ] **Step 4: Run GREEN**

Run the focused test, then `cargo test -p deppy-sijo --locked ui::file_tree::tests:: -- --nocapture`.

Expected: both commands pass.

### Task 2: Drive one-shot startup restore from Catalog completion

**Files:**
- Modify: `crates/app/src/app.rs:6431-6480`
- Modify: `crates/app/src/app.rs:10547-10566`
- Modify: `crates/app/src/app.rs:11430-11472`
- Modify: `crates/app/src/app.rs:15263-15277`
- Test: `crates/app/src/app.rs`

- [ ] **Step 1: Write failing decision and wiring tests**

```rust
#[test]
fn catalog_restore_stages_only_when_needed() {
    assert!(should_stage_catalog_restore(true, false));
    assert!(!should_stage_catalog_restore(false, false));
    assert!(!should_stage_catalog_restore(true, true));
}

#[test]
fn startup_restore_is_driven_after_catalog_apply() {
    let source = include_str!("app.rs");
    let constructor_tail = source
        .split_once("app.sync_agent_hooks();").unwrap().1
        .split_once("// 시작 시 config가 remote").unwrap().0;
    assert!(!constructor_tail.contains("persisted_activity_panes"));
    let catalog_arm = source
        .split_once("AgentStateSection::Catalog =>").unwrap().1
        .split_once("AgentStateSection::ResumeProbe").unwrap().0;
    assert!(catalog_arm.contains("ensure_active_runtime_restore"));
}
```

- [ ] **Step 2: Run RED**

Run each new test by its exact substring. Expected: the helper is missing and the current constructor still contains the invalid early projection check.

- [ ] **Step 3: Implement runtime-lifetime admission**

Add `restore_staged: bool` to `WorkspaceRuntime`, initialized to `false` in `make_runtime`, and add:

```rust
fn should_stage_catalog_restore(has_panes: bool, restore_staged: bool) -> bool {
    has_panes && !restore_staged
}

fn ensure_active_runtime_restore(&mut self) {
    let has_panes = self.persisted_activity_panes
        .get(&self.active.id)
        .is_some_and(|panes| !panes.is_empty());
    if should_stage_catalog_restore(has_panes, self.active.restore_staged) {
        self.stage_runtime_restore(self.active.runtime_instance);
    } else if !has_panes && self.bench.is_none() && self.perf_harness_next.is_none() {
        self.offer_agent_launcher_for_active();
    }
}
```

Make `stage_runtime_restore` return `bool` and mark the exact runtime only after dotenv continuation admission succeeds. Call `ensure_active_runtime_restore()` after Catalog replaces `persisted_activity_panes`. Remove the constructor's early check and launcher branch.

- [ ] **Step 4: Run GREEN**

Run both focused tests. Expected: both pass.

### Task 3: Restore the selected pane first

**Files:**
- Modify: `crates/app/src/app.rs:1311-1365`
- Modify: `crates/app/src/app.rs:15263-15670`
- Test: `crates/app/src/app.rs`

- [ ] **Step 1: Write the failing command-order test**

```rust
#[test]
fn primary_persisted_pane_activation_restores_focuses_then_finishes_workspace() {
    let pane = runtime::MuxPaneId("pane-exact".to_owned());
    let mut commands = Vec::new();
    assert!(send_primary_pane_activation(pane.clone(), |command| {
        commands.push(command);
        true
    }));
    assert!(matches!(commands.as_slice(), [
        runtime::RuntimeCommand::RestoreWorkspacePane { pane: restored },
        runtime::RuntimeCommand::FocusPane { pane: focused },
        runtime::RuntimeCommand::RestoreWorkspace,
    ] if restored == &pane && focused == &pane));
}
```

Also assert that failure of the first send prevents later sends.

- [ ] **Step 2: Run RED**

Run `cargo test -p deppy-sijo --locked primary_persisted_pane_activation_restores_focuses_then_finishes_workspace -- --nocapture`.

Expected: compilation fails because the helper does not exist.

- [ ] **Step 3: Implement ordered delivery and a private continuation**

```rust
fn send_primary_pane_activation(
    pane: runtime::MuxPaneId,
    mut send: impl FnMut(runtime::RuntimeCommand) -> bool,
) -> bool {
    send(runtime::RuntimeCommand::RestoreWorkspacePane { pane: pane.clone() })
        && send(runtime::RuntimeCommand::FocusPane { pane })
        && send(runtime::RuntimeCommand::RestoreWorkspace)
}
```

Add `PendingDotenvContinuation::PrimaryPaneActivation { command }`, where `command` is the retained `RestoreWorkspacePane`. Reuse the existing retention preflight and dotenv result validation. Keep durable barriers limited to the cross-workspace continuation; after env/cache policy delivery, the new continuation invokes the helper. Add `stage_primary_pane_activation(runtime_instance, pane) -> bool` and mark the exact runtime as restore-staged after admission.

- [ ] **Step 4: Run GREEN**

Run the new ordering test, `cross_workspace_app_restore_workspace_pane_requires_dotenv`, and the app dotenv-focused tests. Expected: all pass.

### Task 4: Route activation and retain focus until the terminal exists

**Files:**
- Modify: `crates/app/src/ui/workspace.rs:1580-1600`
- Modify: `crates/app/src/app.rs:7570-7580`
- Modify: `crates/app/src/app.rs:8048-8070`
- Modify: `crates/app/src/app.rs:12780-13050`
- Modify: `crates/app/src/app.rs:13959-14135`
- Modify: `crates/app/src/app.rs:20955-21105`
- Test: `crates/app/src/ui/workspace.rs`
- Test: `crates/app/src/app.rs`

- [ ] **Step 1: Write failing focus and identity tests**

```rust
#[test]
fn app_armed_terminal_focus_waits_for_exact_pane_surface() {
    let mut workspace = WorkspaceUi::new();
    let pane = pane_id("pane-exact");
    workspace.arm_terminal_focus(pane.clone());
    assert_eq!(workspace.pending_focus, Some(pane));
    assert!(!workspace.take_terminal_focus_claimed());
}

#[test]
fn persisted_activation_requires_current_catalog_identity() {
    let rows = vec![storage::PersistedActivityPane {
        workspace_id: "workspace-b".to_owned(),
        pane_id: "pane-exact".to_owned(),
        title: "Saved".to_owned(),
        cwd: "/tmp/project".to_owned(),
    }];
    assert!(persisted_pane_is_current(
        &rows,
        &runtime::MuxPaneId("pane-exact".to_owned())
    ));
    assert!(!persisted_pane_is_current(
        &[],
        &runtime::MuxPaneId("pane-exact".to_owned())
    ));
}
```

- [ ] **Step 2: Run RED**

Run both tests by substring. Expected: the APIs do not exist.

- [ ] **Step 3: Implement exact activation and runtime fencing**

Expose the existing WorkspaceUi mechanism:

```rust
pub(crate) fn arm_terminal_focus(&mut self, pane: runtime::MuxPaneId) {
    self.begin_terminal_refocus(pane);
}
```

Add matching `ActivatePersistedSession { workspace_id, pane }` to `WorkspaceControllerAction`, route the sidebar action to it, and add:

```rust
pending_pane_focus: Option<(String, u64, runtime::MuxPaneId)>,
```

`activate_persisted_session` must:

1. Verify the pane remains in `persisted_activity_panes[workspace_id]`.
2. Switch a different workspace through a preferred-pane variant of `switch_workspace`; header switching passes `None`.
3. Fence pending focus with the active runtime instance and arm WorkspaceUi focus.
4. Send `FocusPane` immediately if already materialized.
5. Otherwise stage exact-pane-first activation when restore is not already staged; if full restore is already in flight, retain pending focus until that exact pane appears.

Extend the existing pending-focus poll to discard stale workspace/runtime identities, wait for the exact pane to gain a session, arm native focus, send `FocusPane` if needed, and then clear the intent.

- [ ] **Step 4: Run GREEN**

Run the two new tests plus `pending_focus` and `workspace_focus` test filters. Expected: all pass.

### Task 5: Verify, commit, and integrate without touching user work

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`
- Verify: the three production files above

- [ ] **Step 1: Run proportional gates in the isolated worktree**

```bash
cargo test -p deppy-sijo --locked ui::file_tree::tests:: -- --nocapture
cargo test -p deppy-sijo --locked ui::workspace::tests:: -- --nocapture
cargo test -p deppy-sijo --locked primary_persisted_pane_activation -- --nocapture
cargo test -p deppy-sijo --locked startup_restore -- --nocapture
cargo check -p deppy-sijo --all-targets --locked
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Expected: every command exits 0. Record any actual failure and do not claim it passed.

- [ ] **Step 2: Review exact scope**

```bash
git diff -- crates/app/src/app.rs crates/app/src/ui/file_tree.rs crates/app/src/ui/workspace.rs
rg -n "ActivatePersistedSession|PrimaryPaneActivation|restore_staged|pending_pane_focus|ensure_active_runtime_restore|arm_terminal_focus" crates/app/src
```

Confirm there is no runtime wire variant, input buffering, eager all-workspace prewarming, dotenv bypass, launcher redesign, or archived-agent auto-respawn.

- [ ] **Step 3: Commit isolated implementation**

```bash
git add crates/app/src/app.rs crates/app/src/ui/file_tree.rs crates/app/src/ui/workspace.rs
git commit -m "fix(app): activate persisted sessions without startup stall"
```

- [ ] **Step 4: Cherry-pick and verify the dirty main worktree**

```bash
git cherry-pick <implementation-commit>
git status --short --branch
git diff -- crates/app/src/agent_detect.rs crates/app/src/agent_detect_worker.rs crates/app/src/agent_transcript.rs crates/app/src/app.rs crates/app/src/ui/workspace.rs
```

Expected: the isolated commit lands, while prior current-work-display changes remain uncommitted.

- [ ] **Step 5: Update handoff and run final cheap checks**

Record exact RED/GREEN results and remaining manual verification in `docs/CODEX_HANDOFF.md`, then run `cargo fmt --all -- --check`, `git diff --check`, and `git status --short --branch`.

Do not package, rebuild the app bundle, terminate the running bundle, or relaunch it unless the user explicitly requests that next.
