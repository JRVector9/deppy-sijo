use super::*;
use crate::cloud_agent::{Connection, Target};
use agent_mcp::{ConnectionState, now};
use base64::Engine as _;
use egui_kittest::kittest::Queryable as _;
use sha2::{Digest, Sha256};

fn connection_fixture() -> (CloudAgent, Target, String, String, String) {
    let mut bridge = CloudAgent::memory();
    bridge.automatic = false;
    bridge.server = Some(agent_mcp::Server::start(0, "", || {}).unwrap());
    bridge.connection = Connection::Ready;
    let auth = &bridge.server.as_ref().unwrap().auth;
    let (review, token) = auth
        .mint_manual_connection("Review Bot", true, now())
        .unwrap();
    let (research, _) = auth
        .mint_manual_connection("Research Bot", false, now())
        .unwrap();
    let mut target = Target::fixture("review-session", "generation-1");
    target.title = "Review pane".into();
    target.agent_line = Some("Codex · gpt-6.1-sol · xhigh".into());
    bridge.set_targets(vec![target.clone()]);
    (bridge, target, review.id, research.id, token.to_string())
}

fn harness(bridge: CloudAgent) -> egui_kittest::Harness<'static, CloudAgent> {
    let catalog = i18n::Catalog::load("en-US").unwrap();
    egui_kittest::Harness::builder()
        .with_size(egui::vec2(1500.0, 1800.0))
        .build_ui_state(
            move |ui, state: &mut CloudAgent| {
                state.contents(ui, &catalog);
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
                        let id = egui::Id::new("test-cloud-clipboard-copies");
                        let mut prior = data.get_temp::<Vec<String>>(id).unwrap_or_default();
                        prior.extend(copies);
                        data.insert_temp(id, prior);
                    });
                }
                let urls = ui.ctx().output(|output| {
                    output
                        .commands
                        .iter()
                        .filter_map(|command| {
                            if let egui::OutputCommand::OpenUrl(url) = command {
                                Some(url.url.clone())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                });
                if !urls.is_empty() {
                    ui.ctx().data_mut(|data| {
                        let id = egui::Id::new("test-cloud-open-urls");
                        let mut prior = data.get_temp::<Vec<String>>(id).unwrap_or_default();
                        prior.extend(urls);
                        data.insert_temp(id, prior);
                    });
                }
            },
            bridge,
        )
}

#[test]
fn management_ui_issued_credential_waits_for_actual_authentication() {
    let (bridge, _, review, _, token) = connection_fixture();
    let mut harness = harness(bridge);
    assert!(
        harness
            .query_by_label("Review Bot · Credential issued · waiting for authentication")
            .is_some()
    );
    let principal = harness
        .state()
        .server
        .as_ref()
        .unwrap()
        .auth
        .authenticate_principal(&format!("Bearer {token}"), now())
        .unwrap();
    assert_eq!(principal.id, review);
    harness.run();
    assert!(
        harness
            .query_by_label("Review Bot · Authenticated · no successful tools yet")
            .is_some()
    );
    assert!(
        harness
            .query_by_label("Review Bot · Tool call succeeded")
            .is_none()
    );
}

#[test]
fn management_ui_permissions_and_revoke_affect_only_the_selected_connection() {
    let (bridge, target, review, research, _) = connection_fixture();
    let mut harness = harness(bridge);
    harness
        .get_by_label("Review Bot · Credential issued · waiting for authentication")
        .click();
    harness.run();
    harness
        .get_by_role_and_label(
            egui::accesskit::Role::CheckBox,
            "Allow this connection to read · receive answers",
        )
        .click();
    harness.run();
    harness
        .get_by_role_and_label(
            egui::accesskit::Role::CheckBox,
            "Allow this connection to input",
        )
        .click();
    harness.run();
    let granted = harness.state().connection_permission(&review, &target);
    assert!(granted.read && granted.input);
    assert!(
        !harness
            .state()
            .connection_permission(&research, &target)
            .read
    );
    harness.get_by_label("Revoke this connection").click();
    harness.run();
    let connections = harness.state().connection_infos();
    assert_eq!(
        connections
            .iter()
            .find(|info| info.principal.id == review)
            .unwrap()
            .state,
        ConnectionState::Revoked
    );
    assert_eq!(
        connections
            .iter()
            .find(|info| info.principal.id == research)
            .unwrap()
            .state,
        ConnectionState::Active
    );
    assert!(
        !harness
            .state()
            .connection_permission(&review, &target)
            .input
    );
}

