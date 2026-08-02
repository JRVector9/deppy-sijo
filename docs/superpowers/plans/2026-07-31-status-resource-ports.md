# Status Resource and Port Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use the repository TDD rules plus `/Users/jr/.agents/skills/ak/SKILL.md`. The orchestrator uses the `gogo` workflow: disjoint write sets, direct Codex review, and no production change before an observed failing test.

**Goal:** Add lightweight Orca-inspired resource and port managers while simplifying workspace rows and the status bar without adding idle polling or unsafe process termination.

**Architecture:** Reuse the existing bounded `ActivitySnapshot` and runtime two-second resource events. Add append-only runtime inspect/kill commands for provably unattached local sessions, a lazy single-flight app port worker, and pure egui popovers that return typed intents to the App controller. Destructive operations are revalidated by the owning runtime or port backend immediately before execution.

**Tech Stack:** Rust 2024 workspace, egui/eframe 0.35, `runtime` command/event protocol, app-owned `LazyBoundedWorker`, macOS `lsof`/`ps`, libc `SIGTERM`, egui_kittest, Cargo tests and Clippy.

---

## File Map and Ownership

Initial parallel lanes are deliberately disjoint.

- Lane A owns only `crates/app/src/ui/file_tree.rs`.
- Lane B owns only `crates/runtime/src/command.rs`, `crates/runtime/src/event.rs`, and `crates/runtime/src/in_process.rs`.
- Lane C owns only new `crates/app/src/port_inventory.rs` and `crates/app/src/main.rs`.

After those lanes pass direct review, one serial UI lane owns:

- new `crates/app/src/ui/resource_manager.rs`,
- new `crates/app/src/ui/ports.rs`,
- `crates/app/src/ui/activity.rs`,
- `crates/app/src/ui/agent_terminal.rs`,
- `crates/app/src/ui/mod.rs`.

The orchestrator alone owns final integration:

- `crates/app/src/app.rs`,
- the five `crates/i18n/locales/*/messages.txt` bundles,
- `docs/CODEX_HANDOFF.md`.

No implementation worker edits `app.rs`, locale bundles, or the handoff.

## Task 1: Workspace Row Presentation

**Files:**
- Modify: `crates/app/src/ui/file_tree.rs:3660-4040`
- Test: `crates/app/src/ui/file_tree.rs:6184-6235`

- [ ] **Step 1: Change the avatar regression to the requested geometry**

Rename the current test and change only the expected size:

```rust
#[test]
fn designall_워크스페이스_아바타는_18px이고_기존좌측선에_고정된다() {
    let row = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 29.19));
    let avatar = workspace_avatar_rect(row);

    assert!((avatar.width() - 18.0).abs() < 0.01);
    assert!((avatar.height() - 18.0).abs() < 0.01);
    assert!((avatar.left() - 9.8).abs() < 0.01);
    assert!((avatar.center().y - row.center().y).abs() < 0.01);
}
```

- [ ] **Step 2: Add a rendered-row regression proving the count is gone but the state dot remains**

Use egui kittest or a painter shape inspection around the real `workspace_row` renderer. Construct a summary with seven sessions and assert that no text node/galley equals `"7"`, while one circle with `WORKSPACE_STATUS_DOT_DIAMETER / 2.0` remains in the summary slot.

```rust
#[test]
fn 워크스페이스_행은_상태점만_그리고_세션수는_그리지않는다() {
    let summary = SidebarSessionSummary {
        running: 7,
        no_sessions: false,
        ..SidebarSessionSummary::default()
    };
    let frame = render_workspace_row_for_test(summary, 220.0);
    assert!(!frame.texts.iter().any(|text| text == "7"));
    assert_eq!(frame.status_dot_count, 1);
}
```

The helper must invoke the production row renderer; it must not reimplement the badge logic in the test.

- [ ] **Step 3: Run the two tests and observe RED**

Run:

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo 워크스페이스_아바타는_18px --locked -- --nocapture --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo 워크스페이스_행은_상태점만 --locked -- --nocapture --test-threads=1
```

Expected: the first fails with actual 15px; the second fails because the painter still emits `7`.

- [ ] **Step 4: Implement the minimal presentation change**

Change the avatar constant to 18px. Replace the count badge with a fixed-width dot-only helper:

```rust
const WORKSPACE_AVATAR_SIZE: f32 = 18.0;
const WORKSPACE_STATUS_DOT_DIAMETER: f32 = 6.0;
const WORKSPACE_STATUS_SLOT_WIDTH: f32 = WORKSPACE_STATUS_DOT_DIAMETER;

