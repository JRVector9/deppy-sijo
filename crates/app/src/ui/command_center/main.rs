//! Main conversation, task detail, and exactly targeted composer.

use super::*;
use unicode_normalization::UnicodeNormalization as _;

pub(super) fn body(
    ui: &mut egui::Ui,
    state: &mut UiState,
    snapshot: &Snapshot,
    p: Option<&Project>,
    intents: &mut Vec<Intent>,
) {
    let scroll_id = (
        "cc-body",
        p.map(|p| &p.id),
        state.view,
        state.selected_task_id.as_deref(),
        state.detail_tab,
    );
    // IDs do not include composer focus or height: expanding input never resets the reading position.
    egui::ScrollArea::vertical()
        .id_salt(format!("{scroll_id:?}"))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let Some(p) = p else {
                ui.heading(text(snapshot, "no_projects"));
                if action(
                    ui,
                    snapshot,
                    text(snapshot, "add_project"),
                    &snapshot.add_project,
                ) {
                    intents.push(Intent::AddProject);
                }
                return;
            };
            let content_width = ui.available_width().min(1000.0);
            let inset = ((ui.available_width() - content_width) * 0.5).max(0.0);
            ui.horizontal(|ui| {
                ui.add_space(inset);
                ui.allocate_ui_with_layout(
                    egui::vec2(content_width, ui.available_height()),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_max_width(content_width);
                        match state.view {
                            View::Main => home(ui, state, snapshot, p, intents),
                            View::Tasks => task_list(ui, state, snapshot, p),
                            View::Task => task_detail(ui, state, snapshot, p, intents),
                            View::Settings => settings::show(ui, state, snapshot, p, intents),
                            View::Devices => devices::show(ui, state, snapshot, p, intents),
                            View::History => history(ui, snapshot, p, None, intents),
                            View::Terminal => terminal(ui, state, snapshot, p, intents),
                        }
                    },
                );
            });
        });
}

fn title(ui: &mut egui::Ui, snapshot: &Snapshot, value: &str) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(value)
                .size(17.0 * snapshot.font_scale)
                .strong(),
        )
        .wrap(),
    );
}

