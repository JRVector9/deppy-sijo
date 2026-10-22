// These tests exercise the production WorkspaceUi state machine. Synthetic event tests make
// delivery order explicit; the Unix fixture below also uses the actual runtime and shell PTY.

fn table_resize_stamp(ui: &WorkspaceUi, session: SessionId, epoch: u64) -> runtime::ResizeStamp {
    let request = ui.sessions[&session].resize_request.as_ref().unwrap();
    runtime::ResizeStamp {
        epoch,
        owner_epoch: request.token.owner_epoch,
        token: Some(request.token),
        cols: request.target.0,
        rows: request.target.1,
    }
}

fn table_resize_deliver(
    ui: &mut WorkspaceUi,
    session: SessionId,
    stamp: runtime::ResizeStamp,
    screen: Arc<TerminalViewportSnapshot>,
) {
    ui.handle_events(
        &[
            RuntimeEvent::ResizeApplied { session, stamp },
            RuntimeEvent::ViewportTracked {
                session,
                stamp,
                snapshot: screen,
                bracketed_paste: false,
            },
        ],
        &catalog(),
    );
    ui.settle_session_resize_presentation(
        session,
        std::time::Instant::now() + RESIZE_VIEWPORT_QUIET,
    );
}

#[test]
fn table_resize_rapid_drag_latest_target_survives_old_ack_and_frame_without_touching_sibling() {
    let mut ui = WorkspaceUi::new();
    let ctx = egui::Context::default();
    let session = SessionId(41);
    let sibling = SessionId(42);
    ui.handle_events(
        &[RuntimeEvent::MuxUpdated {
            snapshot: split_mux_snapshot(0.5),
        }],
        &catalog(),
    );
    let sibling_screen = shaped_snapshot(80, 24, "sibling unchanged");
    ui.sessions
        .entry(sibling)
        .or_default()
        .install_snapshot(Arc::clone(&sibling_screen));

    ui.stage_terminal_resize_for_pass(1, false, session, 80, 24);
    assert!(ui.flush_render_side_effects_for_pass(&ctx, 1, false));
    let initial_commands = drain_protocol(&mut ui);
    assert_eq!(initial_commands.len(), 1);
    let initial = table_resize_stamp(&ui, session, 1);
    for (pass, cols, rows) in [(2, 42, 20), (3, 120, 32)] {
        ui.stage_terminal_resize_for_pass(pass, false, session, cols, rows);
        assert!(!ui.flush_render_side_effects_for_pass(&ctx, pass, false));
        assert!(ui.protocol_intents.is_empty());
        assert_eq!(ui.sessions[&session].resize_desired, Some((cols, rows)));
        assert_eq!(ui.sent_sizes[&session], (80, 24));
    }
    table_resize_deliver(
        &mut ui,
        session,
        initial,
        shaped_snapshot(80, 24, "initial"),
    );
    assert_eq!(ui.sessions[&session].resize_desired, Some((120, 32)));

    ui.stage_terminal_resize_for_pass(4, false, session, 120, 32);
    assert!(ui.flush_render_side_effects_for_pass(&ctx, 4, false));
    let commands = drain_protocol(&mut ui);
    assert!(
        matches!(commands.as_slice(), [RuntimeCommand::ResizeTracked {
        session: actual, cols: 120, rows: 32, ..
    }] if *actual == session)
    );
    let latest = table_resize_stamp(&ui, session, 2);
    table_resize_deliver(&mut ui, session, initial, shaped_snapshot(80, 24, "stale"));
    assert_eq!(ui.sessions[&session].resize_desired, Some((120, 32)));
    assert!(
        ui.sessions[&session]
            .resize_request
            .unwrap()
            .applied
            .is_none()
    );

    let latest_screen = shaped_snapshot(120, 32, "latest table");
    table_resize_deliver(&mut ui, session, latest, Arc::clone(&latest_screen));
    table_resize_deliver(
        &mut ui,
        session,
        initial,
        shaped_snapshot(80, 24, "stale again"),
    );
    assert_eq!(ui.sent_sizes[&session], (120, 32));
    assert_eq!(ui.sessions[&session].resize_watermark, Some(latest));
    assert!(Arc::ptr_eq(
        ui.sessions[&session].snapshot.as_ref().unwrap(),
        &latest_screen
    ));
    assert!(Arc::ptr_eq(
        ui.sessions[&sibling].snapshot.as_ref().unwrap(),
        &sibling_screen
    ));
    assert!(ui.sessions[&sibling].resize_request.is_none());
    assert!(!ui.sent_sizes.contains_key(&sibling));
}

