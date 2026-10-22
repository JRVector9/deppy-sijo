// Actual egui wheel events must reach application history without entering prompt input.
fn application_wheel_fixture(alt: bool) -> egui_kittest::Harness<'static, WorkspaceUi> {
    let mut harness = setup_focused_local_pane_harness(SessionId(7));
    let mut screen = shaped_snapshot(80, 24, "history table").as_ref().clone();
    screen.is_alt_screen = alt;
    harness
        .state_mut()
        .sessions
        .get_mut(&SessionId(7))
        .unwrap()
        .snapshot = Some(Arc::new(screen));
    harness.run();
    drain_protocol(harness.state_mut());
    harness
}

fn application_wheel_event(
    harness: &mut egui_kittest::Harness<'static, WorkspaceUi>,
    modifiers: egui::Modifiers,
) {
    let point = egui::pos2(80.0, TERMINAL_PANE_HEADER_HEIGHT + 40.0);
    harness.input_mut().events.extend([
        egui::Event::PointerMoved(point),
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, 40.0),
            phase: egui::TouchPhase::Move,
            modifiers,
        },
    ]);
    harness.run_steps(1);
}

#[test]
fn application_wheel_egui_routes_alt_history_to_typed_input() {
    let mut harness = application_wheel_fixture(true);
    application_wheel_event(&mut harness, egui::Modifiers::NONE);
    let commands = drain_protocol(harness.state_mut());
    assert!(
        commands.iter().any(|command| matches!(
            command,
            RuntimeCommand::WriteTerminalInput {
                session: SessionId(7),
                input: input @ runtime::TerminalInput::Wheel { .. },
            } if input.is_valid()
        )),
        "alternate history needs application wheel input: {commands:?}"
    );
    assert!(
        !commands.iter().any(|command| matches!(
            command,
            RuntimeCommand::Scroll { .. } | RuntimeCommand::WriteInput { .. }
        )),
        "application wheel must not also scroll native history or inject raw prompt bytes"
    );
}

#[test]
fn application_wheel_egui_keeps_native_history_and_shift_selection_routes() {
    for (alt, modifiers) in [
        (false, egui::Modifiers::NONE),
        (true, egui::Modifiers::SHIFT),
    ] {
        let mut harness = application_wheel_fixture(alt);
        application_wheel_event(&mut harness, modifiers);
        let commands = drain_protocol(harness.state_mut());
        assert!(
            commands.iter().any(|command| matches!(command,
                RuntimeCommand::Scroll { session: SessionId(7), delta } if *delta > 0
            )),
            "native override must keep native scrolling: {commands:?}"
        );
        assert!(!commands.iter().any(|command| matches!(
            command,
            RuntimeCommand::WriteTerminalInput { .. } | RuntimeCommand::WriteInput { .. }
        )));
    }
}

#[test]
fn application_wheel_egui_selection_and_archive_never_become_cli_input() {
    for archive in [false, true] {
        let mut harness = application_wheel_fixture(true);
        if archive {
            harness
                .state_mut()
                .sessions
                .get_mut(&SessionId(7))
                .unwrap()
                .restored_readonly = true;
        } else {
            harness.state_mut().selection = Some((SessionId(7), 0, 5));
        }
        application_wheel_event(&mut harness, egui::Modifiers::NONE);
        let commands = drain_protocol(harness.state_mut());
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, RuntimeCommand::Scroll { .. }))
        );
        assert!(
            !commands.iter().any(|command| matches!(
                command,
                RuntimeCommand::WriteTerminalInput { .. } | RuntimeCommand::WriteInput { .. }
            )),
            "selection/archive must stay on the native view path: {commands:?}"
        );
    }
}

#[test]
fn application_wheel_egui_disabled_surface_never_sends_input() {
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
    let mut screen = shaped_snapshot(80, 24, "read only").as_ref().clone();
    screen.is_alt_screen = true;
    workspace.sessions.entry(session).or_default().snapshot = Some(Arc::new(screen));
    let config = TerminalConfig::default();
    let text = catalog();
    let mut harness = egui_kittest::Harness::new_ui_state(
        move |ui, workspace: &mut WorkspaceUi| {
            workspace.show_with_input(ui, &config, &[], &text, false);
        },
        workspace,
    );
    harness.run_steps(1);
    drain_protocol(harness.state_mut());
    application_wheel_event(&mut harness, egui::Modifiers::NONE);
    assert!(
        !drain_protocol(harness.state_mut())
            .iter()
            .any(|command| matches!(
                command,
                RuntimeCommand::WriteTerminalInput { .. } | RuntimeCommand::WriteInput { .. }
            ))
    );
}

