//! Project navigation and current-project worker monitor.

use super::*;

pub(super) fn state_label(snapshot: &Snapshot, state: TaskState) -> &str {
    text(
        snapshot,
        match state {
            TaskState::PlanReview => "state_plan_review",
            TaskState::Queued => "state_queued",
            TaskState::Running => "state_running",
            TaskState::WaitingApproval => "state_approval",
            TaskState::WaitingInput => "state_input",
            TaskState::ResultSubmitted => "state_result",
            TaskState::VerificationFailed => "state_verification",
            TaskState::AwaitingUserConfirmation => "state_confirmation",
            TaskState::Completed => "state_completed",
            TaskState::Failed => "state_failed",
            TaskState::Cancelled => "state_cancelled",
            TaskState::ObservationUnknown => "state_unknown",
        },
    )
}

pub(super) fn state_color(snapshot: &Snapshot, state: TaskState) -> egui::Color32 {
    let c = colors(snapshot);
    match state {
        TaskState::Running => c.accent,
        TaskState::Completed => c.green,
        TaskState::ObservationUnknown | TaskState::Failed | TaskState::VerificationFailed => c.red,
        state if state.needs_attention() => c.amber,
        _ => c.muted,
    }
}

pub(super) fn status(ui: &mut egui::Ui, snapshot: &Snapshot, state: TaskState) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(format!("● {}", state_label(snapshot, state)))
                .small()
                .color(state_color(snapshot, state)),
        )
        .wrap(),
    );
}

pub(super) fn projects(
    ui: &mut egui::Ui,
    state: &mut UiState,
    snapshot: &Snapshot,
    intents: &mut Vec<Intent>,
) {
    ui.horizontal(|ui| {
        weak(ui, snapshot, text(snapshot, "workspaces"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if action(ui, snapshot, "＋", &snapshot.add_project) {
                intents.push(Intent::AddProject);
            }
        });
    });
    egui::ScrollArea::vertical()
        .id_salt("cc-project-list")
        .max_height((ui.available_height() - 142.0 * snapshot.font_scale).max(30.0))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if snapshot.projects.is_empty() {
                weak(ui, snapshot, text(snapshot, "no_projects"));
            }
            for p in &snapshot.projects {
                ui.add_space(8.0);
                let active = state.active_workspace.as_ref() == Some(&p.id);
                let title = format!("{}  {}", p.name, p.tasks.len());
                let r = ui.add_sized(
                    [ui.available_width(), 34.0],
                    egui::Button::new(egui::RichText::new(title).strong().color(if active {
                        colors(snapshot).accent
                    } else {
                        colors(snapshot).text
                    }))
                    .frame(false),
                );
                if r.clicked() {
                    intents.push(Intent::SelectProject {
                        workspace_id: p.id.clone(),
                    });
                    state.view = View::Main;
                    state.selected_task_id = None;
                    state.left_drawer = false;
                }
                for t in p.tasks.iter().filter(|t| t.target.workspace_id == p.id) {
                    let selected = active
                        && state.view == View::Task
                        && state.selected_task_id == t.target.task_id;
                    let fill = if selected {
                        colors(snapshot).raised
                    } else {
                        egui::Color32::TRANSPARENT
                    };
                    let response = egui::Frame::NONE
                        .fill(fill)
                        .inner_margin(egui::Margin {
                            left: 14,
                            right: 6,
                            top: 8,
                            bottom: 8,
                        })
                        .show(ui, |ui| {
                            ui.set_width((ui.available_width() - 4.0).max(1.0));
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&t.title)
                                        .size(12.0 * snapshot.font_scale)
                                        .strong(),
                                )
                                .wrap(),
                            );
                            weak(
                                ui,
                                snapshot,
                                format!(
                                    "{} · {} · {}",
                                    state_label(snapshot, t.state),
                                    t.provider,
                                    t.device
                                ),
                            );
                        })
                        .response;
                    let r = ui.interact(
                        response.rect,
                        ui.id().with(("task", &p.id, &t.target.task_id)),
                        egui::Sense::click(),
                    );
                    r.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            ui.is_enabled(),
                            &t.title,
                        )
                    });
                    if selected {
                        ui.painter().vline(
                            response.rect.left(),
                            response.rect.y_range(),
                            egui::Stroke::new(2.0, colors(snapshot).accent),
                        );
                    }
                    if r.clicked() {
                        if !active {
                            intents.push(Intent::SelectProject {
                                workspace_id: p.id.clone(),
                            });
                        }
                        state.open_task(t.target.clone());
                    }
                }
                for session in p.sessions.iter().filter(|s| s.target.workspace_id == p.id) {
                    let label = format!(
                        "{}\n{} · {}",
                        session.title, session.provider, session.device
                    );
                    if ui
                        .add_sized(
                            [ui.available_width(), 48.0],
                            egui::Button::new(egui::RichText::new(label).small())
                                .frame(false)
                                .wrap(),
                        )
                        .clicked()
                    {
                        if !active {
                            intents.push(Intent::SelectProject {
                                workspace_id: p.id.clone(),
                            });
                        }
                        state.open_terminal(session.target.clone());
                        intents.push(Intent::OpenSession(session.target.clone()));
                    }
                }
                if p.tasks.is_empty() && p.sessions.is_empty() {
                    weak(ui, snapshot, text(snapshot, "no_sessions"));
                }
                ui.add_space(12.0);
            }
        });
    rule(ui, snapshot);
    if ui.button(text(snapshot, "settings")).clicked() {
        state.view = View::Settings;
        state.settings_draft = None;
        state.left_drawer = false;
    }
    if ui.button(text(snapshot, "back_main")).clicked() {
        state.view = View::Main;
        state.left_drawer = false;
    }
    weak(ui, snapshot, text(snapshot, "project_independent"));
}

