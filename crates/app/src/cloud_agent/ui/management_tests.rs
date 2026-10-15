use super::*;
use crate::cloud_agent::{Connection, Target};
use agent_mcp::{ConnectionState, now};
use base64::Engine as _;
use egui_kittest::kittest::{NodeT as _, Queryable as _};
use sha2::{Digest, Sha256};

pub(super) fn connection_fixture() -> (CloudAgent, Target, String, String, String) {
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
    target.known_ai = true;
    target.local_status = Some(runtime::SessionStatus::Running);
    bridge.set_targets(vec![target.clone()]);
    (bridge, target, review.id, research.id, token.to_string())
}

fn harness(bridge: CloudAgent) -> egui_kittest::Harness<'static, CloudAgent> {
    let catalog = i18n::Catalog::load("en-US").unwrap();
    egui_kittest::Harness::builder()
        .with_size(egui::vec2(1500.0, 1800.0))
        .build_ui_state(
            move |ui, state: &mut CloudAgent| {
                if state.has_cloud_runs()
                    && ui
                        .ctx()
                        .data_mut(|d| d.get_temp::<bool>(egui::Id::new("test-initial-work")))
                        .is_none()
                {
                    let mut view = settings::state(ui);
                    view.tab = settings::Tab::Work;
                    settings::store(ui, view);
                    ui.ctx()
                        .data_mut(|d| d.insert_temp(egui::Id::new("test-initial-work"), true));
                }
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
    harness.get_by_label("Access permissions").click();
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
    harness.get_by_label("Connections").click();
    harness.run();
    harness
        .get_by_label("Connection information and management")
        .click();
    harness.run();
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
    harness.get_by_label("Access permissions").click();
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
    harness
        .get_by_role_and_label(
            egui::accesskit::Role::CheckBox,
            "Allow this connection to input",
        )
        .click();
    harness.run();
    assert!(
        harness
            .state()
            .connection_permission(&review, &target)
            .input
    );
    assert!(
        harness.state().connection_lease(&target).is_none(),
        "owner consent cannot auto-acquire control"
    );
    harness
        .state_mut()
        .control
        .acquire(&review, &target.id, &target.generation, None, now(), 90)
        .unwrap();
    harness.run();
    harness
        .get_by_role_and_label(
            egui::accesskit::Role::CheckBox,
            "Allow this connection to read · receive answers",
        )
        .click();
    harness.run();
    let access = harness.state().connection_permission(&review, &target);
    assert!(!access.read && !access.input);
    assert!(
        harness.state().connection_lease(&target).is_none(),
        "read-off revokes input and the existing lease"
    );
}

#[test]
fn management_ui_connection_rename_updates_the_real_registry() {
    let (bridge, _, review, _, _) = connection_fixture();
    let mut harness = harness(bridge);
    harness
        .get_by_label("Review Bot · Credential issued · waiting for authentication")
        .click();
    harness.run();
    harness.get_by_label("Edit").click();
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
    harness.get_by_label("Advanced settings").click();
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
    harness.get_by_label("⋯").click();
    harness.run();
    harness.get_by_label("Load profile").click();
    harness.run();
    assert_eq!(harness.state().hostname, "mac.example.ts.net:8443");
    assert_eq!(harness.state().port, 8739);
    assert!(
        harness.state().grants.is_empty(),
        "profiles never restore session consent"
    );
    harness.get_by_label("⋯").click();
    harness.run();
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
    harness.get_by_label("Advanced settings").click();
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
    assert!(harness.query_by_label("Review Bot's own answer").is_some());
    assert!(
        harness
            .query_by_label("Research Bot's own answer")
            .is_none()
    );
    harness.get_by_label(&research_header).click();
    harness.run();
    assert!(harness.query_by_label("Review Bot's own answer").is_none());
    assert!(
        harness
            .query_by_label("Research Bot's own answer")
            .is_some()
    );
    harness.state_mut().selected_record = Some(research_notification_key);
    harness.run();
    assert!(
        harness.state().selected_record.is_none(),
        "notification navigation consumed once"
    );
    harness.get_by_label(&review_header).click();
    harness.run();
    assert!(
        harness
            .query_by_label("Research Bot's own answer")
            .is_none()
    );
    harness.run();
    assert!(
        harness.query_by_label("Review Bot's own answer").is_some(),
        "selection persists after notification"
    );
}

const LATE_NOTIFY_FIRST: &str = "fixture-notify-access-token-123456789";
const LATE_NOTIFY_SECOND: &str = "fixture-notify-replaced-token-987654321";
const LATE_NOTIFY_RAW: &str = "First: fixture-notify-\naccess-token-123456789. Second: fixture-notify-\nreplaced-token-987654321.";
const LATE_NOTIFY_FIRST_MASKED: &str =
    "First: [REDACTED]. Second: fixture-notify-\nreplaced-token-987654321.";
const LATE_NOTIFY_BOTH_MASKED: &str = "First: [REDACTED]. Second: [REDACTED].";

fn late_secret_notify_fixture() -> (CloudAgent, Target, String) {
    late_secret_notify_fixture_with_run(LATE_NOTIFY_FIRST)
}

fn late_secret_notify_fixture_with_run(run_id: &str) -> (CloudAgent, Target, String) {
    use crate::cloud_history_worker::{Job, ResultKind};
    let (mut bridge, target, connection, _, _) = connection_fixture();
    let operation = "late-notify-answer";
    let key = agent_mcp::operation_key(&connection, operation).unwrap();
    let token = bridge.operation_token();
    assert!(
        bridge
            .history
            .request(
                token,
                Job::ClaimScoped {
                    id: operation.into(),
                    tool: "notify".into(),
                    args: serde_json::json!({"message":LATE_NOTIFY_RAW}),
                    workspace: target.workspace.clone(),
                    session: target.id.clone(),
                    context: agent_mcp::OperationContext {
                        connection_id: connection.clone(),
                        operation_id: operation.into(),
                        run_id: Some(run_id.into()),
                        generation: target.generation.clone(),
                    },
                }
            )
            .is_ok()
    );
    assert!(matches!(
        bridge.history.wait().unwrap().result,
        ResultKind::ClaimScoped(Ok(agent_mcp::Claim::New))
    ));
    let token = bridge.operation_token();
    assert!(
        bridge
            .history
            .request(
                token,
                Job::FinishScoped {
                    id: operation.into(),
                    connection_id: connection,
                    outcome: serde_json::json!({"status":"stored"}),
                    message: LATE_NOTIFY_RAW.into(),
                }
            )
            .is_ok()
    );
    assert!(matches!(
        bridge.history.wait().unwrap().result,
        ResultKind::FinishScoped(Ok(()))
    ));
    // Reuse the actual OpenPage/bootstrap path before either credential exists.
    bridge.history_initialized = false;
    bridge.drain_history(|_, _| panic!("notify bootstrap must not enter terminal input"));
    assert_eq!(bridge.cloud_answers()[0].message, LATE_NOTIFY_RAW);
    assert_eq!(bridge.cloud_answers()[0].id, key);
    (bridge, target, key)
}

#[test]
fn late_secret_notify_cloud_snapshot_rescrubs_loaded_history_and_replacement() {
    let (bridge, target, key) = late_secret_notify_fixture();
    let first = secret::SecretString::new(LATE_NOTIFY_FIRST.into());
    let first_lease = bridge.redaction.acquire_execution_lease(&[&first]).unwrap();
    let snapshot = bridge.cloud_snapshot();
    let answer = &snapshot.answers[0];
    assert_eq!(answer.body, LATE_NOTIFY_FIRST_MASKED);
    assert_eq!(answer.id, key);
    assert_eq!(answer.workspace_id, target.workspace);
    assert_eq!(answer.session_id, target.id);
    assert_eq!(answer.generation, target.generation);
    assert_eq!(answer.run_id.as_deref(), Some(LATE_NOTIFY_FIRST));
    drop(first_lease);
    let second = secret::SecretString::new(LATE_NOTIFY_SECOND.into());
    let _second_lease = bridge
        .redaction
        .acquire_execution_lease(&[&second])
        .unwrap();
    assert_eq!(
        bridge.cloud_snapshot().answers[0].body,
        LATE_NOTIFY_BOTH_MASKED
    );
}

#[test]
fn late_secret_notify_settings_masks_display_and_actual_copy_after_replacement() {
    let (mut bridge, _, key) = late_secret_notify_fixture();
    bridge.selected_record = Some(key);
    let mut harness = harness(bridge);
    harness.get_by_label(LATE_NOTIFY_RAW);
    let first = secret::SecretString::new(LATE_NOTIFY_FIRST.into());
    let first_lease = harness
        .state()
        .redaction
        .acquire_execution_lease(&[&first])
        .unwrap();
    harness.run();
    harness.get_by_label(LATE_NOTIFY_FIRST_MASKED);
    assert!(harness.query_by_label(LATE_NOTIFY_RAW).is_none());
    harness.get_by_label("Copy answer").click();
    harness.run();
    let copies = harness.ctx.data_mut(|data| {
        data.get_temp::<Vec<String>>(egui::Id::new("test-cloud-clipboard-copies"))
            .unwrap_or_default()
    });
    assert_eq!(
        copies.last().map(String::as_str),
        Some(LATE_NOTIFY_FIRST_MASKED)
    );
    drop(first_lease);
    let second = secret::SecretString::new(LATE_NOTIFY_SECOND.into());
    let _second_lease = harness
        .state()
        .redaction
        .acquire_execution_lease(&[&second])
        .unwrap();
    harness.run();
    harness.get_by_label(LATE_NOTIFY_BOTH_MASKED);
    harness.get_by_label("Copy answer").click();
    harness.run();
    let copies = harness.ctx.data_mut(|data| {
        data.get_temp::<Vec<String>>(egui::Id::new("test-cloud-clipboard-copies"))
            .unwrap_or_default()
    });
    assert_eq!(
        copies.last().map(String::as_str),
        Some(LATE_NOTIFY_BOTH_MASKED)
    );
}

#[test]
fn late_secret_notify_session_safe_view_masks_display_and_actual_copy() {
    let (bridge, target, _) = late_secret_notify_fixture();
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let mut selected = None;
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(900.0, 700.0))
        .build_ui_state(
            move |ui, state: &mut (CloudAgent, Vec<String>)| {
                let answers = state.0.cloud_answers();
                crate::ui::cloud_answer::contents(
                    ui,
                    &answers,
                    &target.id,
                    &mut selected,
                    &catalog,
                );
                state.1.extend(ui.ctx().output(|output| {
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
                }));
            },
            (bridge, Vec::new()),
        );
    harness.get_by_label(LATE_NOTIFY_RAW);
    let first = secret::SecretString::new(LATE_NOTIFY_FIRST.into());
    let first_lease = harness
        .state()
        .0
        .redaction
        .acquire_execution_lease(&[&first])
        .unwrap();
    harness.run();
    harness.get_by_label(LATE_NOTIFY_FIRST_MASKED);
    assert!(harness.query_by_label(LATE_NOTIFY_RAW).is_none());
    harness.get_by_label("Copy answer").click();
    harness.run();
    assert_eq!(
        harness.state().1.last().map(String::as_str),
        Some(LATE_NOTIFY_FIRST_MASKED)
    );
    drop(first_lease);
    let second = secret::SecretString::new(LATE_NOTIFY_SECOND.into());
    let _second_lease = harness
        .state()
        .0
        .redaction
        .acquire_execution_lease(&[&second])
        .unwrap();
    harness.run();
    harness.get_by_label(LATE_NOTIFY_BOTH_MASKED);
    harness.get_by_label("Copy answer").click();
    harness.run();
    assert_eq!(
        harness.state().1.last().map(String::as_str),
        Some(LATE_NOTIFY_BOTH_MASKED)
    );
}

