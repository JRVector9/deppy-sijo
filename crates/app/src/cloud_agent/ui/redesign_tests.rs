use super::*;
use egui_kittest::kittest::{NodeT as _, Queryable as _};

#[test]
fn redesign_tabs_show_only_the_selected_task_and_keep_actions_reachable() {
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1090.0, 730.0))
        .build_ui_state(
            move |ui, bridge: &mut CloudAgent| bridge.contents(ui, &catalog),
            CloudAgent::memory(),
        );
    harness
        .get_by_role_and_label(egui::accesskit::Role::Tab, "Access permissions")
        .click();
    harness.run();
    assert!(harness.query_by_label("Add connection").is_none());
    harness
        .get_by_role_and_label(egui::accesskit::Role::Tab, "Advanced settings")
        .click();
    harness.run();
    harness.get_by_label("Start MCP server");
    assert!(
        harness
            .query_by_label("Allow this connection to input")
            .is_none()
    );
}

#[test]
fn redesign_initial_screen_explains_connecting_without_showing_credential_forms() {
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(710.0, 520.0))
        .build_ui_state(
            move |ui, bridge: &mut CloudAgent| bridge.contents(ui, &catalog),
            CloudAgent::memory(),
        );
    harness.get_by_label("Add first connection");
    assert!(
        harness
            .query_by_label("New manual connection name")
            .is_none()
    );
    assert!(harness.query_by_label("Local MCP port").is_none());
}

#[test]
fn redesign_search_input_keeps_the_same_height_as_buttons() {
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let mut first = true;
    let harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1090.0, 730.0))
        .build_ui_state(
            move |ui, bridge: &mut CloudAgent| {
                if first {
                    let mut view = settings::state(ui);
                    view.tab = settings::Tab::Work;
                    settings::store(ui, view);
                    first = false;
                }
                bridge.contents(ui, &catalog);
            },
            CloudAgent::memory(),
        );
    let rect = harness
        .get_by_role_and_label(egui::accesskit::Role::TextInput, "Search cloud work")
        .rect();
    assert!(
        (rect.height() - 34.0).abs() < 0.1,
        "search input differs from approved control height: {rect:?}"
    );
}

#[test]
fn redesign_arrow_navigation_moves_focus_with_the_selected_tab() {
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let mut h = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1090.0, 730.0))
        .build_ui_state(
            move |ui, bridge: &mut CloudAgent| bridge.contents(ui, &catalog),
            CloudAgent::memory(),
        );
    h.get_by_role_and_label(egui::accesskit::Role::Tab, "Access permissions")
        .click();
    h.run();
    h.key_press(egui::Key::ArrowRight);
    h.run();
    assert_eq!(
        h.ctx
            .data_mut(|d| d.get_temp::<settings::SettingsState>(settings::state_id()))
            .unwrap()
            .tab,
        settings::Tab::Work
    );
    h.key_press(egui::Key::ArrowRight);
    h.run();
    assert_eq!(
        h.ctx
            .data_mut(|d| d.get_temp::<settings::SettingsState>(settings::state_id()))
            .unwrap()
            .tab,
        settings::Tab::Advanced
    );
    h.key_press(egui::Key::Home);
    h.run();
    assert_eq!(
        h.ctx
            .data_mut(|d| d.get_temp::<settings::SettingsState>(settings::state_id()))
            .unwrap()
            .tab,
        settings::Tab::Connections
    );
}

