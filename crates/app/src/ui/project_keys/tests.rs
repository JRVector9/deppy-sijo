use super::*;
use egui_kittest::kittest::{NodeT, Queryable};

fn snapshot() -> ProjectKeysSnapshot {
    let scope = ProjectKeysScope {
        workspace_id: "project-a".into(),
        environment: KeyEnvironment::Development,
    };
    let mut file = new_key(&scope);
    file.id = "file-key".into();
    file.name = "API_TOKEN".into();
    file.source = KeySource::Dotenv {
        file: ".env.development".into(),
    };
    file.credential_id = Some("protected-file-reference".into());
    file.last_good_revision = Some(3);
    file.last_good_at = Some(1791791000);
    file.revision = 3;
    file.storage_state = KeyStorageState::Stored;
    file.updated_at = 1791792000;
    let mut vault = new_key(&scope);
    vault.id = "vault-key".into();
    vault.name = "CLIENT_SECRET".into();
    vault.kind = KeyKind::ClientSecret;
    vault.usage = KeyUsage::StoreOnly;
    let mut legacy = new_key(&scope);
    legacy.id = "legacy-key".into();
    legacy.name = "LEGACY_TOKEN".into();
    legacy.usage = KeyUsage::LegacyAgent;
    ProjectKeysSnapshot {
        scope: scope.clone(),
        generation: 7,
        keys: vec![file, vault, legacy],
        settings: ProjectKeySettings {
            workspace_id: scope.workspace_id.clone(),
            environment: scope.environment,
            revision: 2,
            watch_enabled: true,
            sources: vec![KeySource::Dotenv {
                file: ".env.development".into(),
            }],
        },
        recipes: vec![],
        receipts: vec![],
        projects: vec![
            ProjectChoice {
                id: scope.workspace_id,
                name: "Project A".into(),
                path: "/isolated/project-a".into(),
            },
            ProjectChoice {
                id: "project-b".into(),
                name: "Project B".into(),
                path: "/isolated/project-b".into(),
            },
        ],
        auth_credentials: vec![],
        api_access: vec![],
        source_preview: None,
        error: None,
    }
}

fn harness(
    snapshot: ProjectKeysSnapshot,
    manager: ProjectKeysUi,
) -> egui_kittest::Harness<'static, (ProjectKeysUi, Vec<ProjectKeysIntent>)> {
    let catalog = i18n::Catalog::load("en-US").unwrap();
    egui_kittest::Harness::new_ui_state(
        move |ui, (manager, intents)| {
            intents.extend(manager.show(ui, &snapshot, &catalog));
        },
        (manager, Vec::new()),
    )
}

#[test]
fn table_origins_exact_44_rows_and_28_actions() {
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let snapshot = snapshot();
    let mut harness = egui_kittest::Harness::new_ui_state(
        move |ui, heights: &mut Vec<f32>| {
            heights.clear();
            ui.spacing_mut().item_spacing.y = 0.0;
            for key in &snapshot.keys {
                heights.push(table::row(ui, key, false, true, &catalog).0.height());
            }
        },
        Vec::new(),
    );
    harness.set_size(egui::vec2(900.0, 350.0));
    harness.run();
    assert_eq!(harness.state(), &[44.0, 44.0, 44.0]);
    harness.get_by_label(".env.development");
    assert_eq!(
        table::source_label(&KeySource::Vault, &i18n::Catalog::load("en-US").unwrap()),
        "Vault (manual)"
    );
    assert_eq!(
        table::usage_label(
            KeyUsage::LegacyAgent,
            &i18n::Catalog::load("en-US").unwrap()
        ),
        "Agent / legacy"
    );
    for label in ["Copy API_TOKEN", "Copy CLIENT_SECRET", "Copy LEGACY_TOKEN"] {
        let node = harness.get_by_label(label);
        assert!((node.rect().height() - 28.0).abs() < 0.5);
    }
    for label in ["Edit API_TOKEN", "Edit CLIENT_SECRET", "Edit LEGACY_TOKEN"] {
        let node = harness.get_by_label(label);
        assert!((node.rect().height() - 28.0).abs() < 0.5);
    }
}

#[test]
fn value_only_double_click_emits_copy_and_name_click_selects() {
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let key = snapshot().keys.remove(0);
    let mut harness = egui_kittest::Harness::builder()
        .with_step_dt(1.0 / 60.0)
        .build_ui_state(
            move |ui, actions: &mut Vec<table::RowAction>| {
                if let Some(action) = table::row(ui, &key, false, true, &catalog).1 {
                    actions.push(action);
                }
            },
            Vec::new(),
        );
    harness.set_size(egui::vec2(850.0, 180.0));
    harness.run();
    harness.get_by_label("Copy value for API_TOKEN").click();
    harness.step();
    assert!(
        harness.state().is_empty(),
        "single value click must not copy"
    );
    harness.get_by_label("Copy value for API_TOKEN").click();
    harness.step();
    assert_eq!(harness.state().last(), Some(&table::RowAction::Copy));
    harness.state_mut().clear();
    harness.get_by_label("API_TOKEN").click();
    harness.step();
    assert_eq!(harness.state().last(), Some(&table::RowAction::Select));
}

#[test]
fn explicit_copy_intent_has_exact_key_revision_and_no_optimistic_notice() {
    let mut data = snapshot();
    data.keys.truncate(1);
    let mut harness = harness(data, ProjectKeysUi::default());
    harness.set_size(egui::vec2(850.0, 800.0));
    harness.run();
    harness.get_by_label("Copy API_TOKEN").click();
    harness.run();
    let ProjectKeysIntent::Request {
        fence,
        action:
            ProjectKeysAction::CopyKey {
                key_id,
                expected_revision,
            },
    } = harness.state().1.last().unwrap()
    else {
        panic!("exact copy intent expected");
    };
    assert_eq!(key_id, "file-key");
    assert_eq!(*expected_revision, 3);
    assert_eq!(fence.scope.workspace_id, "project-a");
    assert!(harness.state().0.notice.is_none());
}

#[test]
fn late_ack_after_scope_switch_cannot_close_new_editor_or_show_success() {
    let first = snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&first);
    let Some(ProjectKeysIntent::Request { fence, .. }) = manager.request(
        ProjectKeysAction::Refresh { key_id: None },
        PendingKind::General,
    ) else {
        panic!()
    };
    let mut second = first.clone();
    second.scope.workspace_id = "project-b".into();
    second.generation = 8;
    manager.prepare(&second);
    manager.dialog = Some(Dialog::Edit(Box::new(EditDraft::new(new_key(
        &second.scope,
    )))));
    manager.apply_ack(&ProjectKeysAck {
        fence,
        result: Ok(ProjectKeysAckPayload::Copied),
    });
    assert!(manager.has_dialog());
    assert!(manager.notice.is_none());
    assert!(manager.pending.is_none());
}

#[test]
fn same_scope_old_request_ack_does_not_resolve_new_request() {
    let data = snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    let Some(ProjectKeysIntent::Request { fence: old, .. }) = manager.request(
        ProjectKeysAction::Refresh { key_id: None },
        PendingKind::General,
    ) else {
        panic!()
    };
    manager.apply_ack(&ProjectKeysAck {
        fence: old.clone(),
        result: Err(ProjectKeysError::Storage),
    });
    let Some(ProjectKeysIntent::Request { fence: new, .. }) = manager.request(
        ProjectKeysAction::Refresh { key_id: None },
        PendingKind::General,
    ) else {
        panic!()
    };
    manager.apply_ack(&ProjectKeysAck {
        fence: old,
        result: Ok(ProjectKeysAckPayload::Copied),
    });
    assert_eq!(manager.pending.as_ref().unwrap().fence, new);
    assert!(manager.notice.is_none());
}

#[test]
fn blank_edit_keeps_existing_credential_ids_source_usage_and_revision() {
    let mut key = snapshot().keys.remove(0);
    key.source_baseline_credential_id = key.credential_id.clone();
    key.legacy_reference = Some("legacy-binding".into());
    let mut edit = EditDraft::existing(key.clone());
    assert!(edit.valid());
    let ProjectKeysAction::SaveKey {
        key: saved,
        expected_revision,
        value,
    } = edit.submit()
    else {
        panic!()
    };
    assert_eq!(*saved, key);
    assert_eq!(expected_revision, Some(3));
    assert!(matches!(value, ValueEdit::Keep));
}

#[test]
fn failed_replacement_requires_reentry_and_preserves_editor_metadata() {
    let data = snapshot();
    let key = data.keys[0].clone();
    let mut edit = EditDraft::existing(key.clone());
    edit.value = SensitiveDraft::new("replacement-fixture".into()).unwrap();
    let action = edit.submit();
    assert!(matches!(
        action,
        ProjectKeysAction::SaveKey {
            value: ValueEdit::Replace(_),
            ..
        }
    ));
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.dialog = Some(Dialog::Edit(Box::new(edit)));
    let Some(ProjectKeysIntent::Request { fence, .. }) = manager.request(action, PendingKind::Edit)
    else {
        panic!()
    };
    manager.apply_ack(&ProjectKeysAck {
        fence,
        result: Err(ProjectKeysError::Locked),
    });
    let Some(Dialog::Edit(edit)) = &manager.dialog else {
        panic!("failure must retain editor");
    };
    assert_eq!(edit.key, key);
    assert!(edit.value.is_empty());
    assert!(!edit.valid());
}

#[test]
fn successful_mutation_closes_editor_only_after_matching_ack() {
    let data = snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.dialog = Some(Dialog::Edit(Box::new(EditDraft::existing(
        data.keys[0].clone(),
    ))));
    let Some(ProjectKeysIntent::Request { fence, .. }) = manager.request(
        ProjectKeysAction::Refresh { key_id: None },
        PendingKind::Edit,
    ) else {
        panic!()
    };
    assert!(manager.has_dialog());
    manager.apply_ack(&ProjectKeysAck {
        fence,
        result: Ok(ProjectKeysAckPayload::Done),
    });
    assert!(!manager.has_dialog());
}