fn workspace_status_dot_width() -> f32 {
    WORKSPACE_STATUS_SLOT_WIDTH
}

fn paint_workspace_status_dot(
    ui: &egui::Ui,
    right: f32,
    center_y: f32,
    color: egui::Color32,
) {
    ui.painter().circle_filled(
        egui::pos2(right - WORKSPACE_STATUS_DOT_DIAMETER / 2.0, center_y),
        WORKSPACE_STATUS_DOT_DIAMETER / 2.0,
        color,
    );
}
```

Remove `workspace_total_sessions`, the count-slot constants, and count text painting. Keep all state-color priority functions and summary data unchanged.

- [ ] **Step 5: Run the focused FileTree tests GREEN**

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo ui::file_tree::tests --locked -- --test-threads=1
```

Expected: all FileTree tests pass.

- [ ] **Step 6: Commit the lane**

```bash
git add crates/app/src/ui/file_tree.rs
git commit -m "feat(ui): simplify workspace status rows"
```

## Task 2: Runtime-Owned Unattached Session Inspection

**Files:**
- Modify: `crates/runtime/src/command.rs:762-970`
- Modify: `crates/runtime/src/event.rs:100-210`
- Modify: `crates/runtime/src/in_process.rs:1868-1905`
- Test: inline unit tests in all three files

- [ ] **Step 1: Add append-only protocol order tests first**

Extend the source-locked expected arrays with new variants at the end only:

```rust
"InspectUnattachedSessions",
"KillUnattachedSessions",
```

and events:

```rust
"UnattachedSessionsInspected",
"UnattachedSessionsKilled",
```

Add retention tests proving both commands/events retain no user-controlled heap payload.

- [ ] **Step 2: Add behavior tests against the real worker state**

Construct worker fixtures using the existing in-process test helpers. Cover:

```rust
#[test]
fn inspect_unattached_excludes_mux_and_remote_viewed_sessions() {
    let mut harness = UnattachedHarness::new();
    let attached = harness.spawn_attached();
    let remote = harness.spawn_unattached();
    harness.set_remote_viewing(remote, true);
    let orphan = harness.spawn_unattached();

    assert_eq!(harness.inspect_unattached(), vec![orphan]);
    assert!(harness.session_exists(attached));
    assert!(harness.session_exists(remote));
}

#[test]
fn kill_unattached_recomputes_after_a_session_becomes_attached() {
    let mut harness = UnattachedHarness::new();
    let candidate = harness.spawn_unattached();
    assert_eq!(harness.inspect_unattached(), vec![candidate]);
    harness.attach_new_pane(candidate);

    assert_eq!(harness.kill_unattached(), 0);
    assert!(harness.session_exists(candidate));
}

#[test]
fn kill_unattached_removes_only_current_local_candidates() {
    let mut harness = UnattachedHarness::new();
    let attached = harness.spawn_attached();
    let orphan = harness.spawn_unattached();

    assert_eq!(harness.kill_unattached(), 1);
    assert!(harness.session_exists(attached));
    assert!(!harness.session_exists(orphan));
}
```

`UnattachedHarness` is a test-only wrapper around the existing real worker fixture. Its methods must
send real `RuntimeCommand` values and drain real `RuntimeEvent` values; it may not call the candidate
helper directly.

The revalidation test must inspect, then attach one candidate before kill, and prove the newly attached session survives.

- [ ] **Step 3: Run focused tests and observe RED**

```bash
CARGO_BUILD_JOBS=2 cargo test -p runtime unattached --locked -- --nocapture --test-threads=1
```

Expected: compilation fails because the append-only variants and worker handler do not exist.

- [ ] **Step 4: Append the command and event variants**

Append to `RuntimeCommand`:

```rust
InspectUnattachedSessions,
KillUnattachedSessions,
```

Append to `RuntimeEvent`:

```rust
UnattachedSessionsInspected { count: u16 },
UnattachedSessionsKilled { count: u16 },
```

Update `Debug`, validation, retained-byte accounting, coalescing/source locks, and remote protocol order tests without reordering existing variants.

