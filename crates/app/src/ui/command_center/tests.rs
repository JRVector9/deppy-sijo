use super::*;
use egui_kittest::kittest::{NodeT, Queryable};

#[test]
fn approved_desktop_geometry_and_narrow_drawers() {
    for (width, left, right) in [
        (1440.0, 220.0, Some(312.0)),
        (1280.0, 220.0, Some(312.0)),
        (1000.0, 196.0, None),
        (760.0, 0.0, None),
        (390.0, 0.0, None),
    ] {
        let l = LayoutRects::new(
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 900.0)),
            false,
            false,
            true,
            1.0,
        );
        assert_eq!(l.header.height(), if width <= 760.0 { 46.0 } else { 48.0 });
        assert_eq!(l.sidebar.map(|r| r.width()).unwrap_or_default(), left);
        assert_eq!(l.monitor.map(|r| r.width()), right);
        assert!(l.content.width() >= 300.0);
        assert!(l.content.bottom() <= l.composer.top());
    }
}

#[test]
fn composer_focus_does_not_move_the_content_origin_or_change_target_drafts() {
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 900.0));
    let compact = LayoutRects::new(rect, false, false, true, 1.25);
    let focused = LayoutRects::new(rect, false, true, true, 1.25);
    assert_eq!(compact.content.min, focused.content.min);
    assert!(focused.composer.height() > compact.composer.height());
    let mut state = UiState::default();
    let a = RouteTarget {
        workspace_id: "same".into(),
        session_id: Some("pane-a".into()),
        task_id: Some("task-a".into()),
    };
    let mut b = a.clone();
    b.session_id = Some("pane-b".into());
    b.task_id = Some("task-b".into());
    state.set_draft(a.clone(), "첫 초안".into());
    state.set_draft(b.clone(), "다른 초안".into());
    state.clear_draft(&a);
    assert_eq!(state.draft(&b), "다른 초안");
}

fn fixture() -> Snapshot {
    let target = RouteTarget {
        workspace_id: "project-a".into(),
        session_id: Some("opaque-pane-a".into()),
        task_id: None,
    };
    Snapshot {
        selected_workspace_id: Some("project-a".into()),
        projects: vec![Project {
            id: "project-a".into(),
            name: "프로젝트 A".into(),
            directory: "/projects/a".into(),
            main_target: target,
            main_input: Capability {
                enabled: true,
                reason: String::new(),
            },
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn terminal_view_exposes_native_slot_separate_from_composer() {
    let snapshot = fixture();
    let mut state = UiState {
        active_workspace: snapshot.selected_workspace_id.clone(),
        ..Default::default()
    };
    state.open_terminal(snapshot.projects[0].main_target.clone());
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1280.0, 900.0))
        .build_ui_state(
            |ui, state| {
                show(ui, state, &snapshot);
            },
            state,
        );
    harness.run();
    let s = harness.state();
    let slot = s.terminal_rect.expect("native terminal slot");
    assert_eq!(
        s.terminal_target.as_ref(),
        Some(&snapshot.projects[0].main_target)
    );
    assert!(slot.bottom() <= s.last_layout.unwrap().composer.top());
    assert!(s.last_layout.unwrap().composer.height() > 80.0);
}

#[derive(Default)]
struct HarnessState {
    ui: UiState,
    intents: Vec<Intent>,
}

#[test]
fn task_work_tab_includes_submitted_result_and_has_four_destinations() {
    let mut snapshot = rich_fixture();
    let task = &mut snapshot.projects[0].tasks[0];
    task.result = Some("실제로 제출한 작업 결과".into());
    let mut initial = UiState {
        active_workspace: snapshot.selected_workspace_id.clone(),
        ..Default::default()
    };
    initial.open_task(task.target.clone());
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1440.0, 900.0))
        .build_ui_state(
            |ui, state| {
                show(ui, state, &snapshot);
            },
            initial,
        );
    use_korean_font(&mut harness);
    harness.run();
    harness.get_by_label("작업과 결과");
    harness.get_by_label("실제로 제출한 작업 결과");
    assert!(harness.query_by_label("결과").is_none());
    harness.get_by_label("원문 터미널");
}

#[test]
fn monitor_actions_keep_full_click_height_and_exact_worker_target() {
    let snapshot = rich_fixture();
    let expected = snapshot.projects[0].tasks[0].target.clone();
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1440.0, 900.0))
        .build_ui_state(
            |ui, state: &mut HarnessState| {
                state.intents.extend(show(ui, &mut state.ui, &snapshot));
            },
            HarnessState::default(),
        );
    use_korean_font(&mut harness);
    harness.run();
    let monitor = harness.state().ui.last_layout.unwrap().monitor.unwrap();
    let button = harness
        .get_all_by_label("작업 상세")
        .find(|n| monitor.contains_rect(n.rect()))
        .expect("monitor task action");
    assert!(button.rect().height() >= 28.0, "{:?}", button.rect());
    button.click();
    harness.run();
    assert_eq!(harness.state().ui.selected_task_id, expected.task_id);
    assert_eq!(harness.state().ui.view, View::Task);
}