#[test]
fn late_secret_notify_run_label_snapshot_masks_display_preserves_reference() {
    let (bridge, _, _) = late_secret_notify_fixture();
    let first = secret::SecretString::new(LATE_NOTIFY_FIRST.into());
    let _lease = bridge.redaction.acquire_execution_lease(&[&first]).unwrap();
    let answer = &bridge.cloud_snapshot().answers[0];
    assert_eq!(answer.run_id.as_deref(), Some(LATE_NOTIFY_FIRST));
    assert_eq!(answer.run_label.as_deref(), Some("[REDACTED]"));
}

#[test]
fn late_secret_notify_run_label_snapshot_expands_mask_without_changing_reference() {
    let value = "abcxyz";
    let run_id = format!("{value}-").repeat(18);
    let (bridge, _, _) = late_secret_notify_fixture_with_run(&run_id);
    let secret = secret::SecretString::new(value.into());
    let _lease = bridge
        .redaction
        .acquire_execution_lease(&[&secret])
        .unwrap();
    let snapshot = bridge.cloud_snapshot();
    let answer = &snapshot.answers[0];
    assert_eq!(answer.run_id.as_deref(), Some(run_id.as_str()));
    let label = answer.run_label.as_deref().unwrap();
    assert_eq!(label, "[REDACTED]-".repeat(18));
    assert!(snapshot.validate().is_ok());
}