- [ ] **Step 5: Extract one session-termination helper**

Move the existing `KillSession` body into a helper that performs final drain, process-group cleanup through `remove_session`, cache/lease/detector cleanup, persistence exit, pane detach, and optional mux emission:

```rust
fn kill_session_owned(&mut self, session: SessionId, emit_mux: bool) -> bool {
    if !self.sessions.contains_key(&session) {
        return false;
    }
    self.final_drain(session);
    self.remove_session(session);
    self.exited_order.retain(|candidate| *candidate != session);
    self.hidden_scrollback.remove(&session);
    self.remote_viewing.remove(&session);
    self.detectors.remove(&session);
    self.status_overrides.remove(&session);
    self.close_session_log(session, "killed", None);
    if let Some(pipe) = &mut self.persist {
        pipe.session_exited(session);
    }
    for pane in self.mux.panes.values_mut() {
        if pane.session_id == Some(session) {
            pane.session_id = None;
        }
    }
    if emit_mux {
        self.emit_mux_and_watched();
    }
    true
}
```

The final implementation may adjust borrow order but must preserve every cleanup edge in the original body.

- [ ] **Step 6: Implement bounded candidate recomputation**

Session ownership is capped at `RUNTIME_SESSION_CAP`, so collect at most that many IDs:

```rust
fn unattached_session_ids(&self) -> Vec<SessionId> {
    let attached = self
        .mux
        .panes
        .values()
        .filter_map(|pane| pane.session_id)
        .collect::<std::collections::HashSet<_>>();
    self.sessions
        .keys()
        .copied()
        .filter(|session| {
            !attached.contains(session) && !self.remote_viewing.contains_key(session)
        })
        .take(crate::command::RUNTIME_SESSION_CAP)
        .collect()
}
```

Before using this exact helper, verify from the real restore path that no live session can exist outside mux ownership while waiting for a same-worker restore transition. If such a state exists, add that exact runtime-owned identifier set to the exclusion predicate and a regression test.

- [ ] **Step 7: Handle inspect and kill with execution-time recomputation**

```rust
RuntimeCommand::InspectUnattachedSessions => {
    let count = u16::try_from(self.unattached_session_ids().len()).unwrap_or(u16::MAX);
    self.emit(RuntimeEvent::UnattachedSessionsInspected { count });
}
RuntimeCommand::KillUnattachedSessions => {
    let candidates = self.unattached_session_ids();
    let mut killed = 0_u16;
    for session in candidates {
        killed = killed.saturating_add(u16::from(self.kill_session_owned(session, false)));
    }
    if killed > 0 {
        self.emit_mux_and_watched();
        crate::signal_memory_released();
    }
    self.emit(RuntimeEvent::UnattachedSessionsKilled { count: killed });
}
```

Do not accept UI-supplied session IDs for the bulk action.

- [ ] **Step 8: Run runtime tests GREEN and commit**

```bash
CARGO_BUILD_JOBS=2 cargo test -p runtime unattached --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p runtime --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo clippy -p runtime --all-targets --locked -- -D warnings
git add crates/runtime/src/command.rs crates/runtime/src/event.rs crates/runtime/src/in_process.rs
git commit -m "feat(runtime): manage unattached local sessions"
```

Expected: all runtime tests and strict Clippy pass.

## Task 3: Lazy Bounded Port Inventory and Control

**Files:**
- Create: `crates/app/src/port_inventory.rs`
- Modify: `crates/app/src/main.rs:1-40`
- Test: inline tests in `crates/app/src/port_inventory.rs`

- [ ] **Step 1: Declare the module and write parser RED tests**

Add `mod port_inventory;` to `main.rs`. In the new file define tests before production types:

```rust
#[test]
fn lsof_field_parser_groups_pid_command_and_ipv4_ipv6_listeners() {
    let input = b"p41\ncworkerd\nn*:3000\nn127.0.0.1:8443\np42\ncnode\nn[::1]:9229\n";
    let rows = parse_lsof_fields(input).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!((rows[0].pid, rows[0].port), (41, 3000));
    assert_eq!((rows[1].pid, rows[1].bind.as_ref()), (41, "127.0.0.1"));
    assert_eq!((rows[2].pid, rows[2].bind.as_ref()), (42, "::1"));
}

#[test]
fn listener_admission_accepts_200_and_rejects_201_without_partial_result() {
    let exact = listener_fixture(200);
    assert_eq!(parse_lsof_fields(&exact).unwrap().len(), 200);
    let plus_one = listener_fixture(201);
    assert_eq!(parse_lsof_fields(&plus_one), Err(PortErrorCode::TooManyRows));
}

#[test]
fn longest_workspace_root_owns_nested_listener() {
    let roots = roots_fixture([("root", "/tmp/project"), ("nested", "/tmp/project/apps/api")]);
    let owner = assign_workspace(Path::new("/tmp/project/apps/api/src"), &roots).unwrap();
    assert_eq!(owner.id.as_ref(), "nested");
}
```