#[test]
fn application_wheel_egui_modal_never_sends_cli_input() {
    let mut original = application_wheel_fixture(true);
    let workspace = std::mem::replace(original.state_mut(), WorkspaceUi::new());
    let config = TerminalConfig::default();
    let text = catalog();
    let mut harness = egui_kittest::Harness::new_ui_state(
        move |ui, workspace: &mut WorkspaceUi| {
            super::super::popup::set_pending_modal(ui.ctx(), true);
            workspace.show_with_input(ui, &config, &[], &text, true);
        },
        workspace,
    );
    harness.run_steps(1);
    drain_protocol(harness.state_mut());
    application_wheel_event(&mut harness, egui::Modifiers::NONE);
    assert!(
        !drain_protocol(harness.state_mut())
            .iter()
            .any(|command| matches!(
                command,
                RuntimeCommand::WriteTerminalInput { .. } | RuntimeCommand::WriteInput { .. }
            ))
    );
}

#[test]
fn application_wheel_egui_unfocused_sibling_never_sends_cli_input() {
    let mut harness = application_wheel_fixture(true);
    let workspace = harness.state_mut();
    workspace.mux = Some(split_mux_snapshot(0.5));
    workspace.pending_focus = None;
    for session in [SessionId(41), SessionId(42)] {
        let mut screen = shaped_snapshot(80, 24, "separate history").as_ref().clone();
        screen.is_alt_screen = true;
        workspace.sessions.entry(session).or_default().snapshot = Some(Arc::new(screen));
    }
    harness.set_size(egui::vec2(900.0, 600.0));
    harness.run_steps(1);
    drain_protocol(harness.state_mut());
    harness.input_mut().events.extend([
        egui::Event::PointerMoved(egui::pos2(700.0, TERMINAL_PANE_HEADER_HEIGHT + 40.0)),
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, 40.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        },
    ]);
    harness.run_steps(1);
    assert!(
        !drain_protocol(harness.state_mut())
            .iter()
            .any(|command| matches!(
                command,
                RuntimeCommand::WriteTerminalInput { .. } | RuntimeCommand::WriteInput { .. }
            )),
        "hovering a sibling must not give it input ownership"
    );
}

#[test]
fn application_wheel_protocol_preserves_order_and_rejects_unbounded_input() {
    let mut workspace = WorkspaceUi::new();
    let wheel = runtime::TerminalInput::Wheel {
        cols: 80,
        rows: 24,
        column: 12,
        row: 3,
        delta: 2,
    };
    workspace
        .queue_protocol_intent(RuntimeCommand::WriteInput {
            session: SessionId(7),
            bytes: b"draft".to_vec(),
        })
        .unwrap();
    workspace
        .queue_protocol_intent(RuntimeCommand::WriteTerminalInput {
            session: SessionId(7),
            input: wheel,
        })
        .unwrap();
    workspace
        .queue_protocol_intent(RuntimeCommand::WriteInput {
            session: SessionId(8),
            bytes: b"other".to_vec(),
        })
        .unwrap();
    let commands = drain_protocol(&mut workspace);
    assert!(matches!(
        commands.as_slice(),
        [
            RuntimeCommand::WriteInput {
                session: SessionId(7),
                ..
            },
            RuntimeCommand::WriteTerminalInput {
                session: SessionId(7),
                ..
            },
            RuntimeCommand::WriteInput {
                session: SessionId(8),
                ..
            },
        ]
    ));
    for input in [
        runtime::TerminalInput::Wheel {
            cols: 80,
            rows: 24,
            column: 0,
            row: 3,
            delta: 2,
        },
        runtime::TerminalInput::Wheel {
            cols: 80,
            rows: 24,
            column: 12,
            row: 3,
            delta: 33,
        },
        runtime::TerminalInput::Text {
            text: "not this route".into(),
            paste: false,
        },
    ] {
        assert_eq!(
            workspace.queue_protocol_intent(RuntimeCommand::WriteTerminalInput {
                session: SessionId(7),
                input
            }),
            Err(WorkspaceProtocolErrorCode::InvalidCommand)
        );
    }
}

