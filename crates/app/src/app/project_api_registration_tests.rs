use super::*;

#[test]
fn project_key_dialog_blocks_background_input_without_settings_open() {
    let (mut app, _) = super::tests::ssh_workspace_isolated_app_fixture();
    app.settings_open = false;
    let scope = crate::project_keys::contracts::ProjectKeysScope {
        workspace_id: app.active.id.clone(),
        environment: deppy_core::project_keys::KeyEnvironment::Development,
    };
    let snapshot = crate::project_keys::contracts::ProjectKeysSnapshot::unavailable(
        scope,
        1,
        crate::project_keys::contracts::ProjectKeysError::Busy,
    );
    app.project_keys_ui.prefill(
        ui::environment::EnvironmentSelectionKind::VariableName,
        crate::project_keys::contracts::SensitiveDraft::new("FIXTURE_API_KEY".into()).unwrap(),
    );
    app.project_keys_ui.prepare(&snapshot);
    assert!(app.project_keys_ui.has_dialog());
    assert!(
        app.background_modal_pending(),
        "a direct key editor must fence PTY input even with Settings closed"
    );
}

#[test]
fn quick_api_registration_keeps_original_project_and_does_not_open_settings() {
    let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
    let local = app.db.create_workspace("API local fixture").unwrap();
    app.db
        .set_workspace_path(&local, root.to_str().unwrap())
        .unwrap();
    app.refresh_workspaces();
    app.switch_workspace(&local);
    app.settings_open = false;
    let prefill = ui::environment::EnvironmentPrefill::from_selection(
        ui::environment::EnvironmentSelectionKind::ApiName,
        "내 API",
    )
    .unwrap();
    assert!(app.open_project_api_registration(&local, Some(prefill)));
    assert!(!app.settings_open);
    let origin = app
        .quick_project_api_origin
        .as_ref()
        .expect("direct registration owner");
    assert_eq!(origin.workspace_id, local);
    assert_eq!(origin.runtime_instance, app.active.runtime_instance);
    assert_eq!(origin.primary_workspace_id, app.active.id);
    assert_eq!(app.project_keys_scope.as_ref().unwrap().workspace_id, local);
    assert!(
        app.background_modal_pending(),
        "loading a direct registration is already modal"
    );
}

#[test]
fn quick_api_registration_ssh_unavailable_is_visible_without_selecting_mac_keys() {
    use egui_kittest::kittest::Queryable;
    let (mut app, _) = super::tests::ssh_workspace_isolated_app_fixture();
    let workspace = app.active.id.clone();
    let prefill = ui::environment::EnvironmentPrefill::from_selection(
        ui::environment::EnvironmentSelectionKind::ApiValue,
        "synthetic-ssh-draft",
    )
    .unwrap();
    assert!(app.open_project_api_registration(&workspace, Some(prefill)));
    assert!(!app.settings_open);
    assert!(app.quick_project_api_origin.is_some());
    assert!(app.project_keys_scope.is_none());
    assert!(app.project_keys_snapshot.is_none());
    assert!(!app.project_keys_ui.has_dialog_or_pending_open());
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let mut harness = egui_kittest::Harness::new_ui_state(
        move |ui, app: &mut App| {
            assert!(app.show_project_key_dialog(ui.ctx(), &catalog).is_empty());
        },
        app,
    );
    harness.run_steps(3);
    harness.get_by_label(
        "API registration is unavailable for SSH projects. This Mac’s keys are not shared.",
    );
    assert!(harness.state().background_modal_pending());
    assert!(harness.state().project_keys_scope.is_none());
    harness.get_by_label("Cancel").click();
    harness.run();
    assert!(harness.state().quick_project_api_origin.is_none());
    assert!(!harness.state().background_modal_pending());
}

#[test]
fn quick_api_registration_cancels_when_original_runtime_is_replaced() {
    let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
    let local = app.db.create_workspace("API replacement fixture").unwrap();
    app.db
        .set_workspace_path(&local, root.to_str().unwrap())
        .unwrap();
    app.refresh_workspaces();
    app.switch_workspace(&local);
    assert!(app.open_project_api_registration(&local, None));
    app.active.runtime_instance += 1;
    app.validate_project_api_registration();
    assert!(app.quick_project_api_origin.is_none());
    assert!(!app.project_keys_ui.has_dialog_or_pending_open());
    assert!(app.project_keys_snapshot.is_none());
}