#[test]
fn settings_default_keeps_advanced_scope_collapsed_and_agent_fields_inline() {
    let snapshot = approved_visual_fixture();
    let initial = UiState {
        active_workspace: snapshot.selected_workspace_id.clone(),
        view: View::Settings,
        ..Default::default()
    };
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1440.0, 1100.0))
        .build_ui_state(
            |ui, state| {
                show(ui, state, &snapshot);
            },
            initial,
        );
    use_korean_font(&mut harness);
    harness.run();
    assert!(harness.query_by_label("수정 범위").is_none());
    let content = harness.state().last_layout.unwrap().content;
    let provider = harness
        .get_all_by_label("메인 에이전트")
        .find(|n| content.contains(n.rect().center()))
        .unwrap();
    let model = harness.get_by_label("모델");
    assert!((provider.rect().top() - model.rect().top()).abs() < 1.0);
    harness.get_by_label("취소").click();
    harness.run();
    assert_eq!(harness.state().view, View::Main);
    assert!(harness.state().settings_draft.is_none());
}

#[test]
fn project_navigation_deduplicates_bound_sessions_and_opens_exact_independent_session() {
    let mut snapshot = rich_fixture();
    let p = &mut snapshot.projects[0];
    let unrelated = RouteTarget {
        workspace_id: p.id.clone(),
        session_id: Some("independent-exact".into()),
        task_id: None,
    };
    for (title, target) in [
        ("bound main duplicate", p.main_target.clone()),
        ("bound worker duplicate", p.tasks[0].target.clone()),
        ("independent Codex", unrelated.clone()),
        (
            "different project",
            RouteTarget {
                workspace_id: "another-project".into(),
                session_id: Some("independent-exact".into()),
                task_id: None,
            },
        ),
    ] {
        p.sessions.push(Session {
            title: title.into(),
            target,
            provider: "Codex".into(),
            device: "same computer".into(),
            ..Default::default()
        });
    }
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1440.0, 900.0))
        .build_ui_state(
            |ui, state: &mut HarnessState| {
                state.intents.extend(show(ui, &mut state.ui, &snapshot));
            },
            HarnessState::default(),
        );
    use_korean_font(&mut harness);
    harness.run();
    assert!(harness.query_by_label("bound main duplicate").is_none());
    assert!(harness.query_by_label("bound worker duplicate").is_none());
    assert!(harness.query_by_label("different project").is_none());
    let item = harness
        .get_all_by_label("independent Codex")
        .find(|n| n.accesskit_node().role() == egui::accesskit::Role::Button)
        .unwrap();
    item.click();
    harness.run();
    assert!(
        harness
            .state()
            .intents
            .contains(&Intent::OpenSession(unrelated.clone()))
    );
    assert_eq!(
        harness.state().ui.requested_terminal_target.as_ref(),
        Some(&unrelated)
    );
}

#[test]
fn removing_an_earlier_session_preserves_keyboard_activation_target() {
    struct State {
        ui: UiState,
        snapshot: Snapshot,
        intents: Vec<Intent>,
    }
    let mut snapshot = fixture();
    snapshot.projects[0].main_target.session_id = None;
    for id in ["first", "focused", "later"] {
        snapshot.projects[0].sessions.push(Session {
            title: format!("independent {id}"),
            provider: "Codex".into(),
            target: RouteTarget {
                workspace_id: "project-a".into(),
                session_id: Some(id.into()),
                task_id: None,
            },
            ..Default::default()
        });
    }
    let expected = snapshot.projects[0].sessions[1].target.clone();
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1440.0, 900.0))
        .build_ui_state(
            |ui, state: &mut State| {
                state
                    .intents
                    .extend(show(ui, &mut state.ui, &state.snapshot));
            },
            State {
                ui: UiState::default(),
                snapshot,
                intents: Vec::new(),
            },
        );
    use_korean_font(&mut harness);
    harness.run();
    harness
        .get_all_by_label("independent focused")
        .find(|n| n.accesskit_node().role() == egui::accesskit::Role::Button)
        .unwrap()
        .focus();
    harness.run();
    harness.state_mut().snapshot.projects[0].sessions.remove(0);
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert_eq!(
        harness
            .state()
            .intents
            .iter()
            .filter_map(|i| if let Intent::OpenSession(t) = i {
                Some(t)
            } else {
                None
            })
            .collect::<Vec<_>>(),
        vec![&expected]
    );
}