#[test]
fn late_secret_notify_run_label_session_header_masks_display_preserves_reference() {
    let (bridge, target, _) = late_secret_notify_fixture();
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let header = |run: &str| {
        format!(
            "{} · Review Bot · {run} · {}",
            catalog.t("cloud.history_answer", &[]),
            crate::ui::notifications::relative_time_label(&catalog, 0),
        )
    };
    let raw_header = header(LATE_NOTIFY_FIRST);
    let masked_header = header("[REDACTED]");
    let mut selected = None;
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(900.0, 700.0))
        .build_ui_state(
            move |ui, bridge: &mut CloudAgent| {
                let answers = bridge.cloud_answers();
                crate::ui::cloud_answer::contents(
                    ui,
                    &answers,
                    &target.id,
                    &mut selected,
                    &catalog,
                );
            },
            bridge,
        );
    harness.get_by_label(&raw_header);
    let first = secret::SecretString::new(LATE_NOTIFY_FIRST.into());
    let _lease = harness
        .state()
        .redaction
        .acquire_execution_lease(&[&first])
        .unwrap();
    harness.run();
    harness.get_by_label(&masked_header);
    assert!(harness.query_by_label(&raw_header).is_none());
    assert_eq!(
        harness.state().cloud_answers()[0].run.as_deref(),
        Some(LATE_NOTIFY_FIRST)
    );
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

