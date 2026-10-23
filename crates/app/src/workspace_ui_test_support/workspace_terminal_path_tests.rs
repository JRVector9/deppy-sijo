// Real pointer events exercise the same pane response as terminal selection and focus.
struct TerminalPathPrivateFile {
    root: PathBuf,
    path: PathBuf,
}

impl TerminalPathPrivateFile {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("deppy-terminal-path-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("note.md");
        std::fs::write(&path, "# Private terminal path fixture\n").unwrap();
        Self { root, path }
    }
}

impl Drop for TerminalPathPrivateFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct TerminalPathPointerState {
    workspace: WorkspaceUi,
    point: egui::Pos2,
    cell: egui::Vec2,
    input_enabled: bool,
    attached: bool,
}

fn terminal_path_pointer_fixture(
    path: &Path,
    warm: bool,
) -> egui_kittest::Harness<'static, TerminalPathPointerState> {
    let session = SessionId(7);
    let mut workspace = WorkspaceUi::new();
    workspace.mux = Some(mux(
        "primary",
        vec![tab(
            "primary",
            vec![pane("pane", session)],
            LayoutNode::Pane(pane_id("pane")),
        )],
        "pane",
    ));
    workspace.last_focused_pane = Some(pane_id("pane"));
    workspace.pending_focus = Some(pane_id("pane"));
    let text = path.to_str().unwrap().to_owned();
    workspace.sessions.entry(session).or_default().snapshot = Some(shaped_snapshot(200, 24, &text));
    if warm {
        workspace.path_click_cache = Some((
            session,
            text,
            Some(PathClick::OpenFile(path.to_owned())),
            std::time::Instant::now(),
        ));
    }
    let config = TerminalConfig::default();
    let catalog = catalog();
    let mut harness = egui_kittest::Harness::new_ui_state(
        move |ui, state: &mut TerminalPathPointerState| {
            let layout = terminal_pane_layout(ui.max_rect());
            let cell = renderer_egui::cell_size(
                ui.ctx(),
                renderer_egui::CellMetrics {
                    font_size: config.font_size,
                    line_height: config.line_height,
                },
            );
            state.point = layout.content.min
                + egui::vec2(
                    renderer_egui::HORIZONTAL_PADDING + 5.5 * cell.x,
                    0.5 * cell.y,
                );
            state.cell = cell;
            if state.attached {
                state.workspace.show_attached_pane(
                    ui,
                    &config,
                    &[],
                    &catalog,
                    &AttachedPaneTarget {
                        workspace_id: "attached-workspace".to_owned(),
                        tab: tab_id("primary"),
                        pane: pane_id("pane"),
                        session: SessionId(7),
                    },
                    "Attached",
                    AttachedPaneAvailability::Available,
                    state.input_enabled,
                );
            } else {
                state
                    .workspace
                    .show_with_input(ui, &config, &[], &catalog, state.input_enabled);
            }
        },
        TerminalPathPointerState {
            workspace,
            point: egui::Pos2::ZERO,
            cell: egui::Vec2::ZERO,
            input_enabled: true,
            attached: false,
        },
    );
    harness.set_size(egui::vec2(2000.0, 700.0));
    harness.run();
    drain_protocol(&mut harness.state_mut().workspace);
    harness
}

fn terminal_path_pointer_button(
    harness: &mut egui_kittest::Harness<'_, TerminalPathPointerState>,
    position: egui::Pos2,
    pressed: bool,
) {
    harness.input_mut().events.extend([
        egui::Event::PointerMoved(position),
        egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        },
    ]);
    harness.run_steps(1);
}

#[test]
fn terminal_path_actual_primary_click_opens_cached_file() {
    let file = TerminalPathPrivateFile::new();
    let mut harness = terminal_path_pointer_fixture(&file.path, true);
    let point = harness.state().point;
    terminal_path_pointer_button(&mut harness, point, true);
    terminal_path_pointer_button(&mut harness, point, false);
    let intent = harness.state_mut().workspace.take_io_intent();
    assert!(
        matches!(intent, Some(WorkspaceIoIntent::OpenPath(ref path)) if path.as_path() == file.path),
        "a real primary click on a known file must queue the existing open intent: {intent:?}"
    );
}

