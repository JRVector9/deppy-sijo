//! Main conversation, task detail, and exactly targeted composer.

use super::*;

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
            let content_width = ui.available_width().min(960.0);
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
                .size(
                    (if ui.ctx().viewport_rect().width() <= 760.0 {
                        13.0
                    } else {
                        14.0
                    }) * snapshot.font_scale,
                )
                .line_height(Some(
                    (if ui.ctx().viewport_rect().width() <= 760.0 {
                        20.15
                    } else {
                        21.7
                    }) * snapshot.font_scale,
                ))
                .strong()
                .family(crate::fonts::command_center_semibold_family(ui.ctx())),
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
    ui.spacing_mut().item_spacing.y = 0.0;
    ui.spacing_mut().interact_size.y = 0.0;
    if p.storage_retry.enabled {
        ui.label(&p.storage_retry.reason);
        if ui.button("저장 상태 다시 확인").clicked() {
            intents.push(Intent::RetryStorage {
                workspace_id: p.id.clone(),
            });
        }
        rule(ui, snapshot);
    }
    ui.add(egui::Label::new(
        egui::RichText::new(format!("{} · {}", text(snapshot, "goal"), p.name))
            .size(11.0 * snapshot.font_scale)
            .line_height(Some(17.05 * snapshot.font_scale))
            .color(colors(snapshot).muted),
    ));
    ui.add_space(9.0);
    ui.add(
        egui::Label::new(
            egui::RichText::new(if p.goal.is_empty() {
                text(snapshot, "first_goal")
            } else {
                &p.goal
            })
            .size(
                (if state.last_layout.is_some_and(|l| l.mobile) {
                    16.0
                } else {
                    18.0
                }) * snapshot.font_scale,
            )
            .strong()
            .family(crate::fonts::command_center_semibold_family(ui.ctx()))
            .line_height(Some(
                (if state.last_layout.is_some_and(|l| l.mobile) {
                    25.6
                } else {
                    28.8
                }) * snapshot.font_scale,
            )),
        )
        .wrap(),
    );
    ui.add_space(20.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().interact_size.y = 0.0;
        for (index, key) in ["goal", "plan_assign", "independent", "collect", "verify"]
            .iter()
            .enumerate()
        {
            if index > 0 {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(
                        if state.last_layout.is_some_and(|l| l.mobile) {
                            10.0
                        } else {
                            26.0
                        },
                        12.0,
                    ),
                    egui::Sense::hover(),
                );
                ui.painter().hline(
                    rect.x_range(),
                    rect.center().y,
                    egui::Stroke::new(1.0, colors(snapshot).line),
                );
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
                if *key == "goal" {
                    "목표".into()
                } else {
                    text(snapshot, key).into()
                }
            };
            ui.label(
                egui::RichText::new(label)
                    .size(
                        (if state.last_layout.is_some_and(|l| l.mobile) {
                            9.0
                        } else {
                            10.0
                        }) * snapshot.font_scale,
                    )
                    .line_height(Some(
                        (if state.last_layout.is_some_and(|l| l.mobile) {
                            13.95
                        } else {
                            15.5
                        }) * snapshot.font_scale,
                    ))
                    .color(if *key == "independent" {
                        colors(snapshot).accent
                    } else {
                        colors(snapshot).muted
                    }),
            );
        }
    });
    ui.add_space(24.0);
    let (border, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(
        border.x_range(),
        border.top(),
        egui::Stroke::new(1.0, colors(snapshot).line),
    );
    if p.manual_plan.enabled {
        egui::CollapsingHeader::new("고급 · 배정안 직접 확인")
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
                text(snapshot, "main_plan")
            },
        );
        if p.create_main.enabled && action(ui, snapshot, "새 메인 만들기", &p.create_main) {
            intents.push(Intent::CreateMain {
                workspace_id: p.id.clone(),
            });
        }
        ui.add_space(8.0);
        if !p.main_summary.is_empty() {
            description(ui, snapshot, &p.main_summary);
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
        ui.add_space(16.0);
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, t) in p
                .tasks
                .iter()
                .filter(|t| t.target.workspace_id == p.id)
                .enumerate()
            {
                ui.push_id(
                    (
                        "assignment",
                        &t.target.workspace_id,
                        &t.target.task_id,
                        &t.target.session_id,
                    ),
                    |ui| assignment_row(ui, state, snapshot, i, t),
                );
            }
        });
        if !p.tasks.is_empty() {
            let (r, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
            ui.painter().hline(
                r.x_range(),
                r.top(),
                egui::Stroke::new(1.0, colors(snapshot).line),
            );
            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().interact_size.y = 0.0;
                ui.label(
                    egui::RichText::new(if p.config.use_worktrees {
                        "작업별 독립 worktree"
                    } else {
                        "공유 경로 · 수정 범위 확인"
                    })
                    .size(10.0 * snapshot.font_scale)
                    .color(colors(snapshot).muted),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "동시 실행 최대 {} · {}",
                            p.config.parallel_limit, p.config.automation
                        ))
                        .size(10.0 * snapshot.font_scale)
                        .color(colors(snapshot).muted),
                    );
                });
            });
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
    let pending_approvals: Vec<_> = p
        .pending_approvals
        .iter()
        .filter(|request| {
            request.target.workspace_id == p.id && request.target.session_id.is_some()
        })
        .collect();
    if !attention.is_empty() || !pending_approvals.is_empty() {
        thread(ui, snapshot, "CHECK", |ui| {
            title(
                ui,
                snapshot,
                &format!(
                    "{} {}",
                    text(snapshot, "needs_check"),
                    attention.len() + pending_approvals.len()
                ),
            );
            ui.add_space(8.0);
            description(ui, snapshot, text(snapshot, "check_help"));
            ui.add_space(18.0);
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
            for request in pending_approvals {
                ui.push_id(
                    (
                        &request.target.workspace_id,
                        &request.target.session_id,
                        &request.approval.id,
                    ),
                    |ui| {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "{} · {}",
                                text(snapshot, "approval_title"),
                                request.session_title
                            ))
                            .strong()
                            .family(crate::fonts::command_center_semibold_family(ui.ctx())),
                        );
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&request.approval.command).monospace(),
                            )
                            .wrap()
                            .selectable(true),
                        );
                        weak(ui, snapshot, &request.approval.directory);
                        weak(ui, snapshot, &request.approval.scope);
                        ui.horizontal_wrapped(|ui| {
                            for (allow, key) in [(true, "allow_once"), (false, "deny")] {
                                if action(
                                    ui,
                                    snapshot,
                                    text(snapshot, key),
                                    &request.approval.action,
                                ) {
                                    intents.push(Intent::Approval {
                                        target: request.target.clone(),
                                        approval_id: request.approval.id.clone(),
                                        allow,
                                    });
                                }
                            }
                        });
                        if !request.approval.action.enabled
                            && !request.approval.action.reason.is_empty()
                        {
                            weak(ui, snapshot, &request.approval.action.reason);
                        }
                    },
                );
            }
        });
    }
    if !p.history.is_empty() {
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

fn description(ui: &mut egui::Ui, snapshot: &Snapshot, value: &str) {
    ui.add_sized(
        [
            ui.available_width()
                .min(72.0 * 7.5586 * snapshot.font_scale),
            0.0,
        ],
        egui::Label::new(
            egui::RichText::new(value)
                .size(12.0 * snapshot.font_scale)
                .line_height(Some(22.2 * snapshot.font_scale))
                .color(colors(snapshot).muted),
        )
        .halign(egui::Align::Min)
        .wrap(),
    );
}

fn thread(ui: &mut egui::Ui, snapshot: &Snapshot, label: &str, draw: impl FnOnce(&mut egui::Ui)) {
    let mobile = ui.ctx().viewport_rect().width() <= 760.0;
    ui.add_space(if mobile { 18.0 } else { 22.0 });
    if mobile {
        ui.add(egui::Label::new(
            egui::RichText::new(label)
                .monospace()
                .size(9.0 * snapshot.font_scale)
                .color(colors(snapshot).muted),
        ));
        ui.add_space(7.0);
        draw(ui);
    } else {
        let width = ui.available_width();
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.allocate_ui(egui::vec2(42.0, 16.0), |ui| {
                ui.set_min_width(42.0);
                ui.add(egui::Label::new(
                    egui::RichText::new(label)
                        .monospace()
                        .size(10.0 * snapshot.font_scale)
                        .color(colors(snapshot).muted),
                ));
            });
            ui.allocate_ui_with_layout(
                egui::vec2((width - 54.0).max(1.0), 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_min_width((width - 54.0).max(1.0));
                    draw(ui);
                },
            );
        });
    }
    ui.add_space(if mobile { 18.0 } else { 22.0 });
    let (r, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(
        r.x_range(),
        r.top(),
        egui::Stroke::new(1.0, colors(snapshot).line),
    );
}