pub(super) fn visible_tasks(p: &Project, filter: MonitorFilter) -> impl Iterator<Item = &Task> {
    p.tasks.iter().filter(move |t| {
        t.target.workspace_id == p.id
            && match filter {
                MonitorFilter::All => true,
                MonitorFilter::Attention => t.state.needs_attention(),
                MonitorFilter::Running => t.state == TaskState::Running,
            }
    })
}

pub(super) fn monitor(
    ui: &mut egui::Ui,
    state: &mut UiState,
    snapshot: &Snapshot,
    p: Option<&Project>,
    intents: &mut Vec<Intent>,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(text(snapshot, "workers")).strong());
        if let Some(p) = p {
            weak(
                ui,
                snapshot,
                format!(
                    "{} {} / {}",
                    text(snapshot, "running"),
                    p.tasks
                        .iter()
                        .filter(|t| t.target.workspace_id == p.id && t.state == TaskState::Running)
                        .count(),
                    p.config.parallel_limit
                ),
            );
        }
    });
    rule(ui, snapshot);
    ui.horizontal_wrapped(|ui| {
        for (filter, key) in [
            (MonitorFilter::All, "all"),
            (MonitorFilter::Attention, "attention"),
            (MonitorFilter::Running, "running"),
        ] {
            if ui
                .selectable_label(state.monitor_filter == filter, text(snapshot, key))
                .clicked()
            {
                state.monitor_filter = filter;
            }
        }
    });
    ui.add_space(12.0);
    egui::ScrollArea::vertical()
        .id_salt(("cc-workers", p.map(|p| &p.id)))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let Some(p) = p else {
                weak(ui, snapshot, text(snapshot, "no_projects"));
                return;
            };
            let tasks: Vec<_> = visible_tasks(p, state.monitor_filter).collect();
            if tasks.is_empty() {
                weak(
                    ui,
                    snapshot,
                    text(
                        snapshot,
                        if p.tasks.is_empty() {
                            "no_tasks"
                        } else {
                            "no_filtered"
                        },
                    ),
                );
            }
            for t in tasks {
                ui.push_id((&t.target.workspace_id, &t.target.task_id), |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{} · {}", t.provider, t.role)).small(),
                        );
                        status(ui, snapshot, t.state);
                    });
                    ui.add_space(6.0);
                    ui.add(egui::Label::new(egui::RichText::new(&t.title).strong()).wrap());
                    weak(ui, snapshot, &t.scope);
                    ui.add_space(6.0);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&t.activity).size(12.0 * snapshot.font_scale),
                        )
                        .wrap(),
                    );
                    ui.horizontal_wrapped(|ui| {
                        weak(ui, snapshot, &t.device);
                        weak(ui, snapshot, &t.observed_at);
                    });
                    ui.horizontal_wrapped(|ui| {
                        let key = if t.state == TaskState::WaitingApproval {
                            "approval_check"
                        } else if matches!(
                            t.state,
                            TaskState::ResultSubmitted | TaskState::AwaitingUserConfirmation
                        ) {
                            "result_check"
                        } else {
                            "task_detail"
                        };
                        if ui.small_button(text(snapshot, key)).clicked()
                            && let Some(id) = &t.target.task_id
                        {
                            state.select_task(id.clone());
                            state.detail_tab = if key == "result_check" {
                                DetailTab::Result
                            } else {
                                DetailTab::Work
                            };
                        }
                        if action(ui, snapshot, text(snapshot, "terminal"), &t.terminal) {
                            state.open_terminal(t.target.clone());
                            intents.push(Intent::OpenSession(t.target.clone()));
                        }
                    });
                });
                rule(ui, snapshot);
            }
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(text(snapshot, "rules"))
                    .small()
                    .strong(),
            );
            weak(
                ui,
                snapshot,
                format!("{} · {} {}", p.main_provider, p.main_model, p.main_effort),
            );
            weak(ui, snapshot, p.config.worker_providers.join(" · "));
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
            if ui.small_button(text(snapshot, "settings")).clicked() {
                state.view = View::Settings;
                state.settings_draft = None;
                state.right_drawer = false;
            }
            rule(ui, snapshot);
            if action(
                ui,
                snapshot,
                text(snapshot, "manage_devices"),
                &snapshot.manage_devices,
            ) {
                state.view = View::Devices;
                state.right_drawer = false;
                intents.push(Intent::ManageDevices {
                    workspace_id: Some(p.id.clone()),
                });
            }
        });
}