#[test]
fn settings_agent_change_releases_binding_without_executing_or_sending() {
    let mut snapshot = rich_fixture();
    let p = &mut snapshot.projects[0];
    p.config.provider = "Codex".into();
    p.config.model = "codex-model".into();
    p.config.effort = "xhigh".into();
    p.config.main_session_id = p.main_target.session_id.clone();
    p.sessions.push(Session {
        target: p.main_target.clone(),
        title: "existing Codex main".into(),
        provider: "Codex".into(),
        model: Some("codex-model".into()),
        effort: Some("xhigh".into()),
        device: "이 Mac".into(),
        ..Default::default()
    });
    p.available_providers = vec!["Codex".into(), "Claude".into()];
    p.available_agents = vec![
        AgentOption {
            provider: "Codex".into(),
            model: "codex-model".into(),
            effort: "xhigh".into(),
        },
        AgentOption {
            provider: "Claude".into(),
            model: "claude-model".into(),
            effort: "high".into(),
        },
    ];
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1440.0, 1100.0))
        .build_ui_state(
            |ui, state: &mut HarnessState| {
                state.intents.extend(show(ui, &mut state.ui, &snapshot));
            },
            HarnessState {
                ui: UiState {
                    active_workspace: snapshot.selected_workspace_id.clone(),
                    view: View::Settings,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
    use_korean_font(&mut harness);
    harness.run();
    let content = harness.state().ui.last_layout.unwrap().content;
    harness
        .get_all_by_value("Codex")
        .find(|n| content.contains_rect(n.rect()) && n.rect().width() > 150.0)
        .unwrap()
        .click();
    harness.run();
    harness.get_by_label("Claude").click();
    harness.run();
    let config = &harness.state().ui.settings_draft.as_ref().unwrap().1;
    assert_eq!(
        (&config.provider, &config.model, &config.effort),
        (&"Claude".into(), &"claude-model".into(), &"high".into())
    );
    assert!(config.main_session_id.is_none());
    assert!(harness.state().intents.is_empty());
    harness
        .get_by_label(text(&snapshot, "execution_rules"))
        .click();
    harness.run();
    harness
        .get_all_by_value(text(&snapshot, "unbound"))
        .find(|n| n.accesskit_node().role() == egui::accesskit::Role::ComboBox)
        .unwrap()
        .scroll_to_me();
    harness.run();
    harness
        .get_all_by_value(text(&snapshot, "unbound"))
        .find(|n| n.accesskit_node().role() == egui::accesskit::Role::ComboBox)
        .unwrap()
        .click();
    harness.run();
    harness
        .get_by_label("existing Codex main · Codex · 이 Mac")
        .click();
    harness.run();
    let config = &harness.state().ui.settings_draft.as_ref().unwrap().1;
    assert_eq!(
        (&config.provider, &config.model, &config.effort),
        (&"Codex".into(), &"codex-model".into(), &"xhigh".into())
    );
    assert_eq!(
        config.main_session_id,
        snapshot.projects[0].main_target.session_id
    );
    assert!(harness.state().intents.is_empty());
}

fn rich_fixture() -> Snapshot {
    let mut snapshot = fixture();
    snapshot.status = "로컬 snapshot · 실행 상태는 관측된 근거만 표시".into();
    let p = &mut snapshot.projects[0];
    p.goal = "목표를 구현하고 독립 리뷰와 검증 결과를 모아 주세요.".into();
    p.main_provider = "Codex".into();
    p.main_model = "model from host".into();
    p.main_effort = "xhigh".into();
    p.main_summary =
        "수정 범위와 완료 조건을 확인했습니다. 각 작업의 실제 진행과 결과를 확인합니다.".into();
    p.config = ProjectConfig {
        name: p.name.clone(),
        directory: p.directory.clone(),
        parallel_limit: 3,
        worker_providers: vec!["Codex".into(), "Claude".into()],
        automation: "배정 전 확인".into(),
        use_worktrees: true,
        ..Default::default()
    };
    for (id, role, status) in [
        ("task-a", "구현", TaskState::Running),
        ("task-b", "리뷰", TaskState::AwaitingUserConfirmation),
        ("task-c", "검증", TaskState::WaitingApproval),
    ] {
        p.tasks.push(Task {
            target: RouteTarget {
                workspace_id: p.id.clone(),
                task_id: Some(id.into()),
                session_id: Some(format!("exact-{id}")),
            },
            title: format!("{role} 작업"),
            role: role.into(),
            scope: "이 작업에 지정한 파일과 수정 범위".into(),
            acceptance_criteria: "근거와 실행한 검증 결과 제출".into(),
            provider: "Codex".into(),
            device: "이 Mac".into(),
            directory: format!("/projects/a/.worktrees/{id}"),
            state: status,
            activity: "호스트가 관측해 제공한 실제 진행 내용".into(),
            observed_at: "14:21".into(),
            input: Capability {
                enabled: true,
                reason: String::new(),
            },
            terminal: Capability {
                enabled: true,
                reason: String::new(),
            },
            ..Default::default()
        });
    }
    snapshot
}

fn use_korean_font(harness: &mut egui_kittest::Harness<'_, impl Sized>) {
    crate::fonts::install_cjk_fallback(
        &harness.ctx,
        None,
        crate::fonts::DEFAULT_MONO_FONT,
        crate::fonts::DEFAULT_MONO_WEIGHT,
    );
}

#[test]
fn verification_failed_retry_uses_host_capability_and_exact_task() {
    for enabled in [true, false] {
        let mut snapshot = rich_fixture();
        let task = &mut snapshot.projects[0].tasks[0];
        task.state = TaskState::VerificationFailed;
        task.device = "remote TLS worker".into();
        task.eligible_sessions.clear();
        task.create_worker = Capability {
            enabled,
            reason: "host admission reason".into(),
        };
        let target = task.target.clone();
        let mut initial = HarnessState::default();
        initial.ui.open_task(target.clone());
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1440.0, 1000.0))
            .build_ui_state(
                |ui, state: &mut HarnessState| {
                    state.intents.extend(show(ui, &mut state.ui, &snapshot));
                },
                initial,
            );
        harness.run();
        let retry = harness.get_by_label("새 워커 생성 후 배정");
        assert_eq!(!retry.accesskit_node().is_disabled(), enabled);
        if enabled {
            retry.click();
        }
        harness.run();
        assert_eq!(
            harness
                .state()
                .intents
                .contains(&Intent::CreateWorker { target }),
            enabled
        );
    }
}