#[test]
fn redesign_native_geometry_and_reference_images() {
    use settings::{Tab, state, store};
    let output = std::env::var_os("DEPPY_CLOUD_DESIGN_IMAGES").map(std::path::PathBuf::from);
    if let Some(path) = &output {
        std::fs::create_dir_all(path).unwrap();
    }
    for (width, theme) in [
        (1090.0, egui::Theme::Dark),
        (710.0, egui::Theme::Dark),
        (1090.0, egui::Theme::Light),
    ] {
        for tab in Tab::ALL {
            let (bridge, _, review, _) = super::management_tests::running_fixture();
            let catalog = i18n::Catalog::load("ko-KR").unwrap();
            let mut first = true;
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(width, 730.0))
                .with_theme(theme)
                .build_ui_state(
                    move |ui, bridge: &mut CloudAgent| {
                        let rect =
                            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 730.0));
                        let mut native = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(rect)
                                .layout(egui::Layout::top_down(egui::Align::Min)),
                        );
                        native.set_clip_rect(rect);
                        let ui = &mut native;
                        crate::ui::settings::apply_settings_palette(ui);
                        ui.painter()
                            .rect_filled(ui.max_rect(), 0.0, ui.visuals().extreme_bg_color);
                        if first {
                            let mut view = state(ui);
                            view.tab = tab;
                            view.selected = Some(review.clone());
                            store(ui, view);
                            first = false;
                        }
                        bridge.contents(ui, &catalog);
                    },
                    bridge,
                );
            crate::fonts::install_cjk_fallback(&harness.ctx, None, "JetBrains Mono", "Regular");
            harness.run();
            let name = i18n::Catalog::load("ko-KR").unwrap().t(tab.key(), &[]);
            let bounds = harness
                .get_by_role_and_label(egui::accesskit::Role::Tab, &name)
                .rect();
            assert!(
                (bounds.height() - 42.0).abs() < 0.1,
                "tab height mismatch: {bounds:?}"
            );
            assert!(bounds.left() >= 0.0 && bounds.right() <= width);
            let y = bounds.top();
            assert!(
                (y - 80.0).abs() < 0.1,
                "header does not match approved 80px shell"
            );
            if tab == Tab::Connections {
                let button = harness.get_by_label("연결 추가").rect();
                assert!(
                    (button.height() - 34.0).abs() < 0.1,
                    "button height mismatch: {button:?}"
                );
                let row = harness
                    .get_by_label("Review Bot · 인증됨 · 아직 도구 성공 없음")
                    .rect();
                assert!(
                    (row.left() - 26.0).abs() < 0.1,
                    "content padding differs from approved 26px"
                );
                assert!(
                    (row.height() - 68.0).abs() < 0.1,
                    "row height mismatch: {row:?}"
                );
            }
            if tab == Tab::Work {
                let bar = harness
                    .get_by_role_and_label(
                        egui::accesskit::Role::ProgressIndicator,
                        "에이전트 보고",
                    )
                    .rect();
                assert!(
                    (bar.height() - 3.0).abs() < 0.1,
                    "progress differs from approved 3px line: {bar:?}"
                );
                assert!(
                    harness
                        .query_by_label("원래 세션: review-session · generation-1")
                        .is_none(),
                    "raw target belongs in the closed disclosure"
                );
            }
            if let Some(path) = &output {
                harness
                    .render()
                    .unwrap()
                    .save(path.join(format!("native-{}-{:?}-{:?}.png", width as u32, tab, theme)))
                    .unwrap();
            }
            harness.run();
            assert!(
                (harness
                    .get_by_role_and_label(egui::accesskit::Role::Tab, &name)
                    .rect()
                    .top()
                    - y)
                    .abs()
                    < 0.1,
                "header moved with body content"
            );
        }
    }
}

#[test]
fn redesign_long_detail_actions_remain_visible_after_body_scroll() {
    let mut h = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1090.0, 730.0))
        .build_ui_state(
            |ui, state: &mut (f32, egui::Rect, egui::Rect)| {
                egui::ScrollArea::vertical()
                    .id_salt("long-body")
                    .max_height(500.0)
                    .vertical_scroll_offset(state.0)
                    .show(ui, |ui| {
                        layout::list_detail(
                            ui,
                            "long-detail",
                            0.535,
                            |ui| {
                                ui.label("Selected answer");
                            },
                            |ui| {
                                for i in 0..60 {
                                    ui.label(format!("Answer line {i}"));
                                }
                                state.2 = ui.clip_rect();
                                state.1 = layout::button(ui, "Copy long answer").rect;
                            },
                        );
                    });
            },
            (0.0, egui::Rect::NOTHING, egui::Rect::NOTHING),
        );
    h.state_mut().0 = 950.0;
    h.run_steps(3);
    assert!(
        h.state().1.intersects(h.state().2),
        "scrolled action is clipped: {:?}",
        h.state()
    );
    h.get_by_label("Copy long answer").click();
    h.run();
}