#[test]
fn table_resize_split_then_hidden_restore_keeps_latest_geometry_and_rejects_previous_token() {
    let mut ui = WorkspaceUi::new();
    let ctx = egui::Context::default();
    let session = SessionId(41);
    let visible = split_mux_snapshot(0.5);
    ui.handle_events(
        &[RuntimeEvent::MuxUpdated { snapshot: visible }],
        &catalog(),
    );
    ui.sessions
        .entry(session)
        .or_default()
        .install_snapshot(shaped_snapshot(80, 24, "stable"));
    ui.sent_sizes.insert(session, (80, 24));
    ui.begin_split_drag(tab_id("t"), Vec::new(), 0.72);
    for (pass, cols, rows) in [(1, 42, 20), (2, 120, 32)] {
        ui.stage_terminal_resize_for_pass(pass, false, session, cols, rows);
        assert!(!ui.flush_render_side_effects_for_pass(&ctx, pass, false));
    }
    ui.commit_split_drag(3);
    assert!(ui.flush_render_side_effects_for_pass(&ctx, 3, false));
    assert!(matches!(
        drain_protocol(&mut ui).as_slice(),
        [RuntimeCommand::ResizeSplit { .. }]
    ));
    let visible = split_mux_snapshot(0.72);
    ui.handle_events(
        &[RuntimeEvent::MuxUpdated {
            snapshot: Arc::clone(&visible),
        }],
        &catalog(),
    );
    ui.stage_terminal_resize_for_pass(4, false, session, 120, 32);
    assert!(ui.flush_render_side_effects_for_pass(&ctx, 4, false));
    drain_protocol(&mut ui);
    let before_hide = table_resize_stamp(&ui, session, 1);

    // Preserve live membership while a different tab is visible, as a real tab switch does.
    let mut hidden = visible.as_ref().clone();
    hidden.tabs.push(tab(
        "other",
        vec![pane("other-pane", SessionId(43))],
        LayoutNode::Pane(pane_id("other-pane")),
    ));
    hidden.active_tab = Some(tab_id("other"));
    hidden.focused_pane = Some(pane_id("other-pane"));
    ui.handle_events(
        &[RuntimeEvent::MuxUpdated {
            snapshot: Arc::new(hidden),
        }],
        &catalog(),
    );
    table_resize_deliver(
        &mut ui,
        session,
        before_hide,
        shaped_snapshot(120, 32, "hidden stale"),
    );
    assert!(ui.sessions[&session].snapshot.is_none());
    assert!(!ui.sent_sizes.contains_key(&session));
    ui.handle_events(
        &[RuntimeEvent::MuxUpdated { snapshot: visible }],
        &catalog(),
    );
    ui.stage_terminal_resize_for_pass(5, false, session, 120, 32);
    assert!(ui.flush_render_side_effects_for_pass(&ctx, 5, false));
    drain_protocol(&mut ui);
    let restored = table_resize_stamp(&ui, session, 2);
    assert_ne!(restored.token, before_hide.token);
    table_resize_deliver(
        &mut ui,
        session,
        before_hide,
        shaped_snapshot(120, 32, "old same size"),
    );
    assert!(ui.sessions[&session].snapshot.is_none());
    table_resize_deliver(
        &mut ui,
        session,
        restored,
        shaped_snapshot(120, 32, "restored table"),
    );
    assert_eq!(ui.sessions[&session].resize_desired, Some((120, 32)));
    assert_eq!(ui.sessions[&session].resize_watermark, Some(restored));
    assert_eq!(ui.sent_sizes[&session], (120, 32));
    assert!(ui.sessions[&session].snapshot.is_some());
}

#[cfg(unix)]
struct TableResizeRuntimeFixture {
    client: runtime::InProcessRuntimeClient,
    events: runtime::RuntimeEventReceiver,
    ui: WorkspaceUi,
    ctx: egui::Context,
    root: PathBuf,
    session: SessionId,
    window: egui::Vec2,
    sent: Vec<(u16, u16)>,
    applied: Vec<runtime::ResizeStamp>,
    failures: Vec<runtime::ResizeFailure>,
}