#[test]
fn pending_structured_approval_uses_exact_request_and_host_capability() {
    for enabled in [true, false] {
        let mut snapshot = rich_fixture();
        snapshot.projects[0].tasks.clear();
        let target = RouteTarget {
            workspace_id: "project-a".into(),
            session_id: Some("exact-appserver-session".into()),
            task_id: None,
        };
        snapshot.projects[0]
            .pending_approvals
            .push(PendingApproval {
                target: target.clone(),
                session_title: "Structured Codex".into(),
                approval: Approval {
                    id: "exact-request-key".into(),
                    command: "cargo test --offline".into(),
                    directory: "/projects/a".into(),
                    scope: "프로젝트 폴더의 명령 실행".into(),
                    action: Capability {
                        enabled,
                        reason: "현재 소유자를 다시 확인해 주세요".into(),
                    },
                },
            });
        let mut foreign = snapshot.projects[0].pending_approvals[0].clone();
        foreign.target.workspace_id = "other-project".into();
        foreign.approval.command = "must not render foreign request".into();
        snapshot.projects[0].pending_approvals.push(foreign);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1440.0, 1200.0))
            .build_ui_state(
                |ui, state: &mut HarnessState| {
                    state.intents.extend(show(ui, &mut state.ui, &snapshot));
                },
                HarnessState::default(),
            );
        harness.run();
        assert!(
            harness
                .query_by_label("must not render foreign request")
                .is_none()
        );
        assert!(harness.query_by_label("cargo test --offline").is_some());
        let allow = harness.get_by_label("이번 명령 허용");
        assert_eq!(!allow.accesskit_node().is_disabled(), enabled);
        if enabled {
            allow.click();
        }
        harness.run();
        assert_eq!(
            harness.state().intents.contains(&Intent::Approval {
                target: target.clone(),
                approval_id: "exact-request-key".into(),
                allow: true
            }),
            enabled
        );
        if enabled {
            harness.get_by_label("허용 안 함").click();
            harness.run();
            assert!(harness.state().intents.contains(&Intent::Approval {
                target,
                approval_id: "exact-request-key".into(),
                allow: false
            }));
        }
    }
}

#[test]
fn old_project_snapshot_without_pending_approvals_remains_readable() {
    let value = serde_json::to_value(&rich_fixture().projects[0]).unwrap();
    let mut old = value.as_object().unwrap().clone();
    old.remove("pending_approvals");
    let restored: Project = serde_json::from_value(old.into()).unwrap();
    assert!(restored.pending_approvals.is_empty());
}

#[test]
fn approved_assignment_density_and_owner_column_are_visible() {
    for width in [1440.0, 1280.0, 1000.0, 760.0, 390.0] {
        for scale in [1.0, 1.25] {
            let snapshot = Snapshot {
                font_scale: scale,
                ..rich_fixture()
            };
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(width, 900.0))
                .build_ui_state(
                    |ui, state| {
                        show(ui, state, &snapshot);
                    },
                    UiState::default(),
                );
            use_korean_font(&mut harness);
            harness.run();
            let first = harness.get_by_label("구현 · 구현 작업").rect();
            let second = harness.get_by_label("리뷰 · 리뷰 작업").rect();
            assert!(
                second.top() - first.top() <= 78.0 * scale,
                "compact approved rows at {width}/{scale}: {first:?} {second:?}"
            );
            let content = harness.state().last_layout.unwrap().content;
            let owner = harness
                .get_all_by_label("Codex")
                .find(|n| {
                    let r = n.rect();
                    content.contains(r.center())
                        && r.center().y >= first.top()
                        && r.center().y < second.top()
                })
                .expect("provider in dedicated owner column");
            assert!(
                owner.rect().left() > first.right(),
                "owner stays beside title at {width}/{scale}"
            );
        }
    }
}

#[test]
fn mobile_primary_tabs_keep_font_scale_and_scroll_to_each_destination() {
    let mut first_width = 0.0;
    for scale in [1.0, 1.25] {
        let snapshot = Snapshot {
            font_scale: scale,
            ..rich_fixture()
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(390.0, 844.0))
            .build_ui_state(
                |ui, state| {
                    show(ui, state, &snapshot);
                },
                UiState::default(),
            );
        use_korean_font(&mut harness);
        harness.run();
        let header = harness.state().last_layout.unwrap().header;
        let first = harness
            .get_all_by_label("메인 에이전트")
            .find(|n| header.y_range().contains(n.rect().center().y))
            .unwrap()
            .rect();
        if scale == 1.0 {
            first_width = first.width();
        } else {
            assert!(
                first.width() >= first_width * 1.18,
                "font enlargement must not be shrunk to fit"
            );
        }
        let rollback = harness.get_by_label("기존 화면").rect();
        assert!(header.contains_rect(rollback));
        for label in ["작업 3", "터미널", "에이전트·기기", "이력"] {
            let tab = harness
                .get_all_by_label(label)
                .find(|n| header.y_range().contains(n.rect().center().y))
                .expect(label);
            tab.scroll_to_me();
            harness.run();
            let tab = harness
                .get_all_by_label(label)
                .find(|n| header.y_range().contains(n.rect().center().y))
                .expect(label);
            assert!(
                tab.rect().intersects(header),
                "scrollable {label} at {scale}"
            );
        }
    }
}

#[test]
fn settings_fields_follow_approved_width_and_touch_height() {
    for (width, scale) in [(1440.0, 1.0), (1280.0, 1.25), (760.0, 1.25), (390.0, 1.0)] {
        let snapshot = Snapshot {
            font_scale: scale,
            ..rich_fixture()
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(width, 900.0))
            .build_ui_state(
                |ui, state| {
                    show(ui, state, &snapshot);
                },
                UiState {
                    active_workspace: snapshot.selected_workspace_id.clone(),
                    view: View::Settings,
                    ..Default::default()
                },
            );
        use_korean_font(&mut harness);
        harness.run();
        let field = harness
            .get_all_by_value("/projects/a")
            .find(|n| n.accesskit_node().role() == egui::accesskit::Role::TextInput)
            .expect("settings directory input")
            .rect();
        assert!(
            field.width() <= 960.5,
            "bounded settings column at {width}: {field:?}"
        );
        assert!(
            field.height() >= 36.0 * scale - 0.5,
            "36px settings input at {width}: {field:?}"
        );
    }
}