#[test]
fn project_change_clears_draft_query_selection_and_text_edit_history() {
    let data = snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.query = "old project".into();
    manager.selected = Some("file-key".into());
    manager.dialog = Some(Dialog::Edit(Box::new(EditDraft::existing(
        data.keys[0].clone(),
    ))));
    let mut next = data;
    next.scope.environment = KeyEnvironment::Production;
    next.generation += 1;
    manager.prepare(&next);
    assert!(manager.query.is_empty());
    assert!(manager.selected.is_none());
    assert!(!manager.has_dialog());
}

#[test]
fn quick_api_name_prefill_opens_a_freeform_service_field() {
    let data = snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.begin_api_add(
        data.scope.clone(),
        Some((
            super::super::environment::EnvironmentSelectionKind::ApiName,
            SensitiveDraft::new("내 API".into()).unwrap(),
        )),
    );
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let service_label = catalog.t("project_keys.api_service_name", &[]);
    let binding_label = catalog.t("project_keys.api_binding", &[]);
    let mut h = egui_kittest::Harness::new_ui_state(
        move |ui, manager: &mut ProjectKeysUi| {
            let _ = manager.show_dialog(ui.ctx(), &data, &catalog);
        },
        manager,
    );
    h.set_size(egui::vec2(1280.0, 800.0));
    h.run();
    h.get_by_label(&service_label);
    h.get_by_label(&binding_label);
    assert!(
        h.get_all_by_role(egui::accesskit::Role::TextInput)
            .any(|node| node.accesskit_node().value().as_deref() == Some("내 API"))
    );
    assert!(h.state().has_dialog());
}

#[test]
fn quick_api_value_prefill_stays_masked_in_the_direct_editor() {
    let data = snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.begin_api_add(
        data.scope.clone(),
        Some((
            super::super::environment::EnvironmentSelectionKind::ApiValue,
            SensitiveDraft::new("fixture-api-value".into()).unwrap(),
        )),
    );
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let mut h = egui_kittest::Harness::new_ui_state(
        move |ui, manager: &mut ProjectKeysUi| {
            let _ = manager.show_dialog(ui.ctx(), &data, &catalog);
        },
        manager,
    );
    h.set_size(egui::vec2(1280.0, 800.0));
    h.run();
    let secret = h.get_by_role(egui::accesskit::Role::PasswordInput);
    assert!(
        secret
            .accesskit_node()
            .value()
            .is_some_and(|value| !value.contains("fixture-api-value"))
    );
    assert!(h.output().shapes.iter().all(|shape| match &shape.shape {
        egui::Shape::Text(text) => !text.galley.text().contains("fixture-api-value"),
        _ => true,
    }));
}

fn api_access(key: &ProjectKey) -> deppy_core::project_api_access::ProjectApiAccess {
    deppy_core::project_api_access::ProjectApiAccess {
        key_id: key.id.clone(),
        workspace_id: key.workspace_id.clone(),
        environment: key.environment,
        revision: 11,
        execution_name: None,
        service_name: Some("내 API".into()),
        description: Some("Project service".into()),
        documentation_url: Some("https://example.test/docs".into()),
        agent_enabled: false,
    }
}

#[test]
fn quick_api_unbound_storage_sends_one_scoped_action_without_an_internal_name() {
    let data = snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.begin_api_add(data.scope.clone(), None);
    manager.prepare(&data);
    let Some(Dialog::Edit(draft)) = &mut manager.dialog else {
        panic!()
    };
    assert_eq!(draft.key.usage, KeyUsage::StoreOnly);
    assert_eq!(draft.key.source, KeySource::Vault);
    assert!(!draft.api.as_ref().unwrap().access.agent_enabled);
    draft.api.as_mut().unwrap().access.service_name = Some("내 API".into());
    draft.value = SensitiveDraft::new("fixture-api-value".into()).unwrap();
    assert!(draft.valid());
    let ProjectKeysAction::SaveApiKey {
        key,
        expected_revision,
        value,
        access,
        expected_access_revision,
    } = draft.submit()
    else {
        panic!("single API save action required")
    };
    assert_eq!(key.name, "");
    assert_eq!(key.workspace_id, data.scope.workspace_id);
    assert_eq!(key.environment, data.scope.environment);
    assert_eq!(access.key_id, key.id);
    assert_eq!(access.service_name.as_deref(), Some("내 API"));
    assert!(access.execution_name.is_none());
    assert!(!access.agent_enabled);
    assert!(expected_revision.is_none());
    assert!(expected_access_revision.is_none());
    assert!(matches!(value, ValueEdit::Replace(_)));
}

#[test]
fn api_agent_permission_requires_binding_and_preserves_usage() {
    let data = snapshot();
    let mut draft = EditDraft::new(new_key(&data.scope));
    draft.begin_api();
    draft.api.as_mut().unwrap().access.service_name = Some("Weather API".into());
    draft.value = SensitiveDraft::new("fixture-api-value".into()).unwrap();
    draft.api.as_mut().unwrap().access.agent_enabled = true;
    assert!(!draft.valid());
    assert_eq!(draft.key.usage, KeyUsage::StoreOnly);
    draft.key.name = "WEATHER_TOKEN".into();
    assert!(draft.valid());
    let ProjectKeysAction::SaveApiKey { key, access, .. } = draft.submit() else {
        panic!()
    };
    assert_eq!(key.usage, KeyUsage::StoreOnly);
    assert_eq!(access.execution_name.as_deref(), Some("WEATHER_TOKEN"));
    assert!(access.agent_enabled);
}

#[test]
fn api_edit_retains_both_cas_revisions_and_hides_storage_identity() {
    let mut data = snapshot();
    let mut key = data.keys[1].clone();
    key.id = "00000000-0000-0000-0000-000000000001".into();
    key.name = "DEPPY_API_00000000000000000000000000000001".into();
    key.storage_state = KeyStorageState::Stored;
    key.credential_id = Some("fixture-protected-reference".into());
    key.last_good_revision = Some(key.revision);
    data.api_access.push(api_access(&key));
    let mut draft = existing_editor(&data, &key);
    assert!(draft.key.name.is_empty());
    assert!(draft.valid());
    let ProjectKeysAction::SaveApiKey {
        key: saved,
        expected_revision,
        expected_access_revision,
        value,
        access,
    } = draft.submit()
    else {
        panic!()
    };
    assert_eq!(saved.id, key.id);
    assert!(saved.name.is_empty());
    assert_eq!(expected_revision, Some(key.revision));
    assert_eq!(expected_access_revision, Some(11));
    assert_eq!(access.description.as_deref(), Some("Project service"));
    assert!(matches!(value, ValueEdit::Keep));
    let catalog = i18n::Catalog::load("en-US").unwrap();
    assert_eq!(key_display_name(&data, &key, &catalog), "내 API");
    data.api_access.clear();
    assert_eq!(key_display_name(&data, &key, &catalog), "Stored API");
    assert!(existing_editor(&data, &key).key.name.is_empty());
}

#[test]
fn ordinary_deppy_api_prefix_keeps_the_actual_display_name() {
    let mut data = snapshot();
    data.keys[0].name = "DEPPY_API_TOKEN".into();
    let catalog = i18n::Catalog::load("en-US").unwrap();
    assert!(data.api_access.is_empty());
    assert_eq!(
        key_display_name(&data, &data.keys[0], &catalog),
        "DEPPY_API_TOKEN"
    );
}

#[test]
fn ordinary_deppy_api_prefix_keeps_the_generic_saved_editor_contract() {
    for usage in [KeyUsage::Build, KeyUsage::LegacyAgent] {
        for name in [
            "DEPPY_API_TOKEN",
            "DEPPY_API_00000000000000000000000000000001",
        ] {
            let mut data = snapshot();
            let mut key = github_key(GitHubRemoteState::Sent);
            key.id = "00000000-0000-0000-0000-000000000001".into();
            key.name = name.into();
            key.source = KeySource::Vault;
            key.usage = usage;
            data.keys[0] = key.clone();
            assert!(data.api_access.is_empty());
            let mut draft = existing_editor(&data, &key);
            assert!(
                draft.api.is_none(),
                "an ordinary key must retain the generic editor"
            );
            assert!(draft.valid());
            let ProjectKeysAction::SaveKey {
                key: saved,
                expected_revision,
                value,
            } = draft.submit()
            else {
                panic!("an ordinary key must retain the SaveKey action")
            };
            assert_eq!(
                *saved, key,
                "name, usage, source and GitHub binding must be preserved"
            );
            assert_eq!(expected_revision, Some(key.revision));
            assert!(matches!(value, ValueEdit::Keep));
        }
    }
}

#[test]
fn ordinary_deppy_api_prefix_keeps_github_binding_actions_enabled() {
    let mut data = snapshot();
    let mut key = github_key(GitHubRemoteState::Sent);
    key.name = "DEPPY_API_TOKEN".into();
    data.keys[0] = key.clone();
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let configure = catalog.t("project_keys.github_configure", &[]);
    let send = catalog.t("project_keys.send", &[]);
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.selected = Some(key.id);
    let mut harness = egui_kittest::Harness::new_ui_state(
        move |ui, (manager, intents): &mut (ProjectKeysUi, Vec<ProjectKeysIntent>)| {
            manager.detail(ui, &data, &catalog, intents);
        },
        (manager, Vec::new()),
    );
    harness.set_size(egui::vec2(600.0, 1000.0));
    harness.run();
    assert!(
        !harness
            .get_by_label(&configure)
            .accesskit_node()
            .is_disabled()
    );
    assert!(!harness.get_by_label(&send).accesskit_node().is_disabled());
}

#[test]
fn api_open_waits_through_a_matching_snapshot_error() {
    let mut data = snapshot();
    data.error = Some(ProjectKeysError::Storage);
    let mut manager = ProjectKeysUi::default();
    manager.begin_api_add(data.scope.clone(), None);
    manager.prepare(&data);
    assert!(!manager.has_dialog());
    assert!(manager.has_dialog_or_pending_open());
    data.error = None;
    manager.prepare(&data);
    assert!(manager.has_dialog());
    assert!(manager.pending_open.is_none());
}

