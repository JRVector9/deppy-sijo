# Terminal Document Drop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finder 또는 Deppy 파일 트리에서 로컬 터미널 본문으로 드롭한 모든 bounded UTF-8 파일을 순서대로 문서 탭으로 열고, PTY 경로 붙여넣기를 제거하며, 비동기 다중 로드 뒤에도 문서 retained bytes를 24 MiB 이하로 유지한다.

**Architecture:** `WorkspaceUi`는 기존 pane hit-test와 typed DnD release를 재사용해 실제 드롭 프레임에만 `Vec<PathBuf>` intent와 정확한 local pane focus claim을 반환한다. App은 primary workspace 출력에서 경로를 모아 포커스 전환 intent를 먼저 반영한 뒤 기존 `open_document`를 순서대로 호출한다. 파일 내용은 기존 bounded document worker만 읽고, 결과를 `source`/`saved_source`로 보유하기 직전에 순수 admission helper로 24 MiB cap을 다시 강제한다.

**Tech Stack:** Rust 2024 edition, egui 0.35 raw/typed drag-and-drop, egui_kittest, 기존 `LazyBoundedWorker`, Cargo test/Clippy/rustfmt, macOS Developer ID packaging.

---

## File map

- Modify: `crates/app/src/ui/workspace.rs`
  - `WorkspaceSurfaceOutput`/`PaneRenderOutput`의 transient document-drop intent
  - local terminal OS/typed path drop routing
  - split-pane target/focus and no-PTY regressions
- Modify: `crates/app/src/app.rs`
  - primary surface output의 ordered document-open dispatch
  - post-load retained-byte admission planning and application
  - routing/cap regressions
- Modify: `docs/CODEX_HANDOFF.md`
  - RED/GREEN commands, review/build/relaunch evidence, remaining physical checks
- Reference only: `crates/app/src/document_io.rs`
  - extension-agnostic regular UTF-8 loader and 1 MiB/8 MiB tier behavior remain unchanged
- Reference only: `crates/app/src/ui/file_tree.rs`
  - double-click allowlist and non-terminal drop behavior remain unchanged

### Task 1: Carry transient document-drop paths through Workspace output

**Files:**
- Modify: `crates/app/src/ui/workspace.rs:1534-1597`
- Modify: `crates/app/src/ui/workspace.rs:4729-4753`
- Test: `crates/app/src/ui/workspace.rs` test module near the existing `PaneRenderOutput`/drop tests

- [x] **Step 1: Write the failing output-merge test**

Add this test before production fields exist:

```rust
#[test]
fn pane_render_output_merge는_document_drop_경로_순서를_보존한다() {
    let mut merged = PaneRenderOutput {
        document_drop_paths: vec![PathBuf::from("/tmp/first.rs")],
        ..Default::default()
    };
    merged.merge(PaneRenderOutput {
        document_drop_paths: vec![
            PathBuf::from("/tmp/second.json"),
            PathBuf::from("/tmp/third.yaml"),
        ],
        ..Default::default()
    });

    assert_eq!(
        merged.document_drop_paths,
        vec![
            PathBuf::from("/tmp/first.rs"),
            PathBuf::from("/tmp/second.json"),
            PathBuf::from("/tmp/third.yaml"),
        ]
    );
}
```

- [x] **Step 2: Run the focused test and verify RED**

Run:

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo pane_render_output_merge는_document_drop_경로_순서를_보존한다 --locked -- --test-threads=1
```

Expected: exit 101 with a compile error that `PaneRenderOutput` has no field named `document_drop_paths`. Confirm exactly one source test matches the substring after compilation succeeds; a zero-test run does not count.

- [x] **Step 3: Add the minimal output fields and merge**

Add the public one-frame intent to `WorkspaceSurfaceOutput`:

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceSurfaceOutput {
    pub focus_requested: bool,
    pub local_focus_claimed: Option<runtime::MuxPaneId>,
    pub document_drop_paths: Vec<std::path::PathBuf>,
    // existing aux fields stay unchanged
}
```