#[test]
fn redesign_actor_reclaim_removes_latent_consent_after_session_takeover() {
    let (mut bridge, target, review, _, _) = management_tests::connection_fixture();
    let (other, _) = bridge
        .server
        .as_ref()
        .unwrap()
        .auth
        .mint_manual_connection("Other input actor", true, agent_mcp::now())
        .unwrap();
    bridge
        .set_connection_access(&review, &target, true, true)
        .unwrap();
    bridge.take_session_control(&target);
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let mut first = true;
    let mut h = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1090.0, 730.0))
        .build_ui_state(
            move |ui, bridge: &mut CloudAgent| {
                if first {
                    let mut view = settings::state(ui);
                    view.tab = settings::Tab::Permissions;
                    view.selected = Some(review.clone());
                    settings::store(ui, view);
                    first = false;
                }
                bridge.contents(ui, &catalog);
            },
            bridge,
        );
    h.get_by_label("Revoke this connection’s input").click();
    h.run();
    let review = h
        .state()
        .connection_infos()
        .into_iter()
        .find(|i| i.principal.name == "Review Bot")
        .unwrap()
        .principal
        .id;
    assert!(
        !h.state()
            .control
            .permission(&review, &target.id, &target.generation)
            .input,
        "actor reclaim must clear raw consent even when the shared ceiling is already closed"
    );
    h.state_mut()
        .set_connection_access(&other.id, &target, true, true)
        .unwrap();
    assert!(!h.state().connection_permission(&review, &target).input);
    assert!(h.state().connection_permission(&other.id, &target).input);
}

#[test]
fn redesign_notification_clears_old_search_and_keeps_answer_visible() {
    let mut bridge = CloudAgent::memory();
    bridge.install_history(
        1,
        vec![agent_mcp::Record {
            id: "answer-search-proof".into(),
            connection_id: None,
            run_id: None,
            generation: None,
            sequence: 0,
            tool: "notify".into(),
            workspace: "workspace".into(),
            session: "session".into(),
            created: agent_mcp::now() as i64,
            outcome: r#"{"status":"stored"}"#.into(),
            message: "Notified final answer".into(),
        }],
    );
    let key = bridge.answers[0].id.clone();
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let mut h = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1090.0, 730.0))
        .build_ui_state(
            move |ui, bridge: &mut CloudAgent| bridge.contents(ui, &catalog),
            bridge,
        );
    h.get_by_role_and_label(egui::accesskit::Role::Tab, "Work · answers")
        .click();
    h.run();
    h.get_by_role_and_label(egui::accesskit::Role::TextInput, "Search cloud work")
        .click();
    h.run();
    h.get_by_role_and_label(egui::accesskit::Role::TextInput, "Search cloud work")
        .type_text("old unrelated filter");
    h.run();
    h.state_mut().selected_record = Some(key);
    h.run();
    h.get_by_label("Notified final answer");
    assert!(h.state().selected_record.is_none());
    h.run();
    h.get_by_label("Notified final answer");
}