#[test]
fn api_save_failure_keeps_metadata_and_permission_unacknowledged() {
    let data = snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.begin_api_add(data.scope.clone(), None);
    manager.prepare(&data);
    let Some(Dialog::Edit(draft)) = &mut manager.dialog else {
        panic!()
    };
    draft.key.name = "WEATHER_TOKEN".into();
    draft.api.as_mut().unwrap().access.service_name = Some("Weather API".into());
    draft.api.as_mut().unwrap().access.agent_enabled = true;
    draft.value = SensitiveDraft::new("fixture-api-value".into()).unwrap();
    let action = draft.submit();
    let ProjectKeysIntent::Request { fence, .. } =
        manager.request(action, PendingKind::Edit).unwrap()
    else {
        panic!()
    };
    assert!(manager.has_dialog());
    assert!(manager.notice.is_none());
    manager.apply_ack(&ProjectKeysAck {
        fence,
        result: Err(ProjectKeysError::Storage),
    });
    let Some(Dialog::Edit(draft)) = &manager.dialog else {
        panic!("failure must retain API editor")
    };
    assert_eq!(
        draft.api.as_ref().unwrap().access.service_name.as_deref(),
        Some("Weather API")
    );
    assert!(!draft.valid());
    assert_eq!(manager.dialog_error, Some(ProjectKeysError::Storage));
    assert!(matches!(
        manager.notice,
        Some(Err(ProjectKeysError::Storage))
    ));
}

#[test]
fn partial_api_save_failure_offers_explicit_reload_without_rebasing_the_draft() {
    for (new, key_changed, access_changed) in [
        (true, true, true),
        (false, false, true),
        (false, true, false),
    ] {
        let mut data = snapshot();
        let mut manager = ProjectKeysUi::default();
        manager.prepare(&data);
        let mut draft = if new {
            let mut draft = EditDraft::new(new_key(&data.scope));
            draft.begin_api();
            draft
        } else {
            let mut access = api_access(&data.keys[1]);
            access.execution_name = Some(data.keys[1].name.clone());
            access.agent_enabled = true;
            data.api_access.push(access);
            existing_editor(&data, &data.keys[1])
        };
        if new {
            draft.key.name = "WEATHER_TOKEN".into();
        }
        draft.api.as_mut().unwrap().access.service_name = Some("Weather API".into());
        draft.api.as_mut().unwrap().access.agent_enabled = true;
        draft.value = SensitiveDraft::new("consumed-fixture-value".into()).unwrap();
        let key = draft.key.clone();
        let access = draft.api.as_ref().unwrap().access.clone();
        let action = draft.submit();
        manager.open_dialog(Dialog::Edit(Box::new(draft)));
        let ProjectKeysIntent::Request { fence, .. } =
            manager.request(action, PendingKind::Edit).unwrap()
        else {
            panic!()
        };
        manager.apply_ack(&ProjectKeysAck {
            fence,
            result: Err(ProjectKeysError::Storage),
        });
        let Some(Dialog::Edit(draft)) = &mut manager.dialog else {
            panic!()
        };
        assert!(
            draft.value.is_empty(),
            "the consumed secret must not be restored"
        );
        assert!(
            !draft.valid(),
            "a failed replacement requires explicit reentry"
        );
        draft.value = SensitiveDraft::new("retry-fixture-value".into()).unwrap();
        assert!(draft.valid());

        let mut latest = data.clone();
        let mut saved_key = key.clone();
        if key_changed && !new {
            saved_key.revision += 1;
        }
        if new {
            latest.keys.push(saved_key.clone());
        } else {
            *latest
                .keys
                .iter_mut()
                .find(|saved| saved.id == key.id)
                .unwrap() = saved_key.clone();
        }
        let mut saved_access = access;
        saved_access.agent_enabled = false;
        if access_changed && !new {
            saved_access.revision += 1;
        }
        if new {
            latest.api_access.push(saved_access.clone());
        } else {
            *latest
                .api_access
                .iter_mut()
                .find(|saved| saved.key_id == key.id)
                .unwrap() = saved_access.clone();
        }

        let mut h = harness(latest.clone(), manager);
        h.set_size(egui::vec2(1280.0, 800.0));
        h.run();
        let reload = i18n::Catalog::load("en-US")
            .unwrap()
            .t("project_keys.reload_latest", &[]);
        h.get_by_label(&reload);
        assert!(
            h.get_by_label("Save").accesskit_node().is_disabled(),
            "a revision-changing partial failure must require reload immediately"
        );
        h.get_by_label(&reload).click();
        h.run();
        let ProjectKeysIntent::Request {
            fence,
            action: ProjectKeysAction::ReloadEditor { key_id },
        } = h.state().1.last().unwrap()
        else {
            panic!("reload must request metadata, never resubmit the failed save")
        };
        assert_eq!(key_id, &key.id);
        let fence = fence.clone();
        let Some(Dialog::Edit(draft)) = &mut h.state_mut().0.dialog else {
            panic!()
        };
        let ProjectKeysAction::SaveApiKey {
            expected_revision,
            expected_access_revision,
            access,
            ..
        } = draft.submit()
        else {
            panic!()
        };
        assert_eq!(
            expected_revision,
            (!new).then_some(key.revision),
            "snapshot arrival and a pending refresh must not rebase the key revision"
        );
        assert_eq!(
            expected_access_revision,
            (!new).then_some(11),
            "snapshot arrival and a pending refresh must not rebase the access revision"
        );
        assert!(
            access.agent_enabled,
            "unsaved metadata remains in the draft until the refresh ACK"
        );
        h.state_mut().0.apply_ack(&ProjectKeysAck {
            fence,
            result: Ok(ProjectKeysAckPayload::Refreshed(Box::new(latest.clone()))),
        });
        h.state_mut().0.prepare(&latest);
        let Some(Dialog::Edit(draft)) = &mut h.state_mut().0.dialog else {
            panic!()
        };
        let ProjectKeysAction::SaveApiKey {
            expected_revision,
            expected_access_revision,
            access,
            ..
        } = draft.submit()
        else {
            panic!()
        };
        assert_eq!(expected_revision, Some(saved_key.revision));
        assert_eq!(expected_access_revision, Some(saved_access.revision));
        assert!(!access.agent_enabled);
    }
}

#[test]
fn reloading_an_unstored_vault_api_requires_a_replacement_value() {
    let mut data = snapshot();
    let key = data.keys[1].clone();
    assert_eq!(key.source, KeySource::Vault);
    assert_eq!(key.storage_state, KeyStorageState::NotStored);
    assert!(key.credential_id.is_none());
    data.api_access.push(api_access(&key));
    let mut draft = existing_editor(&data, &key);
    assert!(
        !draft.valid(),
        "a placeholder without a stored value must require reentry after reload"
    );
    draft.value = SensitiveDraft::new("reentered-fixture-value".into()).unwrap();
    assert!(draft.valid());
    let ProjectKeysAction::SaveApiKey {
        expected_revision,
        value,
        ..
    } = draft.submit()
    else {
        panic!()
    };
    assert_eq!(expected_revision, Some(key.revision));
    assert!(matches!(value, ValueEdit::Replace(_)));

    let mut stored = key;
    stored.storage_state = KeyStorageState::Stored;
    stored.credential_id = Some("fixture-protected-reference".into());
    stored.last_good_revision = Some(stored.revision);
    let mut draft = existing_editor(&data, &stored);
    assert!(
        draft.valid(),
        "existing stored API values still support Keep"
    );
    assert!(matches!(
        draft.submit(),
        ProjectKeysAction::SaveApiKey {
            value: ValueEdit::Keep,
            ..
        }
    ));
}

#[test]
fn existing_api_binding_is_read_only_after_first_binding() {
    for bound in [true, false] {
        let mut data = snapshot();
        let key = &mut data.keys[1];
        key.storage_state = KeyStorageState::Stored;
        key.credential_id = Some("fixture-protected-reference".into());
        key.last_good_revision = Some(key.revision);
        let mut access = api_access(key);
        if bound {
            access.execution_name = Some(key.name.clone());
        } else {
            key.name = "DEPPY_API_00000000000000000000000000000001".into();
        }
        let binding = access.execution_name.clone().unwrap_or_default();
        data.api_access.push(access);
        let mut manager = ProjectKeysUi::default();
        manager.prepare(&data);
        manager.open_dialog(Dialog::Edit(Box::new(existing_editor(
            &data,
            &data.keys[1],
        ))));
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut h = egui_kittest::Harness::new_ui_state(
            move |ui, manager: &mut ProjectKeysUi| {
                let _ = manager.show_dialog(ui.ctx(), &data, &catalog);
            },
            manager,
        );
        h.set_size(egui::vec2(1280.0, 800.0));
        h.run();
        let field = h
            .get_all_by_role(egui::accesskit::Role::TextInput)
            .find(|node| node.accesskit_node().value().as_deref() == Some(binding.as_str()))
            .expect("execution binding field");
        assert_eq!(
            field.accesskit_node().is_disabled(),
            bound,
            "an existing bound variable is immutable; the first binding remains editable"
        );
        let service = h
            .get_all_by_role(egui::accesskit::Role::TextInput)
            .find(|node| node.accesskit_node().value().as_deref() == Some("내 API"))
            .expect("freeform service field");
        assert!(!service.accesskit_node().is_disabled());
        assert!(
            !h.get_by_role(egui::accesskit::Role::PasswordInput)
                .accesskit_node()
                .is_disabled()
        );
    }
}

#[test]
fn begin_add_waits_for_the_exact_scope_snapshot() {
    let first = snapshot();
    let mut second = first.clone();
    second.scope.workspace_id = "project-b".into();
    second.generation += 1;
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&first);
    manager.begin_add(
        second.scope.clone(),
        Some((
            super::super::environment::EnvironmentSelectionKind::VariableValue,
            SensitiveDraft::new("fixture-scoped-prefill".into()).unwrap(),
        )),
    );
    assert!(manager.has_dialog_or_pending_open());
    manager.prepare(&first);
    assert!(!manager.has_dialog());
    assert!(manager.has_dialog_or_pending_open());
    manager.prepare(&second);
    let Some(Dialog::Edit(draft)) = &manager.dialog else {
        panic!("matching scope must open the staged editor");
    };
    assert_eq!(draft.key.workspace_id, "project-b");
    assert!(!draft.value.is_empty());
    manager.close_dialog();
    manager.prepare(&second);
    assert!(!manager.has_dialog_or_pending_open());
}