#[test]
fn worker_drawer_header_and_footer_remain_visible_with_many_workers() {
    for width in [1440.0, 1000.0, 760.0, 390.0] {
        let mut snapshot = rich_fixture();
        snapshot.manage_devices.enabled = true;
        snapshot.font_scale = 1.25;
        let task = snapshot.projects[0].tasks[0].clone();
        for i in 0..20 {
            let mut task = task.clone();
            task.target.task_id = Some(format!("extra-{i}"));
            snapshot.projects[0].tasks.push(task);
        }
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(width, 900.0))
            .build_ui_state(
                |ui, state: &mut HarnessState| {
                    state.intents.extend(show(ui, &mut state.ui, &snapshot));
                },
                HarnessState {
                    ui: UiState {
                        active_workspace: snapshot.selected_workspace_id.clone(),
                        right_drawer: width <= 1050.0,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            );
        use_korean_font(&mut harness);
        harness.run();
        let layout = harness.state().ui.last_layout.unwrap();
        let panel = layout.monitor.unwrap_or_else(|| layout.right_drawer());
        if layout.monitor_is_drawer {
            assert_eq!(panel.top(), layout.header.bottom());
            let close = harness.get_by_label("닫기").rect();
            let heading = harness
                .get_all_by_label("작업자")
                .find(|n| panel.contains(n.rect().center()))
                .unwrap()
                .rect();
            assert!(
                (close.center().y - heading.center().y).abs() < 10.0,
                "close shares monitor header"
            );
        }
        let footer = harness.get_by_label("기기 관리 ↗").rect();
        assert!(
            panel.contains_rect(footer) && panel.bottom() - footer.bottom() < 26.0,
            "persistent device footer at {width}: {footer:?} / {panel:?}"
        );
        harness.get_by_label("기기 관리 ↗").click();
        harness.run();
        assert!(harness.state().intents.contains(&Intent::ManageDevices {
            workspace_id: Some("project-a".into())
        }));
    }
}

#[test]
fn headless_width_theme_font_matrix_preserves_readable_content_and_drawer_bounds() {
    for width in [1440.0, 1280.0, 1000.0, 760.0, 390.0] {
        for scale in [1.0, 1.25] {
            for dark in [true, false] {
                let mut snapshot = rich_fixture();
                snapshot.font_scale = scale;
                snapshot.dark_mode = dark;
                let mut harness = egui_kittest::Harness::builder()
                    .with_size(egui::vec2(width, 900.0))
                    .build_ui_state(
                        |ui, state: &mut UiState| {
                            show(ui, state, &snapshot);
                        },
                        UiState::default(),
                    );
                harness.run();
                let l = harness.state().last_layout.unwrap();
                assert!(l.content.width() >= 300.0, "{width} {scale}");
                assert!(l.content.height() > 300.0);
                assert!(l.screen.contains_rect(l.left_drawer()));
                assert!(l.screen.contains_rect(l.right_drawer()));
                assert!(
                    harness.query_by_label("구현 · 구현 작업").is_some(),
                    "main task outline at {width} {scale}"
                );
                if width <= 760.0 {
                    harness.state_mut().left_drawer = true;
                    harness.run();
                    assert!(harness.query_by_label("닫기").is_some());
                    harness.key_press(egui::Key::Escape);
                    harness.run();
                    assert!(!harness.state().left_drawer);
                }
            }
        }
    }
}

#[test]
fn monitor_filter_does_not_leak_a_different_workspace() {
    let mut snapshot = rich_fixture();
    let p = &mut snapshot.projects[0];
    let mut foreign = p.tasks[0].clone();
    foreign.target.workspace_id = "foreign-project".into();
    foreign.title = "must never render".into();
    p.tasks.push(foreign);
    assert_eq!(workers::visible_tasks(p, MonitorFilter::All).count(), 3);
    assert_eq!(
        workers::visible_tasks(p, MonitorFilter::Attention).count(),
        2
    );
    assert_eq!(workers::visible_tasks(p, MonitorFilter::Running).count(), 1);
}

#[test]
fn actual_send_button_retains_draft_and_exact_target() {
    let snapshot = fixture();
    let target = snapshot.projects[0].main_target.clone();
    let mut initial = HarnessState::default();
    initial.ui.set_draft(target.clone(), "동일 문장".into());
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1280.0, 900.0))
        .build_ui_state(
            |ui, state: &mut HarnessState| {
                state.intents.extend(show(ui, &mut state.ui, &snapshot));
            },
            initial,
        );
    harness.run();
    harness.get_by_label("전송").click();
    harness.run();
    assert!(harness.state().intents.contains(&Intent::Submit {
        target: target.clone(),
        text: "동일 문장".into()
    }));
    assert_eq!(harness.state().ui.draft(&target), "동일 문장");
    harness.get_by_label("전송").click();
    harness.run();
    assert_eq!(
        harness
            .state()
            .intents
            .iter()
            .filter(|i| matches!(i, Intent::Submit { .. }))
            .count(),
        2
    );
}

#[test]
fn composer_target_stays_left_and_send_stays_at_right_edge() {
    for width in [1440.0, 390.0] {
        let snapshot = rich_fixture();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(width, 900.0))
            .build_ui_state(
                |ui, state| {
                    show(ui, state, &snapshot);
                },
                UiState::default(),
            );
        use_korean_font(&mut harness);
        harness.run();
        let composer = harness.state().last_layout.unwrap().composer;
        let send = harness.get_by_label("전송").rect();
        let caption = harness
            .get_by_label("프로젝트 A / 메인 에이전트 Codex")
            .rect();
        assert!(
            composer.right() - send.right() <= if width <= 760.0 { 23.0 } else { 35.0 },
            "send at right edge: {send:?} / {composer:?}"
        );
        assert!(
            caption.left() - composer.left() <= if width <= 760.0 { 28.0 } else { 40.0 },
            "exact target at left edge: {caption:?}"
        );
    }
}