#[test]
fn quick_api_registration_loading_escape_clears_one_shot_secret_draft() {
    use egui_kittest::kittest::Queryable;
    let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
    let local = app.db.create_workspace("API cancel fixture").unwrap();
    app.db
        .set_workspace_path(&local, root.to_str().unwrap())
        .unwrap();
    app.refresh_workspaces();
    app.switch_workspace(&local);
    let prefill = ui::environment::EnvironmentPrefill::from_selection(
        ui::environment::EnvironmentSelectionKind::ApiValue,
        "synthetic-cancel-draft",
    )
    .unwrap();
    assert!(app.open_project_api_registration(&local, Some(prefill)));
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let mut harness = egui_kittest::Harness::new_ui_state(
        move |ui, app: &mut App| {
            let _ = app.show_project_key_dialog(ui.ctx(), &catalog);
        },
        app,
    );
    harness.run_steps(3);
    harness.get_by_label("Cancel");
    assert!(harness.state().background_modal_pending());
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(harness.state().quick_project_api_origin.is_none());
    assert!(!harness.state().project_keys_ui.has_dialog_or_pending_open());
}

#[test]
fn quick_api_registration_load_error_keeps_a_visible_cancel_surface() {
    use egui_kittest::kittest::Queryable;
    let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
    let local = app.db.create_workspace("API load error fixture").unwrap();
    app.db
        .set_workspace_path(&local, root.to_str().unwrap())
        .unwrap();
    app.refresh_workspaces();
    app.switch_workspace(&local);
    assert!(app.open_project_api_registration(&local, None));
    app.project_keys_snapshot = Some(Arc::new(
        crate::project_keys::contracts::ProjectKeysSnapshot::unavailable(
            app.project_keys_scope.clone().unwrap(),
            1,
            crate::project_keys::contracts::ProjectKeysError::Locked,
        ),
    ));
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let mut harness = egui_kittest::Harness::new_ui_state(
        move |ui, app: &mut App| {
            let _ = app.show_project_key_dialog(ui.ctx(), &catalog);
        },
        app,
    );
    harness.run_steps(3);
    harness.get_by_label("Unlock the system keychain, then retry.");
    assert!(harness.state().background_modal_pending());
    harness.get_by_label("Cancel").click();
    harness.run();
    assert!(harness.state().quick_project_api_origin.is_none());
    assert!(!harness.state().project_keys_ui.has_dialog_or_pending_open());
}

#[test]
fn quick_api_registration_from_attached_project_keeps_its_own_scope() {
    let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
    let attached = app.db.create_workspace("Attached API fixture").unwrap();
    app.db
        .set_workspace_path(&attached, root.to_str().unwrap())
        .unwrap();
    let primary = app.db.create_workspace("Primary API fixture").unwrap();
    app.db
        .set_workspace_path(&primary, root.to_str().unwrap())
        .unwrap();
    app.refresh_workspaces();
    app.switch_workspace(&attached);
    let original_instance = app.active.runtime_instance;
    app.switch_workspace(&primary);
    assert!(app.warm.contains_key(&attached));
    assert!(app.open_project_api_registration(&attached, None));
    let origin = app.quick_project_api_origin.as_ref().unwrap();
    assert_eq!(origin.runtime_instance, original_instance);
    assert_eq!(origin.primary_workspace_id, primary);
    assert_eq!(
        app.project_keys_scope.as_ref().unwrap().workspace_id,
        attached
    );
    assert_eq!(app.active.id, primary);
    app.warm.get_mut(&attached).unwrap().runtime_instance += 1;
    app.validate_project_api_registration();
    assert!(app.quick_project_api_origin.is_none());
    assert!(!app.project_keys_ui.has_dialog_or_pending_open());
}

#[test]
fn app_project_api_registration_route_preserves_explicit_settings_intent() {
    use ui::environment::{EnvironmentOpenKind, EnvironmentOpenRequest};
    for (kind, expected) in [
        (EnvironmentOpenKind::Settings, false),
        (EnvironmentOpenKind::RegisterApi, true),
    ] {
        let request = EnvironmentOpenRequest {
            kind,
            session: None,
            cwd: None,
            prefill: None,
        };
        assert_eq!(
            requests_direct_project_api_registration(&request),
            expected,
            "only the explicit API registration intent may bypass Settings"
        );
    }
}