#[test]
fn authoritative_reload_ack_keeps_both_revisions_despite_missing_or_older_cached_projection() {
    let mut cached = snapshot();
    let mut key = cached.keys[1].clone();
    key.storage_state = KeyStorageState::Stored;
    key.credential_id = Some("fixture-protected-reference".into());
    key.last_good_revision = Some(key.revision);
    cached.keys[1] = key.clone();
    cached.api_access.push(api_access(&key));
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&cached);
    manager.open_dialog(Dialog::Edit(Box::new(existing_editor(&cached, &key))));
    let ProjectKeysIntent::Request { fence, .. } = manager
        .request(
            ProjectKeysAction::ReloadEditor {
                key_id: key.id.clone(),
            },
            PendingKind::ReloadEdit,
        )
        .unwrap()
    else {
        panic!()
    };
    let mut refreshed = cached.clone();
    refreshed.keys[1].revision += 2;
    refreshed.api_access[0].revision += 3;
    refreshed.api_access[0].service_name = Some("Latest API label".into());
    let expected_key = refreshed.keys[1].revision;
    let expected_access = refreshed.api_access[0].revision;
    manager.apply_ack(&ProjectKeysAck {
        fence,
        result: Ok(ProjectKeysAckPayload::Refreshed(Box::new(refreshed))),
    });
    // No prepare call models an unavailable snapshot try_lock immediately after
    // the ACK. A later old projection models cached data or a delayed watcher.
    for older_projection in [false, true] {
        if older_projection {
            manager.prepare(&cached);
        }
        let Some(Dialog::Edit(draft)) = &mut manager.dialog else {
            panic!("the refreshed editor must remain visible")
        };
        let ProjectKeysAction::SaveApiKey {
            expected_revision,
            expected_access_revision,
            access,
            ..
        } = draft.submit()
        else {
            panic!()
        };
        assert_eq!(expected_revision, Some(expected_key));
        assert_eq!(expected_access_revision, Some(expected_access));
        assert_eq!(access.service_name.as_deref(), Some("Latest API label"));
    }
}

#[test]
fn authoritative_reload_ack_rejects_mismatched_scope_or_generation_without_rebasing() {
    for wrong_scope in [true, false] {
        let mut cached = snapshot();
        let key = cached.keys[1].clone();
        cached.api_access.push(api_access(&key));
        let mut manager = ProjectKeysUi::default();
        manager.prepare(&cached);
        manager.open_dialog(Dialog::Edit(Box::new(existing_editor(&cached, &key))));
        let ProjectKeysIntent::Request { fence, .. } = manager
            .request(
                ProjectKeysAction::ReloadEditor {
                    key_id: key.id.clone(),
                },
                PendingKind::ReloadEdit,
            )
            .unwrap()
        else {
            panic!()
        };
        let mut foreign = cached.clone();
        foreign.keys[1].revision += 7;
        foreign.api_access[0].revision += 8;
        if wrong_scope {
            foreign.scope.workspace_id = "another-workspace".into();
        } else {
            foreign.generation += 1;
        }
        manager.apply_ack(&ProjectKeysAck {
            fence,
            result: Ok(ProjectKeysAckPayload::Refreshed(Box::new(foreign))),
        });
        manager.prepare(&cached);
        let Some(Dialog::Edit(draft)) = &mut manager.dialog else {
            panic!("an invalid refresh projection must preserve the current editor")
        };
        let ProjectKeysAction::SaveApiKey {
            expected_revision,
            expected_access_revision,
            ..
        } = draft.submit()
        else {
            panic!()
        };
        assert_eq!(expected_revision, Some(key.revision));
        assert_eq!(
            expected_access_revision,
            Some(cached.api_access[0].revision)
        );
    }
}

#[test]
fn stale_editor_requires_explicit_reload_before_using_the_latest_revision() {
    fn revision(manager: &mut ProjectKeysUi) -> Option<u64> {
        let Some(Dialog::Edit(draft)) = &mut manager.dialog else {
            panic!()
        };
        let ProjectKeysAction::SaveKey {
            expected_revision, ..
        } = draft.submit()
        else {
            panic!()
        };
        expected_revision
    }
    let data = snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    let mut draft = EditDraft::existing(data.keys[0].clone());
    let action = draft.submit();
    manager.open_dialog(Dialog::Edit(Box::new(draft)));
    let Some(ProjectKeysIntent::Request { fence, .. }) = manager.request(action, PendingKind::Edit)
    else {
        panic!("edit request expected");
    };
    manager.apply_ack(&ProjectKeysAck {
        fence,
        result: Err(ProjectKeysError::Stale),
    });
    let mut h = harness(data.clone(), manager);
    h.set_size(egui::vec2(1280.0, 800.0));
    h.run();
    let label = i18n::Catalog::load("en-US")
        .unwrap()
        .t("project_keys.reload_latest", &[]);
    h.get_by_label(&label).click();
    h.run();
    let Some(ProjectKeysIntent::Request {
        fence,
        action: ProjectKeysAction::ReloadEditor { key_id },
    }) = h.state().1.last()
    else {
        panic!("reload must request the original key metadata without resubmitting edits");
    };
    assert_eq!(key_id, "file-key");
    let fence = fence.clone();
    let mut latest = data;
    latest.keys[0].revision = 4;
    h.state_mut().0.prepare(&latest);
    assert_eq!(
        revision(&mut h.state_mut().0),
        Some(3),
        "snapshot arrival must not silently rebase an edit"
    );
    h.state_mut().0.apply_ack(&ProjectKeysAck {
        fence,
        result: Ok(ProjectKeysAckPayload::Refreshed(Box::new(latest.clone()))),
    });
    h.state_mut().0.prepare(&latest);
    assert_eq!(revision(&mut h.state_mut().0), Some(4));
    let Some(Dialog::Edit(draft)) = &h.state().0.dialog else {
        panic!()
    };
    assert!(draft.value.is_empty());
    assert!(h.state().0.dialog_error.is_none());
}

#[test]
fn compact_editor_keeps_all_controls_above_footer_on_desktop() {
    for (kind, size) in [
        (KeyKind::ApiToken, egui::vec2(1280.0, 720.0)),
        (KeyKind::PrivateKey, egui::vec2(1280.0, 720.0)),
    ] {
        let data = snapshot();
        let mut manager = ProjectKeysUi::default();
        manager.prepare(&data);
        let mut key = data.keys[0].clone();
        key.kind = kind;
        manager.dialog = Some(Dialog::Edit(Box::new(EditDraft::existing(key))));
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, manager: &mut ProjectKeysUi| {
                let _ = manager.render_dialog(ui.ctx(), &data, &catalog);
            },
            manager,
        );
        harness.set_size(size);
        harness.run();
        let modal = harness
            .ctx
            .memory(|memory| memory.area_rect(egui::Id::new("project_key_edit")))
            .unwrap();
        assert!((modal.width() - 562.0).abs() < 1.0, "{modal:?}");
        let save = harness.get_by_label("Save").rect();
        let source = harness.get_by_label("Source").rect();
        let kind_label = harness.get_by_label("Key type").rect();
        assert!((source.top() - kind_label.top()).abs() < 0.5);
        let usage = harness.get_by_label("Usage").rect();
        let delivery = harness.get_by_label("Build delivery").rect();
        assert!((usage.top() - delivery.top()).abs() < 0.5);
        let secret = harness.get_by_role(egui::accesskit::Role::PasswordInput);
        assert!(
            (secret.rect().height()
                - if kind == KeyKind::PrivateKey {
                    80.0
                } else {
                    36.0
                })
            .abs()
                < 0.5
        );
        for rect in harness
            .get_all_by_role(egui::accesskit::Role::ComboBox)
            .map(|node| node.rect())
            .chain(std::iter::once(secret.rect()))
            .chain(std::iter::once(
                harness.get_by_label("Required for build").rect(),
            ))
        {
            assert!(
                rect.top() >= source.top(),
                "control clipped above body: {rect:?}"
            );
            assert!(
                rect.bottom() < save.top() - 12.0,
                "control clipped below body: {rect:?}, footer={save:?}"
            );
        }
        assert!((save.height() - 34.0).abs() < 0.5);
        assert!(modal.top() >= 0.0 && modal.bottom() <= size.y);
    }
}

#[test]
fn page_error_is_not_inherited_by_a_fresh_editor() {
    let data = snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.notice = Some(Err(ProjectKeysError::Stale));
    manager.dialog = Some(Dialog::Edit(Box::new(EditDraft::existing(
        data.keys[0].clone(),
    ))));
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let error = error_label(ProjectKeysError::Stale, &catalog);
    let mut harness = egui_kittest::Harness::new_ui_state(
        move |ui, manager: &mut ProjectKeysUi| {
            let _ = manager.render_dialog(ui.ctx(), &data, &catalog);
        },
        manager,
    );
    harness.set_size(egui::vec2(1280.0, 800.0));
    harness.run();
    assert!(harness.query_by_label(&error).is_none());
    assert_eq!(harness.state().notice, Some(Err(ProjectKeysError::Stale)));
}

#[test]
fn edit_popup_uses_560_shell_36_fields_34_footer_and_narrow_internal_scroll() {
    for width in [800.0, 390.0] {
        let data = design_snapshot();
        let mut manager = ProjectKeysUi::default();
        manager.prepare(&data);
        manager.dialog = Some(Dialog::Edit(Box::new(EditDraft::existing(
            data.keys[0].clone(),
        ))));
        let mut harness = harness(data, manager);
        harness.set_size(egui::vec2(width, 600.0));
        harness.run();
        let rect = harness
            .ctx
            .memory(|m| m.area_rect(egui::Id::new("project_key_edit")))
            .unwrap();
        // The shared frame adds its 1pt border on both sides of the 560pt body.
        assert!(rect.width() <= 562.5, "{rect:?}");
        assert!(rect.left() >= 0.0 && rect.right() <= width, "{rect:?}");
        assert!(rect.top() >= 0.0 && rect.bottom() <= 600.0, "{rect:?}");
        if width >= 800.0 {
            assert!((rect.width() - 562.0).abs() < 1.0, "{rect:?}");
        }
        let save = harness.get_by_label("Save").rect();
        assert!((save.height() - 34.0).abs() < 0.5);
        assert!(save.bottom() <= 600.0);
        harness.get_by_label("Source");
        let secret = harness.get_by_role(egui::accesskit::Role::PasswordInput);
        assert!((secret.rect().height() - 36.0).abs() < 0.5);
        let choices = harness
            .get_all_by_role(egui::accesskit::Role::ComboBox)
            .filter(|node| !node.accesskit_node().is_disabled())
            .collect::<Vec<_>>();
        assert_eq!(choices.len(), 4);
        for choice in choices {
            assert!((choice.rect().height() - 36.0).abs() < 0.5);
        }
    }
}