#[test]
fn composing_enter_never_submits_and_overlimit_draft_is_preserved() {
    let snapshot = fixture();
    let target = snapshot.projects[0].main_target.clone();
    let mut initial = HarnessState::default();
    initial.ui.set_draft(target.clone(), "한".into());
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1000.0, 900.0))
        .build_ui_state(
            |ui, state: &mut HarnessState| {
                state.intents.extend(show(ui, &mut state.ui, &snapshot));
            },
            initial,
        );
    harness.run();
    harness
        .ctx
        .memory_mut(|m| m.request_focus(main::composer_id(&target)));
    harness.run();
    harness.event(egui::Event::Ime(egui::ImeEvent::Preedit {
        text: "글".into(),
        active_range_chars: None,
    }));
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(
        !harness
            .state()
            .intents
            .iter()
            .any(|i| matches!(i, Intent::Submit { .. }))
    );
    let mut state = UiState::default();
    state.set_draft(target.clone(), "가".repeat(6000));
    assert!(main::submit(&state, &target, true).is_none());
    assert_eq!(state.draft(&target).len(), 18000);
}

#[test]
fn project_settings_button_stays_inside_header_at_large_font_scale() {
    for width in [1440.0, 1280.0, 1000.0, 760.0, 390.0] {
        let snapshot = Snapshot {
            font_scale: 1.25,
            ..fixture()
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(width, 900.0))
            .build_ui_state(
                |ui, state: &mut HarnessState| {
                    state.intents.extend(show(ui, &mut state.ui, &snapshot));
                },
                HarnessState::default(),
            );
        use_korean_font(&mut harness);
        harness.run();
        let header = harness.state().ui.last_layout.unwrap().projectbar;
        let button = harness
            .get_all_by_label("프로젝트 설정")
            .find(|button| header.y_range().contains(button.rect().center().y))
            .expect("project header settings button");
        assert!(
            header.contains_rect(button.rect()),
            "width={width} header={header:?} button={:?}",
            button.rect()
        );
        button.click();
        harness.run();
        assert_eq!(harness.state().ui.view, View::Settings);
    }
}

#[test]
fn rollback_is_a_visible_native_button_at_mobile_width() {
    let snapshot = fixture();
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(390.0, 844.0))
        .build_ui_state(
            |ui, state: &mut HarnessState| {
                state.intents.extend(show(ui, &mut state.ui, &snapshot));
            },
            HarnessState::default(),
        );
    harness.run();
    let button = harness.get_by_label("기존 화면");
    assert!(
        harness
            .state()
            .ui
            .last_layout
            .unwrap()
            .header
            .contains_rect(button.rect()),
        "header={:?} button={:?}",
        harness.state().ui.last_layout.unwrap().header,
        button.rect()
    );
    button.click();
    harness.run();
    assert!(harness.state().intents.contains(&Intent::UseOriginalLayout));
}

#[test]
#[ignore = "offscreen GPU evidence; never starts the app"]
fn command_center_render_evidence() {
    let output = std::env::var_os("DEPPY_CC_EVIDENCE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/private/tmp/deppy-cc-native-fix-20261020/screenshots".into());
    std::fs::create_dir_all(&output).unwrap();
    let mut metrics = Vec::new();
    let quick = std::env::var_os("DEPPY_CC_EVIDENCE_QUICK").is_some();
    let widths: &[f32] = if quick {
        &[1440.0, 1200.0, 390.0]
    } else {
        &[1440.0, 1280.0, 1200.0, 1000.0, 760.0, 390.0]
    };
    let scales: &[f32] = if quick { &[1.0] } else { &[1.0, 1.25] };
    for &width in widths {
        for &scale in scales {
            for dark in [true, false] {
                let mut modes = vec![
                    "main",
                    "settings",
                    "empty",
                    "no-project",
                    "devices",
                    "tasks",
                    "task",
                    "history",
                ];
                if width <= 1050.0 {
                    modes.push("workers");
                }
                if width <= 760.0 {
                    modes.push("projects");
                }
                for mode in modes {
                    let mut snapshot = approved_visual_fixture();
                    snapshot.font_scale = scale;
                    snapshot.dark_mode = dark;
                    if mode == "empty" {
                        snapshot.projects[0].tasks.clear();
                        snapshot.projects[0].goal.clear();
                        snapshot.projects[0].main_summary.clear();
                    } else if mode == "no-project" {
                        snapshot.projects.clear();
                        snapshot.selected_workspace_id = None;
                    }
                    let initial = UiState {
                        active_workspace: snapshot.selected_workspace_id.clone(),
                        view: match mode {
                            "settings" => View::Settings,
                            "devices" => View::Devices,
                            "tasks" => View::Tasks,
                            "task" => View::Task,
                            "history" => View::History,
                            _ => View::Main,
                        },
                        selected_task_id: if mode == "task" {
                            snapshot.projects[0].tasks[1].target.task_id.clone()
                        } else {
                            None
                        },
                        left_drawer: mode == "projects",
                        right_drawer: mode == "workers",
                        ..Default::default()
                    };
                    let height = if width == 390.0 {
                        844.0
                    } else if width == 1200.0 {
                        800.0
                    } else {
                        900.0
                    };
                    let mut harness = egui_kittest::Harness::builder()
                        .with_size(egui::vec2(width, height))
                        .build_ui_state(
                            |ui, state| {
                                // The harness supplies a default CentralPanel margin; render the actual app surface at the exact viewport size.
                                let rect = ui.ctx().viewport_rect();
                                let mut surface =
                                    ui.new_child(egui::UiBuilder::new().max_rect(rect));
                                surface.set_clip_rect(rect);
                                show(&mut surface, state, &snapshot);
                            },
                            initial,
                        );
                    use_korean_font(&mut harness);
                    harness.remove_cursor();
                    harness.run();
                    let name = format!(
                        "{mode}-{}-{}-font{}",
                        width as u32,
                        if dark { "dark" } else { "light" },
                        (scale * 100.0) as u32
                    );
                    let l = harness.state().last_layout.unwrap();
                    let rect = |r: egui::Rect| [r.left(), r.top(), r.right(), r.bottom()];
                    let title = harness
                        .query_by_label("구현 · 결제 화면 구현")
                        .map(|n| rect(n.rect()));
                    metrics.push(serde_json::json!({"name": name, "width": width, "height": height, "scale": scale, "dark": dark, "screen": rect(l.screen), "header": rect(l.header), "drawer": rect(l.right_drawer()), "assignment_title": title}));
                    harness
                        .render()
                        .expect("offscreen native egui rendering")
                        .save(output.join(format!("{name}.png")))
                        .unwrap();
                    eprintln!("NATIVE_CC_EVIDENCE {name}");
                }
            }
        }
    }
    std::fs::write(
        output.join("metrics.json"),
        serde_json::to_vec_pretty(&metrics).unwrap(),
    )
    .unwrap();
}

