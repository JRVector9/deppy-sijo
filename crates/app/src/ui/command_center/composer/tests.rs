use super::*;
use egui_kittest::kittest::{NodeT, Queryable};

#[derive(Default)]
struct ComposerState {
    ui: UiState,
    project: Project,
    intents: Vec<Intent>,
}

fn unbound() -> ComposerState {
    ComposerState {
        project: Project {
            id: "project-a".into(),
            name: "Project A".into(),
            main_target: RouteTarget {
                workspace_id: "project-a".into(),
                ..Default::default()
            },
            main_provider: "unbound".into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn harness(state: ComposerState) -> egui_kittest::Harness<'static, ComposerState> {
    egui_kittest::Harness::builder()
        .with_size(egui::vec2(1000.0, 600.0))
        .build_ui_state(
            |ui, state: &mut ComposerState| {
                composer(
                    ui,
                    &mut state.ui,
                    &Snapshot::default(),
                    &state.project,
                    &mut state.intents,
                );
            },
            state,
        )
}

#[test]
fn composer_unbound_accepts_typing_and_enter_opens_selection_without_submit() {
    let mut harness = harness(unbound());
    let target = harness.state().project.main_target.clone();
    harness.run();
    assert!(
        !harness
            .get_by_label("메인 에이전트 선택 · 전송")
            .accesskit_node()
            .is_disabled()
    );
    harness
        .ctx
        .memory_mut(|m| m.request_focus(composer_id(&target)));
    harness.event(egui::Event::Text("가 draft".into()));
    harness.run();
    assert_eq!(harness.state().ui.draft(&target), "가 draft");
    assert!(
        harness.state().ui.draft_version(&target) > 0,
        "direct native editor changes must advance receipt identity"
    );
    let version = harness.state().ui.draft_version(&target);
    harness
        .state_mut()
        .ui
        .set_draft(target.clone(), "가 draft".into());
    assert_eq!(
        harness.state().ui.draft_version(&target),
        version,
        "a host echo is not another edit"
    );
    assert!(harness.state().intents.iter().any(|intent| matches!(intent, Intent::DraftChanged { target: found, text } if found == &target && text == "가 draft")));
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(
        !harness
            .state()
            .intents
            .iter()
            .any(|i| matches!(i, Intent::Submit { .. }))
    );
    assert!(
        harness
            .get_by_label("설정한 뒤 전송을 다시 눌러 주세요.")
            .rect()
            .is_positive()
    );
    assert!(
        harness.state().ui.composer_focused,
        "chooser owns terminal input even while editor is unfocused"
    );
    assert_eq!(harness.state().ui.draft(&target), "가 draft");
}

#[test]
fn composer_main_binding_keeps_same_project_draft_without_sending() {
    let mut initial = unbound();
    let before = initial.project.main_target.clone();
    initial.ui.set_draft(before.clone(), "보존할 초안".into());
    let other = RouteTarget {
        workspace_id: "project-b".into(),
        ..Default::default()
    };
    initial.ui.set_draft(other.clone(), "다른 프로젝트".into());
    let mut harness = harness(initial);
    harness.run();
    harness.state_mut().project.main_target.session_id = Some("exact-main".into());
    harness.state_mut().project.main_input.enabled = true;
    harness.run();
    let after = harness.state().project.main_target.clone();
    assert_eq!(harness.state().ui.draft(&after), "보존할 초안");
    assert_eq!(harness.state().ui.draft(&other), "다른 프로젝트");
    assert!(
        !harness
            .state()
            .intents
            .iter()
            .any(|i| matches!(i, Intent::Submit { .. }))
    );
}

#[test]
fn composer_collision_keeps_prebind_draft_visible_and_can_restore_session_draft() {
    let mut initial = unbound();
    let before = initial.project.main_target.clone();
    let mut after = before.clone();
    after.session_id = Some("exact-main".into());
    initial
        .ui
        .set_draft(before.clone(), "main before bind".into());
    initial
        .ui
        .set_draft(after.clone(), "independent session draft".into());
    let mut harness = harness(initial);
    harness.run();
    harness.state_mut().project.main_target = after.clone();
    harness.run();
    assert_eq!(harness.state().ui.draft(&after), "main before bind");
    assert_eq!(
        harness.state().ui.draft(&before),
        "independent session draft"
    );
    harness.get_by_label("다른 초안 보기").click();
    harness.run();
    assert_eq!(
        harness.state().ui.draft(&after),
        "independent session draft"
    );
    assert_eq!(harness.state().ui.draft(&before), "main before bind");
    assert!(
        !harness
            .state()
            .intents
            .iter()
            .any(|i| matches!(i, Intent::Submit { .. }))
    );
}

#[test]
fn composer_explicit_binding_save_and_cancel_never_send_draft() {
    let mut initial = unbound();
    initial.project.save_settings.enabled = true;
    initial.project.config = ProjectConfig {
        name: "Project A".into(),
        directory: "/project-a".into(),
        provider: "codex".into(),
        model: "real-model".into(),
        effort: "xhigh".into(),
        worker_providers: vec!["codex".into()],
        allowed_scope_roots: vec!["src".into()],
        revision: "1".into(),
        parallel_limit: 2,
        ..Default::default()
    };
    initial.project.available_agents.push(AgentOption {
        provider: "codex".into(),
        model: "real-model".into(),
        effort: "xhigh".into(),
    });
    let mut bound = initial.project.main_target.clone();
    bound.session_id = Some("actual-session".into());
    initial.project.sessions.push(Session {
        target: bound.clone(),
        title: "Real session".into(),
        provider: "codex".into(),
        model: Some("real-model".into()),
        effort: Some("xhigh".into()),
        device: "local".into(),
        input: Capability {
            enabled: true,
            reason: String::new(),
        },
        ..Default::default()
    });
    let before = initial.project.main_target.clone();
    initial.ui.set_draft(before.clone(), "가 goal".into());
    let mut harness = harness(initial);
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "메인 에이전트 선택")
        .click();
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::ComboBox, "메인 세션")
        .click();
    harness.run();
    harness.get_by_label("Real session · codex · local").click();
    harness.run();
    harness.get_by_label("설정 저장").click();
    harness.run();
    let saved = harness
        .state()
        .intents
        .iter()
        .find_map(|intent| {
            if let Intent::SaveProject { config, .. } = intent {
                Some(config.clone())
            } else {
                None
            }
        })
        .expect("explicit binding save");
    assert_eq!(saved.main_session_id, bound.session_id);
    assert_eq!(
        (&saved.provider, &saved.model, &saved.effort),
        (
            &"codex".to_owned(),
            &"real-model".to_owned(),
            &"xhigh".to_owned()
        )
    );
    assert!(
        !harness
            .state()
            .intents
            .iter()
            .any(|i| matches!(i, Intent::Submit { .. } | Intent::CreateMain { .. }))
    );
    harness.state_mut().project.main_target = bound.clone();
    harness.state_mut().project.config = saved;
    harness.state_mut().project.config.revision = "2".into();
    harness.state_mut().project.main_input.enabled = true;
    harness.run();
    assert_eq!(harness.state().ui.draft(&bound), "가 goal");
    assert!(
        !harness
            .state()
            .intents
            .iter()
            .any(|i| matches!(i, Intent::Submit { .. }))
    );
    harness.get_by_label("취소").click();
    harness.run();
    assert_eq!(harness.state().ui.draft(&bound), "가 goal");
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(harness.state().intents.contains(&Intent::Submit {
        target: bound,
        text: "가 goal".into()
    }));
}