fn scenario_harness(bridge: CloudAgent) -> egui_kittest::Harness<'static, CloudAgent> {
    let catalog = i18n::Catalog::load("ko-KR").unwrap();
    let mut h = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1090.0, 730.0))
        .build_ui_state(
            move |ui, bridge: &mut CloudAgent| {
                let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1090.0, 730.0));
                let mut native = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                native.set_clip_rect(rect);
                crate::ui::settings::apply_settings_palette(&mut native);
                bridge.contents(&mut native, &catalog);
            },
            bridge,
        );
    crate::fonts::install_cjk_fallback(&h.ctx, None, "JetBrains Mono", "Regular");
    h.run();
    h
}
fn save_scenario(h: &mut egui_kittest::Harness<'_, CloudAgent>, name: &str) {
    if let Some(path) = std::env::var_os("DEPPY_CLOUD_DESIGN_IMAGES") {
        h.render()
            .unwrap()
            .save(std::path::PathBuf::from(path).join(format!("native-state-{name}.png")))
            .unwrap();
    }
}
#[test]
fn redesign_initial_verifying_failed_and_expired_states_render_the_actual_registry() {
    let c = i18n::Catalog::load("ko-KR").unwrap();
    for name in ["empty", "verifying", "failure", "expired"] {
        let mut bridge = CloudAgent::memory();
        if name != "empty" {
            bridge.server = Some(agent_mcp::Server::start(0, "", || {}).unwrap());
        }
        match name {
            "verifying" => bridge.connection = crate::cloud_agent::Connection::Verifying,
            "failure" => bridge.error = Some("tunnel_fixture_failure".into()),
            "expired" => {
                bridge.server.as_mut().unwrap().auth =
                    std::sync::Arc::new(agent_mcp::Auth::new_at(
                        agent_mcp::now().saturating_sub(agent_mcp::TOKEN_TTL + 1),
                    ));
                bridge.set_targets(vec![crate::cloud_agent::Target::fixture(
                    "expired-session",
                    "expired-generation",
                )]);
            }
            _ => {}
        }
        let mut h = scenario_harness(bridge);
        match name {
            "empty" => {
                h.get_by_label(&c.t("cloud.ui.first_add", &[]));
            }
            "verifying" => {
                h.get_by_label(&c.t("cloud.verifying", &[]));
            }
            "failure" => {
                h.get_by_label(&c.t("cloud.ui.retry", &[]));
            }
            "expired" => {
                h.get_by_label(&c.t("cloud.reauth_hint", &[]));
            }
            _ => unreachable!(),
        }
        assert!(h.state().grants.is_empty());
        save_scenario(&mut h, name);
        if name == "expired" {
            h.get_by_role_and_label(
                egui::accesskit::Role::Tab,
                &c.t("cloud.ui.permissions", &[]),
            )
            .click();
            h.run();
            let read_label = c.t("cloud.connection_read", &[]);
            let read = h.get_by_role_and_label(egui::accesskit::Role::CheckBox, &read_label);
            assert!(read.accesskit_node().is_disabled());
            save_scenario(&mut h, "expired-permissions");
        }
    }
}
fn oauth_http(server: &agent_mcp::Server, method: &str, path: &str, body: &str) -> String {
    use std::io::{Read, Write};
    let mut socket = std::net::TcpStream::connect(server.addr).unwrap();
    socket
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    write!(socket, "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", server.addr, body.len()).unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    response
}
#[test]
fn redesign_actual_oauth_request_requires_inspection_and_never_grants_session_access() {
    let mut bridge = CloudAgent::memory();
    let server = agent_mcp::Server::start(0, "", || {}).unwrap();
    let response = oauth_http(
        &server,
        "GET",
        "/.well-known/oauth-protected-resource/mcp",
        "",
    );
    let resource: serde_json::Value =
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    let response = oauth_http(
        &server,
        "POST",
        "/oauth/register",
        r#"{"client_name":"Review connector","redirect_uris":["https://client.example/callback"]}"#,
    );
    assert!(response.starts_with("HTTP/1.1 201"), "{response}");
    let client: serde_json::Value =
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    let challenge = "A".repeat(43);
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("response_type", "code"),
            ("client_id", client["client_id"].as_str().unwrap()),
            ("redirect_uri", "https://client.example/callback"),
            ("resource", resource["resource"].as_str().unwrap()),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("scope", "deppy.read deppy.input"),
            ("state", "design-proof"),
        ])
        .finish();
    let response = oauth_http(&server, "GET", &format!("/oauth/authorize?{query}"), "");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert_eq!(server.auth.approvals().len(), 1);
    bridge.server = Some(server);
    bridge.set_targets(vec![crate::cloud_agent::Target::fixture(
        "approval-session",
        "approval-generation",
    )]);
    let c = i18n::Catalog::load("ko-KR").unwrap();
    let mut h = scenario_harness(bridge);
    assert!(h.query_by_label(&c.t("cloud.oauth_approve", &[])).is_none());
    h.get_by_label(&c.t("cloud.ui.inspect", &[])).click();
    h.run();
    let approve = h.get_by_label(&c.t("cloud.oauth_approve", &[])).rect();
    assert!((approve.height() - 34.0).abs() < 0.1);
    save_scenario(&mut h, "oauth-approval");
    assert!(h.state().grants.is_empty());
    h.get_by_label(&c.t("cloud.oauth_approve", &[])).click();
    h.run();
    assert!(
        h.state()
            .server
            .as_ref()
            .unwrap()
            .auth
            .approvals()
            .is_empty()
    );
    assert!(
        h.state().grants.is_empty(),
        "connector authentication consent cannot grant session read/input"
    );
}