fn task_list(ui: &mut egui::Ui, state: &mut UiState, snapshot: &Snapshot, p: &Project) {
    ui.heading("프로젝트 작업");
    weak(
        ui,
        snapshot,
        format!(
            "{} · {}개 독립 작업",
            p.name,
            p.tasks
                .iter()
                .filter(|t| t.target.workspace_id == p.id)
                .count()
        ),
    );
    ui.add_space(24.0);
    let mobile = state.last_layout.is_some_and(|l| l.mobile);
    if p.tasks.is_empty() {
        weak(ui, snapshot, text(snapshot, "no_tasks"));
    }
    if !mobile {
        ui.horizontal(|ui| {
            weak(ui, snapshot, "나눈 작업");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                weak(ui, snapshot, "상태");
                ui.add_space(70.0);
                weak(ui, snapshot, text(snapshot, "owner_device"));
            });
        });
        rule(ui, snapshot);
    }
    for t in p.tasks.iter().filter(|t| t.target.workspace_id == p.id) {
        let width = ui.available_width();
        let mut clicked = false;
        let r = egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(if mobile { 0 } else { 12 }, 13))
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    let owner = if mobile {
                        0.0
                    } else if width < 700.0 {
                        140.0
                    } else {
                        190.0
                    };
                    let state_w = 100.0;
                    ui.allocate_ui_with_layout(
                        egui::vec2((width - owner - state_w - 40.0).max(60.0), 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            clicked = ui
                                .add(
                                    egui::Label::new(
                                        egui::RichText::new(&t.title)
                                            .size(
                                                (if mobile { 13.0 } else { 14.0 })
                                                    * snapshot.font_scale,
                                            )
                                            .strong()
                                            .family(crate::fonts::command_center_semibold_family(
                                                ui.ctx(),
                                            )),
                                    )
                                    .wrap()
                                    .sense(egui::Sense::click()),
                                )
                                .clicked();
                            weak(ui, snapshot, &t.scope);
                            if mobile {
                                weak(
                                    ui,
                                    snapshot,
                                    format!("{} · {} · {}", t.provider, t.role, t.device),
                                );
                            }
                        },
                    );
                    if !mobile {
                        ui.allocate_ui_with_layout(
                            egui::vec2(owner, 0.0),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                ui.label(
                                    egui::RichText::new(format!("{} · {}", t.provider, t.role))
                                        .size(13.0 * snapshot.font_scale),
                                );
                                weak(ui, snapshot, &t.device);
                            },
                        );
                    }
                    workers::status(ui, snapshot, t.state);
                });
            })
            .response;
        ui.painter().hline(
            r.rect.x_range(),
            r.rect.bottom(),
            egui::Stroke::new(1.0, colors(snapshot).line),
        );
        if (clicked
            || ui
                .interact(
                    r.rect,
                    ui.id()
                        .with(("task-list", &t.target.workspace_id, &t.target.task_id)),
                    egui::Sense::click(),
                )
                .clicked())
            && let Some(id) = &t.target.task_id
        {
            state.select_task(id.clone());
        }
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
    if state.detail_tab == DetailTab::Result {
        state.detail_tab = DetailTab::Work;
    }
    ui.heading(&t.title);
    ui.horizontal_wrapped(|ui| {
        workers::status(ui, snapshot, t.state);
        weak(
            ui,
            snapshot,
            format!("{} · {} · {}", t.provider, t.role, t.device),
        );
    });
    ui.add_space(12.0);
    egui::ScrollArea::horizontal()
        .id_salt("cc-detail-tabs")
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = if ui.ctx().viewport_rect().width() <= 760.0 {
                    16.0
                } else {
                    22.0
                };
                for (tab, key) in [
                    (DetailTab::Work, "work_result"),
                    (DetailTab::Terminal, "original_terminal"),
                    (DetailTab::Git, "git"),
                    (DetailTab::History, "history"),
                ] {
                    let r = ghost_button(
                        ui,
                        snapshot,
                        text(snapshot, key),
                        38.0,
                        if ui.ctx().viewport_rect().width() <= 760.0 {
                            11.0
                        } else {
                            14.0
                        },
                    );
                    if state.detail_tab == tab {
                        ui.painter().hline(
                            r.rect.x_range(),
                            r.rect.bottom(),
                            egui::Stroke::new(2.0, colors(snapshot).accent),
                        );
                    }
                    if r.clicked() {
                        state.detail_tab = tab;
                        if t.target.session_id.is_some() {
                            match tab {
                                DetailTab::Git => intents.push(Intent::OpenGit(t.target.clone())),
                                DetailTab::History => {
                                    intents.push(Intent::OpenHistory(t.target.clone()))
                                }
                                DetailTab::Terminal => {
                                    intents.push(Intent::OpenSession(t.target.clone()))
                                }
                                _ => {}
                            }
                        }
                    }
                }
            });
        });
    rule(ui, snapshot);
    match state.detail_tab {
        DetailTab::Work | DetailTab::Result => {
            title(ui, snapshot, "진행 내용");
            description(ui, snapshot, &t.activity);
            if t.reconcile_remote.enabled
                && action(ui, snapshot, "원래 요청 상태만 확인", &t.reconcile_remote)
            {
                intents.push(Intent::ReconcileRemote {
                    target: t.target.clone(),
                });
            }
            if t.attempt_id.is_some()
                && t.target.session_id.is_some()
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
            if (t.create_worker.enabled || t.state == TaskState::VerificationFailed)
                && action(ui, snapshot, "새 워커 생성 후 배정", &t.create_worker)
            {
                intents.push(Intent::CreateWorker {
                    target: t.target.clone(),
                });
            }
            egui::Grid::new(("cc-task-facts", &t.target.workspace_id, &t.target.task_id))
                .num_columns(2)
                .min_col_width(if ui.ctx().viewport_rect().width() <= 760.0 {
                    80.0
                } else {
                    110.0
                })
                .spacing([16.0, 10.0])
                .show(ui, |ui| {
                    for (key, value) in [
                        ("scope", &t.scope),
                        ("directory", &t.directory),
                        ("criteria", &t.acceptance_criteria),
                        ("device", &t.device),
                    ] {
                        if !value.is_empty() {
                            weak(ui, snapshot, text(snapshot, key));
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(value).size(13.0 * snapshot.font_scale),
                                )
                                .wrap(),
                            );
                            ui.end_row();
                        }
                    }
                });
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
            ui.add_space(24.0);
            rule(ui, snapshot);
            title(ui, snapshot, "제출된 결과");
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