Define `listener_fixture` and `roots_fixture` as bounded test-only builders in the same module; both
must reject requests above the production caps instead of allocating arbitrary test sizes.

Fixtures must include `*:3000`, `127.0.0.1:8443`, and `[::1]:9229`.

- [ ] **Step 2: Add command lifecycle and termination RED tests**

Use a fake backend/runner to prove:

```rust
#[test]
fn scanner_accepts_exact_2mib_and_rejects_plus_one() {
    let exact = FakeCommandRunner::stdout(vec![b'x'; PORT_OUTPUT_MAX_BYTES]);
    assert!(run_lsof_with(&exact).is_ok());
    let plus_one = FakeCommandRunner::stdout(vec![b'x'; PORT_OUTPUT_MAX_BYTES + 1]);
    assert_eq!(run_lsof_with(&plus_one), Err(PortErrorCode::OutputTooLarge));
}

#[test]
fn timeout_kills_process_group_and_joins_readers() {
    let before = test_reader_thread_count();
    let result = run_test_hanging_command(Duration::from_millis(100));
    assert_eq!(result, Err(PortErrorCode::Timeout));
    assert!(wait_until_process_group_is_gone(Duration::from_secs(1)));
    assert_eq!(test_reader_thread_count(), before);
}

#[test]
fn terminate_rejects_when_fresh_pid_port_or_workspace_differs() {
    let mut backend = FakePortBackend::with_scan(row_fixture("ws-a", 77, 3000));
    backend.replace_next_scan(row_fixture("ws-b", 77, 3000));
    let target = termination_fixture("ws-a", 77, 3000);
    assert_eq!(backend.terminate(target), Err(PortErrorCode::OwnershipChanged));
    assert_eq!(backend.signals(), &[]);
}

#[test]
fn protected_and_external_listeners_never_receive_sigterm() {
    for ownership in [PortOwnership::Protected, PortOwnership::External] {
        let mut backend = FakePortBackend::with_owned_state(ownership);
        assert_eq!(
            backend.terminate(termination_fixture("ws-a", 77, 3000)),
            Err(PortErrorCode::NotTerminable)
        );
        assert_eq!(backend.signals(), &[]);
    }
}
```

The fake types are test-only implementations of the same command/rescan/signal traits used by the
production backend, so tests exercise production admission and revalidation logic.