fn home(
    ui: &mut egui::Ui,
    state: &mut UiState,
    snapshot: &Snapshot,
    p: &Project,
    intents: &mut Vec<Intent>,
) {
    if p.storage_retry.enabled {
        ui.label(&p.storage_retry.reason);
        if ui.button("저장 상태 다시 확인").clicked() {
            intents.push(Intent::RetryStorage {
                workspace_id: p.id.clone(),
            });
        }
        rule(ui, snapshot);
    }
    weak(
        ui,
        snapshot,
        format!("{} · {}", text(snapshot, "goal"), p.name),
    );
    ui.add_space(6.0);
    title(
        ui,
        snapshot,
        if p.goal.is_empty() {
            text(snapshot, "first_goal")
        } else {
            &p.goal
        },
    );
    ui.add_space(14.0);
    ui.horizontal_wrapped(|ui| {
        for (index, key) in ["goal", "plan_assign", "independent", "collect", "verify"]
            .iter()
            .enumerate()
        {
            if index > 0 {
                weak(ui, snapshot, "—");
            }
            let label = if *key == "independent" {
                format!(
                    "{} {}",
                    text(snapshot, key),
                    p.tasks
                        .iter()
                        .filter(|t| t.target.workspace_id == p.id && t.state == TaskState::Running)
                        .count()
                )
            } else {
                text(snapshot, key).into()
            };
            ui.label(
                egui::RichText::new(label)
                    .small()
                    .color(if *key == "independent" {
                        colors(snapshot).accent
                    } else {
                        colors(snapshot).muted
                    }),
            );
        }
    });
    rule(ui, snapshot);
    if p.manual_plan.enabled {
        egui::CollapsingHeader::new("배정안 JSON 직접 확인")
            .id_salt("manual-plan")
            .show(ui, |ui| {
                ui.label("메인 응답의 배정안 JSON을 붙여넣으면 프로젝트·설정·범위를 검증합니다.");
                if let Some(prompt) = &p.planner_prompt {
                    egui::CollapsingHeader::new("메인에게 전달한 계획 요청").show(ui, |ui| {
                        ui.add(egui::Label::new(prompt).wrap());
                    });
                }
                let json = state.plan_json.entry(p.id.clone()).or_default();
                ui.add(
                    egui::TextEdit::multiline(json)
                        .desired_rows(5)
                        .desired_width(f32::INFINITY)
                        .char_limit(16 * 1024),
                );
                if ui
                    .add_enabled(
                        !json.trim().is_empty(),
                        egui::Button::new("배정안 검증 후 표시"),
                    )
                    .clicked()
                {
                    intents.push(Intent::InstallPlanJson {
                        workspace_id: p.id.clone(),
                        json: json.clone(),
                    });
                }
            });
        rule(ui, snapshot);
    }
    thread(ui, snapshot, "MAIN", |ui| {
        title(
            ui,
            snapshot,
            if p.tasks.is_empty() && p.goal.is_empty() {
                text(snapshot, "first_title")
            } else {
                text(snapshot, "main_collects")
            },
        );
        if p.create_main.enabled && action(ui, snapshot, "새 메인 만들기", &p.create_main) {
            intents.push(Intent::CreateMain {
                workspace_id: p.id.clone(),
            });
        }
        if !p.main_summary.is_empty() {
            ui.add(egui::Label::new(&p.main_summary).wrap());
        } else {
            weak(
                ui,
                snapshot,
                text(
                    snapshot,
                    if p.tasks.is_empty() {
                        "first_body"
                    } else {
                        "no_main_summary"
                    },
                ),
            );
        }
        for (i, t) in p
            .tasks
            .iter()
            .filter(|t| t.target.workspace_id == p.id)
            .enumerate()
        {
            rule(ui, snapshot);
            ui.horizontal(|ui| {
                weak(ui, snapshot, format!("{:02}", i + 1));
                ui.vertical(|ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(format!("{} · {}", t.role, t.title)).strong(),
                            )
                            .frame(false)
                            .wrap(),
                        )
                        .clicked()
                        && let Some(id) = &t.target.task_id
                    {
                        state.select_task(id.clone());
                    }
                    weak(ui, snapshot, &t.scope);
                    weak(ui, snapshot, format!("{} · {}", t.provider, t.device));
                });
            });
        }
        if !p.tasks.is_empty() {
            rule(ui, snapshot);
            weak(
                ui,
                snapshot,
                format!(
                    "{} {} · {}",
                    text(snapshot, "parallel_limit"),
                    p.config.parallel_limit,
                    p.config.automation
                ),
            );
        }
        if let Some(goal_id) = &p.pending_goal_id {
            ui.add_space(10.0);
            if action(
                ui,
                snapshot,
                text(snapshot, "confirm_plan"),
                &p.plan_confirmation,
            ) {
                intents.push(Intent::ConfirmPlan {
                    workspace_id: p.id.clone(),
                    goal_id: goal_id.clone(),
                });
            }
        }
    });
    let attention: Vec<_> = p
        .tasks
        .iter()
        .filter(|t| t.target.workspace_id == p.id && t.state.needs_attention())
        .collect();
    if !attention.is_empty() {
        rule(ui, snapshot);
        thread(ui, snapshot, "CHECK", |ui| {
            title(
                ui,
                snapshot,
                &format!("{} {}", text(snapshot, "needs_check"), attention.len()),
            );
            weak(ui, snapshot, text(snapshot, "check_help"));
            ui.horizontal_wrapped(|ui| {
                for t in attention {
                    if ui
                        .button(format!(
                            "{} · {}",
                            workers::state_label(snapshot, t.state),
                            t.title
                        ))
                        .clicked()
                        && let Some(id) = &t.target.task_id
                    {
                        state.select_task(id.clone());
                    }
                }
            });
        });
    }
    if !p.history.is_empty() {
        rule(ui, snapshot);
        thread(ui, snapshot, "LIVE", |ui| {
            title(ui, snapshot, text(snapshot, "main_collects"));
            for h in p.history.iter().take(3) {
                weak(ui, snapshot, format!("{} {}", h.time, h.text));
            }
            if ui.small_button(text(snapshot, "all_history")).clicked() {
                state.view = View::History;
                intents.push(Intent::OpenHistory(p.main_target.clone()));
            }
        });
    }
}