#[test]
fn management_ui_local_takeover_revokes_the_displayed_lease() {
    let (mut bridge, target, review, _, _) = connection_fixture();
    bridge
        .set_connection_access(&review, &target, true, true)
        .unwrap();
    bridge
        .control
        .acquire(&review, &target.id, &target.generation, None, now(), 90)
        .unwrap();
    let mut harness = harness(bridge);
    harness
        .get_by_label("Review Bot · Credential issued · waiting for authentication")
        .click();
    harness.run();
    assert!(
        harness
            .query_by_label("Input controller: Review Bot")
            .is_some()
    );
    harness
        .get_by_label("Take back this session's input")
        .click();
    harness.run();
    assert!(harness.state().connection_lease(&target).is_none());
    assert!(
        !harness
            .state()
            .connection_permission(&review, &target)
            .input
    );
    assert!(harness.query_by_label("Input controller: You").is_some());
}

#[test]
fn management_ui_connection_rename_updates_the_real_registry() {
    let (bridge, _, review, _, _) = connection_fixture();
    let mut harness = harness(bridge);
    harness
        .get_by_label("Review Bot · Credential issued · waiting for authentication")
        .click();
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::TextInput, "Connection name")
        .click();
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::TextInput, "Connection name")
        .type_text("Reviewer");
    harness.run();
    harness.get_by_label("Save connection name").click();
    harness.run();
    assert_eq!(
        harness
            .state()
            .connection_infos()
            .iter()
            .find(|info| info.principal.id == review)
            .unwrap()
            .principal
            .name,
        "Reviewer"
    );
    assert!(
        harness
            .query_by_label("Reviewer · Credential issued · waiting for authentication")
            .is_some()
    );
}

#[test]
fn management_ui_expired_connection_explains_reauthentication_without_restoring_input() {
    let mut bridge = CloudAgent::memory();
    bridge.server = Some(agent_mcp::Server::start(0, "", || {}).unwrap());
    bridge.server.as_mut().unwrap().auth = std::sync::Arc::new(agent_mcp::Auth::new_at(
        now().saturating_sub(agent_mcp::TOKEN_TTL + 1),
    ));
    let mut harness = harness(bridge);
    assert!(
        harness
            .query_by_label("Shared manual bearer · Expired · re-authenticate")
            .is_some()
    );
    assert!(harness.query_by_label("For a manual connection, issue a new token and update the connector; for OAuth, repeat authentication and approve the new request here. Recheck session permissions; previous input is never replayed.").is_some());
    assert!(harness.state().grants.is_empty());
    harness
        .get_by_label("Re-authentication instructions")
        .click();
    harness.run();
    assert!(harness.state().grants.is_empty());
}

#[test]
fn management_ui_profile_save_load_delete_preserve_supported_endpoint_coordinates() {
    let mut bridge = CloudAgent::memory();
    bridge.automatic = false;
    bridge.hostname = "mac.example.ts.net:8443".into();
    bridge.profile_name = "Stable Mac".into();
    let mut harness = harness(bridge);
    harness.get_by_label("Connection profiles").click();
    harness.run();
    harness.get_by_label("Save profile").click();
    harness.run();
    assert_eq!(harness.state().profiles.len(), 1);
    assert_eq!(
        harness.state().profiles[0].public_host,
        "mac.example.ts.net:8443"
    );
    harness.state_mut().hostname = "other.example.com".into();
    harness.state_mut().port = 8745;
    harness.run();
    harness.get_by_label("Load profile").click();
    harness.run();
    assert_eq!(harness.state().hostname, "mac.example.ts.net:8443");
    assert_eq!(harness.state().port, 8739);
    assert!(
        harness.state().grants.is_empty(),
        "profiles never restore session consent"
    );
    harness.get_by_label("Delete profile").click();
    harness.run();
    assert!(harness.state().profiles.is_empty());
}