#[test]
fn redesign_narrow_settings_description_fits_before_the_next_row() {
    let mut bridge = CloudAgent::memory();
    bridge.automatic = false;
    let c = i18n::Catalog::load("ko-KR").unwrap();
    let description = c.t("cloud.tunnel_hint", &[]);
    let next_row = c.t("cloud.port", &[]);
    let mut first = true;
    let h = egui_kittest::Harness::builder()
        .with_size(egui::vec2(710.0, 730.0))
        .build_ui_state(
            move |ui, bridge: &mut CloudAgent| {
                if first {
                    let mut view = settings::state(ui);
                    view.tab = settings::Tab::Advanced;
                    settings::store(ui, view);
                    first = false;
                }
                bridge.contents(ui, &c);
            },
            bridge,
        );
    let description = h.get_by_label(&description).rect();
    let next = h.get_by_label(&next_row).rect();
    assert!(
        description.bottom() + 8.0 <= next.top(),
        "wrapped description is clipped by the fixed row: {description:?}, next={next:?}"
    );
}

#[test]
fn redesign_answer_search_matches_the_actual_connection_name() {
    let (mut bridge, target, review, _, _) = management_tests::connection_fixture();
    bridge.install_history(
        1,
        vec![agent_mcp::Record {
            id: "connection-search-answer".into(),
            connection_id: Some(review),
            run_id: None,
            generation: Some(target.generation),
            sequence: 0,
            tool: "notify".into(),
            workspace: target.workspace,
            session: target.id,
            created: agent_mcp::now() as i64,
            outcome: r#"{"status":"stored"}"#.into(),
            message: "Analysis complete".into(),
        }],
    );
    let c = i18n::Catalog::load("en-US").unwrap();
    let mut first = true;
    let mut h = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1090.0, 730.0))
        .build_ui_state(
            move |ui, bridge: &mut CloudAgent| {
                if first {
                    let mut view = settings::state(ui);
                    view.tab = settings::Tab::Work;
                    view.work_kind = settings::WorkKind::Answers;
                    settings::store(ui, view);
                    first = false;
                }
                bridge.contents(ui, &c);
            },
            bridge,
        );
    h.get_by_role_and_label(egui::accesskit::Role::TextInput, "Search cloud work")
        .click();
    h.run();
    h.get_by_role_and_label(egui::accesskit::Role::TextInput, "Search cloud work")
        .type_text("Review Bot");
    h.run();
    h.get_by_label("Analysis complete");
}