fn thread(ui: &mut egui::Ui, snapshot: &Snapshot, label: &str, draw: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(8.0);
    if ui.available_width() < 480.0 {
        weak(ui, snapshot, label);
        draw(ui);
    } else {
        ui.horizontal_top(|ui| {
            ui.allocate_ui(egui::vec2(42.0, 20.0), |ui| weak(ui, snapshot, label));
            ui.vertical(|ui| {
                ui.set_max_width(ui.available_width());
                draw(ui);
            });
        });
    }
    ui.add_space(10.0);
}

fn task_list(ui: &mut egui::Ui, state: &mut UiState, snapshot: &Snapshot, p: &Project) {
    ui.heading(text(snapshot, "tasks"));
    weak(ui, snapshot, &p.name);
    ui.add_space(14.0);
    if p.tasks.is_empty() {
        weak(ui, snapshot, text(snapshot, "no_tasks"));
    }
    for t in p.tasks.iter().filter(|t| t.target.workspace_id == p.id) {
        ui.push_id(&t.target.task_id, |ui| {
            if ui
                .add(
                    egui::Button::new(egui::RichText::new(&t.title).strong())
                        .frame(false)
                        .wrap(),
                )
                .clicked()
                && let Some(id) = &t.target.task_id
            {
                state.select_task(id.clone());
            }
            weak(ui, snapshot, &t.scope);
            ui.horizontal_wrapped(|ui| {
                weak(
                    ui,
                    snapshot,
                    format!("{} · {} · {}", t.provider, t.role, t.device),
                );
                workers::status(ui, snapshot, t.state);
            });
            rule(ui, snapshot);
        });
    }
}