- [ ] **Step 3: Run the module tests and observe RED**

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo port_inventory::tests --locked -- --nocapture --test-threads=1
```

Expected: compilation fails on missing DTOs/parser/backend.

- [ ] **Step 4: Add bounded DTOs with redacted Debug**

Define fixed ceilings and immutable result types:

```rust
pub(crate) const PORT_ROW_MAX: usize = 200;
const PORT_OUTPUT_MAX_BYTES: usize = 2 * 1024 * 1024;
const PORT_COMMAND_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PortWorkspaceRoot {
    pub id: Arc<str>,
    pub name: Arc<str>,
    pub path: Arc<Path>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PortOwnership {
    Workspace,
    External,
    Protected,
    Ambiguous,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PortRow {
    pub pid: u32,
    pub port: u16,
    pub bind: Arc<str>,
    pub protocol: PortProtocol,
    pub process: Arc<str>,
    pub workspace_id: Option<Arc<str>>,
    pub workspace_name: Option<Arc<str>>,
    pub ownership: PortOwnership,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PortSnapshot {
    pub generation: u64,
    pub sampled_at_ms: u64,
    pub rows: Arc<[PortRow]>,
}
```

Debug implementations expose counts/status only, never raw paths or complete command lines.

- [ ] **Step 5: Implement the lazy worker job contract**

Use the existing inert `LazyBoundedWorker`:

```rust
pub(crate) enum PortJob {
    Scan {
        generation: u64,
        roots: Arc<[PortWorkspaceRoot]>,
    },
    Terminate {
        generation: u64,
        roots: Arc<[PortWorkspaceRoot]>,
        target: PortTerminationTarget,
    },
}

pub(crate) enum PortOutcome {
    Scanned(Result<PortSnapshot, PortErrorCode>),
    Terminated(Result<PortSnapshot, PortErrorCode>),
}

pub(crate) fn worker(
    wake: impl Fn() + Send + Sync + 'static,
) -> LazyBoundedWorker<PortJob, PortOutcome> {
    LazyBoundedWorker::new(
        "port-inventory",
        Duration::from_secs(30),
        || move |job| execute_job(job),
        wake,
    )
}
```

Construction must perform no I/O and spawn no thread.

- [ ] **Step 6: Implement bounded macOS collection**

Launch `lsof` in a fresh process group, read stdout/stderr on joined bounded readers, enforce 4 seconds and 2 MiB, then parse `-F pcn` records. Point lookups for cwd/command use the same bounded runner and only for admitted PIDs. Retain at most 200 final rows.

Use cwd containment first and bounded command-path evidence second. When multiple roots match, choose the longest canonical lexical root. Failed or ambiguous ownership is read-only external/ambiguous.

- [ ] **Step 7: Implement backend revalidation before SIGTERM**

`PortTerminationTarget` carries the stable tuple only:

```rust
pub(crate) struct PortTerminationTarget {
    pub workspace_id: Arc<str>,
    pub pid: u32,
    pub port: u16,
    pub bind: Arc<str>,
    pub protocol: PortProtocol,
}
```

The backend performs a fresh scan, requires one exact row with the same workspace/PID/port/bind/protocol and `PortOwnership::Workspace`, rejects the current Deppy PID and protected agent owners, then calls `libc::kill(pid, libc::SIGTERM)`. It never sends `SIGKILL`. Return a new scan outcome after termination.

- [ ] **Step 8: Run focused tests GREEN and commit**

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo port_inventory::tests --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo clippy -p deppy-sijo --bin deppy-sijo --locked -- -D warnings
git add crates/app/src/main.rs crates/app/src/port_inventory.rs
git commit -m "feat(app): add bounded local port inventory"
```

Expected: parser, worker, timeout/reaping, and revalidation tests pass.

## Task 4: Resource and Port Popover UI

**Dependency:** Tasks 1-3 are integrated and reviewed first.

**Files:**
- Create: `crates/app/src/ui/resource_manager.rs`
- Create: `crates/app/src/ui/ports.rs`
- Modify: `crates/app/src/ui/activity.rs:15-204`
- Modify: `crates/app/src/ui/agent_terminal.rs:55-238`
- Modify: `crates/app/src/ui/mod.rs`
- Test: inline egui kittests in the owned UI files

- [ ] **Step 1: Extend activity identity with RED retained-budget tests**

Add stable identifiers required for typed actions:

```rust
pub struct ActivityWorkspaceRow {
    pub workspace_id: Arc<str>,
    // existing fields unchanged
}

pub struct ActivitySessionRow {
    pub session: Option<runtime::SessionId>,
    // existing fields unchanged
}
```

Update the byte-budget test first so `workspace_id` allocation/bytes are counted. Add exact-bound and plus-one tests for identifier text.

- [ ] **Step 2: Add pure resource manager RED kittests**

Test real rendering and typed intents:

```rust
#[test]
fn resource_manager_renders_app_workspace_session_and_unavailable_remote_rows() {
    let mut harness = resource_manager_harness(resource_snapshot_fixture());
    harness.run();
    harness.get_by_label("Workspace A");
    harness.get_by_label("Local Session");
    harness.get_by_label("Remote Session");
    harness.get_by_label("—");
}

#[test]
fn resource_manager_refresh_and_kill_buttons_emit_typed_intents_only() {
    let mut harness = resource_manager_harness(resource_snapshot_fixture());
    harness.get_by_label("Refresh").click();
    assert_eq!(harness.state().last_intent, Some(ResourceManagerIntent::Refresh));
    harness.get_by_label("Review unattached sessions").click();
    assert_eq!(harness.state().last_intent, Some(ResourceManagerIntent::InspectUnattached));
}

#[test]
fn closed_resource_manager_emits_no_intent_and_requests_no_host_work() {
    let mut ui = ResourceManagerUi::default();
    let mut host_requests = Vec::new();
    let intent = render_closed_resource_manager(&mut ui, &mut host_requests);
    assert_eq!(intent, None);
    assert!(host_requests.is_empty());
}
```

The harness helpers render the production `ResourceManagerUi`; they do not reproduce its row logic.

Use a snapshot containing one local session and one `resource: None` remote session. Assert the remote row displays `—`.

- [ ] **Step 3: Add pure ports UI RED kittests**

```rust
#[test]
fn ports_manager_groups_active_other_and_external_rows() {
    let mut harness = ports_manager_harness(port_snapshot_fixture());
    harness.run();
    harness.get_by_label("Active workspace");
    harness.get_by_label("Other workspaces");
    harness.get_by_label("External");
}

#[test]
fn ports_manager_exposes_terminate_only_for_owned_workspace_rows() {
    let mut harness = ports_manager_harness(port_snapshot_fixture());
    harness.run();
    assert_eq!(harness.query_all_by_label("Terminate").count(), 1);
}

#[test]
fn unopened_ports_manager_emits_no_scan_intent() {
    let mut ui = PortsUi::default();
    assert_eq!(render_closed_ports_manager(&mut ui), None);
}
```

- [ ] **Step 4: Add status bar RED kittest**

Render the real status bar and assert:

- no `터미널`/`Terminal` view label,
- one accessible resource button containing CPU and memory summary,
- one accessible `포트 —` button before the first scan,
- keyboard activation returns the same intent as pointer activation.

```rust
#[test]
fn status_bar_has_resource_and_port_actions_without_terminal_label() {
    let mut harness = status_bar_harness(None);
    harness.run();
    assert!(harness.query_by_label("Terminal").is_none());
    harness.get_by_label_contains("CPU");
    harness.get_by_label("Ports —");
}
```

`status_bar_harness` invokes the production `AgentTerminalUi::status_bar` with an empty bounded
activity snapshot and no port snapshot.

- [ ] **Step 5: Run UI tests and observe RED**

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo resource_manager --locked -- --nocapture --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo ports_manager --locked -- --nocapture --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo status_bar_has_resource --locked -- --nocapture --test-threads=1
```

Expected: compilation fails on missing modules/types or tests fail because labels remain static.

- [ ] **Step 6: Implement typed UI state and intents**

Resource UI:

```rust
pub(crate) enum ResourceManagerIntent {
    Refresh,
    InspectUnattached,
    KillUnattached { workspace_id: Arc<str> },
    FocusSession { workspace_id: Arc<str>, session: runtime::SessionId },
    KillSession { workspace_id: Arc<str>, session: runtime::SessionId },
}

pub(crate) struct ResourceManagerUi {
    open: bool,
    expanded: std::collections::BTreeSet<Arc<str>>,
    confirm: Option<ResourceConfirm>,
}
```

Port UI:

```rust
pub(crate) enum PortsIntent {
    Refresh,
    Terminate(crate::port_inventory::PortTerminationTarget),
    OpenAddress(Arc<str>),
    CopyAddress(Arc<str>),
}

pub(crate) struct PortsUi {
    open: bool,
    confirm: Option<crate::port_inventory::PortTerminationTarget>,
}
```

Both render bounded scroll areas and only retain identifiers/small disclosure state.

- [ ] **Step 7: Convert status bar labels to accessible buttons**

Add UI fields to `AgentTerminalUi`, initialize them in `new`, and return one typed status action:

```rust
pub(crate) enum StatusBarIntent {
    Resource(ResourceManagerIntent),
    Ports(PortsIntent),
}

pub fn status_bar(
    &mut self,
    ui: &mut egui::Ui,
    claude_usage: Option<(u8, u8)>,
    codex_usage: Option<(u8, u8)>,
    rows: &[ActivityWorkspaceRow],
    waiting: usize,
    mcp_count: usize,
    feed: &StatusFeedSnapshot,
    ports: Option<&PortSnapshot>,
    orphan_counts: &std::collections::HashMap<String, u16>,
    catalog: &i18n::Catalog,
) -> Option<StatusBarIntent>
```

Remove the view label and its separator. Paint the CPU/memory segment and port segment as buttons with `WidgetInfo::labeled`, hover text, and keyboard activation. Opening either popover must not itself execute host work; opening Ports emits one `PortsIntent::Refresh` only when there is no prior snapshot or the user explicitly refreshes.

- [ ] **Step 8: Run all owned UI tests GREEN and commit**

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo ui::activity::tests --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo resource_manager --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo ports_manager --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo agent_terminal::tests --locked -- --test-threads=1
git add crates/app/src/ui/activity.rs crates/app/src/ui/agent_terminal.rs crates/app/src/ui/resource_manager.rs crates/app/src/ui/ports.rs crates/app/src/ui/mod.rs
git commit -m "feat(ui): add resource and port managers"
```

## Task 5: App Controller Integration and Localization

**Files:**
- Modify: `crates/app/src/app.rs:7000-7120`
- Modify: `crates/app/src/app.rs:9740-9850`
- Modify: `crates/app/src/app.rs:16400-16620`
- Modify: `crates/app/src/app.rs:19028-19066`
- Modify: `crates/i18n/locales/en-US/messages.txt`
- Modify: `crates/i18n/locales/ja-JP/messages.txt`
- Modify: `crates/i18n/locales/ko-KR/messages.txt`
- Modify: `crates/i18n/locales/zh-Hans/messages.txt`
- Modify: `crates/i18n/locales/zh-Hant/messages.txt`
- Test: inline App tests and i18n package tests

- [ ] **Step 1: Add App RED tests for snapshot identities and status intents**

Add tests proving:

- `activity_rows()` projects exact workspace/session IDs into the bounded snapshot,
- resource refresh clears only `activity_rows_cache`,
- inspect/kill unattached commands go only to local runtime owners and are staged in a bounded sequence,
- port worker remains unspawned before first refresh,
- repeated port refresh while one job is outstanding creates no second worker/job,
- stale port outcomes are ignored by generation,
- termination results refresh the cached list.

- [ ] **Step 2: Run App tests and observe RED**

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo status_resource_port_app --locked -- --nocapture --test-threads=1
```

Expected: compilation fails on missing App fields/dispatch methods.

- [ ] **Step 3: Add bounded App-owned state**

Add fields:

```rust
port_worker: crate::lazy_worker::LazyBoundedWorker<
    crate::port_inventory::PortJob,
    crate::port_inventory::PortOutcome,
>,
port_snapshot: Option<crate::port_inventory::PortSnapshot>,
port_generation: u64,
pending_port_job: Option<crate::port_inventory::PortJob>,
unattached_counts: std::collections::HashMap<String, u16>,
pending_resource_maintenance: std::collections::VecDeque<ResourceMaintenanceTarget>,
```

Enforce caps derived from `MAX_ACTIVITY_WORKSPACES`; do not retain raw port command output or process command lines.

- [ ] **Step 4: Project stable activity identities**

Populate `workspace_id` and `session` in every active/warm/idle construction path. Update retained-byte accounting and all test fixtures. Do not synthesize IDs from display names.

- [ ] **Step 5: Dispatch status intents outside render closures**

Capture one returned `StatusBarIntent` from the panel closure and dispatch after the closure releases UI borrows:

```rust
let mut status_intent = None;
egui::Panel::bottom("agent_terminal_status_bar").show(ui, |ui| {
    status_intent = self.agent_terminal_ui.status_bar(
        ui,
        claude_usage_snapshot().or_else(|| crate::claude_usage::current(ui.ctx())),
        self.agent_sessions_ui.codex_usage(),
        activity_rows.rows(),
        waiting_count,
        mcp_count,
        &self.status_feed,
        self.port_snapshot.as_ref(),
        &self.unattached_counts,
        &text,
    );
});
self.dispatch_status_bar_intent(status_intent, ui.ctx());
```

Dispatch rules:

- resource refresh: `activity_rows_cache = None`, request immediate repaint,
- inspect/kill: queue at most one command per local runtime and admit one per logic tick,
- focus/kill one session: resolve exact workspace/runtime/session and reuse existing controller paths,
- port refresh/terminate: submit through the lazy worker; keep one known-unsent replacement only,
- open/copy address: use existing app-host/clipboard boundaries, never the UI module.

SSH workspaces do not receive local inspect/kill or port-scan commands. Their resource and port rows
remain visible as unavailable (`—`) until a separate remote capability is implemented.

- [ ] **Step 6: Consume runtime maintenance events**

When draining each runtime event, update its workspace count from `UnattachedSessionsInspected`; on `UnattachedSessionsKilled`, refresh that count and clear the activity cache. Prune counts when workspaces close/delete.

- [ ] **Step 7: Add complete locale keys**

Add equivalent keys to all five bundles:

```text
status_bar.resources
status_bar.resources_hint
status_bar.ports
status_bar.ports_unknown
resource_manager.title
resource_manager.refresh
resource_manager.last_sample
resource_manager.remote_unavailable
resource_manager.inspect_unattached
resource_manager.kill_unattached
resource_manager.kill_session
resource_manager.confirm_title
resource_manager.confirm_body
ports.title
ports.refresh
ports.active_workspace
ports.other_workspaces
ports.external
ports.copy_address
ports.open_address
ports.terminate
ports.confirm_title
ports.confirm_body
ports.scan_failed
ports.ownership_changed
```

Korean labels should use `리소스 관리자`, `포트`, `고아 세션 검토`, and `소유권이 변경되어 종료하지 않았습니다`.

- [ ] **Step 8: Run App and i18n tests GREEN and commit**

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo status_resource_port_app --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p i18n --locked -- --test-threads=1
git add crates/app/src/app.rs crates/i18n/locales/*/messages.txt
git commit -m "feat(app): integrate resource and port controls"
```

## Task 6: Direct Review, Corrections, and Full Gates

**Files:**
- Review: every file changed by Tasks 1-5
- Modify only files identified by a reliable review finding
- Update: `docs/CODEX_HANDOFF.md`

- [ ] **Step 1: Run direct Codex reviews in parallel by owned diff**

The orchestrator runs `codex exec` directly, not through another agent wrapper:

```bash
codex exec -m gpt-5.5 -c model_reasoning_effort="high" -s read-only \
  --skip-git-repo-check -C /private/tmp/deppy-sf06-integration \
  "Review workspace-row diff for correctness and UI regressions. End with CONCLUSION: OK or CONCLUSION: 문제있음." \
  </dev/null > /tmp/status-row-review.txt 2>&1
```

Run equivalent direct reviews for runtime safety, port lifecycle/security, and App/UI integration. Reliable findings return to the original implementation lane for at most two correction rounds.

- [ ] **Step 2: Run focused suites serially**

```bash
CARGO_BUILD_JOBS=2 cargo test -p runtime unattached --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo ui::file_tree::tests --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo port_inventory::tests --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo resource_manager --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo ports_manager --locked -- --test-threads=1
```

Expected: all selected tests pass and none report zero selected tests.

- [ ] **Step 3: Run full tests and static gates**

```bash
CARGO_BUILD_JOBS=2 cargo test -p runtime --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo clippy -p runtime --all-targets --locked -- -D warnings
CARGO_BUILD_JOBS=2 cargo clippy -p deppy-sijo --bin deppy-sijo --locked -- -D warnings
cargo fmt --all --check
git diff --check
```

Do not report hardware/release ignored tests as passes.

- [ ] **Step 4: Package and verify the signed app**

```bash
DEPPY_SIGN_IDENTITY='Developer ID Application: VectorNine INC (ZDTU5LS35K)' \
  CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 sh scripts/package-macos.sh
codesign --verify --deep --strict --verbose=2 'target/bundle/Deppy Sijo.app'
unzip -tq 'target/bundle/Deppy Sijo.zip'
```

- [ ] **Step 5: Relaunch and record physical checks**

Terminate only the exact old bundle executable, launch the rebuilt bundle with `open -n`, and verify the new PID remains alive after two seconds.

Physical checklist:

- workspace avatars are 18px and no session numbers remain,
- status bar has no `터미널` text,
- resource segment opens the resource manager,
- resource refresh does not cause visible burst/stall,
- remote rows display unavailable,
- unattached review and confirmation work without closing attached sessions,
- `포트 —` becomes the cached count after first open,
- port rows group correctly and ownership-change rejection is visible,
- owned local listener termination refreshes the list.

- [ ] **Step 6: Update handoff and final report**

Record objective, commits, modified files, review outcomes, exact commands/results, failed approaches, build path/PID, remaining manual evidence, and next commands in `docs/CODEX_HANDOFF.md`.
