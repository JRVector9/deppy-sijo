use egui_kittest::{Harness, kittest::Queryable};

use super::*;
use connector_contract::OperationId;

const FIXTURE_SECRET: &str = "fake-review-secret";

struct State {
    connector: ConnectorUi,
    snapshot: ConnectorSnapshot,
    visible: bool,
    intents: Vec<ConnectorIntent>,
}

fn client_snapshot(operation: &str, server: &str, revision: u64) -> ConnectorSnapshot {
    ConnectorSnapshot {
        oauth: Some(OAuthUiState {
            operation_id: OperationId::new(operation),
            server_id: ServerId::new(server),
            server_name: "Fixture OAuth".into(),
            config_revision: Revision(revision),
            phase: OAuthUiPhase::AwaitingClient {
                reason: ErrorCode::AuthenticationRequired,
                workspace_hint: None,
            },
        }),
        ..ConnectorSnapshot::default()
    }
}

fn harness_with_secret() -> Harness<'static, State> {
    let catalog = Catalog::load("en-US").unwrap();
    let mut harness = Harness::new_ui_state(
        |ui, state: &mut State| {
            if state.visible
                && let Some(intent) = state.connector.render(ui, &state.snapshot)
            {
                state.intents.push(intent);
            }
        },
        State {
            connector: ConnectorUi::new(&catalog),
            snapshot: client_snapshot("first-operation", "first-server", 9),
            visible: true,
            intents: Vec::new(),
        },
    );
    harness.run();
    harness
        .get_by_role(egui::accesskit::Role::PasswordInput)
        .focus();
    harness.run();
    harness
        .get_by_role(egui::accesskit::Role::PasswordInput)
        .type_text(FIXTURE_SECRET);
    harness.run();
    // egui records a stable edit after one simulated second. Exercise that
    // real recording path before crossing the draft's lifetime boundary.
    harness.run_steps(5);
    assert_eq!(
        harness
            .state()
            .connector
            .oauth_client
            .as_ref()
            .unwrap()
            .client_secret,
        FIXTURE_SECRET
    );
    harness
}

fn submit_client(harness: &mut Harness<'_, State>) {
    harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .next()
        .unwrap()
        .focus();
    harness.run();
    harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .next()
        .unwrap()
        .type_text("fixture-client-id");
    harness.run();
    let save = harness.state().connector.labels.save_client.clone();
    harness.get_by_label(&save).click();
    harness.run();
}

fn assert_next_submission_is_empty(harness: &mut Harness<'_, State>) {
    harness
        .get_by_role(egui::accesskit::Role::PasswordInput)
        .focus();
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
    harness.run();
    submit_client(harness);
    let secret = harness
        .state()
        .intents
        .iter()
        .rev()
        .find_map(|intent| match intent {
            ConnectorIntent::SubmitOAuthClient { client_secret, .. } => {
                Some(client_secret.expose_bytes())
            }
            _ => None,
        })
        .expect("new client submit intent");
    assert!(
        secret.is_empty(),
        "a previous OAuth client's secret must not re-enter a later submission through undo"
    );
}

#[test]
fn oauth_secret_cancel_clears_undo_before_another_operation_submits() {
    let mut harness = harness_with_secret();
    let cancel = harness.state().connector.labels.cancel.clone();
    harness.get_by_label(&cancel).click();
    harness.run();
    assert!(harness.state().intents.iter().any(
        |intent| matches!(intent, ConnectorIntent::Cancel(operation) if operation.as_str() == "first-operation")
    ));
    harness.state_mut().connector.clear_sensitive_drafts();
    harness.state_mut().snapshot = ConnectorSnapshot::default();
    harness.run();
    harness.state_mut().snapshot = client_snapshot("second-operation", "second-server", 10);
    harness.run();
    assert_next_submission_is_empty(&mut harness);
    assert!(matches!(
        harness.state().intents.last(),
        Some(ConnectorIntent::SubmitOAuthClient { operation_id, server_id, config_revision: Revision(10), .. })
            if operation_id.as_str() == "second-operation" && server_id.as_str() == "second-server"
    ));
}