fn task_detail(
    ui: &mut egui::Ui,
    state: &mut UiState,
    snapshot: &Snapshot,
    p: &Project,
    intents: &mut Vec<Intent>,
) {
    let Some(t) = selected_task(state, p) else {
        weak(ui, snapshot, text(snapshot, "unavailable_task"));
        return;
    };
    ui.heading(&t.title);
    ui.horizontal_wrapped(|ui| {
        workers::status(ui, snapshot, t.state);
        weak(
            ui,
            snapshot,
            format!("{} · {} · {}", t.provider, t.role, t.device),
        );
    });
    weak(ui, snapshot, &t.directory);
    ui.add_space(12.0);
    ui.horizontal_wrapped(|ui| {
        for (tab, key) in [
            (DetailTab::Work, "work"),
            (DetailTab::Result, "result"),
            (DetailTab::Git, "git"),
            (DetailTab::History, "history"),
            (DetailTab::Terminal, "terminal"),
        ] {
            if ui
                .selectable_label(state.detail_tab == tab, text(snapshot, key))
                .clicked()
            {
                state.detail_tab = tab;
                if t.target.session_id.is_some() {
                    match tab {
                        DetailTab::Git => intents.push(Intent::OpenGit(t.target.clone())),
                        DetailTab::History => intents.push(Intent::OpenHistory(t.target.clone())),
                        DetailTab::Terminal => intents.push(Intent::OpenSession(t.target.clone())),
                        _ => {}
                    }
                }
            }
        }
    });
    rule(ui, snapshot);
    match state.detail_tab {
        DetailTab::Work => {
            title(ui, snapshot, &t.activity);
            if t.close_worker.enabled
                && action(ui, snapshot, "워커 실행 종료 · 용량 반환", &t.close_worker)
                && let Some(attempt_id) = &t.attempt_id
            {
                intents.push(Intent::CloseWorker {
                    target: t.target.clone(),
                    attempt_id: attempt_id.clone(),
                });
            }
            if t.state == TaskState::PlanReview {
                let choices: Vec<_> = snapshot
                    .execution_setup
                    .assignments
                    .iter()
                    .filter(|c| c.workspace_id == p.id)
                    .collect();
                if !choices.is_empty() {
                    let key = |c: &AssignmentChoice| {
                        serde_json::to_string(&(c.device_id.as_deref(), c.profile_id.as_str()))
                            .unwrap_or_default()
                    };
                    egui::ComboBox::from_id_salt(("task-assignment", &p.id, &t.target.task_id))
                        .selected_text(
                            choices
                                .iter()
                                .find(|c| Some(key(c)) == t.selected_assignment)
                                .map(|c| c.label.as_str())
                                .unwrap_or("실행 기기 · 프로필 선택"),
                        )
                        .width(ui.available_width().min(420.0))
                        .show_ui(ui, |ui| {
                            for c in choices {
                                if ui
                                    .add_enabled(
                                        c.capability.enabled,
                                        egui::Button::selectable(
                                            Some(key(c)) == t.selected_assignment,
                                            &c.label,
                                        ),
                                    )
                                    .on_hover_text(&c.capability.reason)
                                    .clicked()
                                {
                                    intents.push(Intent::SetTaskAssignment {
                                        target: t.target.clone(),
                                        config_revision: p.config.revision.clone(),
                                        device_id: c.device_id.clone(),
                                        profile_id: c.profile_id.clone(),
                                    });
                                }
                            }
                        });
                    weak(
                        ui,
                        snapshot,
                        "기기·프로필 변경을 저장한 뒤 전체 배정안을 확인하면 실행할 수 있습니다.",
                    );
                }
            }
            if t.dispatch.enabled {
                let selected = state
                    .task_workers
                    .entry(format!("{}:{:?}", p.id, t.target.task_id))
                    .or_default();
                egui::ComboBox::from_id_salt("task-worker")
                    .selected_text(
                        t.eligible_sessions
                            .iter()
                            .find(|s| s.target.session_id == *selected)
                            .map(|s| s.title.as_str())
                            .unwrap_or("실행 세션 선택"),
                    )
                    .show_ui(ui, |ui| {
                        ui.selectable_value(selected, None, "실행 세션 선택");
                        for session in &t.eligible_sessions {
                            ui.selectable_value(
                                selected,
                                session.target.session_id.clone(),
                                &session.title,
                            );
                        }
                    });
                if selected.is_some()
                    && action(ui, snapshot, text(snapshot, "dispatch_task"), &t.dispatch)
                {
                    let mut target = t.target.clone();
                    target.session_id = selected.clone();
                    intents.push(Intent::DispatchTask { target });
                }
            }
            if matches!(
                t.state,
                TaskState::PlanReview
                    | TaskState::Queued
                    | TaskState::Failed
                    | TaskState::Cancelled
            ) && action(ui, snapshot, "새 워커 생성 후 배정", &t.create_worker)
            {
                intents.push(Intent::CreateWorker {
                    target: t.target.clone(),
                });
            }
            for (key, value) in [
                ("scope", &t.scope),
                ("criteria", &t.acceptance_criteria),
                ("directory", &t.directory),
                ("device", &t.device),
            ] {
                if !value.is_empty() {
                    weak(ui, snapshot, text(snapshot, key));
                    ui.add(egui::Label::new(value).wrap());
                }
            }
            if let Some(a) = &t.approval {
                rule(ui, snapshot);
                title(ui, snapshot, text(snapshot, "approval_title"));
                ui.add(egui::Label::new(egui::RichText::new(&a.command).monospace()).wrap());
                weak(ui, snapshot, &a.directory);
                weak(ui, snapshot, &a.scope);
                weak(ui, snapshot, text(snapshot, "approval_note"));
                ui.horizontal_wrapped(|ui| {
                    for (allow, key) in [(true, "allow_once"), (false, "deny")] {
                        if action(ui, snapshot, text(snapshot, key), &a.action) {
                            intents.push(Intent::Approval {
                                target: t.target.clone(),
                                approval_id: a.id.clone(),
                                allow,
                            });
                        }
                    }
                });
                if !a.action.enabled && !a.action.reason.is_empty() {
                    weak(ui, snapshot, &a.action.reason);
                }
            }
        }
        DetailTab::Result => {
            if !t.verification.is_empty() {
                weak(ui, snapshot, &t.verification);
            }
            if t.result_report.enabled {
                egui::CollapsingHeader::new("수동 결과 보고")
                    .id_salt("manual-result")
                    .show(ui, |ui| {
                        ui.label("실제 작업 결과를 기록합니다. 제출 후 검증 상태는 미실행입니다.");
                        let report = state.result_reports.entry(t.target.clone()).or_default();
                        ui.add(
                            egui::TextEdit::multiline(report)
                                .desired_rows(4)
                                .desired_width(f32::INFINITY)
                                .char_limit(8192),
                        );
                        if ui
                            .add_enabled(
                                !report.trim().is_empty(),
                                egui::Button::new("결과 제출 · 검증 미실행"),
                            )
                            .clicked()
                        {
                            intents.push(Intent::ReportResult {
                                target: t.target.clone(),
                                attempt_id: t.attempt_id.clone().unwrap_or_default(),
                                text: report.clone(),
                            });
                        }
                    });
            }
            if let Some(result) = &t.result {
                ui.add(egui::Label::new(result).wrap());
            } else {
                weak(ui, snapshot, text(snapshot, "no_result"));
            }
            if t.verify_result.enabled {
                ui.label("직접 수행한 검증 명령과 결과를 기록합니다.");
                let evidence = state
                    .verification_reports
                    .entry(t.target.clone())
                    .or_default();
                ui.add(
                    egui::TextEdit::multiline(evidence)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .char_limit(4096),
                );
                ui.horizontal_wrapped(|ui| {
                    for (passed, label) in [(true, "검증 통과 기록"), (false, "검증 실패 기록")]
                    {
                        if ui
                            .add_enabled(
                                !evidence.trim().is_empty() && t.result_revision.is_some(),
                                egui::Button::new(label),
                            )
                            .clicked()
                        {
                            intents.push(Intent::VerifyResult {
                                target: t.target.clone(),
                                attempt_id: t.attempt_id.clone().unwrap_or_default(),
                                revision: t.result_revision.clone().unwrap(),
                                passed,
                                evidence: evidence.clone(),
                            });
                        }
                    }
                });
            }
            ui.horizontal_wrapped(|ui| {
                let available = Capability {
                    enabled: t.confirm_result.enabled
                        && t.result_revision.is_some()
                        && t.result.is_some(),
                    reason: t.confirm_result.reason.clone(),
                };
                if action(ui, snapshot, text(snapshot, "confirm_result"), &available) {
                    intents.push(Intent::ConfirmResult {
                        target: t.target.clone(),
                        revision: t.result_revision.clone().unwrap(),
                    });
                }
                if ui.button(text(snapshot, "rework")).clicked() {
                    ui.memory_mut(|m| m.request_focus(composer_id(&t.target)));
                    state.composer_focused = true;
                }
            });
        }
        DetailTab::Git => {
            if action(ui, snapshot, text(snapshot, "open_git"), &t.git) {
                intents.push(Intent::OpenGit(t.target.clone()));
            }
            if !t.git.enabled {
                weak(ui, snapshot, &t.git.reason);
            }
        }
        DetailTab::History => history(ui, snapshot, p, Some(&t.target), intents),
        DetailTab::Terminal => {
            if action(ui, snapshot, text(snapshot, "open_terminal"), &t.terminal) {
                intents.push(Intent::OpenSession(t.target.clone()));
            }
            if !t.terminal.enabled {
                weak(ui, snapshot, &t.terminal.reason);
            }
        }
    }
}