#[test]
fn terminal_path_actual_cold_click_waits_for_exact_file_resolution() {
    let file = TerminalPathPrivateFile::new();
    let mut harness = terminal_path_pointer_fixture(&file.path, false);
    let point = harness.state().point;
    terminal_path_pointer_button(&mut harness, point, true);
    terminal_path_pointer_button(&mut harness, point, false);
    let intent = harness.state_mut().workspace.take_io_intent().unwrap();
    let (operation, generation) = match intent {
        WorkspaceIoIntent::ResolvePath {
            operation,
            generation,
            word,
            session,
            ..
        } => {
            assert_eq!(session, SessionId(7));
            assert_eq!(word, file.path.to_str().unwrap());
            (operation, generation)
        }
        other => panic!("an unresolved path must be checked by the host first: {other:?}"),
    };
    assert!(harness.state_mut().workspace.take_io_intent().is_none());
    harness
        .state_mut()
        .workspace
        .complete_io(WorkspaceIoCompletion::PathResolved {
            operation,
            generation,
            result: Some(WorkspacePathResolution {
                kind: WorkspacePathKind::OpenableFile,
                path: WorkspacePathPayload::try_new(file.path.clone()).unwrap(),
            }),
        });
    let intent = harness.state_mut().workspace.take_io_intent();
    assert!(
        matches!(intent, Some(WorkspaceIoIntent::OpenPath(ref path)) if path.as_path() == file.path),
        "a cold click must survive until its exact host result: {intent:?}"
    );
}

#[test]
fn terminal_path_actual_drag_selects_without_opening_file() {
    let file = TerminalPathPrivateFile::new();
    let mut harness = terminal_path_pointer_fixture(&file.path, true);
    let point = harness.state().point;
    let end = point + egui::vec2(harness.state().cell.x * 5.0, 0.0);
    terminal_path_pointer_button(&mut harness, point, true);
    harness
        .input_mut()
        .events
        .push(egui::Event::PointerMoved(end));
    harness.run_steps(1);
    harness.run_steps(1);
    terminal_path_pointer_button(&mut harness, end, false);
    assert!(harness.state().workspace.selection.is_some());
    assert!(
        harness.state_mut().workspace.take_io_intent().is_none(),
        "dragging a path must remain terminal selection"
    );
}

fn terminal_path_cold_request(
    harness: &mut egui_kittest::Harness<'_, TerminalPathPointerState>,
) -> (WorkspaceIoOperation, u64) {
    let point = harness.state().point;
    terminal_path_pointer_button(harness, point, true);
    terminal_path_pointer_button(harness, point, false);
    match harness.state_mut().workspace.take_io_intent().unwrap() {
        WorkspaceIoIntent::ResolvePath {
            operation,
            generation,
            ..
        } => (operation, generation),
        other => panic!("expected an exact file resolution: {other:?}"),
    }
}

fn terminal_path_complete_file(
    harness: &mut egui_kittest::Harness<'_, TerminalPathPointerState>,
    operation: WorkspaceIoOperation,
    generation: u64,
    path: &Path,
) {
    harness
        .state_mut()
        .workspace
        .complete_io(WorkspaceIoCompletion::PathResolved {
            operation,
            generation,
            result: Some(WorkspacePathResolution {
                kind: WorkspacePathKind::OpenableFile,
                path: WorkspacePathPayload::try_new(path.to_owned()).unwrap(),
            }),
        });
}

#[test]
fn terminal_path_actual_cold_click_survives_hover_and_rejects_stale_completion() {
    let file = TerminalPathPrivateFile::new();
    let mut harness = terminal_path_pointer_fixture(&file.path, false);
    harness
        .state_mut()
        .workspace
        .sessions
        .get_mut(&SessionId(7))
        .unwrap()
        .snapshot = Some(shaped_snapshot(
        200,
        24,
        &format!("{:<400}other.md", file.path.to_str().unwrap()),
    ));
    let (operation, generation) = terminal_path_cold_request(&mut harness);
    let point = harness.state().point + egui::vec2(0.0, harness.state().cell.y * 2.0);
    harness
        .input_mut()
        .events
        .push(egui::Event::PointerMoved(point));
    harness.run_steps(1);
    assert!(
        harness.state_mut().workspace.take_io_intent().is_none(),
        "hovering a different word must not replace an explicit click's host query"
    );
    terminal_path_complete_file(&mut harness, operation, generation + 1, &file.path);
    assert!(harness.state_mut().workspace.take_io_intent().is_none());
    terminal_path_complete_file(&mut harness, operation, generation, &file.path);
    assert!(matches!(
        harness.state_mut().workspace.take_io_intent(),
        Some(WorkspaceIoIntent::OpenPath(path)) if path.as_path() == file.path
    ));
    terminal_path_complete_file(&mut harness, operation, generation, &file.path);
    assert!(harness.state_mut().workspace.take_io_intent().is_none());
}