pub(super) fn running_fixture() -> (CloudAgent, Target, String, String) {
    running_fixture_with_id("review-run")
}

fn running_fixture_with_id(run_id: &str) -> (CloudAgent, Target, String, String) {
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
            "session_id":target.id,"generation":target.generation,"run_id":run_id,"title":"Inspect project","instruction":"Read the project and report findings"
        }),
    );
    management_call(
        &mut bridge,
        &target,
        &token,
        "update_run",
        serde_json::json!({
            "session_id":target.id,"generation":target.generation,"run_id":run_id,"state":"running","progress":37,"message":"Inspecting dependencies"
        }),
    );
    (bridge, target, review, token)
}

#[test]
fn late_secret_run_reference_footer_masks_display_preserves_identity() {
    let (bridge, target, connection, _) = running_fixture_with_id(LATE_NOTIFY_FIRST);
    let raw_footer = format!("{connection} · {LATE_NOTIFY_FIRST}");
    let masked_footer = format!("{connection} · [REDACTED]");
    let mut harness = harness(bridge);
    harness
        .get_by_label("Inspect project · Running · Review Bot")
        .click();
    harness.run();
    harness.get_by_label("Work details").click();
    harness.run();
    harness.get_by_label(&raw_footer);
    let first = secret::SecretString::new(LATE_NOTIFY_FIRST.into());
    let _lease = harness
        .state()
        .redaction
        .acquire_execution_lease(&[&first])
        .unwrap();
    harness.run();
    harness.get_by_label(&masked_footer);
    assert!(harness.query_by_label(&raw_footer).is_none());
    let run = &harness.state().cloud_runs()[0];
    assert_eq!(run.id, LATE_NOTIFY_FIRST);
    assert_eq!(run.connection_id, connection);
    assert_eq!(run.workspace, target.workspace);
    assert_eq!(run.session, target.id);
}