#[test]
fn management_ui_reconnect_opt_out_cancels_pending_and_future_recovery() {
    let mut bridge = CloudAgent::memory();
    bridge.reconnect = true;
    bridge.recovery.start(true);
    let at = std::time::Instant::now();
    bridge.recovery.failed(at);
    assert!(bridge.recovery.due().is_some());
    let mut harness = harness(bridge);
    harness.get_by_label("Connection profiles").click();
    harness.run();
    harness
        .get_by_role_and_label(
            egui::accesskit::Role::CheckBox,
            "Recover the automatic address after a temporary network failure",
        )
        .click();
    harness.run();
    assert!(!harness.state().reconnect);
    assert!(
        harness.state().recovery.due().is_none(),
        "opting out must cancel an already scheduled automatic retry"
    );
    assert!(
        !harness
            .state_mut()
            .recovery
            .retry(at + std::time::Duration::from_secs(120)),
        "a cancelled retry must never become due later"
    );
    harness.state_mut().recovery.failed(at);
    assert!(
        harness.state().recovery.due().is_none(),
        "further failures must not schedule recovery after opt-out"
    );
}

#[test]
fn management_ui_scoped_answer_navigation_and_widgets_isolate_duplicate_external_ids() {
    let (mut bridge, _, review, research, _) = connection_fixture();
    let records = [
        (&review, "review-answer-session", "Review Bot's own answer"),
        (
            &research,
            "research-answer-session",
            "Research Bot's own answer",
        ),
    ]
    .into_iter()
    .map(|(connection, session, message)| agent_mcp::Record {
        id: "same-external-answer".into(),
        connection_id: Some(connection.clone()),
        run_id: None,
        generation: Some("generation-1".into()),
        sequence: 0,
        tool: "notify".into(),
        workspace: "workspace".into(),
        session: session.into(),
        created: now() as i64,
        outcome: r#"{"status":"stored"}"#.into(),
        message: message.into(),
    })
    .collect();
    bridge.install_history(1, records);
    let research_notification_key = bridge
        .answers
        .iter()
        .find(|answer| answer.session == "research-answer-session")
        .unwrap()
        .id
        .clone();
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let header = |session: &str| {
        format!(
            "{} · {} · {}",
            catalog.t("cloud.history_answer", &[]),
            crate::ui::notifications::relative_time_label(&catalog, 0),
            session.chars().take(8).collect::<String>(),
        )
    };
    let review_header = header("review-answer-session");
    let research_header = header("research-answer-session");
    let mut harness = harness(bridge);
    assert!(harness.query_by_label("Review Bot's own answer").is_none());
    assert!(
        harness
            .query_by_label("Research Bot's own answer")
            .is_none()
    );
    // Use the actual scoped key supplied to cloud-answer notifications.
    harness.state_mut().selected_record = Some(research_notification_key.clone());
    harness.run();
    harness.get_by_label("Research Bot's own answer");
    assert!(harness.query_by_label("Review Bot's own answer").is_none());

    harness.get_by_label(&review_header).click();
    harness.run();
    harness.get_by_label("Review Bot's own answer");
    harness.get_by_label("Research Bot's own answer");
    harness.get_by_label(&research_header).click();
    harness.run();
    harness.get_by_label("Review Bot's own answer");
    assert!(
        harness
            .query_by_label("Research Bot's own answer")
            .is_none()
    );

    harness.state_mut().selected_record = Some(research_notification_key);
    harness.run();
    harness.get_by_label("Research Bot's own answer");
    harness.get_by_label(&review_header).click();
    harness.run();
    assert!(harness.query_by_label("Review Bot's own answer").is_none());
    harness.get_by_label("Research Bot's own answer");
}

fn management_call(
    bridge: &mut CloudAgent,
    target: &Target,
    token: &str,
    tool: &str,
    args: serde_json::Value,
) {
    let auth = &bridge.server.as_ref().unwrap().auth;
    let header = format!("Bearer {token}");
    let (reply, response) = std::sync::mpsc::sync_channel(1);
    let request = agent_mcp::Request {
        epoch: auth.authenticate(&header, now()).unwrap(),
        principal: auth.authenticate_principal(&header, now()).unwrap(),
        access_key: Some(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(token.as_bytes())),
        ),
        input_scope: true,
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(3),
        tool: tool.into(),
        args,
        reply,
    };
    assert!(
        bridge
            .prepare_management(&request, Some(target))
            .unwrap()
            .is_none()
    );
    bridge.drain_history(|_, _| panic!("run reporting must not send terminal input"));
    response
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap()
        .unwrap();
}