#[test]
fn application_wheel_egui_trackpad_retains_native_modifier_until_gesture_end() {
    for modifiers in [egui::Modifiers::SHIFT, egui::Modifiers::ALT] {
        let mut harness = application_wheel_fixture(true);
        let pos = egui::pos2(80.0, TERMINAL_PANE_HEADER_HEIGHT + 40.0);
        for (phase, current) in [
            (egui::TouchPhase::Start, modifiers),
            (egui::TouchPhase::Move, modifiers),
            (egui::TouchPhase::Move, egui::Modifiers::NONE),
        ] {
            harness.input_mut().events.extend([
                egui::Event::PointerMoved(pos),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, 40.0),
                    phase,
                    modifiers: current,
                },
            ]);
            harness.run_steps(1);
            let commands = drain_protocol(harness.state_mut());
            assert!(
                !commands
                    .iter()
                    .any(|command| matches!(command, RuntimeCommand::WriteTerminalInput { .. })),
                "modifier release in an active gesture must keep native routing: {commands:?}"
            );
            if phase == egui::TouchPhase::Move {
                assert!(commands.iter().any(|command| matches!(command, RuntimeCommand::Scroll { delta, .. } if *delta > 0)), "native trackpad gesture must keep scrolling after modifier release: {commands:?}");
            }
        }
        harness.input_mut().events.push(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::Vec2::ZERO,
            phase: egui::TouchPhase::End,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(1);
        drain_protocol(harness.state_mut());
        application_wheel_event(&mut harness, egui::Modifiers::NONE);
        assert!(
            drain_protocol(harness.state_mut())
                .iter()
                .any(|command| matches!(command, RuntimeCommand::WriteTerminalInput { .. })),
            "a later unmodified gesture must return to application history"
        );
    }
}

#[cfg(unix)]
#[test]
fn application_wheel_native_ui_dispatch_reaches_only_negotiated_pty_without_prompt_evidence() {
    use runtime::{RuntimeCommandSink, RuntimeEventStream};
    let mut fixture = TableResizeRuntimeFixture::new(1.0);
    let original = fixture.session;
    let capture = fixture.root.join("wheel-input.bin");
    fixture
        .client
        .send_command(RuntimeCommand::SpawnAgent {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
            agent_config_id: None,
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                r#"stty raw -echo
printf '\033[?1049h\033[?1000h\033[?1006h\033[H\033[2JWHEEL_READY'
cat > "$1"
"#
                .into(),
                "table-wheel-fixture".into(),
                capture.to_string_lossy().into_owned(),
            ],
            env_plain: vec![],
            env_secrets: vec![],
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
            transient_args: vec![],
        })
        .unwrap();
    fixture.wait_until(|fixture| {
        fixture.session != original
            && fixture
                .ui
                .sessions
                .get(&fixture.session)
                .and_then(|view| view.snapshot.as_ref())
                .is_some_and(|screen| {
                    screen.is_alt_screen && table_resize_screen_text(screen).contains("WHEEL_READY")
                })
    });
    fixture.render_target((80, 24));
    fixture.wait_until(|fixture| {
        fixture.ui.sessions[&fixture.session]
            .snapshot
            .as_ref()
            .is_some_and(|screen| screen.cols == 80 && screen.rows == 24)
    });
    let navigation_events = fixture.client.subscribe();
    let config = TerminalConfig::default();
    let mut expected = Vec::new();
    let point = egui::pos2(80.0, TERMINAL_PANE_HEADER_HEIGHT + 40.0);
    let frame = fixture.ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, fixture.window)),
            events: vec![
                egui::Event::PointerMoved(point),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    // One frame of egui smoothing must cross a complete terminal row.
                    delta: egui::vec2(0.0, 80.0),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        },
        |ui| {
            fixture
                .ui
                .show_with_input(ui, &config, &[], &catalog(), true);
            fixture.ui.flush_render_side_effects(ui.ctx());
            crate::app::dispatch_terminal_protocol_tail(&mut fixture.ui, ui.ctx(), |command| {
                if let RuntimeCommand::WriteTerminalInput {
                    session,
                    input:
                        runtime::TerminalInput::Wheel {
                            column, row, delta, ..
                        },
                } = &command
                {
                    assert_eq!(*session, fixture.session);
                    let button = if *delta > 0 { 64 } else { 65 };
                    expected.extend(
                        format!("\x1b[<{button};{column};{row}M")
                            .as_bytes()
                            .repeat(usize::from(delta.unsigned_abs())),
                    );
                }
                fixture.client.send_command_owned(command)
            });
        },
    );
    frame.drop_without_applying_deltas();
    assert!(
        !expected.is_empty(),
        "the actual pane must emit a typed wheel command; residual={}",
        fixture.ui.scroll_residual
    );
    fixture
        .wait_until(|_| std::fs::read(&capture).is_ok_and(|bytes| bytes.len() >= expected.len()));
    assert_eq!(
        std::fs::read(capture).unwrap(),
        expected,
        "actual UI→tail dispatcher→worker→PTY bytes"
    );
    assert!(
        !navigation_events.drain().iter().any(|event| matches!(
            event,
            RuntimeEvent::SessionInputAccepted { .. } | RuntimeEvent::SessionInputSubmitted { .. }
        )),
        "wheel navigation must not become a sent prompt or completion intervention"
    );
}