fn approved_visual_fixture() -> Snapshot {
    let mut snapshot = rich_fixture();
    snapshot.manage_devices.enabled = true;
    snapshot.add_project.enabled = true;
    let p = &mut snapshot.projects[0];
    p.name = "nomorevibe".into();
    p.directory = "~/projects/nomorevibe".into();
    p.config.name = p.name.clone();
    p.config.directory = p.directory.clone();
    p.main_model = "GPT-6.1-Sol".into();
    p.config.model = p.main_model.clone();
    p.config.provider = p.main_provider.clone();
    p.config.effort = p.main_effort.clone();
    p.config.main_session_id = p.main_target.session_id.clone();
    p.goal = "모바일 결제 화면을 개선하고, 리뷰와 동작 검증까지 진행해 줘.".into();
    p.main_summary = "메인은 범위와 완료 조건을 정합니다. 각 작업자는 독립 세션에서 실행하고, 현재 진행은 오른쪽에서 계속 확인할 수 있습니다.".into();
    p.available_providers = vec!["Codex".into(), "Claude".into()];
    p.available_models = vec![p.main_model.clone()];
    p.available_efforts = vec!["xhigh".into()];
    p.available_automation = vec!["배정 전 확인".into()];
    p.save_settings.enabled = true;
    for (i, (title, scope, provider, device, activity, time)) in [
        (
            "결제 화면 구현",
            "결제 요약·버튼·좁은 화면",
            "Claude",
            "이 Mac",
            "결제 요약과 버튼 간격을 수정하고 있습니다.",
            "14:21",
        ),
        (
            "독립 코드 리뷰",
            "변경 범위·예외 처리 검토",
            "Codex",
            "gpu3090",
            "중복 제출과 실패 응답 처리 검토를 마쳤습니다.",
            "14:24",
        ),
        (
            "모바일 동작 검증",
            "구현 완료 후 시나리오 확인",
            "Codex",
            "gpu3090",
            "모바일 mock 검증 명령의 범위 확인을 기다립니다. 구현·리뷰 결과 이후 실행합니다.",
            "14:23",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let task = &mut p.tasks[i];
        task.title = title.into();
        task.scope = scope.into();
        task.provider = provider.into();
        task.device = device.into();
        task.activity = activity.into();
        task.observed_at = time.into();
    }
    snapshot.devices = ["이 Mac", "gpu3090", "휴대폰"]
        .into_iter()
        .enumerate()
        .map(|(i, name)| Device {
            id: format!("visual-device-{i}"),
            name: name.into(),
            description: if i == 2 {
                "원격 관측".into()
            } else {
                "프로젝트 실행 기기".into()
            },
            capabilities: if i == 2 {
                "상태·승인·결과 확인".into()
            } else {
                "Codex · Claude".into()
            },
            connection: "연결됨".into(),
            connected: true,
            ..Default::default()
        })
        .collect();
    snapshot.execution_setup.enabled = true;
    snapshot.server_execution = ServerExecutionSetup {
        running: true,
        endpoint: "127.0.0.1:4270".into(),
        fingerprint: vec!["ab"; 32].join(":"),
        status: "이 컴퓨터의 선택한 프로젝트 실행을 허용했습니다.".into(),
        workspace_id: snapshot.selected_workspace_id.clone(),
        exported_workspace_id: Some("controller-project".into()),
        folder: "/projects/command-center".into(),
        agents: vec!["Codex · GPT-6.1-Sol · xhigh".into()],
        requested_agents: vec!["Codex · GPT-6.1-Sol · xhigh".into()],
        allow: Capability {
            enabled: true,
            ..Default::default()
        },
        revoke: Capability {
            enabled: true,
            ..Default::default()
        },
        copy: Capability {
            enabled: true,
            ..Default::default()
        },
        ..Default::default()
    };
    snapshot
}

#[test]
fn snapshot_and_exact_target_intents_roundtrip_without_ui_state() {
    let snapshot = fixture();
    let value = serde_json::to_value(&snapshot).unwrap();
    let restored: Snapshot = serde_json::from_value(value).unwrap();
    assert_eq!(
        restored.projects[0].main_target,
        snapshot.projects[0].main_target
    );
    let intent = Intent::Submit {
        target: snapshot.projects[0].main_target.clone(),
        text: "동일한 입력".into(),
    };
    let value = serde_json::to_value(&intent).unwrap();
    assert_eq!(value["type"], "submit");
    assert_eq!(value["target"]["session_id"], "opaque-pane-a");
    assert_eq!(serde_json::from_value::<Intent>(value).unwrap(), intent);
    assert!(
        serde_json::from_str::<Intent>(r#"{"type":"submit","target":{"workspace_id":"a"}}"#)
            .is_err()
    );
}

#[test]
fn task_native_aux_and_terminal_preserve_detail_header_and_composer() {
    let mut snapshot = fixture();
    let mut task = Task {
        target: snapshot.projects[0].main_target.clone(),
        ..Default::default()
    };
    task.target.task_id = Some("task-a".into());
    snapshot.projects[0].tasks.push(task);
    let state = UiState {
        active_workspace: snapshot.selected_workspace_id.clone(),
        view: View::Task,
        selected_task_id: Some("task-a".into()),
        detail_tab: DetailTab::Git,
        ..Default::default()
    };
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1440.0, 900.0))
        .build_ui_state(
            |ui, state| {
                show(ui, state, &snapshot);
            },
            state,
        );
    harness.run();
    let state = harness.state();
    assert!(state.terminal_rect.is_none());
    assert_eq!(
        state.aux_target.as_ref().unwrap().task_id.as_deref(),
        Some("task-a")
    );
    let layout = state.last_layout.unwrap();
    assert!(state.aux_rect.unwrap().top() > layout.content.top() + 100.0);
    harness.state_mut().detail_tab = DetailTab::Terminal;
    harness.run();
    let state = harness.state();
    assert!(state.aux_rect.is_none());
    assert!(state.terminal_rect.unwrap().top() > layout.content.top() + 100.0);
}