#[test]
fn late_secret_run_reference_hover_masks_display_preserves_identity() {
    let (bridge, _, connection, _) = running_fixture_with_id(LATE_NOTIFY_FIRST);
    let raw_footer = format!("{connection} · {LATE_NOTIFY_FIRST}");
    let masked_footer = format!("{connection} · [REDACTED]");
    let mut harness = harness(bridge);
    harness
        .get_by_label("Inspect project · Running · Review Bot")
        .click();
    harness.run();
    harness.get_by_label("Work details").click();
    harness.run();
    let first = secret::SecretString::new(LATE_NOTIFY_FIRST.into());
    let _lease = harness
        .state()
        .redaction
        .acquire_execution_lease(&[&first])
        .unwrap();
    harness.run();
    let footer = if harness.query_by_label(&masked_footer).is_some() {
        &masked_footer
    } else {
        &raw_footer
    };
    harness.get_by_label(footer).hover();
    harness.run();
    harness.get_by_label(&format!("{connection}\n[REDACTED]"));
    assert!(
        harness
            .query_by_label(&format!("{connection}\n{LATE_NOTIFY_FIRST}"))
            .is_none()
    );
    assert_eq!(harness.state().cloud_runs()[0].id, LATE_NOTIFY_FIRST);
}

#[test]
fn management_ui_late_secret_registration_masks_display_copy_and_links() {
    let (mut bridge, target, _, token) = running_fixture();
    let first = "fixture-secret-access-token-123456789";
    let second = "fixture-replaced-access-token-987654321";
    let result = "First: fixture-secret-\naccess-token-123456789. Second: fixture-replaced-\naccess-token-987654321.";
    let exact_uri = "https://example.test/safe-report?q=exact%2Fpath#part";
    management_call(
        &mut bridge,
        &target,
        &token,
        "update_run",
        serde_json::json!({
            "session_id":target.id,"generation":target.generation,"run_id":"review-run","state":"completed","message":"Stored before credentials are loaded","result":result,
            "artifacts":[
                {"label":"Safe report","uri":exact_uri},
                {"label":"Credential-bearing report","uri":format!("https://example.test/{first}")}
            ]
        }),
    );
    let mut harness = harness(bridge);
    harness
        .get_by_label("Inspect project · Completed · Review Bot")
        .click();
    harness.run();
    harness.get_by_label(result);
    let first_secret = secret::SecretString::new(first.into());
    let first_lease = harness
        .state()
        .redaction
        .acquire_execution_lease(&[&first_secret])
        .unwrap();
    harness.run();
    let first_expected = "First: [REDACTED]. Second: fixture-replaced-\naccess-token-987654321.";
    harness.get_by_label(first_expected);
    assert!(
        harness
            .query_by_label("Credential-bearing report")
            .is_none()
    );
    harness.get_by_label("Copy result").click();
    harness.run();
    let copies = harness.ctx.data_mut(|data| {
        data.get_temp::<Vec<String>>(egui::Id::new("test-cloud-clipboard-copies"))
            .unwrap_or_default()
    });
    assert_eq!(copies.last().map(String::as_str), Some(first_expected));
    drop(first_lease);
    let second_secret = secret::SecretString::new(second.into());
    let _second_lease = harness
        .state()
        .redaction
        .acquire_execution_lease(&[&second_secret])
        .unwrap();
    harness.run();
    let expected = "First: [REDACTED]. Second: [REDACTED].";
    harness.get_by_label(expected);
    harness.get_by_label("Copy result").click();
    harness.run();
    let copies = harness.ctx.data_mut(|data| {
        data.get_temp::<Vec<String>>(egui::Id::new("test-cloud-clipboard-copies"))
            .unwrap_or_default()
    });
    assert_eq!(copies.last().map(String::as_str), Some(expected));
    harness.get_by_label("Safe report").click();
    harness.run();
    let opened = harness.ctx.data_mut(|data| {
        data.get_temp::<Vec<String>>(egui::Id::new("test-cloud-open-urls"))
            .unwrap_or_default()
    });
    assert_eq!(opened, vec![exact_uri]);
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
    harness.get_by_role_and_label(egui::accesskit::Role::Label, "Running");
    harness.get_by_label("Work details").click();
    harness.run();
    assert_eq!(
        harness
            .get_all_by_label("Agent · Codex · gpt-6.1-sol · xhigh")
            .count(),
        1,
        "selected task retains the original session identity metadata"
    );
    assert!(
        harness
            .get_all_by_label("Status observation time unavailable")
            .count()
            >= 1
    );
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
    harness.get_by_label("Add connection").click();
    harness.run();
    harness.get_by_label("Independent token").click();
    harness.run();
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
    let selected = harness.ctx.data_mut(|data| {
        data.get_temp::<settings::SettingsState>(settings::state_id())
            .unwrap()
            .selected
    });
    assert_eq!(
        selected.as_deref(),
        Some(principal.id.as_str()),
        "newly issued identity is selected regardless of name sorting"
    );
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
            move |ui, state: &mut CloudAgent| state.contents(ui, &catalog),
            bridge,
        );
    for tab in [
        "Connections",
        "Access permissions",
        "Work · answers",
        "Advanced settings",
    ] {
        let bounds = harness
            .get_by_role_and_label(egui::accesskit::Role::Tab, tab)
            .rect();
        assert!(
            bounds.left() >= 0.0 && bounds.right() <= 720.0,
            "tab clipped: {bounds:?}"
        );
    }
    harness.get_by_label("Work · answers").click();
    harness.run();
    let bounds = harness.get_by_label("Request cancellation").rect();
    assert!(bounds.left() >= 0.0 && bounds.right() <= 720.0);
    harness.get_by_label("Access permissions").click();
    harness.run();
    let bounds = harness
        .get_by_role_and_label(
            egui::accesskit::Role::CheckBox,
            "Allow this connection to input",
        )
        .rect();
    assert!(bounds.left() >= 0.0 && bounds.right() <= 720.0);
    let path = std::env::temp_dir().join("deppy-cloud-settings-native-720-20261013.png");
    harness.render().unwrap().save(&path).unwrap();
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
    let request = harness.state_mut().take_run_navigation().unwrap();
    assert_eq!(request.workspace, original.workspace);
    assert_eq!(request.session, original.id);
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

