use super::*;
use egui_kittest::kittest::NodeT as _;
use egui_kittest::kittest::Queryable as _;

fn fixture() -> (CloudAgent, String) {
    let (mut bridge, mut target, selected, _, _) = management_tests::connection_fixture();
    bridge.hostname = "deppy.example.com".into();
    target.provider = Some(crate::agent_surface::AgentProvider::Codex);
    bridge.set_targets(vec![target]);
    (bridge, selected)
}

fn harness(bridge: CloudAgent, selected: String) -> egui_kittest::Harness<'static, CloudAgent> {
    harness_at_size(bridge, selected, egui::vec2(1090.0, 1000.0))
}

fn harness_at_size(
    bridge: CloudAgent,
    selected: String,
    size: egui::Vec2,
) -> egui_kittest::Harness<'static, CloudAgent> {
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let mut first = true;
    egui_kittest::Harness::builder()
        .with_size(size)
        .build_ui_state(
            move |ui, bridge: &mut CloudAgent| {
                if first {
                    let mut view = settings::state(ui);
                    view.selected = Some(selected.clone());
                    settings::store(ui, view);
                    first = false;
                }
                bridge.contents(ui, &catalog);
                let copies = ui.ctx().output(|output| {
                    output
                        .commands
                        .iter()
                        .filter_map(|command| {
                            if let egui::OutputCommand::CopyText(text) = command {
                                Some(text.clone())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                });
                if !copies.is_empty() {
                    ui.ctx().data_mut(|data| {
                        let id = egui::Id::new("one-screen-copies");
                        let mut previous = data.get_temp::<Vec<String>>(id).unwrap_or_default();
                        previous.extend(copies);
                        data.insert_temp(id, previous);
                    });
                }
            },
            bridge,
        )
}

fn save_layout_image(h: &mut egui_kittest::Harness<'_, CloudAgent>, name: &str) {
    if let Some(path) = std::env::var_os("DEPPY_CLOUD_ONE_SCREEN_IMAGES") {
        let path = std::path::PathBuf::from(path);
        std::fs::create_dir_all(&path).unwrap();
        h.render().unwrap().save(path.join(name)).unwrap();
    }
}

#[test]
fn one_screen_long_connection_names_keep_fixed_width_and_do_not_overlap_status() {
    for (width, expected_width, margin) in [
        (1090.0, 185.0, 26.0),
        (660.0, 185.0, 20.0),
        (410.0, 148.0, 16.0),
    ] {
        let (bridge, selected) = fixture();
        let name = "A cloud agent connection with a long display name ".repeat(2);
        bridge
            .server
            .as_ref()
            .unwrap()
            .auth
            .rename_connection(&selected, &name);
        let mut h = harness_at_size(bridge, selected, egui::vec2(width, 1600.0));
        h.run();
        let combo = h.get_by_role(egui::accesskit::Role::ComboBox).rect();
        let add = h.get_by_label("Add connection").rect();
        let status = h.get_by_label("Awaiting authentication").rect();
        assert!(
            (combo.width() - expected_width).abs() < 0.1,
            "width{width}: selector {combo:?}"
        );
        assert!((combo.height() - 34.0).abs() < 0.1);
        assert!(
            add.intersect(status).area() <= 0.0,
            "width{width}: {add:?}, {status:?}"
        );
        assert!(status.right() <= width - 8.0 - margin + 0.1);
        save_layout_image(&mut h, &format!("native-long-name-{}.png", width as u32));
        h.get_by_role(egui::accesskit::Role::ComboBox).click();
        h.run();
        h.get_by_label("Research Bot").click();
        h.run();
        let selection = h.ctx.data_mut(|data| {
            data.get_temp::<settings::SettingsState>(settings::state_id())
                .unwrap()
                .selected
        });
        let other_id = h
            .state()
            .connection_infos()
            .into_iter()
            .find(|info| info.principal.name == "Research Bot")
            .unwrap()
            .principal
            .id;
        assert_eq!(selection.as_deref(), Some(other_id.as_str()));
    }
}

#[test]
fn one_screen_compact_margin_and_footer_match_the_approved_layout() {
    let (bridge, selected) = fixture();
    let mut h = harness_at_size(bridge, selected, egui::vec2(410.0, 1600.0));
    h.run();
    let title = h.get_by_label("Cloud agents").rect();
    let hint = h
        .get_by_label("Start the connection from your agent after entering its connection details.")
        .rect();
    let check = h.get_by_label("Check connection").rect();
    let details = h.get_by_label("Connection details for your agent").rect();
    let copy = h.get_by_label("Copy all connection details").rect();
    save_layout_image(&mut h, "native-compact-footer.png");
    // Harness의 기본8pt 본문 바깥 여백에 시안의16/18pt를 더한다.
    assert!((title.left() - 24.0).abs() < 0.1, "{title:?}");
    assert!((title.top() - 26.0).abs() < 0.1, "{title:?}");
    assert!(check.top() >= hint.bottom() + 8.9, "{hint:?}, {check:?}");
    assert!((check.right() - 386.0).abs() < 0.1, "{check:?}");
    assert!(
        details.intersect(copy).area() <= 0.0,
        "{details:?}, {copy:?}"
    );
}

#[test]
fn one_screen_renew_confirmation_keeps_actions_right_on_wide_and_below_on_compact() {
    for (width, margin) in [(1090.0, 26.0), (410.0, 16.0)] {
        let (bridge, selected) = fixture();
        let name = bridge
            .connection_infos()
            .into_iter()
            .find(|info| info.principal.id == selected)
            .unwrap()
            .principal
            .name;
        let c = i18n::Catalog::load("en-US").unwrap();
        let message = c.t("cloud.one.renew_confirm", &[("name", &name)]);
        let mut h = harness_at_size(bridge, selected, egui::vec2(width, 1600.0));
        h.get_by_label("Issue new token").click();
        h.run();
        let text = h.get_by_label(&message).rect();
        let issue = h.get_by_label("Issue token now").rect();
        let cancel = h.get_by_label("Cancel").rect();
        if width > 600.0 {
            assert!(
                (cancel.right() - (width - 8.0 - margin - 12.0)).abs() < 0.1,
                "right actions: {cancel:?}"
            );
            assert!(text.right() <= issue.left(), "{text:?}, {issue:?}");
            assert!(text.y_range().intersects(issue.y_range()));
        } else {
            assert!(issue.top() >= text.bottom() + 7.9, "{text:?}, {issue:?}");
            assert!(cancel.right() <= width - 8.0 - margin - 12.0 + 0.1);
        }
        save_layout_image(&mut h, &format!("native-renew-{}.png", width as u32));
    }
}

#[test]
fn one_screen_shows_connection_facts_permissions_and_separate_guide_without_tabs() {
    let (bridge, selected) = fixture();
    let h = harness(bridge, selected);
    h.get_by_label("Connection details for your agent");
    h.get_by_label("Sessions to share");
    h.get_by_label("https://deppy.example.com/mcp");
    h.get_by_label("Allow this connection to read · receive answers");
    h.get_by_label("Permission settings");
    assert!(
        h.query_by_role_and_label(egui::accesskit::Role::Tab, "Access permissions")
            .is_none()
    );
    assert!(h.query_by_label("Local MCP port").is_none());
}

#[test]
fn one_screen_never_exposes_local_endpoint_in_connection_facts() {
    let (mut bridge, selected) = fixture();
    bridge.hostname.clear();
    let local = format!("http://{}/mcp", bridge.server.as_ref().unwrap().addr);
    let h = harness(bridge, selected);
    assert!(h.query_by_label(&local).is_none());
}

#[test]
fn one_screen_token_renewal_requires_confirmation_and_copy_is_explicit() {
    let (mut bridge, selected) = fixture();
    let target = bridge.targets[0].clone();
    bridge
        .set_connection_access(&selected, &target, true, true)
        .unwrap();
    let other = bridge
        .connection_infos()
        .into_iter()
        .find(|info| info.principal.name == "Research Bot")
        .unwrap()
        .principal
        .id;
    let mut h = harness(bridge, selected.clone());
    h.get_by_label("Issue new token").click();
    h.run();
    assert!(
        h.state()
            .connection_infos()
            .iter()
            .any(|info| info.principal.id == selected
                && info.state == agent_mcp::ConnectionState::Active)
    );
    h.get_by_label("Cancel").click();
    h.run();
    assert!(h.state().connection_permission(&selected, &target).input);
    h.get_by_label("Issue new token").click();
    h.run();
    h.get_by_label("Issue token now").click();
    h.run();
    assert!(
        h.state()
            .connection_infos()
            .iter()
            .any(|info| info.principal.id == selected
                && info.state == agent_mcp::ConnectionState::Revoked)
    );
    assert!(
        h.state()
            .connection_infos()
            .iter()
            .any(|info| info.principal.id == other
                && info.state == agent_mcp::ConnectionState::Active)
    );
    let (id, token) = h.state().displayed_connection_token.as_ref().unwrap();
    let token = token.expose().to_owned();
    let principal = h
        .state()
        .server
        .as_ref()
        .unwrap()
        .auth
        .authenticate_principal(&format!("Bearer {token}"), agent_mcp::now())
        .unwrap();
    assert_eq!(&principal.id, id);
    let permission = h.state().connection_permission(id, &target);
    assert!(!permission.read && !permission.input && !permission.local_completion);
    assert!(h.query_by_label(&token).is_some());
    assert!(
        h.ctx
            .data_mut(|data| data.get_temp::<Vec<String>>(egui::Id::new("one-screen-copies")))
            .is_none()
    );
    h.get_by_label("Copy token").click();
    h.run();
    assert_eq!(
        h.ctx
            .data_mut(|data| data.get_temp::<Vec<String>>(egui::Id::new("one-screen-copies")))
            .unwrap(),
        vec![token.clone()]
    );
    h.get_by_label("Hide").click();
    h.run();
    assert!(h.query_by_label(&token).is_none());
    h.get_by_label("Show").click();
    h.run();
    assert!(h.query_by_label(&token).is_some());
    h.state_mut().clear_connection_token();
    h.run();
    assert!(h.query_by_label(&token).is_none());
}

#[test]
fn one_screen_discarded_token_does_not_leave_copy_new_token_feedback() {
    let (bridge, selected) = fixture();
    let mut h = harness(bridge, selected);
    h.get_by_label("Issue new token").click();
    h.run();
    h.get_by_label("Issue token now").click();
    h.run();
    h.state_mut().clear_connection_token();
    h.run();
    let c = i18n::Catalog::load("en-US").unwrap();
    assert!(h.query_by_label(&c.t("cloud.one.issued", &[])).is_none());
}

#[test]
fn one_screen_existing_token_hint_keeps_the_same_readout_start() {
    let (bridge, selected) = fixture();
    let h = harness(bridge, selected);
    let c = i18n::Catalog::load("en-US").unwrap();
    let hint = h.get_by_label(&c.t("cloud.one.unavailable", &[])).rect();
    let address = h.get_by_label("https://deppy.example.com/mcp").rect();
    assert!((hint.left() - address.left()).abs() < 0.1);
}

#[test]
fn one_screen_failed_reissue_keeps_existing_access_and_never_exposes_a_token() {
    let (mut bridge, selected) = fixture();
    let target = bridge.targets[0].clone();
    bridge
        .set_connection_access(&selected, &target, true, true)
        .unwrap();
    let mut state = settings::SettingsState::default();
    assert!(
        bridge
            .issue_display_token("", true, Some(&selected), &mut state)
            .is_err()
    );
    assert!(bridge.connection_permission(&selected, &target).input);
    assert!(bridge.displayed_connection_token.is_none());
}

#[test]
fn one_screen_connection_switch_and_revocation_discard_the_displayed_secret() {
    for revoke in [false, true] {
        let (mut bridge, previous) = fixture();
        let mut state = settings::SettingsState::default();
        bridge
            .issue_display_token("New independent connection", true, None, &mut state)
            .unwrap();
        let selected = state.selected.unwrap();
        let mut h = harness(bridge, selected.clone());
        assert!(h.state().displayed_connection_token.is_some());
        if revoke {
            h.state_mut().revoke_connection(&selected);
        } else {
            h.ctx.data_mut(|data| {
                let mut state = data
                    .get_temp::<settings::SettingsState>(settings::state_id())
                    .unwrap();
                state.selected = Some(previous);
                data.insert_temp(settings::state_id(), state);
            });
        }
        h.run();
        assert!(h.state().displayed_connection_token.is_none());
        assert!(h.query_by_label("Hide").is_none());
    }
}

#[test]
fn one_screen_more_preserves_global_input_reclaim_without_revoking_reads() {
    let (mut bridge, selected) = fixture();
    let target = bridge.targets[0].clone();
    bridge
        .issue_manual_connection("Second writer", true)
        .unwrap();
    let ids = bridge
        .connection_infos()
        .into_iter()
        .filter(|info| {
            info.principal.id != agent_mcp::LEGACY_CONNECTION_ID && info.principal.input_scope
        })
        .map(|info| info.principal.id)
        .collect::<Vec<_>>();
    for id in &ids {
        bridge
            .set_connection_access(id, &target, true, true)
            .unwrap();
    }
    let mut h = harness(bridge, selected);
    h.get_by_label("Other connection methods · more settings")
        .click();
    h.run();
    let c = i18n::Catalog::load("en-US").unwrap();
    h.get_by_label(&c.t("cloud.take_control", &[])).click();
    h.run();
    for id in &ids {
        let permission = h.state().connection_permission(id, &target);
        assert!(permission.read && !permission.input);
    }
}

#[test]
fn one_screen_offline_keeps_existing_consent_visible_and_revocable() {
    let (mut bridge, selected) = fixture();
    let target = bridge.targets[0].clone();
    bridge
        .set_connection_access(&selected, &target, true, true)
        .unwrap();
    bridge.hostname.clear();
    let mut h = harness(bridge, selected.clone());
    let read = h.get_by_label("Allow this connection to read · receive answers");
    assert_eq!(
        read.accesskit_node().toggled(),
        Some(egui::accesskit::Toggled::True)
    );
    assert!(!read.accesskit_node().is_disabled());
    read.click();
    h.run();
    let permission = h.state().connection_permission(&selected, &target);
    assert!(!permission.read && !permission.input);
}

#[test]
fn one_screen_long_session_metadata_keeps_current_status_inside_its_column() {
    let (mut bridge, selected) = fixture();
    let mut target = bridge.targets[0].clone();
    target.workspace_name = "Workspace with a long name ".repeat(20);
    target.agent_line = Some("Codex · a long model and reasoning description ".repeat(20));
    bridge.set_targets(vec![target]);
    let h = harness(bridge, selected);
    let status = h.get_by_label("Running").rect();
    let read = h
        .get_by_label("Allow this connection to read · receive answers")
        .rect();
    assert!(
        status.right() <= read.center().x - layout::PERMISSION_COLUMN / 2.0,
        "status: {status:?}, read: {read:?}"
    );
}

#[test]
fn one_screen_non_codex_completion_is_unsupported_without_changing_other_grants() {
    let (mut bridge, selected) = fixture();
    let mut target = bridge.targets[0].clone();
    target.provider = Some(crate::agent_surface::AgentProvider::Claude);
    bridge.set_targets(vec![target]);
    let h = harness(bridge, selected);
    h.get_by_label("Unsupported");
    assert!(h.query_by_label("Receive local task completion").is_none());
}

#[test]
fn one_screen_native_reference_images_and_alignment() {
    let output = std::env::var_os("DEPPY_CLOUD_ONE_SCREEN_IMAGES").map(std::path::PathBuf::from);
    if let Some(path) = &output {
        std::fs::create_dir_all(path).unwrap();
    }
    for (width, theme) in [
        (1000.0, egui::Theme::Dark),
        (1000.0, egui::Theme::Light),
        (660.0, egui::Theme::Dark),
        (410.0, egui::Theme::Light),
    ] {
        let (mut bridge, selected) = fixture();
        bridge.automatic = true;
        bridge.generated_hostname = Some("deppy-demo.example".into());
        bridge
            .server
            .as_ref()
            .unwrap()
            .auth
            .rename_connection(&selected, "dot");
        let mut targets = Vec::new();
        for (i, (title, workspace, agent, provider)) in [
            (
                "결제 흐름 검토",
                "nomorevibe",
                "Claude · 세션 1",
                crate::agent_surface::AgentProvider::Claude,
            ),
            (
                "모바일 레이아웃 개선",
                "Design",
                "Codex · 세션 2",
                crate::agent_surface::AgentProvider::Codex,
            ),
            (
                "API 구현",
                "nomorevibe",
                "Codex · 세션 3",
                crate::agent_surface::AgentProvider::Codex,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let mut target = bridge.targets[0].clone();
            target.id = format!("visual-{i}");
            target.generation = format!("visual-generation-{i}");
            target.title = title.into();
            target.workspace_name = workspace.into();
            target.agent_line = Some(agent.into());
            target.provider = Some(provider);
            targets.push(target);
        }
        bridge.set_targets(targets);
        bridge.displayed_connection_token = Some((
            selected.clone(),
            secret::SecretString::new("demo_only_dot_7hK2_w9F4_n3P6".into()),
        ));
        let c = i18n::Catalog::load("ko-KR").unwrap();
        let mut first = true;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(width, 1000.0))
            .with_theme(theme)
            .build_ui_state(
                move |ui, bridge: &mut CloudAgent| {
                    if first {
                        let mut state = settings::state(ui);
                        state.selected = Some(selected.clone());
                        settings::store(ui, state);
                        first = false;
                    }
                    let rect =
                        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 1000.0));
                    let mut native = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                    native.set_clip_rect(rect);
                    bridge.contents(&mut native, &c);
                },
                bridge,
            );
        crate::fonts::install_cjk_fallback(&h.ctx, None, "JetBrains Mono", "Regular");
        h.run();
        let add = h.get_by_label("연결 추가").rect();
        assert!((add.height() - 34.0).abs() < 0.1);
        let page_margin = if width < 600.0 {
            16.0
        } else if width < 800.0 {
            20.0
        } else {
            26.0
        };
        let service = h.get_by_label("외부 접속 준비됨").rect();
        assert!(
            (service.right() - (width - page_margin)).abs() < 0.1,
            "right-aligned service at width{width}: {service:?}"
        );
        if width == 410.0 {
            let token = h.get_by_label("demo_only_dot_7hK2_w9F4_n3P6").rect();
            assert!(token.bottom() < 500.0, "narrow token: {token:?}");
            assert!(h.get_by_label("공유할 세션").rect().top() < 650.0);
            assert!(h.get_by_label("새 토큰 발급").rect().right() <= width - 16.0);
        }
        if width >= 660.0 {
            let read_label = i18n::Catalog::load("ko-KR")
                .unwrap()
                .t("cloud.connection_read", &[]);
            let read = h
                .query_all_by_role_and_label(egui::accesskit::Role::CheckBox, &read_label)
                .collect::<Vec<_>>();
            // 세션별 위젯이 서로 같은 열에 놓이며 별도 target ID를 유지한다.
            assert_eq!(read.len(), 3);
            assert!(
                (read[0].rect().center().x - h.get_by_label("화면 보기").rect().center().x).abs()
                    < 0.1
            );
            if read.len() > 1 {
                assert!((read[0].rect().center().x - read[1].rect().center().x).abs() < 0.1);
            }
            assert!(add.right() <= width);
        }
        if width == 1000.0 {
            let address = h.get_by_label("https://deppy-demo.example/mcp").rect();
            let token = h.get_by_label("demo_only_dot_7hK2_w9F4_n3P6").rect();
            assert!(
                (token.center().y - address.center().y - 42.0).abs() < 0.1,
                "34pt row + 8pt gap: {address:?}, {token:?}"
            );
            assert!(
                // 본문 26 + 테두리 1 + 안쪽 여백 14 + 라벨/간격 104.
                (address.left() - 145.0).abs() < 0.1,
                "address start: {address:?}"
            );
            assert!(
                (token.left() - address.left()).abs() < 1.0,
                "token start: {token:?}"
            );
        }
        if let Some(path) = &output {
            h.render()
                .unwrap()
                .save(path.join(format!("native-one-screen-{}-{theme:?}.png", width as u32)))
                .unwrap();
        }
    }
}