Add the matching private field and ordered append:

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PaneRenderOutput {
    focus_requested: bool,
    local_focus_claimed: Option<runtime::MuxPaneId>,
    document_drop_paths: Vec<std::path::PathBuf>,
    // existing aux fields stay unchanged
}

impl PaneRenderOutput {
    fn merge(&mut self, other: Self) {
        self.focus_requested |= other.focus_requested;
        if other.local_focus_claimed.is_some() {
            self.local_focus_claimed = other.local_focus_claimed;
        }
        self.document_drop_paths.extend(other.document_drop_paths);
        // existing aux merge stays unchanged
    }
}
```

Propagate the paths at the existing `show_with_input` return boundary:

```rust
WorkspaceSurfaceOutput {
    focus_requested: pane_output.focus_requested,
    local_focus_claimed: pane_output.local_focus_claimed,
    document_drop_paths: pane_output.document_drop_paths,
    aux_tab_intent: pane_output.aux_tab_intent,
    aux_body_rect: pane_output.aux_body_rect,
    aux_search_toggle_requested: pane_output.aux_search_toggle_requested,
}
```

Early/default/session-less returns remain empty through `Default`; attached output types are not changed.

- [x] **Step 4: Run the focused test and Workspace compile regression**

Run:

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo pane_render_output_merge는_document_drop_경로_순서를_보존한다 --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo correction_round_input_disabled_missing --locked -- --test-threads=1
```

Expected: the merge test passes 1/1; both input-disabled surface tests pass and return an empty drop vector by default.

- [x] **Step 5: Commit Task 1**

```bash
git add crates/app/src/ui/workspace.rs
git commit -m "feat(app): carry terminal document drop intents"
```

### Task 2: Replace terminal file-path paste with ordered document intents

**Files:**
- Modify: `crates/app/src/ui/workspace.rs:5763-5825`
- Modify: `crates/app/src/ui/workspace.rs:6282-6304`
- Test: `crates/app/src/ui/workspace.rs:15294-15416` and adjacent DnD tests

- [x] **Step 1: Add a drop-output harness without changing the IME harness**

Add a dedicated helper so existing IME tests keep `Harness<WorkspaceUi>`:

```rust
fn setup_focused_local_pane_drop_harness(
    session: SessionId,
) -> egui_kittest::Harness<'static, (WorkspaceUi, WorkspaceSurfaceOutput)> {
    let catalog = catalog();
    let config = TerminalConfig::default();
    let target_pane = pane_id("pane");
    let mut workspace = WorkspaceUi::new();
    workspace.mux = Some(mux(
        "primary",
        vec![tab(
            "primary",
            vec![pane("pane", session)],
            LayoutNode::Pane(target_pane.clone()),
        )],
        "pane",
    ));
    workspace.last_focused_pane = Some(target_pane.clone());
    workspace.pending_focus = Some(target_pane);
    workspace.sessions.entry(session).or_default().snapshot = Some(snapshot("ready"));
    let mut harness = egui_kittest::Harness::new_ui_state(
        move |ui, state: &mut (WorkspaceUi, WorkspaceSurfaceOutput)| {
            let frame = state.0.show_with_input(ui, &config, &[], &catalog, true);
            state.1.document_drop_paths.extend(frame.document_drop_paths);
            if frame.local_focus_claimed.is_some() {
                state.1.local_focus_claimed = frame.local_focus_claimed;
            }
        },
        (workspace, WorkspaceSurfaceOutput::default()),
    );
    harness.run();
    drain_protocol(&mut harness.state_mut().0);
    harness
}
```

- [x] **Step 2: Rewrite the OS drop regression as RED for multiple arbitrary extensions**

Replace the old “경로를 붙여넣는다” expectation with:

```rust
#[test]
fn kittest_터미널_pane_위_os_드롭은_모든_경로를_문서_intent로_보낸다() {
    let session = SessionId(7);
    let mut harness = setup_focused_local_pane_drop_harness(session);
    let dropped = [
        PathBuf::from("/x/main.rs"),
        PathBuf::from("/x/config.json"),
        PathBuf::from("/x/settings.toml"),
        PathBuf::from("/x/deploy.yaml"),
    ];
    let pane_point = egui::pos2(80.0, TERMINAL_PANE_HEADER_HEIGHT + 40.0);
    harness
        .input_mut()
        .events
        .push(egui::Event::PointerMoved(pane_point));
    harness.input_mut().dropped_files.extend(dropped.iter().cloned().map(|path| {
        egui::DroppedFile {
            path: Some(path),
            ..Default::default()
        }
    }));
    harness.run();

    assert_eq!(harness.state().1.document_drop_paths, dropped.to_vec());
    assert!(written_bytes(drain_protocol(&mut harness.state_mut().0)).is_empty());
}
```

- [x] **Step 3: Run the OS drop test and verify behavioral RED**

Run:

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo kittest_터미널_pane_위_os_드롭은_모든_경로를_문서_intent로_보낸다 --locked -- --test-threads=1
```

Expected: exit 101; `document_drop_paths` is empty and/or `written_bytes` contains escaped paths because production still sends `RuntimeCommand::WriteInput`.

- [x] **Step 4: Add RED coverage for outside-pane, split target/focus, and typed `PathBuf`**

Update the outside-pane test to use the drop harness and assert both channels stay empty:

```rust
assert!(harness.state().1.document_drop_paths.is_empty());
assert!(written_bytes(drain_protocol(&mut harness.state_mut().0)).is_empty());
```

Change the split test state to `(WorkspaceUi, WorkspaceSurfaceOutput)`, start with the left pane focused, and accumulate the real frame output with this harness closure:

```rust
let mut harness = egui_kittest::Harness::builder()
    .with_size(egui::vec2(600.0, 400.0))
    .build_ui_state(
        move |ui, state: &mut (WorkspaceUi, WorkspaceSurfaceOutput)| {
            let frame = state.0.show_with_input(ui, &config, &[], &catalog, true);
            state.1.document_drop_paths.extend(frame.document_drop_paths);
            if frame.local_focus_claimed.is_some() {
                state.1.local_focus_claimed = frame.local_focus_claimed;
            }
        },
        (workspace, WorkspaceSurfaceOutput::default()),
    );
```

Use a point inside the right pane while the left pane remains the initial focus:

```rust
let right_point = egui::pos2(500.0, TERMINAL_PANE_HEADER_HEIGHT + 40.0);
harness
    .input_mut()
    .events
    .push(egui::Event::PointerMoved(right_point));