#[test]
fn management_ui_closed_original_navigation_reports_failure_and_keeps_results() {
    let (bridge, _, _, _) = running_fixture();
    let mut harness = harness(bridge);
    harness
        .get_by_label("Inspect project · Running · Review Bot")
        .click();
    harness.run();
    harness.get_by_label("Open original session").click();
    harness.run();
    let request = harness.state_mut().take_run_navigation().unwrap();
    harness
        .state_mut()
        .finish_run_navigation(&request, Err("session_changed_or_closed".into()));
    harness.run();
    assert!(
        harness
            .query_by_label("Opening the original session…")
            .is_none()
    );
    harness.get_by_label("Could not open the original session. It may have been closed; the report remains available.");
    harness.get_by_label("Inspect project · Running · Review Bot");
}

#[test]
fn management_ui_original_navigation_reports_observed_success() {
    let (bridge, _, _, _) = running_fixture();
    let mut harness = harness(bridge);
    harness
        .get_by_label("Inspect project · Running · Review Bot")
        .click();
    harness.run();
    harness.get_by_label("Open original session").click();
    harness.run();
    let request = harness.state_mut().take_run_navigation().unwrap();
    harness.state_mut().finish_run_navigation(&request, Ok(()));
    harness.run();
    assert!(
        harness
            .query_by_label("Opening the original session…")
            .is_none()
    );
    harness.get_by_label("Opened the original session.");
}