fn assignment_row(ui: &mut egui::Ui, state: &mut UiState, snapshot: &Snapshot, i: usize, t: &Task) {
    let scale = snapshot.font_scale.clamp(0.75, 2.0);
    let mobile = state.last_layout.is_some_and(|l| l.mobile);
    let (number_w, owner_w, gap) = if mobile {
        (18.0, 76.0, 7.0)
    } else if state
        .last_layout
        .is_some_and(|l| l.screen.width() <= 1250.0)
    {
        (20.0, 85.0, 12.0)
    } else {
        (24.0, 104.0, 12.0)
    };
    let title_w = (ui.available_width() - number_w - owner_w - 2.0 * gap).max(1.0);
    let line = egui::Stroke::new(1.0, colors(snapshot).line);
    let mut owner_left = 0.0;
    ui.add_space(1.0);
    let response = egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(0, 12))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(gap, 3.0);
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(number_w, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_min_width(number_w);
                        ui.label(
                            egui::RichText::new(format!("{:02}", i + 1))
                                .size(10.0 * scale)
                                .color(colors(snapshot).muted),
                        );
                    },
                );
                ui.allocate_ui_with_layout(
                    egui::vec2(title_w, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(title_w);
                        let label = format!("{} · {}", t.role, t.title);
                        let response = ui.add(
                            egui::Label::new(
                                egui::RichText::new(&label)
                                    .size(12.0 * scale)
                                    .line_height(Some(18.6 * scale))
                                    .strong()
                                    .family(crate::fonts::command_center_semibold_family(ui.ctx())),
                            )
                            .wrap()
                            .sense(egui::Sense::click()),
                        );
                        response.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                ui.is_enabled(),
                                &label,
                            )
                        });
                        if response.clicked()
                            && let Some(id) = &t.target.task_id
                        {
                            state.select_task(id.clone());
                        }
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&t.scope)
                                    .size((if mobile { 9.0 } else { 10.0 }) * scale)
                                    .line_height(Some((if mobile { 13.95 } else { 15.5 }) * scale))
                                    .color(colors(snapshot).muted),
                            )
                            .wrap(),
                        );
                    },
                );
                owner_left = ui.next_widget_position().x;
                ui.allocate_ui_with_layout(
                    egui::vec2(owner_w, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(owner_w);
                        ui.horizontal(|ui| {
                            ui.add_space(if mobile { 8.0 } else { 12.0 });
                            ui.vertical(|ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&t.provider)
                                            .size((if mobile { 10.0 } else { 11.0 }) * scale)
                                            .line_height(Some(
                                                (if mobile { 15.5 } else { 17.05 }) * scale,
                                            )),
                                    )
                                    .wrap(),
                                );
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&t.device)
                                            .size(10.0 * scale)
                                            .line_height(Some(15.5 * scale))
                                            .color(colors(snapshot).muted),
                                    )
                                    .wrap(),
                                );
                            });
                        });
                    },
                );
            });
        })
        .response;
    ui.painter()
        .hline(response.rect.x_range(), response.rect.top(), line);
    ui.painter()
        .vline(owner_left, response.rect.y_range().shrink(12.0), line);
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
            let width = ui.available_width();
            let row = egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(0, 13))
                .show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        ui.allocate_ui(
                            egui::vec2(if width < 500.0 { 35.0 } else { 50.0 }, 0.0),
                            |ui| weak(ui, snapshot, &h.time),
                        );
                        ui.allocate_ui_with_layout(
                            egui::vec2((width - 80.0).max(1.0), 0.0),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&h.text)
                                            .size(13.0 * snapshot.font_scale)
                                            .strong()
                                            .family(crate::fonts::command_center_semibold_family(
                                                ui.ctx(),
                                            )),
                                    )
                                    .wrap(),
                                );
                                weak(ui, snapshot, &h.detail);
                                if let Some(target) = &h.target
                                    && ghost_button(
                                        ui,
                                        snapshot,
                                        text(snapshot, "open_history"),
                                        28.0,
                                        11.0,
                                    )
                                    .clicked()
                                {
                                    intents.push(Intent::OpenHistory(target.clone()));
                                }
                            },
                        );
                    });
                })
                .response;
            ui.painter().hline(
                row.rect.x_range(),
                row.rect.bottom(),
                egui::Stroke::new(1.0, colors(snapshot).line),
            );
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

#[cfg(test)]
pub(super) use super::composer::submit;
pub(super) use super::composer::{composer, composer_id};