#[test]
fn context_prefill_opens_new_key_in_current_scope_without_snapshot_secret() {
    let data = snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.prefill(
        super::super::environment::EnvironmentSelectionKind::VariableValue,
        SensitiveDraft::new("fixture-only-value".into()).unwrap(),
    );
    manager.prepare(&data);
    let Some(Dialog::Edit(edit)) = &mut manager.dialog else {
        panic!()
    };
    assert_eq!(edit.key.workspace_id, "project-a");
    assert!(!edit.value.is_empty());
    assert!(data.keys.iter().all(|k| k.name != "fixture-only-value"));
}

#[test]
fn long_build_keeps_copy_edit_and_source_actions_available() {
    let mut data = snapshot();
    data.keys.truncate(1);
    data.recipes.push(ProjectBuildRecipe {
        id: "recipe".into(),
        workspace_id: data.scope.workspace_id.clone(),
        environment: data.scope.environment,
        revision: 1,
        name: "Fixture build".into(),
        command: vec!["fixture-runner".into()],
        cwd: "/isolated/project-a".into(),
    });
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    let Some(ProjectKeysIntent::Request { fence, .. }) = manager.request(
        ProjectKeysAction::RunRecipe {
            recipe_id: "recipe".into(),
            expected_revision: 1,
        },
        PendingKind::General,
    ) else {
        panic!()
    };
    assert!(manager.pending.is_none());
    assert_eq!(manager.build_pending.as_ref(), Some(&fence));
    let mut harness = harness(data, manager);
    harness.set_size(egui::vec2(850.0, 800.0));
    harness.run();
    harness.get_by_label("Copy API_TOKEN").click();
    harness.run();
    assert!(matches!(
        harness.state().1.last(),
        Some(ProjectKeysIntent::Request {
            action: ProjectKeysAction::CopyKey { .. },
            ..
        })
    ));
    let manager = &mut harness.state_mut().0;
    let copy_fence = manager.pending.as_ref().unwrap().fence.clone();
    manager.apply_ack(&ProjectKeysAck {
        fence: copy_fence,
        result: Ok(ProjectKeysAckPayload::Copied),
    });
    harness.get_by_label("Edit API_TOKEN").click();
    harness.run();
    assert!(harness.state().0.has_dialog());
    harness.get_by_label("Cancel").click();
    harness.run();
    harness.get_by_label("Sources").click();
    harness.run();
    assert!(harness.state().0.has_dialog());
}

#[test]
fn build_completion_from_old_project_releases_slot_without_current_success() {
    let data = snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    let Some(ProjectKeysIntent::Request { fence, .. }) = manager.request(
        ProjectKeysAction::RunRecipe {
            recipe_id: "recipe".into(),
            expected_revision: 1,
        },
        PendingKind::General,
    ) else {
        panic!()
    };
    let mut next = data;
    next.scope.workspace_id = "project-b".into();
    next.generation += 1;
    manager.prepare(&next);
    assert!(manager.build_pending.is_some());
    manager.apply_ack(&ProjectKeysAck {
        fence,
        result: Ok(ProjectKeysAckPayload::Done),
    });
    assert!(manager.build_pending.is_none());
    assert!(manager.notice.is_none());
}

#[test]
fn selected_source_preview_ack_drives_exact_import_intent() {
    let mut data = snapshot();
    data.keys.clear();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.dialog = Some(Dialog::Source(dialogs::SourceDraft::new(&data)));
    let mut harness = harness(data, manager);
    harness.set_size(egui::vec2(800.0, 850.0));
    harness.run();
    harness.get_by_label("Read names").click();
    harness.run();
    let ProjectKeysIntent::Request {
        fence,
        action: ProjectKeysAction::PreviewSource { source },
    } = harness.state().1.last().unwrap()
    else {
        panic!("preview request expected");
    };
    assert_eq!(
        source,
        &KeySource::Dotenv {
            file: ".env.development".into()
        }
    );
    let fence = fence.clone();
    let source = source.clone();
    harness.state_mut().0.apply_ack(&ProjectKeysAck {
        fence,
        result: Ok(ProjectKeysAckPayload::Preview(SourcePreview {
            source: source.clone(),
            names: vec!["API_TOKEN".into(), "CLIENT_SECRET".into()],
        })),
    });
    harness.run();
    harness.get_by_label("CLIENT_SECRET").click();
    harness.run();
    harness.get_by_label("Import selected").click();
    harness.run();
    let ProjectKeysIntent::Request {
        action:
            ProjectKeysAction::ImportSource {
                source: imported,
                names,
            },
        ..
    } = harness.state().1.last().unwrap()
    else {
        panic!("import request expected");
    };
    assert_eq!(imported, &source);
    assert_eq!(names, &["API_TOKEN".to_owned()]);
    assert!(
        harness.state().0.has_dialog(),
        "import waits for actual acknowledgement"
    );
}

fn github_key(state: GitHubRemoteState) -> ProjectKey {
    let mut key = snapshot().keys.remove(0);
    key.github = Some(KeyGitHubStatus {
        target: GitHubTarget {
            auth_credential_id: Some("explicit-auth-token".into()),
            account_identity: "fixture-account".into(),
            scope: GitHubScope::Environment {
                owner: "fixture-owner".into(),
                repository: "fixture-repo".into(),
                environment: "production".into(),
            },
            secret_name: "REMOTE_API_TOKEN".into(),
            auto_send: false,
        },
        state,
        last_sent_revision: Some(2),
        remote_updated_at: Some("2026-10-12T00:00:00Z".into()),
        error_code: None,
    });
    key
}

#[test]
fn github_config_save_preserves_exact_auth_scope_and_opt_in() {
    let data = snapshot();
    let key = github_key(GitHubRemoteState::Sent);
    let target = key.github.as_ref().unwrap().target.clone();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.dialog = Some(Dialog::GitHub(dialogs::GitHubDraft::existing(key)));
    let mut harness = harness(data, manager);
    harness.set_size(egui::vec2(850.0, 950.0));
    harness.run();
    harness.get_by_label("Save").click();
    harness.run();
    let ProjectKeysIntent::Request {
        action:
            ProjectKeysAction::SaveGitHubTarget {
                key_id,
                expected_revision,
                target: Some(saved),
            },
        ..
    } = harness.state().1.last().unwrap()
    else {
        panic!("target save expected");
    };
    assert_eq!(saved.as_ref(), &target);
    assert!(!saved.auto_send);
    assert_eq!(key_id, "file-key");
    assert_eq!(*expected_revision, 3);
}

#[test]
fn github_reserved_prefix_hint_and_explicit_alternate_target_are_localized() {
    const HINT: &str = "GitHub reserves the GITHUB_ prefix for remote Secret names. Choose another destination name.";
    for locale in ["en-US", "ko-KR", "ja-JP", "zh-Hans", "zh-Hant"] {
        let catalog = i18n::Catalog::load(locale).unwrap();
        let hint = catalog.t("project_keys.secret_name_hint", &[]);
        assert_ne!(hint, "project_keys.secret_name_hint");
        assert!(hint.contains("GITHUB_"));
    }
    let data = snapshot();
    let mut key = github_key(GitHubRemoteState::Sent);
    key.name = "GITHUB_TOKEN".into();
    key.github.as_mut().unwrap().target.secret_name = "gItHuB_TOKEN".into();
    let mut expected = key.github.as_ref().unwrap().target.clone();
    expected.secret_name = "REMOTE_GITHUB_TOKEN".into();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.dialog = Some(Dialog::GitHub(dialogs::GitHubDraft::existing(key)));
    let mut h = harness(data, manager);
    h.set_size(egui::vec2(850.0, 1000.0));
    h.run();
    h.get_by_label(HINT);
    assert!(h.get_by_label("Save").accesskit_node().is_disabled());
    h.get_all_by_role(egui::accesskit::Role::TextInput)
        .find(|node| node.value().as_deref() == Some("gItHuB_TOKEN"))
        .unwrap()
        .click();
    h.run();
    h.event(egui::Event::Key {
        key: egui::Key::A,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::COMMAND,
    });
    h.event(egui::Event::Paste(expected.secret_name.clone()));
    h.run();
    assert!(!h.get_by_label("Save").accesskit_node().is_disabled());
    h.get_by_label("Save").click();
    h.run();
    let ProjectKeysIntent::Request {
        action:
            ProjectKeysAction::SaveGitHubTarget {
                target: Some(saved),
                ..
            },
        ..
    } = h.state().1.last().unwrap()
    else {
        panic!("exact destination save expected")
    };
    assert_eq!(saved.as_ref(), &expected);
}

#[test]
fn external_github_change_needs_explicit_overwrite_choice() {
    let data = snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.dialog = Some(Dialog::GitHub(dialogs::GitHubDraft::existing(github_key(
        GitHubRemoteState::ExternalChange,
    ))));
    let mut harness = harness(data, manager);
    harness.set_size(egui::vec2(850.0, 1000.0));
    harness.run();
    harness.get_by_label("Send to GitHub").click();
    harness.run();
    assert!(harness.state().1.is_empty());
    harness
        .get_by_label("I reviewed the remote change and allow replacement")
        .click();
    harness.run();
    harness.get_by_label("Send to GitHub").click();
    harness.run();
    assert!(matches!(
        harness.state().1.last(),
        Some(ProjectKeysIntent::Request {
            action: ProjectKeysAction::SendGitHub {
                expected_revision: 3,
                allow_external_change: true,
                ..
            },
            ..
        })
    ));
}