#[test]
fn terminal_path_actual_cold_click_is_cancelled_by_navigation_session_change_or_alt_screen() {
    for change in ["navigation", "session", "alt", "cwd"] {
        let file = TerminalPathPrivateFile::new();
        let mut harness = terminal_path_pointer_fixture(&file.path, false);
        let (operation, generation) = terminal_path_cold_request(&mut harness);
        let workspace = &mut harness.state_mut().workspace;
        match change {
            "navigation" => workspace.cancel_terminal_path_activation(),
            "session" => {
                Arc::make_mut(workspace.mux.as_mut().unwrap()).tabs[0].panes[0].session_id =
                    Some(SessionId(8));
            }
            "alt" => {
                Arc::make_mut(
                    workspace
                        .sessions
                        .get_mut(&SessionId(7))
                        .unwrap()
                        .snapshot
                        .as_mut()
                        .unwrap(),
                )
                .is_alt_screen = true;
            }
            "cwd" => workspace.set_session_cwds(
                HashMap::from([(SessionId(7), "/changed-cwd".to_owned())]),
                crate::config::SessionNameStyle::default(),
            ),
            _ => unreachable!(),
        }
        terminal_path_complete_file(&mut harness, operation, generation, &file.path);
        assert!(
            harness.state_mut().workspace.take_io_intent().is_none(),
            "a late result must not open after {change}"
        );
    }
}

#[test]
fn terminal_path_actual_cold_click_is_cancelled_before_a_later_drag() {
    let file = TerminalPathPrivateFile::new();
    let mut harness = terminal_path_pointer_fixture(&file.path, false);
    let (operation, generation) = terminal_path_cold_request(&mut harness);
    let point = harness.state().point;
    let end = point + egui::vec2(harness.state().cell.x * 6.0, 0.0);
    terminal_path_pointer_button(&mut harness, point, true);
    harness
        .input_mut()
        .events
        .push(egui::Event::PointerMoved(end));
    harness.run_steps(1);
    terminal_path_complete_file(&mut harness, operation, generation, &file.path);
    assert!(harness.state_mut().workspace.take_io_intent().is_none());
    terminal_path_pointer_button(&mut harness, end, false);
    assert!(harness.state().workspace.selection.is_some());
}

#[test]
fn terminal_path_actual_disabled_attached_and_alt_surfaces_do_not_open_local_paths() {
    for surface in ["disabled", "attached", "alt"] {
        let file = TerminalPathPrivateFile::new();
        let mut harness = terminal_path_pointer_fixture(&file.path, true);
        match surface {
            "disabled" => harness.state_mut().input_enabled = false,
            "attached" => harness.state_mut().attached = true,
            "alt" => {
                Arc::make_mut(
                    harness
                        .state_mut()
                        .workspace
                        .sessions
                        .get_mut(&SessionId(7))
                        .unwrap()
                        .snapshot
                        .as_mut()
                        .unwrap(),
                )
                .is_alt_screen = true;
            }
            _ => unreachable!(),
        }
        harness.run_steps(1);
        let point = harness.state().point;
        terminal_path_pointer_button(&mut harness, point, true);
        terminal_path_pointer_button(&mut harness, point, false);
        assert!(
            harness.state_mut().workspace.take_io_intent().is_none(),
            "{surface} must not queue a local path lookup or open"
        );
    }
}