#[test]
fn manual_result_and_verification_controls_capture_exact_displayed_attempt_and_revision() {
    let mut snapshot = rich_fixture();
    let task = &mut snapshot.projects[0].tasks[0];
    task.attempt_id = Some("attempt-current".into());
    task.result_report.enabled = true;
    task.verify_result.enabled = true;
    task.result = Some("actual report".into());
    task.result_revision = Some("7".into());
    let target = task.target.clone();
    let mut initial = HarnessState::default();
    initial.ui.open_task(target.clone());
    initial.ui.detail_tab = DetailTab::Result;
    initial
        .ui
        .result_reports
        .insert(target.clone(), "actual manual report".into());
    initial
        .ui
        .verification_reports
        .insert(target.clone(), "cargo test: 12 passed".into());
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1440.0, 1000.0))
        .build_ui_state(
            |ui, state: &mut HarnessState| {
                state.intents.extend(show(ui, &mut state.ui, &snapshot));
            },
            initial,
        );
    harness.run();
    harness.get_by_label("수동 결과 보고").click();
    harness.run();
    harness
        .get_by_label("결과 제출 · 검증 미실행")
        .scroll_to_me();
    harness.run();
    harness.get_by_label("결과 제출 · 검증 미실행").click();
    harness.run();
    assert!(harness.state().intents.contains(&Intent::ReportResult {
        target: target.clone(),
        attempt_id: "attempt-current".into(),
        text: "actual manual report".into()
    }));
    harness.get_by_label("검증 통과 기록").scroll_to_me();
    harness.run();
    harness.get_by_label("검증 통과 기록").click();
    harness.run();
    assert!(harness.state().intents.contains(&Intent::VerifyResult {
        target,
        attempt_id: "attempt-current".into(),
        revision: "7".into(),
        passed: true,
        evidence: "cargo test: 12 passed".into()
    }));
}

#[test]
fn plan_review_assignment_picker_emits_exact_native_profile_and_revision() {
    let mut snapshot = rich_fixture();
    snapshot.projects[0].config.revision = "12".into();
    let task = &mut snapshot.projects[0].tasks[0];
    task.state = TaskState::PlanReview;
    task.selected_assignment = None;
    let target = task.target.clone();
    snapshot.execution_setup.assignments.push(AssignmentChoice {
        workspace_id: target.workspace_id.clone(),
        device_id: Some("tls-device-b".into()),
        profile_id: "immutable-profile-7".into(),
        label: "실제 기기 B · Claude · 실제 모델".into(),
        capability: Capability {
            enabled: true,
            reason: String::new(),
        },
        ..Default::default()
    });
    let mut initial = HarnessState::default();
    initial.ui.open_task(target.clone());
    initial.ui.detail_tab = DetailTab::Work;
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1440.0, 1000.0))
        .build_ui_state(
            |ui, state: &mut HarnessState| state.intents.extend(show(ui, &mut state.ui, &snapshot)),
            initial,
        );
    harness.run();
    harness.get_by_value("실행 기기 · 프로필 선택").click();
    harness.run();
    harness
        .get_by_label("실제 기기 B · Claude · 실제 모델")
        .click();
    harness.run();
    let intent = Intent::SetTaskAssignment {
        target,
        config_revision: "12".into(),
        device_id: Some("tls-device-b".into()),
        profile_id: "immutable-profile-7".into(),
    };
    assert!(harness.state().intents.contains(&intent));
    assert!(
        serde_json::to_value(intent).is_err(),
        "native trust assignment cannot arrive as unauthenticated JSON"
    );
}