fn running_fixture() -> (CloudAgent, Target, String, String) {
    let (mut bridge, target, review, _, token) = connection_fixture();
    bridge
        .set_connection_access(&review, &target, true, false)
        .unwrap();
    management_call(
        &mut bridge,
        &target,
        &token,
        "start_run",
        serde_json::json!({
            "session_id":target.id,"generation":target.generation,"run_id":"review-run","title":"Inspect project","instruction":"Read the project and report findings"
        }),
    );
    management_call(
        &mut bridge,
        &target,
        &token,
        "update_run",
        serde_json::json!({
            "session_id":target.id,"generation":target.generation,"run_id":"review-run","state":"running","progress":37,"message":"Inspecting dependencies"
        }),
    );
    (bridge, target, review, token)
}

#[test]
fn management_ui_cloud_report_is_separate_from_local_status_and_keeps_result_origin() {
    let (mut bridge, target, _, token) = running_fixture();
    management_call(
        &mut bridge,
        &target,
        &token,
        "update_run",
        serde_json::json!({
            "session_id":target.id,"generation":target.generation,"run_id":"review-run","state":"completed","progress":100,"message":"Review complete","result":"Found the dependency issue","artifacts":[{"label":"Review report","uri":"https://example.com/report"}]
        }),
    );
    let mut harness = harness(bridge);
    harness
        .get_by_label("Inspect project · Completed · Review Bot")
        .click();
    harness.run();
    harness.get_by_label("Progress: 100% · agent reported");
    harness.get_by_label("Local status for this run's session: Codex · gpt-6.1-sol · xhigh");
    harness.get_by_label("Found the dependency issue");
    // egui's selectable link text is exposed as a Label; verify the actual link intent.
    harness.get_by_label("Review report").click();
    harness.run();
    let opened = harness.ctx.data_mut(|data| {
        data.get_temp::<Vec<String>>(egui::Id::new("test-cloud-open-urls"))
            .unwrap_or_default()
    });
    assert_eq!(opened, vec!["https://example.com/report"]);
    harness.get_by_label("Original session: review-session · generation-1");
    assert_eq!(harness.state().cloud_runs()[0].session, target.id);
}

#[test]
fn management_ui_cancel_request_stays_running_until_the_agent_acknowledges() {
    let (bridge, target, _, token) = running_fixture();
    let mut harness = harness(bridge);
    harness
        .get_by_label("Inspect project · Running · Review Bot")
        .click();
    harness.run();
    harness.get_by_label("Request cancellation").click();
    harness.run();
    assert_eq!(
        harness.state().cloud_runs()[0].state,
        agent_mcp::RunState::Running
    );
    harness
        .state_mut()
        .drain_history(|_, _| panic!("cancellation must not send Ctrl+C"));
    harness.run();
    harness.get_by_label("Cancellation requested · awaiting agent acknowledgement");
    assert_eq!(
        harness.state().cloud_runs()[0].state,
        agent_mcp::RunState::Running
    );
    management_call(
        harness.state_mut(),
        &target,
        &token,
        "update_run",
        serde_json::json!({
            "session_id":target.id,"generation":target.generation,"run_id":"review-run","state":"cancelled","message":"Stopped my analysis"
        }),
    );
    harness.run();
    harness.get_by_label("Cancellation acknowledged by agent");
    assert_eq!(
        harness.state().cloud_runs()[0].state,
        agent_mcp::RunState::Cancelled
    );
}

#[test]
fn management_ui_run_search_hides_other_runs_without_changing_the_cache() {
    let (mut bridge, target, _, token) = running_fixture();
    management_call(
        &mut bridge,
        &target,
        &token,
        "start_run",
        serde_json::json!({
            "session_id":target.id,"generation":target.generation,"run_id":"other-run","title":"Other task"
        }),
    );
    let mut harness = harness(bridge);
    harness.get_by_label("Other task · Queued · Review Bot");
    harness
        .get_by_role_and_label(egui::accesskit::Role::TextInput, "Search cloud work")
        .click();
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::TextInput, "Search cloud work")
        .type_text("Inspect");
    harness.run();
    harness.get_by_label("Inspect project · Running · Review Bot");
    assert!(
        harness
            .query_by_label("Other task · Queued · Review Bot")
            .is_none()
    );
    assert_eq!(harness.state().cloud_runs().len(), 2);
}