#[test]
fn composer_unavailable_execution_still_edits_but_cannot_submit_or_fall_back() {
    let mut initial = unbound();
    initial.project.main_target.session_id = Some("blocked-main".into());
    let target = initial.project.main_target.clone();
    initial.ui.set_draft(target.clone(), "local only".into());
    let mut harness = harness(initial);
    harness.run();
    assert!(
        !harness
            .get_by_label("Project A / 메인 에이전트 unbound · 전송")
            .accesskit_node()
            .is_disabled()
    );
    assert!(harness.get_by_label("전송").accesskit_node().is_disabled());
    harness
        .ctx
        .memory_mut(|m| m.request_focus(composer_id(&target)));
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(
        !harness
            .state()
            .intents
            .iter()
            .any(|i| matches!(i, Intent::Submit { .. }))
    );
    harness.state_mut().ui.view = View::Task;
    harness.state_mut().ui.selected_task_id = Some("vanished-task".into());
    harness.run();
    assert_eq!(harness.state().ui.draft(&target), "local only");
    assert!(
        harness
            .get_by_label("Project A / 메인 에이전트 unbound · 전송")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn composer_snapshot_binding_while_picker_open_keeps_newest_typed_draft() {
    let mut initial = unbound();
    let before = initial.project.main_target.clone();
    initial
        .ui
        .set_draft(before.clone(), "processed local".into());
    let mut harness = harness(initial);
    harness.run();
    harness
        .ctx
        .memory_mut(|m| m.request_focus(composer_id(&before)));
    harness.event(egui::Event::Text(" + newest edit".into()));
    harness.run();
    let newest = harness.state().ui.draft(&before).to_owned();
    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "메인 에이전트 선택")
        .click();
    harness.run();
    let mut bound = before.clone();
    bound.session_id = Some("confirmed-main".into());
    // An arriving snapshot may still contain the last processed draft while the
    // native editor already has another local change.
    harness
        .state_mut()
        .ui
        .set_draft(bound.clone(), "processed local".into());
    harness.state_mut().project.main_target = bound.clone();
    harness.state_mut().project.loaded = true;
    harness.state_mut().project.main_input.enabled = true;
    harness.run();
    assert_eq!(harness.state().ui.draft(&bound), newest);
    assert!(harness.state().ui.composer_focused);
    assert!(
        harness
            .query_by_label("설정한 뒤 전송을 다시 눌러 주세요.")
            .is_some()
    );
    assert!(
        !harness
            .state()
            .intents
            .iter()
            .any(|i| matches!(i, Intent::Submit { .. } | Intent::CreateMain { .. }))
    );
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert_eq!(harness.state().ui.draft(&bound), newest);
    assert!(
        harness
            .query_by_label("설정한 뒤 전송을 다시 눌러 주세요.")
            .is_none()
    );
    assert!(
        harness.ctx.memory(|m| m.has_focus(composer_id(&bound))),
        "Escape returns focus to the current exact composer"
    );
}

#[test]
fn composer_canonically_identical_binding_drafts_do_not_offer_duplicate_alternate() {
    let mut initial = unbound();
    let before = initial.project.main_target.clone();
    let mut bound = before.clone();
    bound.session_id = Some("confirmed-main".into());
    initial.ui.set_draft(before, "가 draft".into());
    initial.ui.set_draft(bound.clone(), "가 draft".into());
    let mut harness = harness(initial);
    harness.run();
    harness.state_mut().project.main_target = bound.clone();
    harness.run();
    assert_eq!(harness.state().ui.draft(&bound), "가 draft");
    assert!(harness.query_by_label("다른 초안 보기").is_none());
    assert!(
        !harness
            .state()
            .intents
            .iter()
            .any(|i| matches!(i, Intent::Submit { .. }))
    );
}

#[test]
fn composer_confirmed_save_rebases_newer_picker_edits_without_sending() {
    let mut initial = unbound();
    let target = initial.project.main_target.clone();
    initial
        .ui
        .set_draft(target.clone(), "preserved draft".into());
    initial.project.save_settings.enabled = true;
    initial.project.config = ProjectConfig {
        name: "Project A".into(),
        directory: "/project-a".into(),
        provider: "codex".into(),
        worker_providers: vec!["codex".into()],
        allowed_scope_roots: vec!["src".into()],
        revision: "1".into(),
        parallel_limit: 2,
        ..Default::default()
    };
    initial.project.available_agents = vec![
        AgentOption {
            provider: "codex".into(),
            ..Default::default()
        },
        AgentOption {
            provider: "claude".into(),
            ..Default::default()
        },
    ];
    let mut harness = harness(initial);
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "메인 에이전트 선택")
        .click();
    harness.run();
    harness.get_by_label("설정 저장").click();
    harness.run();
    let saved = harness
        .state()
        .intents
        .iter()
        .find_map(|intent| match intent {
            Intent::SaveProject { config, .. } => Some(config.clone()),
            _ => None,
        })
        .expect("explicit save A");
    harness
        .get_by_role_and_label(egui::accesskit::Role::ComboBox, "메인 에이전트")
        .click();
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "claude")
        .click();
    harness.run();
    harness.state_mut().project.config = saved;
    harness.state_mut().project.config.revision = "2".into();
    harness.run();
    harness.get_by_label("설정 저장").click();
    harness.run();
    let latest = harness
        .state()
        .intents
        .iter()
        .rev()
        .find_map(|intent| match intent {
            Intent::SaveProject { config, .. } => Some(config),
            _ => None,
        })
        .unwrap();
    assert_eq!(latest.provider, "claude");
    assert_eq!(
        latest.revision, "2",
        "the acknowledged own save must rebase newer local edits"
    );
    assert!(
        latest.model.is_empty() && latest.effort.is_empty(),
        "actual provider-default tuples remain selectable"
    );
    assert_eq!(harness.state().ui.draft(&target), "preserved draft");
    assert!(
        !harness
            .state()
            .intents
            .iter()
            .any(|i| matches!(i, Intent::Submit { .. } | Intent::CreateMain { .. }))
    );
}

#[test]
fn composer_text_editor_matches_approved_focus_heights() {
    for (width, compact, expanded) in [(1000.0, 62.0, 100.0), (390.0, 60.0, 88.0)] {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(width, 600.0))
            .build_ui_state(
                |ui, state: &mut ComposerState| {
                    composer(
                        ui,
                        &mut state.ui,
                        &Snapshot::default(),
                        &state.project,
                        &mut state.intents,
                    );
                },
                unbound(),
            );
        harness.run();
        let editor = harness.get_by_label("메인 에이전트 선택 · 전송").rect();
        assert!(
            (editor.height() - compact).abs() <= 1.0,
            "compact editor {editor:?}, expected {compact}"
        );
        let target = harness.state().project.main_target.clone();
        harness
            .ctx
            .memory_mut(|memory| memory.request_focus(composer_id(&target)));
        harness.run();
        harness.run();
        let editor = harness.get_by_label("메인 에이전트 선택 · 전송").rect();
        assert!(
            (editor.height() - expanded).abs() <= 1.0,
            "focused editor {editor:?}, expected {expanded}"
        );
        let send = harness.get_by_label("전송").rect();
        assert_eq!(send.size(), egui::vec2(34.0, 34.0));
    }
}