#[cfg(unix)]
impl TableResizeRuntimeFixture {
    fn new(pixels_per_point: f32) -> Self {
        use runtime::{RuntimeCommandSink, RuntimeEventStream};
        struct NoSecrets;
        impl runtime::RuntimeSecretResolver for NoSecrets {
            fn resolve(&self, _: &str) -> anyhow::Result<runtime::RuntimeSecret> {
                anyhow::bail!("table resize fixture never resolves credentials")
            }
        }
        let root =
            std::env::temp_dir().join(format!("deppy-table-resize-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let client = runtime::InProcessRuntimeClient::try_new_with_resolver(
            5,
            Arc::new(NoSecrets),
            root.clone(),
            secret::RedactionService::new(),
            None,
            Some(root.clone()),
            vec![],
        )
        .unwrap();
        let events = client.subscribe();
        // No provider, prompt, refresh key or input is involved. The shell owns the source
        // table and redraws only on SIGWINCH, so stty proves the actual child PTY geometry.
        let script = r#"
stty -echo
draw() {
    set -- $(stty size)
    printf '\033[H\033[2JWIDTH=%s ROWS=%s\r\n' "$2" "$1"
    printf '| %-*s |\r\n' "$(($2 - 6))" 'agent table'
    printf '| %-*s |\r\n' "$(($2 - 6))" 'table content'
    printf 'READY=%sx%s\r\n' "$2" "$1"
}
trap 'draw' WINCH
draw
while :; do read -r command || continue; done
"#;
        client
            .send_command(RuntimeCommand::SpawnAgent {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                agent_config_id: None,
                command: "/bin/sh".into(),
                args: vec!["-c".into(), script.into()],
                env_plain: vec![],
                env_secrets: vec![],
                waiting_regex: None,
                approval_regex: None,
                error_regex: None,
                done_regex: None,
                transient_args: vec![],
            })
            .unwrap();
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(pixels_per_point);
        let mut fixture = Self {
            client,
            events,
            ui: WorkspaceUi::new(),
            ctx,
            root,
            session: SessionId(0),
            window: egui::vec2(800.0, 600.0),
            sent: vec![],
            applied: vec![],
            failures: vec![],
        };
        fixture.wait_until(|fixture| {
            fixture.session != SessionId(0)
                && fixture
                    .ui
                    .sessions
                    .get(&fixture.session)
                    .and_then(|view| view.snapshot.as_ref())
                    .is_some_and(|screen| table_resize_screen_text(screen).contains("READY=80x24"))
        });
        fixture
    }

    fn pump(&mut self) {
        let events = self.events.drain();
        for event in &events {
            match event {
                RuntimeEvent::AgentSpawned { session } => self.session = *session,
                RuntimeEvent::ResizeApplied { session, stamp } if *session == self.session => {
                    self.applied.push(*stamp);
                }
                RuntimeEvent::ResizeFailed {
                    session, reason, ..
                } if *session == self.session => {
                    self.failures.push(*reason);
                }
                RuntimeEvent::SpawnFailed { .. } | RuntimeEvent::SessionExited { .. } => {
                    panic!("private table shell failed to start or exited");
                }
                _ => {}
            }
        }
        self.ui.handle_events(&events, &catalog());
    }

    fn wait_until(&mut self, mut ready: impl FnMut(&mut Self) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            self.pump();
            if ready(self) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "private resize fixture timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    // Exercise actual WorkspaceUi pane layout and terminal rendering. The only window edge
    // supplied by this headless test is its logical screen rectangle; no desktop app starts.
    fn render_target(&mut self, target: (u16, u16)) -> Vec<egui::epaint::ClippedShape> {
        let config = TerminalConfig::default();
        let mut actual = None;
        for _ in 0..8 {
            let mut cell = egui::Vec2::ZERO;
            let mut output = self.ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, self.window)),
                    ..Default::default()
                },
                |ui| {
                    cell = renderer_egui::cell_size(
                        ui.ctx(),
                        renderer_egui::CellMetrics {
                            font_size: config.font_size,
                            line_height: config.line_height,
                        },
                    );
                    self.ui.show_with_input(ui, &config, &[], &catalog(), false);
                    actual = self
                        .ui
                        .staged_terminal_resizes
                        .get(&self.session)
                        .map(|resize| (resize.cols, resize.rows));
                    if actual == Some(target) {
                        self.ui.flush_render_side_effects(ui.ctx());
                        crate::app::dispatch_terminal_protocol_tail(
                            &mut self.ui,
                            ui.ctx(),
                            |command| {
                                if let RuntimeCommand::ResizeTracked {
                                    session,
                                    cols,
                                    rows,
                                    ..
                                } = &command
                                {
                                    assert_eq!(*session, self.session);
                                    self.sent.push((*cols, *rows));
                                }
                                self.client.send_command_owned(command)
                            },
                        );
                    }
                },
            );
            output.textures_delta.clear();
            if actual == Some(target) {
                return output.shapes;
            }
            let actual = actual.expect("a ready native pane must stage its measured grid");
            self.window += egui::vec2(
                (f32::from(target.0) - f32::from(actual.0)) * cell.x,
                (f32::from(target.1) - f32::from(actual.1)) * cell.y,
            );
        }
        panic!("native pane geometry did not converge: {actual:?} vs {target:?}");
    }

    fn wait_table(&mut self, pane: (u16, u16), actual: (u16, u16)) {
        let mut shapes = vec![];
        self.wait_until(|fixture| {
            shapes = fixture.render_target(pane);
            fixture.ui.sessions[&fixture.session]
                .snapshot
                .as_ref()
                .is_some_and(|screen| {
                    let text = table_resize_screen_text(screen);
                    let mut lines = text.lines().map(str::trim_end);
                    (screen.cols, screen.rows) == actual
                        && lines.next()
                            == Some(format!("WIDTH={} ROWS={}", actual.0, actual.1).as_str())
                        && lines.nth(2) == Some(format!("READY={}x{}", actual.0, actual.1).as_str())
                        && text.lines().filter(|line| line.starts_with('|')).count() == 2
                        && fixture.ui.sessions[&fixture.session]
                            .resize_presentation
                            .is_none()
                })
        });
        let screen = self.ui.sessions[&self.session].snapshot.as_ref().unwrap();
        let text = table_resize_screen_text(screen);
        assert!(
            text.contains(&format!("WIDTH={} ROWS={}", actual.0, actual.1)),
            "{text}"
        );
        let table = text
            .lines()
            .filter(|line| line.starts_with('|'))
            .collect::<Vec<_>>();
        assert_eq!(table.len(), 2, "{text}");
        assert!(
            table
                .iter()
                .all(|line| line.trim_end().chars().count() == usize::from(actual.0 - 2)),
            "{table:?}"
        );
        let mut bars = vec![];
        fn collect_bars(shape: &egui::Shape, bars: &mut Vec<f32>) {
            match shape {
                egui::Shape::Text(text) if text.galley.job.text == "|" => bars.push(text.pos.x),
                egui::Shape::Vec(shapes) => {
                    shapes.iter().for_each(|shape| collect_bars(shape, bars))
                }
                _ => {}
            }
        }
        for shape in &shapes {
            collect_bars(&shape.shape, &mut bars);
        }
        assert_eq!(bars.len(), 4, "actual renderer must draw both table edges");
        let left = bars.iter().copied().reduce(f32::min).unwrap();
        let right = bars.iter().copied().reduce(f32::max).unwrap();
        let cell = renderer_egui::cell_size(
            &self.ctx,
            renderer_egui::CellMetrics {
                font_size: TerminalConfig::default().font_size,
                line_height: TerminalConfig::default().line_height,
            },
        );
        let glyph_scale = (right - left) / (f32::from(actual.0 - 3) * cell.x);
        if actual.0 > pane.0 && self.ui.sessions[&self.session].remote_resize_controlled {
            // The measured pane contains pane.cols full cells and less than one leftover cell.
            // A remotely owned wider grid is fit into that space without resizing its source.
            let minimum = f32::from(pane.0) / f32::from(actual.0);
            let maximum = f32::from(pane.0 + 1) / f32::from(actual.0);
            assert!(
                (minimum - 0.002..maximum + 0.002).contains(&glyph_scale),
                "cached table glyphs must use the current fit bucket: scale={glyph_scale}, pane={pane:?}, PTY={actual:?}"
            );
        } else {
            assert!(
                (glyph_scale - 1.0).abs() < 0.002,
                "cached table glyph edges must match the current grid: scale={glyph_scale}, left={left}, right={right}, cell={cell:?}, cols={}",
                actual.0
            );
        }
        eprintln!(
            "real resize ppp={} pane={}x{} PTY={}x{} table={}cells glyph_edges={:.2}pt glyph_scale={glyph_scale:.4} window={:?}",
            self.ctx.pixels_per_point(),
            pane.0,
            pane.1,
            actual.0,
            actual.1,
            actual.0 - 2,
            right - left,
            self.window
        );
    }
}