#[test]
fn build_recipe_save_keeps_argument_boundaries_and_scope() {
    let data = snapshot();
    let recipe = ProjectBuildRecipe {
        id: "recipe-id".into(),
        workspace_id: data.scope.workspace_id.clone(),
        environment: data.scope.environment,
        revision: 4,
        name: "Fixture recipe".into(),
        command: vec![
            "fixture-program".into(),
            "one argument with spaces".into(),
            "$(literal)".into(),
        ],
        cwd: "/isolated/project-a".into(),
    };
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.dialog = Some(Dialog::Recipe(dialogs::RecipeDraft::existing(
        recipe.clone(),
    )));
    let mut harness = harness(data, manager);
    harness.set_size(egui::vec2(800.0, 850.0));
    harness.run();
    harness.get_by_label("Save").click();
    harness.run();
    let ProjectKeysIntent::Request {
        action:
            ProjectKeysAction::SaveRecipe {
                recipe: saved,
                expected_revision,
            },
        ..
    } = harness.state().1.last().unwrap()
    else {
        panic!("recipe save expected");
    };
    assert_eq!(saved, &recipe);
    assert_eq!(*expected_revision, Some(4));
}

#[test]
fn conflict_and_missing_source_have_real_repair_intents() {
    let mut data = snapshot();
    data.keys.truncate(1);
    data.keys[0].storage_state = KeyStorageState::MissingSource;
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.selected = Some("file-key".into());
    let mut harness = harness(data, manager);
    harness.set_size(egui::vec2(850.0, 900.0));
    harness.run();
    harness.get_by_label("Retry").click();
    harness.run();
    assert!(matches!(
        harness.state().1.last(),
        Some(ProjectKeysIntent::Request {
            action: ProjectKeysAction::Refresh { key_id: None },
            ..
        })
    ));
    let fence = harness.state().0.pending.as_ref().unwrap().fence.clone();
    harness.state_mut().0.apply_ack(&ProjectKeysAck {
        fence,
        result: Ok(ProjectKeysAckPayload::Done),
    });
    let key = snapshot().keys.remove(0);
    harness.state_mut().0.dialog = Some(Dialog::Conflict(key));
    harness.run();
    harness.get_by_label("Use source file").click();
    harness.run();
    assert!(matches!(
        harness.state().1.last(),
        Some(ProjectKeysIntent::Request {
            action: ProjectKeysAction::ResolveConflict {
                expected_revision: 3,
                resolution: ConflictResolution::KeepSource,
                ..
            },
            ..
        })
    ));
}

#[test]
fn secret_input_bounds_unicode_masks_accessibility_and_clears_undo() {
    struct State {
        value: SensitiveDraft,
        rejected: bool,
    }
    let id = egui::Id::new("project-key-secret-bounds");
    let mut harness = egui_kittest::Harness::new_ui_state(
        move |ui, state: &mut State| {
            let mut buffer =
                super::super::popup::MaskedSecretBuffer::new(state.value.text_mut(), 12);
            let response =
                super::super::popup::secret_input(ui, &mut buffer, id, "Fixture secret", false);
            if buffer.finish() {
                state.rejected = true;
            } else if response.changed() {
                state.rejected = false;
            }
        },
        State {
            value: SensitiveDraft::default(),
            rejected: false,
        },
    );
    harness.run();
    harness
        .get_by_role(egui::accesskit::Role::PasswordInput)
        .click();
    harness.run();
    harness
        .get_by_role(egui::accesskit::Role::PasswordInput)
        .type_text("가나다라");
    harness.run();
    assert_eq!(harness.state_mut().value.text_mut(), "가나다라");
    let node = harness.get_by_role(egui::accesskit::Role::PasswordInput);
    assert!(node.value().is_none_or(|value| !value.contains("가나다라")));
    harness.event(egui::Event::Paste("마바사".into()));
    harness.run();
    assert!(harness.state().rejected);
    assert_eq!(harness.state_mut().value.text_mut(), "가나다라");
    let state = egui::text_edit::TextEditState::load(&harness.ctx, id).unwrap();
    let cursor = egui::text::CCursorRange::one(egui::text::CCursor::new(4));
    assert!(!state.undoer().has_undo(&(cursor, "가나다라".into())));
    let mut selected = state;
    selected
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(4),
        )));
    selected.store(&harness.ctx, id);
    harness.event(egui::Event::Paste("oversized-selected-replacement".into()));
    harness.run();
    assert_eq!(
        harness.state_mut().value.text_mut(),
        "가나다라",
        "rejected paste preserves selected secret"
    );
}

#[test]
#[allow(deprecated)] // Exercise Enabled/Disabled events emitted by older native IME integrations.
fn project_key_secret_accepts_korean_ime_commit_and_preserves_rejected_selection() {
    let data = snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.dialog = Some(Dialog::Edit(Box::new(EditDraft::existing(
        data.keys[0].clone(),
    ))));
    let mut h = harness(data, manager);
    h.set_size(egui::vec2(850.0, 1000.0));
    h.run();
    h.get_by_role(egui::accesskit::Role::PasswordInput).click();
    h.run();
    let id = h.ctx.memory(|memory| memory.focused()).unwrap();
    for event in [
        egui::ImeEvent::Enabled,
        egui::ImeEvent::Preedit {
            text: "ㅎ".into(),
            active_range_chars: Some(0..1),
        },
        egui::ImeEvent::Preedit {
            text: "한".into(),
            active_range_chars: Some(0..1),
        },
    ] {
        h.event(egui::Event::Ime(event));
        h.run();
        let exposed = h.get_by_role(egui::accesskit::Role::PasswordInput).value();
        assert!(exposed.is_none_or(|value| !value.contains(['ㅎ', '한'])));
        let state = egui::text_edit::TextEditState::load(&h.ctx, id).unwrap();
        assert!(!format!("{:?}", state.undoer()).contains(['ㅎ', '한']));
    }
    let Some(Dialog::Edit(edit)) = &mut h.state_mut().0.dialog else {
        panic!("edit dialog expected")
    };
    assert_eq!(
        edit.value.text_mut(),
        "한",
        "preedit replaces the previous composition"
    );
    h.event(egui::Event::Ime(egui::ImeEvent::Commit("한국어".into())));
    h.run();
    h.event(egui::Event::Ime(egui::ImeEvent::Disabled));
    h.run();
    let Some(Dialog::Edit(edit)) = &mut h.state_mut().0.dialog else {
        panic!("edit dialog expected")
    };
    assert_eq!(edit.value.text_mut(), "한국어");
    let cursor =
        egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(3));
    let mut selected = egui::text_edit::TextEditState::load(&h.ctx, id).unwrap();
    assert!(!format!("{:?}", selected.undoer()).contains("한국어"));
    selected.cursor.set_char_range(Some(cursor));
    selected.store(&h.ctx, id);
    h.event(egui::Event::Ime(egui::ImeEvent::Enabled));
    h.run();
    h.event(egui::Event::Ime(egui::ImeEvent::Preedit {
        text: "x".repeat(SECRET_INPUT_BYTES_MAX + 1),
        active_range_chars: None,
    }));
    h.run();
    h.event(egui::Event::Ime(egui::ImeEvent::Commit(
        "x".repeat(SECRET_INPUT_BYTES_MAX + 1),
    )));
    h.run();
    h.event(egui::Event::Ime(egui::ImeEvent::Disabled));
    h.run();
    let Some(Dialog::Edit(edit)) = &mut h.state_mut().0.dialog else {
        panic!("edit dialog expected")
    };
    assert_eq!(
        edit.value.text_mut(),
        "한국어",
        "rejected IME replacement preserves the selected secret"
    );
    let state = egui::text_edit::TextEditState::load(&h.ctx, id).unwrap();
    assert_eq!(state.cursor.char_range(), Some(cursor));
    assert!(!format!("{:?}", state.undoer()).contains("한국어"));
    assert!(
        h.get_by_role(egui::accesskit::Role::PasswordInput)
            .value()
            .is_none_or(|value| !value.contains("한국어"))
    );
    assert!(h.get_by_label("Save").accesskit_node().is_disabled());
}

#[test]
fn github_credential_pagination_emits_scoped_cursor_and_keeps_target() {
    let mut data = snapshot();
    data.auth_credentials = (0..128)
        .map(|index| CredentialChoice {
            id: format!("auth-{index:03}"),
            label: format!("Token {index}"),
            provider: "github".into(),
        })
        .collect();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    let key = github_key(GitHubRemoteState::Sent);
    let target = key.github.as_ref().unwrap().target.clone();
    manager.dialog = Some(Dialog::GitHub(dialogs::GitHubDraft::existing(key)));
    let mut harness = harness(data, manager);
    harness.set_size(egui::vec2(850.0, 1000.0));
    harness.run();
    harness.get_by_label("Next credentials").click();
    harness.run();
    let ProjectKeysIntent::Request {
        fence,
        action: ProjectKeysAction::LoadCredentials { after },
    } = harness.state().1.last().unwrap()
    else {
        panic!()
    };
    assert_eq!(after.as_deref(), Some("auth-127"));
    let fence = fence.clone();
    harness.state_mut().0.apply_ack(&ProjectKeysAck {
        fence,
        result: Ok(ProjectKeysAckPayload::CredentialsPage {
            choices: vec![CredentialChoice {
                id: "auth-128".into(),
                label: "Token 128".into(),
                provider: "github".into(),
            }],
            after: None,
        }),
    });
    harness.run();
    harness.get_by_label("Save").click();
    harness.run();
    let ProjectKeysIntent::Request {
        action:
            ProjectKeysAction::SaveGitHubTarget {
                target: Some(saved),
                ..
            },
        ..
    } = harness.state().1.last().unwrap()
    else {
        panic!()
    };
    assert_eq!(saved.as_ref(), &target);
}