#[test]
fn oauth_secret_success_does_not_restore_the_submitted_value() {
    let mut harness = harness_with_secret();
    submit_client(&mut harness);
    assert!(matches!(
        harness.state().intents.last(),
        Some(ConnectorIntent::SubmitOAuthClient { client_secret, .. })
            if client_secret.expose_bytes() == FIXTURE_SECRET.as_bytes()
    ));
    // Completed authorization removes the service-owned OAuth prompt.
    harness.state_mut().snapshot = ConnectorSnapshot::default();
    harness.run();
    harness.state_mut().snapshot = client_snapshot("second-operation", "second-server", 10);
    harness.run();
    assert_next_submission_is_empty(&mut harness);
}

#[test]
fn oauth_secret_operation_switch_clears_undo() {
    let mut harness = harness_with_secret();
    harness.state_mut().snapshot = client_snapshot("second-operation", "first-server", 9);
    harness.run();
    assert_next_submission_is_empty(&mut harness);
}

#[test]
fn oauth_secret_server_switch_clears_undo() {
    let mut harness = harness_with_secret();
    harness.state_mut().snapshot = client_snapshot("first-operation", "second-server", 9);
    harness.run();
    assert_next_submission_is_empty(&mut harness);
}

#[test]
fn oauth_secret_revision_switch_clears_undo() {
    let mut harness = harness_with_secret();
    harness.state_mut().snapshot = client_snapshot("first-operation", "first-server", 10);
    harness.run();
    assert_next_submission_is_empty(&mut harness);
}

#[test]
fn oauth_secret_hidden_surface_clears_undo_without_rendering() {
    let mut harness = harness_with_secret();
    harness.state_mut().visible = false;
    harness.state_mut().connector.clear_sensitive_drafts();
    harness.run();
    harness.state_mut().visible = true;
    harness.run();
    assert_next_submission_is_empty(&mut harness);
}

#[test]
fn oauth_secret_missing_snapshot_clears_undo() {
    let mut harness = harness_with_secret();
    harness.state_mut().snapshot = ConnectorSnapshot::default();
    harness.run();
    harness.state_mut().snapshot = client_snapshot("second-operation", "second-server", 10);
    harness.run();
    assert_next_submission_is_empty(&mut harness);
}

#[test]
fn oauth_secret_active_input_has_no_undo_history() {
    let harness = harness_with_secret();
    let id = harness.ctx.memory(|memory| memory.focused()).unwrap();
    let state = egui::text_edit::TextEditState::load(&harness.ctx, id).unwrap();
    let cursor = egui::text::CCursorRange::one(egui::text::CCursor::new(0));
    assert!(!state.undoer().has_undo(&(cursor, String::new())));
}

#[test]
fn oauth_secret_clear_also_clears_cloned_undo_state() {
    let mut harness = harness_with_secret();
    let id = harness.ctx.memory(|memory| memory.focused()).unwrap();
    let mut shared = egui::text_edit::TextEditState::load(&harness.ctx, id).unwrap();
    let cursor = egui::text::CCursorRange::one(egui::text::CCursor::new(0));
    shared.clear_undoer();
    let mut undoer = shared.undoer();
    undoer.feed_state(0.0, &(cursor, FIXTURE_SECRET.into()));
    shared.set_undoer(undoer);
    assert!(shared.undoer().has_undo(&(cursor, String::new())));
    harness.state_mut().connector.clear_sensitive_drafts();
    assert!(egui::text_edit::TextEditState::load(&harness.ctx, id).is_none());
    assert!(!shared.undoer().has_undo(&(cursor, String::new())));
    assert!(!harness.ctx.memory(|memory| memory.has_focus(id)));
}
