use super::*;
use egui_kittest::kittest::Queryable;

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
        if let Ok(font) = std::fs::read("/System/Library/Fonts/AppleSDGothicNeo.ttc") {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("cc-cjk".into(), egui::FontData::from_owned(font).into());
            fonts
                .families
                .get_mut(&egui::FontFamily::Proportional)
                .unwrap()
                .insert(0, "cc-cjk".into());
            harness.ctx.set_fonts(fonts);
        }
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
        .unwrap_or_else(|| {
            std::path::PathBuf::from("/private/tmp/deppy-command-center-native-evidence-20261020")
        });
    std::fs::create_dir_all(&output).unwrap();
    for (name, width, scale, dark, left, right) in [
        ("desktop-dark", 1440.0, 1.0, true, false, false),
        ("desktop-light", 1440.0, 1.0, false, false, false),
        ("medium", 1280.0, 1.25, true, false, false),
        ("workers-drawer", 1000.0, 1.0, true, false, true),
        ("narrow", 760.0, 1.25, true, false, false),
        ("mobile", 390.0, 1.0, true, false, false),
        ("mobile-projects", 390.0, 1.25, false, true, false),
        ("mobile-workers", 390.0, 1.25, true, false, true),
        ("devices-desktop", 1440.0, 1.0, true, false, false),
        ("devices-mobile", 390.0, 1.0, true, false, false),
    ] {
        let mut snapshot = rich_fixture();
        snapshot.font_scale = scale;
        snapshot.dark_mode = dark;
        let device_view = name.starts_with("devices-");
        if device_view {
            let enabled = || Capability {
                enabled: true,
                reason: String::new(),
            };
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
                allow: enabled(),
                revoke: enabled(),
                copy: enabled(),
                ..Default::default()
            };
        }
        let initial = UiState {
            view: if device_view {
                View::Devices
            } else {
                View::Main
            },
            active_workspace: snapshot.selected_workspace_id.clone(),
            left_drawer: left,
            right_drawer: right,
            ..Default::default()
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(width, 900.0))
            .build_ui_state(
                |ui, state| {
                    show(ui, state, &snapshot);
                },
                initial,
            );
        if let Ok(font) = std::fs::read("/System/Library/Fonts/AppleSDGothicNeo.ttc") {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("cc-cjk".into(), egui::FontData::from_owned(font).into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cc-cjk".into());
            harness.ctx.set_fonts(fonts);
        }
        harness.remove_cursor();
        harness.run();
        let image = harness.render().expect("offscreen native egui rendering");
        image.save(output.join(format!("{name}.png"))).unwrap();
        eprintln!(
            "NATIVE_CC_EVIDENCE {name} {}x{} scale={scale} path={}",
            image.width(),
            image.height(),
            output.display()
        );
    }
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
    harness.get_by_label("결과 제출 · 검증 미실행").click();
    harness.run();
    assert!(harness.state().intents.contains(&Intent::ReportResult {
        target: target.clone(),
        attempt_id: "attempt-current".into(),
        text: "actual manual report".into()
    }));
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