#[test]
fn generated_cli_commands_quote_exact_scope_and_recipe_identifiers() {
    let scope = ProjectKeysScope {
        workspace_id: "project's id".into(),
        environment: KeyEnvironment::Production,
    };
    assert_eq!(
        build_command(&scope, Some("recipe id")),
        "deppy-sijo env run --workspace 'project'\\''s id' --environment prod --recipe 'recipe id'"
    );
    assert_eq!(
        build_command(&scope, None),
        "deppy-sijo env run --workspace 'project'\\''s id' --environment prod -- <program> <arguments>"
    );
}

#[test]
fn korean_popup_with_app_fonts_keeps_fields_and_footer_inside_narrow_viewport() {
    let data = design_snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.dialog = Some(Dialog::Edit(Box::new(EditDraft::existing(
        data.keys[0].clone(),
    ))));
    let catalog = i18n::Catalog::load("ko-KR").unwrap();
    let mut harness = egui_kittest::Harness::new_ui_state(
        move |ui, manager: &mut ProjectKeysUi| {
            let _ = manager.show(ui, &data, &catalog);
        },
        manager,
    );
    harness.set_size(egui::vec2(390.0, 600.0));
    crate::fonts::install_cjk_fallback(&harness.ctx, None, "D2Coding", "Regular");
    crate::theme::install_palette(&harness.ctx);
    harness.ctx.set_theme(egui::Theme::Dark);
    harness.run();
    let rect = harness
        .ctx
        .memory(|memory| memory.area_rect(egui::Id::new("project_key_edit")))
        .unwrap();
    assert!(
        rect.top() >= 0.0 && rect.bottom() <= 600.0 && rect.left() >= 0.0 && rect.right() <= 390.0,
        "{rect:?}"
    );
    let save = harness.get_by_label("저장").rect();
    assert!((save.height() - 34.0).abs() < 0.5 && save.bottom() <= 600.0);
    let secret = harness
        .get_by_role(egui::accesskit::Role::PasswordInput)
        .rect();
    assert!((secret.height() - 36.0).abs() < 0.5);
    for choice in harness
        .get_all_by_role(egui::accesskit::Role::ComboBox)
        .filter(|node| !node.accesskit_node().is_disabled())
    {
        assert!((choice.rect().height() - 36.0).abs() < 0.5);
    }
}

fn design_snapshot() -> ProjectKeysSnapshot {
    let mut data = snapshot();
    data.keys[0] = github_key(GitHubRemoteState::Sent);
    let mut pem = new_key(&data.scope);
    pem.id = "pem-key".into();
    pem.name = "GITHUB_PRIVATE_KEY".into();
    pem.source = KeySource::PemFile {
        path: "github-app.pem".into(),
    };
    pem.kind = KeyKind::PrivateKey;
    pem.delivery = KeyDelivery::TemporaryFile;
    pem.required = true;
    pem.credential_id = Some("pem-reference".into());
    pem.revision = 4;
    pem.last_good_revision = Some(4);
    pem.last_good_at = Some(1791792000);
    pem.storage_state = KeyStorageState::Stored;
    pem.updated_at = 1791792000;
    data.keys.push(pem);
    data.recipes.push(ProjectBuildRecipe {
        id: "fixture-recipe".into(),
        workspace_id: data.scope.workspace_id.clone(),
        environment: data.scope.environment,
        revision: 1,
        name: "개발 빌드".into(),
        command: vec!["fixture-program".into(), "build".into()],
        cwd: "/isolated/project-a".into(),
    });
    data.receipts.push(ProjectBuildReceipt {
        owner: None,
        id: "fixture-receipt".into(),
        workspace_id: data.scope.workspace_id.clone(),
        environment: data.scope.environment,
        recipe_id: Some("fixture-recipe".into()),
        started_at: 1791792000,
        finished_at: Some(1791792012),
        exit_code: Some(0),
        used_keys: vec![
            ProjectKeyUsedVersion {
                key_id: "file-key".into(),
                name: "API_TOKEN".into(),
                revision: 3,
            },
            ProjectKeyUsedVersion {
                key_id: "pem-key".into(),
                name: "GITHUB_PRIVATE_KEY".into(),
                revision: 4,
            },
        ],
    });
    data
}

#[test]
#[ignore = "exports isolated offscreen UI PNGs for design review"]
fn render_project_keys_design_review() {
    let output = std::path::Path::new("/private/tmp/deppy-project-keys-ui-20261012");
    std::fs::create_dir_all(output).unwrap();
    for (name, size, dialog) in [
        ("desktop", egui::vec2(1360.0, 900.0), false),
        ("narrow", egui::vec2(390.0, 900.0), false),
        ("edit", egui::vec2(800.0, 700.0), true),
        ("api-quick-add", egui::vec2(1280.0, 720.0), false),
    ] {
        let data = design_snapshot();
        let mut manager = ProjectKeysUi::default();
        manager.prepare(&data);
        manager.selected = Some("file-key".into());
        let changes = vec![
            ProjectKeyChange {
                sequence: 2,
                key_id: "file-key".into(),
                workspace_id: data.scope.workspace_id.clone(),
                environment: data.scope.environment,
                revision: 3,
                occurred_at: 1791792000,
                kind: ProjectKeyChangeKind::RemoteChanged,
                storage_state: KeyStorageState::Stored,
                remote_state: Some(GitHubRemoteState::Sent),
            },
            ProjectKeyChange {
                sequence: 1,
                key_id: "file-key".into(),
                workspace_id: data.scope.workspace_id.clone(),
                environment: data.scope.environment,
                revision: 3,
                occurred_at: 1791791000,
                kind: ProjectKeyChangeKind::ValueSaved,
                storage_state: KeyStorageState::Stored,
                remote_state: None,
            },
        ];
        if dialog {
            manager.dialog = Some(Dialog::Edit(Box::new(EditDraft::existing(
                data.keys[0].clone(),
            ))));
        } else if name == "api-quick-add" {
            manager.begin_api_add(
                data.scope.clone(),
                Some((
                    super::super::environment::EnvironmentSelectionKind::ApiName,
                    SensitiveDraft::new("Weather API".into()).unwrap(),
                )),
            );
        }
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, manager: &mut ProjectKeysUi| {
                let _ = manager.show(ui, &data, &catalog);
            },
            manager,
        );
        harness.set_size(size);
        crate::fonts::install_cjk_fallback(&harness.ctx, None, "D2Coding", "Regular");
        crate::theme::install_palette(&harness.ctx);
        harness.ctx.set_theme(egui::Theme::Dark);
        harness.run();
        if let Some(fence) = harness.state().history_pending.clone() {
            harness.state_mut().apply_ack(&ProjectKeysAck {
                fence,
                result: Ok(ProjectKeysAckPayload::KeyChanges {
                    key_id: "file-key".into(),
                    changes,
                }),
            });
            harness.run();
        }
        harness
            .render()
            .unwrap()
            .save(output.join(format!("{name}.png")))
            .unwrap();
    }
}

#[test]
fn source_editor_keeps_opening_revision_and_production_default() {
    let mut data = snapshot();
    data.scope.environment = KeyEnvironment::Production;
    data.settings.environment = KeyEnvironment::Production;
    data.settings.sources = vec![KeySource::Dotenv {
        file: ".env.production".into(),
    }];
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.dialog = Some(Dialog::Source(dialogs::SourceDraft::new(&data)));
    data.settings.revision = 12;
    data.settings.sources = vec![KeySource::Dotenv {
        file: "other.production.env".into(),
    }];
    let mut h = harness(data, manager);
    h.set_size(egui::vec2(800.0, 900.0));
    h.run();
    assert!(
        h.get_by_label("Add source").accesskit_node().is_disabled(),
        "duplicate source is disabled"
    );
    h.get_by_label("PEM file").click();
    h.run();
    h.get_by_label("Environment file").click();
    h.run();
    let Some(Dialog::Source(draft)) = &h.state().0.dialog else {
        panic!()
    };
    assert_eq!(
        draft.source,
        KeySource::Dotenv {
            file: ".env.production".into()
        }
    );
    h.get_by_label("Save sources").click();
    h.run();
    assert!(matches!(
        h.state().1.last(),
        Some(ProjectKeysIntent::Request {
            action: ProjectKeysAction::SetSources {
                expected_revision: 2,
                ..
            },
            ..
        })
    ));
}

#[test]
fn recipe_editor_round_trips_empty_and_multiline_arguments() {
    let data = snapshot();
    let recipe = ProjectBuildRecipe {
        id: "exact-args".into(),
        workspace_id: data.scope.workspace_id.clone(),
        environment: data.scope.environment,
        revision: 3,
        name: "Exact".into(),
        command: vec![
            "/long path/program".into(),
            "".into(),
            "one\ntwo".into(),
            "".into(),
        ],
        cwd: "/isolated/project-a".into(),
    };
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.dialog = Some(Dialog::Recipe(dialogs::RecipeDraft::existing(
        recipe.clone(),
    )));
    let mut h = harness(data, manager);
    h.set_size(egui::vec2(800.0, 900.0));
    h.run();
    h.get_by_label("Save").click();
    h.run();
    let Some(ProjectKeysIntent::Request {
        action: ProjectKeysAction::SaveRecipe { recipe: saved, .. },
        ..
    }) = h.state().1.last()
    else {
        panic!()
    };
    assert_eq!(saved.command, recipe.command);
}

#[test]
fn receipt_view_takes_newest_five_and_marks_only_running_recipe() {
    let mut data = snapshot();
    data.keys.clear();
    for i in 1..=2 {
        data.recipes.push(ProjectBuildRecipe {
            id: format!("recipe-{i}"),
            workspace_id: data.scope.workspace_id.clone(),
            environment: data.scope.environment,
            revision: 1,
            name: format!("Recipe {i}"),
            command: vec!["true".into()],
            cwd: "/isolated/project-a".into(),
        });
    }
    for i in (1..=6).rev() {
        data.receipts.push(ProjectBuildReceipt {
            owner: None,
            id: format!("receipt-{i}"),
            workspace_id: data.scope.workspace_id.clone(),
            environment: data.scope.environment,
            recipe_id: Some("recipe-1".into()),
            started_at: 1791792000 + i * 60,
            finished_at: Some(1791792001 + i * 60),
            exit_code: Some(i as i32),
            used_keys: vec![],
        });
    }
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager
        .request(
            ProjectKeysAction::RunRecipe {
                recipe_id: "recipe-2".into(),
                expected_revision: 1,
            },
            PendingKind::General,
        )
        .unwrap();
    let mut h = harness(data, manager);
    h.set_size(egui::vec2(900.0, 1100.0));
    h.run();
    let running: Vec<_> = h
        .query_all(egui_kittest::kittest::By::new().label("Build running"))
        .collect();
    assert_eq!(running.len(), 1);
    h.get_by_label(&format!(
        "Build exited: 6 · {}",
        table::saved_time(1791792360)
    ));
    assert!(
        h.query_all(egui_kittest::kittest::By::new().label(&format!(
            "Build exited: 1 · {}",
            table::saved_time(1791792060)
        )))
        .next()
        .is_none()
    );
}