fn history(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    p: &Project,
    target: Option<&RouteTarget>,
    intents: &mut Vec<Intent>,
) {
    ui.heading(text(snapshot, "history"));
    let entries: Vec<_> = p
        .history
        .iter()
        .filter(|h| target.is_none() || h.target.as_ref() == target)
        .collect();
    if entries.is_empty() {
        weak(ui, snapshot, text(snapshot, "no_history"));
    }
    for h in entries {
        ui.push_id(&h.id, |ui| {
            weak(ui, snapshot, &h.time);
            ui.add(egui::Label::new(egui::RichText::new(&h.text).strong()).wrap());
            weak(ui, snapshot, &h.detail);
            if let Some(target) = &h.target
                && ui.small_button(text(snapshot, "open_history")).clicked()
            {
                intents.push(Intent::OpenHistory(target.clone()));
            }
            rule(ui, snapshot);
        });
    }
}

fn terminal(
    ui: &mut egui::Ui,
    state: &UiState,
    snapshot: &Snapshot,
    p: &Project,
    intents: &mut Vec<Intent>,
) {
    let task = selected_task(state, p);
    let target = task.map(|t| &t.target).unwrap_or(&p.main_target);
    let cap = task
        .map(|t| t.terminal.clone())
        .unwrap_or_else(|| Capability {
            enabled: target.session_id.is_some(),
            reason: text(snapshot, "unbound").into(),
        });
    ui.heading(text(snapshot, "terminal"));
    if action(ui, snapshot, text(snapshot, "open_terminal"), &cap) {
        intents.push(Intent::OpenSession(target.clone()));
    }
    if !cap.enabled {
        weak(ui, snapshot, &cap.reason);
    }
}