#[cfg(unix)]
impl Drop for TableResizeRuntimeFixture {
    fn drop(&mut self) {
        self.client.shutdown();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(unix)]
fn table_resize_screen_text(screen: &TerminalViewportSnapshot) -> String {
    screen
        .visible_cells
        .chunks(usize::from(screen.cols))
        .map(|row| {
            row.iter()
                .filter(|cell| !cell.wide_spacer())
                .map(|cell| cell.c)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(unix)]
#[test]
fn table_resize_native_geometry_runtime_pty_redraw_and_remote_restore_use_latest_target() {
    use runtime::{RuntimeCommandSink, TerminalControlRequest as Control};
    for pixels_per_point in [1.0, 2.0] {
        let mut fixture = TableResizeRuntimeFixture::new(pixels_per_point);
        fixture.wait_table((80, 24), (80, 24));
        fixture.wait_table((42, 20), (42, 20));
        fixture.wait_table((80, 24), (80, 24));
        fixture.render_target((42, 20));
        // Deliberately withhold runtime events while the native geometry changes again.
        fixture.render_target((60, 26));
        fixture.render_target((120, 32));
        assert_eq!(
            fixture.ui.sessions[&fixture.session].resize_desired,
            Some((120, 32))
        );
        assert_eq!(fixture.sent, [(80, 24), (42, 20), (80, 24), (42, 20)]);
        fixture.wait_table((120, 32), (120, 32));
        assert_eq!(
            fixture.sent,
            [(80, 24), (42, 20), (80, 24), (42, 20), (120, 32)]
        );
        let before_remote = *fixture.applied.last().unwrap();
        let owner = [7; 16];
        fixture
            .client
            .send_command(RuntimeCommand::TerminalControl {
                session: fixture.session,
                operation_id: 1,
                request: Control::Acquire {
                    owner,
                    expected_epoch: 0,
                    ttl_ms: 15_000,
                },
            })
            .unwrap();
        fixture
            .wait_until(|fixture| fixture.ui.sessions[&fixture.session].remote_resize_controlled);
        // Exercise retained row-cache scale changes independently of SIGWINCH/source redraw.
        fixture.wait_table((42, 20), (120, 32));
        fixture.wait_table((120, 32), (120, 32));
        fixture
            .client
            .send_command(RuntimeCommand::TerminalControl {
                session: fixture.session,
                operation_id: 2,
                request: Control::Resize {
                    owner,
                    lease_epoch: 1,
                    revision: 1,
                    cols: 40,
                    rows: 12,
                },
            })
            .unwrap();
        fixture.wait_table((120, 32), (40, 12));
        fixture.render_target((42, 20));
        fixture.render_target((120, 32));
        fixture.wait_until(|fixture| {
            fixture.ui.sessions[&fixture.session]
                .resize_request
                .is_some_and(|request| request.target == (120, 32) && request.deferred)
        });
        let deferred = fixture.ui.sessions[&fixture.session]
            .resize_request
            .unwrap()
            .token;
        fixture
            .client
            .send_command(RuntimeCommand::TerminalControl {
                session: fixture.session,
                operation_id: 3,
                request: Control::Release {
                    owner,
                    lease_epoch: 1,
                },
            })
            .unwrap();
        fixture.wait_table((120, 32), (120, 32));
        assert_eq!(
            fixture.ui.sessions[&fixture.session]
                .resize_watermark
                .unwrap()
                .token,
            Some(deferred)
        );
        fixture
            .client
            .send_command(RuntimeCommand::ResizeTracked {
                session: fixture.session,
                token: before_remote.token.unwrap(),
                cols: before_remote.cols,
                rows: before_remote.rows,
            })
            .unwrap();
        fixture.wait_until(|fixture| fixture.failures.contains(&runtime::ResizeFailure::Stale));
        fixture.wait_table((120, 32), (120, 32));
        fixture.wait_table((80, 24), (80, 24));
    }
}