harness.input_mut().dropped_files.push(egui::DroppedFile {
    path: Some(PathBuf::from("/x/right-pane.rs")),
    ..Default::default()
});
harness.run();
```

Then assert:

```rust
assert_eq!(
    harness.state().1.document_drop_paths,
    vec![PathBuf::from("/x/right-pane.rs")]
);
assert_eq!(
    harness.state().1.local_focus_claimed,
    Some(pane_id("right"))
);
assert!(written_bytes(drain_protocol(&mut harness.state_mut().0)).is_empty());
```

Add an internal typed-path test using the same drop harness. After the initial frame, place a `PathBuf` payload, release over the terminal body, and assert a single path intent and no PTY write:

```rust
#[test]
fn kittest_파일트리_PathBuf_드롭은_문서_intent이고_pty_write가_아니다() {
    let session = SessionId(7);
    let mut harness = setup_focused_local_pane_drop_harness(session);
    let point = egui::pos2(80.0, TERMINAL_PANE_HEADER_HEIGHT + 40.0);
    harness.hover_at(point);
    harness.drag_at(point);
    harness.run();
    egui::DragAndDrop::set_payload(&harness.ctx, PathBuf::from("/x/lib.rs"));
    harness.event(egui::Event::PointerMoved(point));
    harness.event(egui::Event::PointerButton {
        pos: point,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    harness.run();

    assert_eq!(
        harness.state().1.document_drop_paths,
        vec![PathBuf::from("/x/lib.rs")]
    );
    assert!(written_bytes(drain_protocol(&mut harness.state_mut().0)).is_empty());
}
```

Run all four new/updated tests and confirm they fail on the intended empty-intent/path-write assertions, not on harness setup.

- [x] **Step 5: Implement minimal OS and typed-path routing**

In the pane-background handler, replace path writes with path intents. Preserve text payload writes:

```rust
if let Some(path) = release_typed_dnd_payload::<std::path::PathBuf>(&pane_resp) {
    render_output.document_drop_paths.push(path.as_ref().clone());
    render_output.local_focus_claimed = Some(pane_id.clone());
    if !focused {
        self.request_pane_focus(pane_id.clone());
    }
}
// TerminalTextDragPayload branch remains RuntimeCommand::WriteInput.
if !os_dropped.is_empty() && os_over_pane {
    render_output.document_drop_paths.extend(os_dropped);
    render_output.local_focus_claimed = Some(pane_id.clone());
    if !focused {
        self.request_pane_focus(pane_id.clone());
    }
}
```

In the terminal-renderer response handler, replace only `PathBuf` paste:

```rust
if mode.is_local()
    && input_enabled
    && let Some(path) = release_typed_dnd_payload::<std::path::PathBuf>(&output.response)
{
    render_output.document_drop_paths.push(path.as_ref().clone());
    render_output.local_focus_claimed = Some(pane_id.clone());
    if !focused {
        self.request_pane_focus(pane_id.clone());
    }
}
```

Do not remove `path_insert_paste_bytes` or `paths_insert_paste_bytes`; clipboard path paste still uses them. Do not change `TerminalTextDragPayload` handling or attached mode.

- [x] **Step 6: Verify GREEN and unrelated payload/text preservation**

Run:

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo kittest_터미널_pane_위_os_드롭은_모든_경로를_문서_intent로_보낸다 --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo kittest_pane_밖_os_드롭은_무시된다 --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo kittest_분할된_pane_중_포인터_아래_pane에만_os_드롭이_들어간다 --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo kittest_파일트리_PathBuf_드롭은_문서_intent이고_pty_write가_아니다 --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo typed_drop_release_preserves_unrelated_payload --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo terminal_text_dnd --locked -- --test-threads=1
```

Expected: each focused group passes; arbitrary extensions are not classified in Workspace; unrelated payload and terminal text remain unchanged.

- [x] **Step 7: Commit Task 2**

```bash
git add crates/app/src/ui/workspace.rs
git commit -m "feat(app): route terminal file drops to documents"
```

### Task 3: Dispatch primary Workspace paths through App document opening

**Files:**
- Modify: `crates/app/src/app.rs:27378-27390`
- Modify: `crates/app/src/app.rs:27740-27827`
- Modify: `crates/app/src/app.rs:27876-27899`
- Test: `crates/app/src/app.rs` document test section near `open_document` tests

- [x] **Step 1: Write failing ordered-dispatch and App wiring tests**

Add a pure dispatch test:

```rust
#[test]
fn dispatch_document_drop_paths는_입력_순서를_그대로_보존한다() {
    let paths = vec![
        PathBuf::from("/tmp/a.rs"),
        PathBuf::from("/tmp/b.json"),
        PathBuf::from("/tmp/c.yaml"),
    ];
    let mut opened = Vec::new();

    dispatch_document_drop_paths(paths.clone(), |path| opened.push(path));

    assert_eq!(opened, paths);
}
```

Add a source-wiring test consistent with the existing App test style:

```rust
#[test]
fn app은_primary_workspace_document_drop을_포커스_claim_뒤에_열고_pty로_보내지_않는다() {
    let source = include_str!("app.rs");
    let production = source.split_once("#[cfg(test)]\nmod tests").unwrap().0;
    assert_eq!(
        production
            .matches("dropped_document_paths.extend(primary_output.document_drop_paths);")
            .count(),
        2,
        "cross-workspace strip 유무 두 primary render 경로 모두 수집해야 한다"
    );
    let focus = production
        .find("if let Some(pane) = primary_local_focus_claim")
        .expect("primary focus claim 적용이 있어야 한다");
    let dispatch = production
        .find("dispatch_document_drop_paths(dropped_document_paths")
        .expect("drop dispatch가 있어야 한다");
    assert!(focus < dispatch, "drop 대상 pane focus를 문서 열기보다 먼저 적용해야 한다");
    let dispatch_tail = &production[dispatch..];
    assert!(dispatch_tail.contains("self.open_document(path)"));
    assert!(!dispatch_tail.lines().take(8).any(|line| line.contains("WriteInput")));
}
```

- [x] **Step 2: Run tests and verify RED**

Run:

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo dispatch_document_drop_paths --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo app은_primary_workspace_document_drop을_포커스_claim_뒤에_열고_pty로_보내지_않는다 --locked -- --test-threads=1
```

Expected: exit 101; the first test cannot find `dispatch_document_drop_paths`, and the wiring test cannot find the two output collection sites.

- [x] **Step 3: Add minimal ordered dispatch and collect both primary outputs**

Add the small free helper near the document path helpers:

```rust
fn dispatch_document_drop_paths(
    paths: Vec<PathBuf>,
    mut open: impl FnMut(PathBuf),
) {
    for path in paths {
        open(path);
    }
}
```

At the start of the central render state, add:

```rust
let mut dropped_document_paths = Vec::new();
```

After each of the two primary `show_with_input` calls, add:

```rust
dropped_document_paths.extend(primary_output.document_drop_paths);
```

After the existing `primary_local_focus_claim` block, dispatch:

```rust
dispatch_document_drop_paths(dropped_document_paths, |path| {
    self.open_document(path);
});
```

This ordering preserves the dropped pane's focus intent before the document group becomes active. Do not collect from `PreparedAttachedPaneOutput`.

- [x] **Step 4: Verify GREEN and existing document semantics**

Run:

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo dispatch_document_drop_paths --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo app은_primary_workspace_document_drop을_포커스_claim_뒤에_열고_pty로_보내지_않는다 --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo find_open_document_by_path --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo begin_document_open은_새_문서를_추가만_하고_기존_문서를_건드리지_않는다 --locked -- --test-threads=1
```

Expected: ordered dispatch and both wiring assertions pass; exact duplicate-path activation and append-only multi-document behavior remain green.

- [x] **Step 5: Commit Task 3**

```bash
git add crates/app/src/app.rs
git commit -m "feat(app): open dropped files in document tabs"
```

Execution note: Tasks 1 and 2 share `workspace.rs` and were integrated as one
reviewed commit, `739fea0`; Task 3 is `e3aafb9`. No documentation changes were
staged into either source commit.

### Task 4: Enforce the 24 MiB retained cap when load results arrive

**Files:**
- Modify: `crates/app/src/app.rs:9411-9456`
- Modify: `crates/app/src/app.rs:16608-16643`
- Test: `crates/app/src/app.rs:34444-34714` document state tests

- [x] **Step 1: Write failing prospective-admission tests**

Add three pure tests. The first proves 0-byte loading placeholders are not discarded instead of a useful retained document:

```rust
#[test]
fn plan_document_load_admission은_0byte_loading을_건너뛰고_oldest_clean_loaded를_고른다() {
    let eight_mib = "a".repeat(8 * 1024 * 1024);
    let documents = vec![
        stub_open_document_id(1, "/tmp/loaded.md", &eight_mib, &eight_mib, false),
        stub_open_document_id(2, "/tmp/still-loading.md", "", "", false),
        stub_open_document_id(3, "/tmp/incoming.md", "", "", false),
    ];

    assert_eq!(
        plan_document_load_admission(
            &documents,
            ui::workspace::DocumentTabId(3),
            eight_mib.len() as u64,
            Some(ui::workspace::DocumentTabId(3)),
        ),
        Some(vec![ui::workspace::DocumentTabId(1)])
    );
}
```

The second proves exact-cap admission needs no eviction:

```rust
#[test]
fn plan_document_load_admission은_exact_24mib를_허용한다() {
    let four_mib = "a".repeat(4 * 1024 * 1024);
    let documents = vec![
        stub_open_document_id(1, "/tmp/existing.md", &four_mib, &four_mib, false),
        stub_open_document_id(2, "/tmp/incoming.md", "", "", false),
    ];
    assert_eq!(
        plan_document_load_admission(
            &documents,
            ui::workspace::DocumentTabId(2),
            (8 * 1024 * 1024) as u64,
            Some(ui::workspace::DocumentTabId(2)),
        ),
        Some(Vec::new())
    );
}
```

The remaining decision tests prove missing-result safety, saving/dirty/active
preservation, oldest-first multi-victim selection, exact-cap admission, and
checked overflow. Fixtures that represent retained content must explicitly use
`DocumentLoadState::Loaded`; the default `stub_open_document_id` loading state
is not sufficient evidence for these cases.

The dirty case rejects only the incoming result:

```rust
#[test]
fn plan_document_load_admission은_dirty를_닫지_않고_incoming을_거부한다() {
    let eight_mib = "a".repeat(8 * 1024 * 1024);
    let documents = vec![
        stub_open_document_id(1, "/tmp/dirty.md", &eight_mib, &eight_mib, true),
        stub_open_document_id(2, "/tmp/incoming.md", "", "", false),
    ];
    assert_eq!(
        plan_document_load_admission(
            &documents,
            ui::workspace::DocumentTabId(2),
            eight_mib.len() as u64,
            Some(ui::workspace::DocumentTabId(2)),
        ),
        None
    );
}
```

- [x] **Step 2: Run tests and verify RED**

Run:

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo plan_document_load_admission --locked -- --test-threads=1
```

Expected: exit 101 with missing function `plan_document_load_admission`. The failure must precede any production cap change.

- [x] **Step 3: Implement the minimal pure admission planner**

Extract the existing retained-byte calculation and add the new planner near `plan_document_eviction`:

```rust
fn retained_document_logical_bytes(document: &OpenDocument) -> u64 {
    document.source.len() as u64 + document.saved_source.len() as u64
}

#[derive(Debug, PartialEq, Eq)]
enum DocumentLoadAdmission {
    Missing,
    Admit { evict: Vec<ui::workspace::DocumentTabId> },
    Reject,
}

fn plan_document_load_admission(
    documents: &[OpenDocument],
    incoming_id: ui::workspace::DocumentTabId,
    incoming_source_bytes: u64,
    active_document: Option<ui::workspace::DocumentTabId>,
) -> DocumentLoadAdmission {
    if !documents.iter().any(|document| document.id == incoming_id) {
        return DocumentLoadAdmission::Missing;
    }
    let Some(incoming_retained) = incoming_source_bytes.checked_mul(2) else {
        return DocumentLoadAdmission::Reject;
    };
    let mut remaining: Vec<&OpenDocument> = documents
        .iter()
        .filter(|document| document.id != incoming_id)
        .collect();
    let mut retained = remaining
        .iter()
        .try_fold(incoming_retained, |total, document| {
            total.checked_add(retained_document_logical_bytes(document))
        });
    let Some(mut retained) = retained else {
        return DocumentLoadAdmission::Reject;
    };
    let mut evict = Vec::new();

    while retained > DOCUMENT_TOTAL_RETAINED_BYTES_MAX {
        let position = remaining.iter().position(|document| {
            !document.dirty
                && !document.saving
                && Some(document.id) != active_document
                && retained_document_logical_bytes(document) > 0
        });
        let Some(position) = position else {
            return DocumentLoadAdmission::Reject;
        };
        let victim = remaining.remove(position);
        retained -= retained_document_logical_bytes(victim);
        evict.push(victim.id);
    }
    DocumentLoadAdmission::Admit { evict }
}
```

Use `retained_document_logical_bytes` inside existing `plan_document_eviction`
to keep the two policies consistent, and exclude `saving` there as well. The
24 MiB policy intentionally measures logical UTF-8 content bytes, not allocator
capacity. Do not count the incoming placeholder's empty strings twice; replace
them with the prospective outcome size.

- [x] **Step 4: Verify pure planner GREEN**

Run:

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo plan_document_load_admission --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo plan_document_eviction --locked -- --test-threads=1
```

Expected: all three new admission tests and all existing eviction tests pass. Peak test allocation remains bounded to the explicit fixtures.

- [x] **Step 5: Write failing behavioral load-outcome tests**

Add App-state tests before changing `apply_document_load_outcome`. Cover a
missing incoming id with no eviction/notice mutation, saving/dirty/active victim
protection, oldest-first multi-victim eviction, active-incoming rejection with
the documented adjacent fallback, successful retained logical bytes at or below
exactly 24 MiB, and `u64::MAX` overflow failing closed. Keep the following
source-order assertion only as a supplemental guard, not the primary evidence:

```rust
#[test]
fn apply_document_load_outcome은_source_clone전에_post_load_cap을_강제한다() {
    let source = include_str!("app.rs");
    let body = source
        .split_once("fn apply_document_load_outcome(")
        .expect("apply_document_load_outcome 정의")
        .1
        .split_once("\n    fn ")
        .expect("다음 함수 경계")
        .0;
    let admission = body
        .find("plan_document_load_admission(")
        .expect("로드 결과 적용 전 admission이 있어야 한다");
    let clone = body
        .find("document.saved_source = source.clone();")
        .expect("기존 source clone이 있어야 한다");
    assert!(admission < clone);
    assert!(body.contains("self.close_document_entry(id);"));
    assert!(body.contains("self.document_cap_notice = true;"));
}
```

- [x] **Step 6: Run the wiring test and verify RED**

Run:

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo apply_document_load_outcome은_source_clone전에_post_load_cap을_강제한다 --locked -- --test-threads=1
```

Expected: exit 101 because `plan_document_load_admission` is not called in the function body yet.

- [x] **Step 7: Apply admission before mutably borrowing the incoming document**

At the start of `apply_document_load_outcome`, after computing `load_state`, inspect content outcomes by reference:

```rust
let incoming_source_bytes = match &outcome {
    document_io::DocumentLoadOutcome::Loaded { source, .. }
    | document_io::DocumentLoadOutcome::ViewOnly { source, .. } => Some(source.len() as u64),
    document_io::DocumentLoadOutcome::Refused { .. }
    | document_io::DocumentLoadOutcome::Binary { .. }
    | document_io::DocumentLoadOutcome::Failed { .. } => None,
};
if let Some(incoming_source_bytes) = incoming_source_bytes {
match plan_document_load_admission(
        &self.documents,
        id,
        incoming_source_bytes,
        self.active_document,
) {
    DocumentLoadAdmission::Missing => return,
    DocumentLoadAdmission::Reject => {
        self.close_document_entry(id);
        self.document_cap_notice = true;
        return;
    }
    DocumentLoadAdmission::Admit { evict } => {
        for victim in evict {
            self.close_document_entry(victim);
        }
    }
}
```

Only after that block, find the incoming `OpenDocument`, set `load_state`, and move/clone the source exactly as before. Refused/Binary/Failed outcomes retain their existing error tabs because they hold no document source.

- [x] **Step 8: Verify load admission, tiers, and lifecycle GREEN**

Run:

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo apply_document_load_outcome은_source_clone전에_post_load_cap을_강제한다 --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo plan_document_load_admission --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo document_load_state_from_outcome --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo document --locked -- --test-threads=1
```

Expected: all focused tests pass; Full/ViewOnly/Refused/Binary behavior and dirty/save lifecycle remain unchanged.

- [x] **Step 9: Commit Task 4**

```bash
git add crates/app/src/app.rs
git commit -m "fix(app): enforce document cap after async load"
```

Execution note: the reviewed Task 4 implementation is commit `f771ea3`.

### Task 5: Integrated verification, code review, signed rebuild, and relaunch

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`
- Verify only: all production/test files changed in Tasks 1-4

- [x] **Step 1: Run focused and component test groups serially**

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo ui::workspace::tests --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo document --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo document_io::tests --locked -- --test-threads=1
```

Expected: every selected test passes; the three document layout measurement benches remain explicitly ignored unless separately requested.

- [x] **Step 2: Run the full package and static gates**

```bash
CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo run -p xtask --locked -- i18n-check
cargo run -p xtask --locked -- check-boundary
cargo fmt --all -- --check
git diff --check
```

Expected: exit 0 for every command. Record exact passed/ignored counts rather than reusing older counts.

- [x] **Step 3: Review the feature diff and fix only confirmed findings via TDD**

Inspect:

```bash
git diff 244b12f..HEAD -- crates/app/src/ui/workspace.rs crates/app/src/app.rs
codex review --base 244b12f
```

Review for duplicate consumption across overlapping responses, non-focused split drop ownership, attached-pane leakage, PTY fallback, path ordering, output retention beyond one frame, zero-byte loading eviction, dirty/active eviction, arithmetic overflow, source clone ordering, periodic repaint, and runtime resize side effects. For every confirmed behavior defect, add a focused failing regression, observe RED, apply the minimal fix, rerun the relevant component group, and commit the correction separately.

- [x] **Step 4: Update the handoff with exact evidence**

Record in `docs/CODEX_HANDOFF.md`:

```text
Current objective
Completed work
Modified files and commit ids
Key routing and cap decisions
Every test command with actual result/count
RED failures and their expected cause
Review findings and corrections
Failed approaches
Remaining physical checks
Exact next commands
```

Then run `git diff --check` again. Commit only the handoff/plan status that belongs to this feature:

```bash
git add docs/CODEX_HANDOFF.md docs/superpowers/plans/2026-08-24-terminal-document-drop.md
git commit -m "docs: record terminal document drop delivery"
```

- [x] **Step 5: Build and verify the signed macOS bundle**

```bash
DEPPY_SIGN_IDENTITY='Developer ID Application: VectorNine INC (ZDTU5LS35K)' CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 sh scripts/package-macos.sh
codesign --verify --deep --strict --verbose=2 'target/bundle/Deppy Sijo.app'
codesign -dv --verbose=4 'target/bundle/Deppy Sijo.app' 2>&1 | rg 'Identifier|TeamIdentifier|Authority'
unzip -tq 'target/bundle/Deppy Sijo.zip'
shasum -a 256 'target/bundle/Deppy Sijo.app/Contents/MacOS/deppy-sijo' 'target/bundle/Deppy Sijo.app/Contents/MacOS/deppy-mcp-proxy' 'target/bundle/Deppy Sijo.zip'
```

Expected: package script and independent deep/strict verification pass; identifier is `app.vector9.deppy-sijo`, team is `ZDTU5LS35K`, ZIP integrity passes, and all three hashes are recorded.

- [x] **Step 6: Relaunch only the exact signed bundle process**

Resolve candidates first:

```bash
pgrep -fl '/Users/jr/Desktop/projects/deppy-sijo/target/bundle/Deppy Sijo.app/Contents/MacOS/deppy-sijo'
```

For each returned PID, verify the exact executable with `ps -p <PID> -o pid=,ppid=,etime=,state=,command=`. Send `SIGTERM` only to the exact verified old bundle PID, wait in short bounded polls until it exits, then run:

```bash
open -n 'target/bundle/Deppy Sijo.app'
```

Resolve and verify the new exact PID twice, at least two seconds apart. Do not kill debug binaries or unrelated Deppy processes.

- [ ] **Step 7: Physical smoke-test checklist**

In the relaunched signed app:

```text
1. Drop .md, .rs, .json, .toml, and .yaml onto a local terminal body.
2. Confirm a document tab opens and the shell prompt receives no path text.
3. Drop several small UTF-8 files together; confirm order and last-active behavior.
4. Drop the same path again; confirm the existing tab activates without duplication.
5. Drop PNG/binary, directory, 1-8 MiB UTF-8, and >8 MiB fixtures; confirm existing Binary/Failed/ViewOnly/Refused surfaces and no PTY write.
6. Drop onto the non-focused side of a split; confirm that pane becomes the document-group owner.
7. Drag terminal-selected text; confirm it still pastes as text.
8. Observe idle CPU/repaint and terminal pixels after drop; confirm no new periodic repaint or flicker.
```

If physical interaction cannot be automated reliably, report it as remaining user-visible confirmation rather than claiming it passed.