pub(super) fn composer_id(target: &RouteTarget) -> egui::Id {
    egui::Id::new((
        "command-center-composer",
        &target.workspace_id,
        &target.session_id,
        &target.task_id,
    ))
}

pub(super) fn submit(state: &UiState, target: &RouteTarget, enabled: bool) -> Option<Intent> {
    let text: String = state.draft(target).nfc().collect();
    if enabled && !text.trim().is_empty() && text.len() <= 16 * 1024 {
        Some(Intent::Submit {
            target: target.clone(),
            text,
        })
    } else {
        None
    }
}

pub(super) fn composer(
    ui: &mut egui::Ui,
    state: &mut UiState,
    snapshot: &Snapshot,
    p: &Project,
    intents: &mut Vec<Intent>,
) {
    let terminal_target = if state.view == View::Terminal {
        state.terminal_target.clone()
    } else {
        None
    };
    let task = if state.view == View::Task {
        selected_task(state, p)
    } else {
        terminal_target
            .as_ref()
            .and_then(|target| p.tasks.iter().find(|t| t.target == *target))
    };
    let target_owned = terminal_target
        .unwrap_or_else(|| task.map(|t| &t.target).unwrap_or(&p.main_target).clone());
    let target = &target_owned;
    let session = p.sessions.iter().find(|s| s.target == *target);
    let unavailable = Capability {
        enabled: false,
        reason: text(snapshot, "input_unavailable").into(),
    };
    let cap = task
        .map(|t| &t.input)
        .or_else(|| session.map(|s| &s.input))
        .unwrap_or(if target == &p.main_target {
            &p.main_input
        } else {
            &unavailable
        });
    // A vanished task cannot silently fall back to the project's main input.
    let valid = state.view != View::Task || task.is_some();
    let enabled = valid && cap.enabled && target.workspace_id == p.id;
    let target_label = task
        .map(|t| format!("{} · {} · {}", t.provider, t.role, t.device))
        .or_else(|| session.map(|s| format!("{} · {} · {}", s.title, s.provider, s.device)))
        .unwrap_or_else(|| {
            format!(
                "{} / {} {}",
                p.name,
                text(snapshot, "main"),
                p.main_provider
            )
        });
    let available = ui.available_width().min(1000.0);
    let inset = ((ui.available_width() - available) * 0.5).max(0.0);
    let frame_rect = egui::Rect::from_min_size(
        egui::pos2(ui.max_rect().left() + inset, ui.cursor().top()),
        egui::vec2(available, ui.available_height()),
    );
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(frame_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| {
            egui::Frame::NONE
                .fill(colors(snapshot).bg)
                .stroke(egui::Stroke::new(
                    1.0,
                    if state.composer_focused {
                        colors(snapshot).accent
                    } else {
                        colors(snapshot).line
                    },
                ))
                .corner_radius(6)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_width((available - 24.0).max(1.0));
                    let id = composer_id(target);
                    if state.ime_target.as_ref() != Some(target) {
                        state.ime_composing = false;
                        state.ime_target = Some(target.clone());
                    }
                    let was_composing = state.ime_composing;
                    let frame_has_ime = ui.input(|i| {
                        i.raw
                            .events
                            .iter()
                            .any(|e| matches!(e, egui::Event::Ime(_)))
                    });
                    ui.input(|i| {
                        for event in &i.raw.events {
                            match event {
                                egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) => {
                                    state.ime_composing = !text.is_empty()
                                }
                                egui::Event::Ime(egui::ImeEvent::Commit(_)) => {
                                    state.ime_composing = false
                                }
                                _ => {}
                            }
                        }
                    });
                    let draft = state.drafts.entry(target.clone()).or_default();
                    let hint = if !enabled {
                        if cap.reason.is_empty() {
                            text(snapshot, "input_unavailable")
                        } else {
                            &cap.reason
                        }
                    } else {
                        text(
                            snapshot,
                            if task.is_some() {
                                "composer_task"
                            } else {
                                "composer_main"
                            },
                        )
                    };
                    let editor = ui.add_enabled(
                        enabled,
                        egui::TextEdit::multiline(draft)
                            .id(id)
                            .desired_width(f32::INFINITY)
                            .desired_rows(if state.composer_focused { 4 } else { 2 })
                            .hint_text(hint)
                            .return_key(egui::KeyboardShortcut::new(
                                egui::Modifiers::SHIFT,
                                egui::Key::Enter,
                            )),
                    );
                    editor.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::TextEdit,
                            enabled,
                            format!("{target_label} · {}", text(snapshot, "send")),
                        )
                    });
                    if editor.changed() {
                        intents.push(Intent::DraftChanged {
                            target: target.clone(),
                            text: draft.clone(),
                        });
                    }
                    state.composer_focused = editor.has_focus();
                    if !editor.has_focus() {
                        state.ime_composing = false;
                    }
                    let too_long = draft.nfc().collect::<String>().len() > 16 * 1024;
                    let can_send = enabled && !draft.trim().is_empty() && !too_long;
                    let enter_key = editor.has_focus()
                        && ui.input_mut(|i| {
                            !i.modifiers.shift
                                && i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                        });
                    let enter =
                        enter_key && !was_composing && !frame_has_ime && !state.ime_composing;
                    let mut clicked = false;
                    ui.horizontal(|ui| {
                        let caption = if too_long {
                            text(snapshot, "input_limit")
                        } else {
                            &target_label
                        };
                        ui.add_sized(
                            [
                                ((available - 24.0) - 72.0 * snapshot.font_scale).max(1.0),
                                34.0,
                            ],
                            egui::Label::new(egui::RichText::new(caption).small().color(
                                if too_long {
                                    colors(snapshot).red
                                } else {
                                    colors(snapshot).muted
                                },
                            ))
                            .halign(egui::Align::Min)
                            .truncate(),
                        )
                        .on_hover_text(caption);
                        clicked = ui
                            .add_enabled(
                                can_send,
                                egui::Button::new(text(snapshot, "send"))
                                    .fill(colors(snapshot).accent),
                            )
                            .clicked();
                    });
                    if (clicked || enter)
                        && can_send
                        && let Some(intent) = submit(state, target, enabled)
                    {
                        intents.push(intent);
                    }
                });
        },
    );
}