#[test]
fn detail_history_is_scoped_bounded_and_does_not_block_copy() {
    let data = snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.selected = Some("file-key".into());
    let mut h = harness(data.clone(), manager);
    h.set_size(egui::vec2(1280.0, 1000.0));
    h.run();
    let fence = h.state().0.history_pending.clone().unwrap();
    assert!(h.state().0.pending.is_none());
    h.get_by_label("Copy API_TOKEN").click();
    h.run();
    assert!(matches!(
        h.state().1.last(),
        Some(ProjectKeysIntent::Request {
            action: ProjectKeysAction::CopyKey { .. },
            ..
        })
    ));
    let changes: Vec<_> = (1..=40)
        .rev()
        .map(|i| ProjectKeyChange {
            sequence: i,
            key_id: "file-key".into(),
            workspace_id: "project-a".into(),
            environment: KeyEnvironment::Development,
            revision: i,
            occurred_at: 1791792000 + i as i64 * 60,
            kind: ProjectKeyChangeKind::ValueSaved,
            storage_state: KeyStorageState::Stored,
            remote_state: None,
        })
        .collect();
    h.state_mut().0.apply_ack(&ProjectKeysAck {
        fence: fence.clone(),
        result: Ok(ProjectKeysAckPayload::KeyChanges {
            key_id: "file-key".into(),
            changes,
        }),
    });
    h.run();
    assert_eq!(
        h.state()
            .0
            .history
            .as_ref()
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .len(),
        32
    );
    h.get_by_label("Value stored · v40");
    h.get_by_label("Value stored · v37");
    assert!(
        h.query_all(egui_kittest::kittest::By::new().label("Value stored · v36"))
            .next()
            .is_none()
    );
    assert!(
        h.state().0.notice.is_none(),
        "history ACK cannot overwrite copy feedback"
    );
    let mut changed = data;
    changed.scope.workspace_id = "project-b".into();
    changed.generation = 8;
    h.state_mut().0.prepare(&changed);
    h.state_mut().0.apply_ack(&ProjectKeysAck {
        fence,
        result: Err(ProjectKeysError::Storage),
    });
    assert!(h.state().0.history.is_none());
}

#[test]
fn saved_timestamp_is_distinct_from_latest_metadata_change() {
    let mut data = snapshot();
    data.keys.truncate(1);
    let saved = data.keys[0].last_good_at.unwrap();
    let changed = data.keys[0].updated_at;
    assert_ne!(saved, changed);
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.selected = Some("file-key".into());
    let mut h = harness(data, manager);
    h.set_size(egui::vec2(1280.0, 1000.0));
    h.run();
    h.get_by_label(&table::saved_time(saved));
    h.get_by_label(&table::saved_time(changed));
}

#[test]
fn narrow_outer_scroll_exposes_build_below_detail_and_submits_actual_recipe() {
    let data = design_snapshot();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.selected = Some("file-key".into());
    let mut h = harness(data, manager);
    h.set_size(egui::vec2(390.0, 600.0));
    h.run();
    h.event(egui::Event::PointerMoved(egui::pos2(375.0, 590.0)));
    h.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, -1800.0),
        phase: egui::TouchPhase::Move,
        modifiers: egui::Modifiers::NONE,
    });
    h.run();
    let run = h.get_by_label("Run");
    assert!(
        run.rect().top() >= 0.0 && run.rect().bottom() <= 600.0,
        "{:?}",
        run.rect()
    );
    run.click();
    h.run();
    assert!(
        matches!(h.state().1.last(), Some(ProjectKeysIntent::Request { action: ProjectKeysAction::RunRecipe { recipe_id, expected_revision:1 }, .. }) if recipe_id=="fixture-recipe")
    );
}

#[test]
fn same_revision_remote_ack_reloads_history_and_rejects_previous_read_ack() {
    let mut data = snapshot();
    data.keys[0] = github_key(GitHubRemoteState::Pending);
    let catalog = i18n::Catalog::load("en-US").unwrap();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.selected = Some("file-key".into());
    let mut h = egui_kittest::Harness::new_ui_state(
        move |ui, state: &mut (ProjectKeysUi, ProjectKeysSnapshot, Vec<ProjectKeysIntent>)| {
            state.2.extend(state.0.show(ui, &state.1, &catalog));
        },
        (manager, data, vec![]),
    );
    h.set_size(egui::vec2(1280.0, 1000.0));
    h.run();
    let old = h.state().0.history_pending.clone().unwrap();
    let revision = h.state().1.keys[0].revision;
    let remote = h.state_mut().1.keys[0].github.as_mut().unwrap();
    remote.state = GitHubRemoteState::Sent;
    remote.last_sent_revision = Some(revision);
    remote.remote_updated_at = Some("2026-10-12T09:00:00Z".into());
    h.run();
    let new = h.state().0.history_pending.clone().unwrap();
    assert_ne!(old, new);
    assert_eq!(h.state().1.keys[0].revision, revision);
    h.state_mut().0.apply_ack(&ProjectKeysAck {
        fence: old,
        result: Err(ProjectKeysError::Storage),
    });
    assert_eq!(h.state().0.history_pending.as_ref(), Some(&new));
    assert!(h.state().0.history.as_ref().unwrap().result.is_none());
    h.state_mut().0.apply_ack(&ProjectKeysAck {
        fence: new,
        result: Ok(ProjectKeysAckPayload::KeyChanges {
            key_id: "file-key".into(),
            changes: vec![ProjectKeyChange {
                sequence: 1,
                key_id: "file-key".into(),
                workspace_id: "project-a".into(),
                environment: KeyEnvironment::Development,
                revision,
                occurred_at: 1791792000,
                kind: ProjectKeyChangeKind::RemoteChanged,
                storage_state: KeyStorageState::Stored,
                remote_state: Some(GitHubRemoteState::Sent),
            }],
        }),
    });
    h.run();
    h.get_by_label("GitHub status changed · v3");
}

#[test]
fn new_remote_only_source_saves_metadata_without_secret_and_keeps_config_action() {
    let data = snapshot();
    let mut key = new_key(&data.scope);
    key.name = "REMOTE_TOKEN".into();
    let mut edit = EditDraft::new(key);
    edit.value = SensitiveDraft::new("discard-on-remote-choice".into()).unwrap();
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.dialog = Some(Dialog::Edit(Box::new(edit)));
    let mut h = harness(data, manager);
    h.set_size(egui::vec2(850.0, 950.0));
    h.run();
    h.get_all_by_role(egui::accesskit::Role::ComboBox)
        .find(|node| !node.accesskit_node().is_disabled())
        .unwrap()
        .click();
    h.run();
    h.get_by_label("GitHub only").click();
    h.run();
    assert!(
        h.query_by_role(egui::accesskit::Role::PasswordInput)
            .is_none()
    );
    let Some(Dialog::Edit(edit)) = &h.state().0.dialog else {
        panic!()
    };
    assert!(edit.value.is_empty());
    assert_eq!(edit.key.usage, KeyUsage::StoreOnly);
    assert!(!edit.key.required);
    h.get_by_label("Save").click();
    h.run();
    assert!(
        matches!(h.state().1.last(),Some(ProjectKeysIntent::Request { action: ProjectKeysAction::SaveKey {key,expected_revision:None,value:ValueEdit::Keep},..}) if key.source==KeySource::GitHubOnly && key.credential_id.is_none() && key.last_good_revision.is_none())
    );
    let mut data = snapshot();
    let mut key = github_key(GitHubRemoteState::Sent);
    key.source = KeySource::GitHubOnly;
    key.usage = KeyUsage::StoreOnly;
    key.credential_id = None;
    key.last_good_revision = None;
    key.last_good_at = None;
    key.storage_state = KeyStorageState::NotStored;
    data.keys = vec![key];
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.selected = Some("file-key".into());
    let mut h = harness(data, manager);
    h.set_size(egui::vec2(1280.0, 1100.0));
    h.run();
    assert!(
        h.get_by_label("Copy API_TOKEN")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        h.get_by_label("Copy value for API_TOKEN")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        h.get_by_label("Send to GitHub")
            .accesskit_node()
            .is_disabled()
    );
    h.get_by_label("Configure GitHub").click();
    h.run();
    assert!(h.state().0.has_dialog());
}

#[test]
fn refresh_checks_only_selected_exact_remote_key_and_plain_scope_sources() {
    let mut data = snapshot();
    data.keys[0] = github_key(GitHubRemoteState::Sent);
    let mut manager = ProjectKeysUi::default();
    manager.prepare(&data);
    manager.selected = Some("file-key".into());
    let mut h = harness(data.clone(), manager);
    h.set_size(egui::vec2(1280.0, 1000.0));
    h.run();
    h.get_by_label("Refresh").click();
    h.run();
    assert!(
        matches!(h.state().1.last(),Some(ProjectKeysIntent::Request { action: ProjectKeysAction::Refresh {key_id:Some(id)},..}) if id=="file-key")
    );
    h.state_mut().0.selected = Some("vault-key".into());
    assert!(matches!(
        h.state().0.refresh_action(&data),
        ProjectKeysAction::Refresh { key_id: None }
    ));
    h.state_mut().0.selected = None;
    assert!(matches!(
        h.state().0.refresh_action(&data),
        ProjectKeysAction::Refresh { key_id: None }
    ));
}