#[test]
fn management_ui_new_operator_navigation_does_not_leave_the_old_request_opening() {
    let (bridge, _, _, _) = running_fixture();
    let mut harness = harness(bridge);
    harness
        .get_by_label("Inspect project · Running · Review Bot")
        .click();
    harness.run();
    harness.get_by_label("Open original session").click();
    harness.run();
    let previous = harness.state_mut().take_run_navigation().unwrap();
    harness
        .state_mut()
        .request_run_navigation("another-workspace", "missing-session")
        .unwrap();
    harness.state_mut().finish_run_navigation(&previous, Ok(()));
    harness.run();
    assert!(
        harness
            .query_by_label("Opening the original session…")
            .is_none()
    );
    harness.get_by_label("Could not open the original session. It may have been closed; the report remains available.");
}

#[test]
fn review_20261015_owner_can_revoke_read_consent_after_session_exit() {
    let (mut bridge, mut target, review, research, _) = connection_fixture();
    bridge
        .set_connection_access(&review, &target, true, false)
        .unwrap();
    bridge
        .set_connection_access(&research, &target, true, false)
        .unwrap();
    target.live = false;
    bridge.set_targets(vec![target.clone()]);
    assert!(bridge.connection_permission(&review, &target).read);
    assert!(!bridge.connection_permission(&review, &target).input);
    // The controller already accepts safe revocation on the retained exited target.
    bridge
        .set_connection_access(&review, &target, false, false)
        .unwrap();
    assert!(!bridge.connection_permission(&review, &target).read);
    bridge
        .set_connection_access(&review, &target, true, false)
        .unwrap();
    let mut harness = harness(bridge);
    harness
        .get_by_label("Review Bot · Credential issued · waiting for authentication")
        .click();
    harness.run();
    harness.get_by_label("Access permissions").click();
    harness.run();
    harness
        .get_by_role_and_label(
            egui::accesskit::Role::CheckBox,
            "Allow this connection to read · receive answers",
        )
        .click();
    harness.run();
    assert!(
        !harness.state().connection_permission(&review, &target).read,
        "the owner cannot withdraw retained read consent using the exited session's checkbox"
    );
    assert!(
        harness
            .state()
            .connection_permission(&research, &target)
            .read
    );
    let read = harness.get_by_role_and_label(
        egui::accesskit::Role::CheckBox,
        "Allow this connection to read · receive answers",
    );
    assert!(read.accesskit_node().is_disabled());
    read.click();
    harness.run();
    assert!(!harness.state().connection_permission(&review, &target).read);
    let input = harness.get_by_role_and_label(
        egui::accesskit::Role::CheckBox,
        "Allow this connection to input",
    );
    assert!(input.accesskit_node().is_disabled());
    input.click();
    harness.run();
    assert!(
        !harness
            .state()
            .connection_permission(&review, &target)
            .input
    );
}

#[test]
fn management_ui_ended_read_revocation_preserves_other_session_consent() {
    let (mut bridge, mut target, review, _, _) = connection_fixture();
    let other = Target::fixture("other-session", "generation-2");
    bridge.set_targets(vec![target.clone(), other.clone()]);
    bridge
        .set_connection_access(&review, &target, true, true)
        .unwrap();
    bridge
        .set_connection_access(&review, &other, true, true)
        .unwrap();
    target.live = false;
    bridge.set_targets(vec![target.clone(), other.clone()]);
    let mut harness = harness(bridge);
    harness
        .get_by_label("Review Bot · Credential issued · waiting for authentication")
        .click();
    harness.run();
    harness.get_by_label("Access permissions").click();
    harness.run();
    let reads = harness
        .query_all_by_role_and_label(
            egui::accesskit::Role::CheckBox,
            "Allow this connection to read · receive answers",
        )
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 2);
    reads[0].click();
    drop(reads);
    harness.run();
    let ended = harness.state().connection_permission(&review, &target);
    let retained = harness.state().connection_permission(&review, &other);
    assert!(!ended.read && !ended.input);
    assert!(retained.read && retained.input);
}