#[test]
fn terminal_path_actual_unfocused_sibling_first_click_only_requests_focus() {
    let file = TerminalPathPrivateFile::new();
    let mut harness = terminal_path_pointer_fixture(&file.path, false);
    let workspace = &mut harness.state_mut().workspace;
    workspace.mux = Some(split_mux_snapshot(0.5));
    workspace.pending_focus = None;
    workspace.last_focused_pane = Some(pane_id("left"));
    workspace
        .sessions
        .entry(SessionId(41))
        .or_default()
        .snapshot = Some(shaped_snapshot(200, 24, "focused sibling"));
    workspace
        .sessions
        .entry(SessionId(42))
        .or_default()
        .snapshot = Some(shaped_snapshot(200, 24, file.path.to_str().unwrap()));
    workspace.path_click_cache = Some((
        SessionId(42),
        file.path.to_str().unwrap().to_owned(),
        Some(PathClick::OpenFile(file.path.clone())),
        std::time::Instant::now(),
    ));
    harness.run_steps(1);
    drain_protocol(&mut harness.state_mut().workspace);
    let point = egui::pos2(1100.0, harness.state().point.y);
    terminal_path_pointer_button(&mut harness, point, true);
    terminal_path_pointer_button(&mut harness, point, false);
    assert!(harness.state_mut().workspace.take_io_intent().is_none());
    assert!(
        drain_protocol(&mut harness.state_mut().workspace)
            .iter()
            .any(|command| matches!(command, RuntimeCommand::FocusPane { pane } if pane == &pane_id("right")))
    );
}

#[test]
fn terminal_path_actual_url_and_directory_keep_their_existing_click_actions() {
    let mut url = terminal_path_pointer_fixture(Path::new("https://example.test/scope"), true);
    let point = url.state().point;
    terminal_path_pointer_button(&mut url, point, true);
    terminal_path_pointer_button(&mut url, point, false);
    assert!(matches!(
        url.state_mut().workspace.take_io_intent(),
        Some(WorkspaceIoIntent::OpenUrl(url)) if url.as_str() == "https://example.test/scope"
    ));

    let file = TerminalPathPrivateFile::new();
    let mut directory = terminal_path_pointer_fixture(&file.root, true);
    directory
        .state_mut()
        .workspace
        .path_click_cache
        .as_mut()
        .unwrap()
        .2 = Some(PathClick::Dir(file.root.clone()));
    let point = directory.state().point;
    terminal_path_pointer_button(&mut directory, point, true);
    terminal_path_pointer_button(&mut directory, point, false);
    assert!(directory.state_mut().workspace.take_io_intent().is_none());
    let commands = drain_protocol(&mut directory.state_mut().workspace);
    assert!(
        commands.iter().any(|command| matches!(command,
            RuntimeCommand::WriteInput { session: SessionId(7), bytes } if bytes.starts_with(b"cd ")
        )),
        "directory clicks must retain shell navigation: {commands:?}"
    );
}

#[test]
fn terminal_path_actual_scope_cancel_discards_queued_open() {
    for change in ["navigation", "pane"] {
        let file = TerminalPathPrivateFile::new();
        let mut harness = terminal_path_pointer_fixture(&file.path, true);
        let point = harness.state().point;
        terminal_path_pointer_button(&mut harness, point, true);
        terminal_path_pointer_button(&mut harness, point, false);
        let workspace = &mut harness.state_mut().workspace;
        if change == "pane" {
            Arc::make_mut(workspace.mux.as_mut().unwrap()).tabs[0]
                .panes
                .push(pane("other", SessionId(8)));
            workspace.request_pane_focus(pane_id("other"));
        } else {
            workspace.cancel_terminal_path_activation();
        }
        assert!(
            harness.state_mut().workspace.take_io_intent().is_none(),
            "{change} while the host is busy must discard a queued file open"
        );
    }
}

#[test]
fn terminal_path_actual_punctuated_file_is_distinct_from_its_unpunctuated_neighbor() {
    let file = TerminalPathPrivateFile::new();
    let plain = file.root.join("README");
    let punctuated = file.root.join("README)");
    std::fs::write(&plain, "Unpunctuated neighbor\n").unwrap();
    std::fs::write(&punctuated, "Requested file\n").unwrap();
    let mut harness = terminal_path_pointer_fixture(&punctuated, true);
    let point = harness.state().point;
    terminal_path_pointer_button(&mut harness, point, true);
    terminal_path_pointer_button(&mut harness, point, false);
    let intent = harness.state_mut().workspace.take_io_intent();
    assert!(
        matches!(intent, Some(WorkspaceIoIntent::OpenPath(ref path)) if path.as_path() == punctuated),
        "the host must receive the punctuated filename, even when its neighbor exists: {intent:?}"
    );
}