#[test]
fn management_ui_independent_manual_token_is_copied_once_without_default_session_consent() {
    let (bridge, target, _, _, _) = connection_fixture();
    let mut harness = harness(bridge);
    harness
        .get_by_role_and_label(
            egui::accesskit::Role::TextInput,
            "New manual connection name",
        )
        .click();
    harness.run();
    harness
        .get_by_role_and_label(
            egui::accesskit::Role::TextInput,
            "New manual connection name",
        )
        .type_text("Audit Bot");
    harness.run();
    harness
        .get_by_label("Issue and copy independent token")
        .click();
    harness.run();
    let copied = harness.ctx.data_mut(|data| {
        data.get_temp::<Vec<String>>(egui::Id::new("test-cloud-clipboard-copies"))
            .unwrap_or_default()
    });
    assert_eq!(copied.len(), 1);
    let token = copied[0].to_owned();
    let principal = harness
        .state()
        .server
        .as_ref()
        .unwrap()
        .auth
        .authenticate_principal(&format!("Bearer {token}"), now())
        .unwrap();
    assert_eq!(principal.name, "Audit Bot");
    assert!(
        !principal.input_scope,
        "independent credentials default to read scope"
    );
    let permission = harness
        .state()
        .connection_permission(&principal.id, &target);
    assert!(
        !permission.read && !permission.input,
        "authentication never supplies owner session consent"
    );
    assert!(harness.state().redact(&token).contains("REDACTED"));
    assert!(
        harness.query_by_label(&token).is_none(),
        "token is never rendered in the registry"
    );
    harness.run();
    assert!(
        harness
            .ctx
            .data_mut(|data| data
                .get_temp::<Vec<String>>(egui::Id::new("test-cloud-clipboard-copies"))
                .unwrap_or_default())
            .len()
            == 1,
        "subsequent frames must not copy the token again"
    );
}

#[test]
fn management_ui_controls_fit_a_narrow_native_settings_pane() {
    let (bridge, _, _, _) = running_fixture();
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(720.0, 1800.0))
        .build_ui_state(
            move |ui, state: &mut CloudAgent| {
                ui.ctx().set_visuals(egui::Visuals::dark());
                state.contents(ui, &catalog);
            },
            bridge,
        );
    harness
        .get_by_label("Review Bot · Authenticated · no successful tools yet")
        .click();
    harness.run();
    harness
        .get_by_label("Inspect project · Running · Review Bot")
        .click();
    harness.run();
    for (role, label) in [
        (
            egui::accesskit::Role::TextInput,
            "New manual connection name",
        ),
        (
            egui::accesskit::Role::Button,
            "Issue and copy independent token",
        ),
        (
            egui::accesskit::Role::CheckBox,
            "Allow this connection to read · receive answers",
        ),
        (
            egui::accesskit::Role::CheckBox,
            "Allow this connection to input",
        ),
        (egui::accesskit::Role::Button, "Request cancellation"),
    ] {
        let bounds = harness.get_by_role_and_label(role, label).rect();
        assert!(
            bounds.left() >= 0.0 && bounds.right() <= 720.0,
            "control is clipped: {label} {bounds:?}"
        );
        assert!(
            bounds.top() >= 0.0 && bounds.bottom() <= 1800.0,
            "control is outside the settings pane: {label} {bounds:?}"
        );
    }
    let path = std::env::temp_dir().join("deppy-cloud-settings-native-720-20261013.png");
    harness.render().unwrap().save(&path).unwrap();
    eprintln!("Native settings image: {}", path.display());
}

#[test]
fn management_ui_navigation_queues_the_original_workspace_and_session() {
    let (mut bridge, original, review, _) = running_fixture();
    let mut active = Target::fixture("active-session", "active-generation");
    active.workspace = "another-workspace".into();
    bridge.set_targets(vec![active, original.clone()]);
    let mut harness = harness(bridge);
    harness
        .get_by_label("Inspect project · Running · Review Bot")
        .click();
    harness.run();
    harness.get_by_label("Open original session").click();
    harness.run();
    assert_eq!(
        harness.state_mut().take_run_navigation(),
        Some((original.workspace.clone(), original.id.clone()))
    );
    assert!(
        harness.state_mut().take_run_navigation().is_none(),
        "native navigation intent is consumed once"
    );
    assert_eq!(
        harness.state().cloud_runs()[0].state,
        agent_mcp::RunState::Running
    );
    assert!(
        !harness
            .state()
            .connection_permission(&review, &original)
            .input,
        "navigation grants no input permission"
    );
}
