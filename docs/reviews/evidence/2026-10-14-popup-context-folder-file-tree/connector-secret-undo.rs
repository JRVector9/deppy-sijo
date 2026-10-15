use connector_contract::*;
use connector_ui::ConnectorUi;
use egui_kittest::kittest::Queryable;

struct State { ui: ConnectorUi, snapshot: ConnectorSnapshot, intents: Vec<ConnectorIntent> }
fn client(op: &str) -> ConnectorSnapshot {
    ConnectorSnapshot { oauth: Some(OAuthUiState {
        operation_id: OperationId::new(op), server_id: ServerId::new(op),
        server_name: "OAuth".into(), config_revision: Revision(9),
        phase: OAuthUiPhase::AwaitingClient { reason: ErrorCode::AuthenticationRequired, workspace_hint: None },
    }), ..Default::default() }
}
fn main() {
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let mut h = egui_kittest::Harness::new_ui_state(
        |ui, state: &mut State| { if let Some(intent) = state.ui.render(ui, &state.snapshot) { state.intents.push(intent); } },
        State { ui: ConnectorUi::new(&catalog), snapshot: client("first"), intents: vec![] });
    h.run();
    h.get_by_role(egui::accesskit::Role::PasswordInput).focus();
    h.run();
    h.get_by_role(egui::accesskit::Role::PasswordInput).type_text("fake-review-secret");
    h.run();
    let old_id = h.ctx.memory(|m| m.focused()).unwrap();
    println!("First secret widget: {old_id:?}");
    h.get_by_label(catalog.t("action.cancel", &[]).as_str()).click();
    h.run();
    assert!(h.state().intents.iter().any(|i| matches!(i, ConnectorIntent::Cancel(op) if op.as_str() == "first")));
    h.state_mut().ui.clear_sensitive_drafts();
    h.state_mut().snapshot = ConnectorSnapshot::default();
    h.run();
    h.state_mut().snapshot = client("second");
    h.run();
    h.get_by_role(egui::accesskit::Role::PasswordInput).focus();
    h.run();
    let new_id = h.ctx.memory(|m| m.focused()).unwrap();
    println!("Second secret widget: {new_id:?}");
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
    h.run();
    let password_value = h.get_by_role(egui::accesskit::Role::PasswordInput).value();
    println!("New OAuth draft after Undo masked AccessKit value: {password_value:?}");
    h.get_all_by_role(egui::accesskit::Role::TextInput).next().unwrap().focus();
    h.run();
    h.get_all_by_role(egui::accesskit::Role::TextInput).next().unwrap().type_text("fake-client-id");
    h.run();
    h.get_by_label(catalog.t("connector.action.save_client_continue", &[]).as_str()).click();
    h.run();
    let restored = h.state().intents.iter().find_map(|i| match i {
        ConnectorIntent::SubmitOAuthClient { operation_id, client_secret, .. } if operation_id.as_str() == "second" => Some(client_secret.expose_bytes()),
        _ => None,
    }).expect("second client submit intent");
    println!("Second operation submitted fake secret byte count: {}", restored.len());
    println!("Matches cancelled first operation fake fixture: {}", restored == b"fake-review-secret");
    assert_eq!(restored, b"fake-review-secret", "probe must identify the first operation's cancelled fixture exactly");
    assert!(restored.is_empty(), "Cancelled first OAuth client secret must not survive in widget undo history or re-enter a later OAuth operation");
}
