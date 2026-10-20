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
    let panel = ui.available_rect_before_wrap();
    let scale = snapshot.font_scale.clamp(0.75, 2.0);
    let header_h = 48.0_f32.max(38.0 * scale);
    let footer_h = 46.0_f32.max(38.0 * scale);
    let header =
        egui::Rect::from_min_max(panel.min, egui::pos2(panel.right(), panel.top() + header_h));
    let footer = egui::Rect::from_min_max(
        egui::pos2(
            panel.left(),
            (panel.bottom() - footer_h).max(header.bottom()),
        ),
        panel.max,
    );
    let list = egui::Rect::from_min_max(header.left_bottom(), footer.right_top());
    let c = colors(snapshot);
    region(ui, header, c.bg, "cc-monitor-heading", 0.0, |ui| {
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.add_space(16.0);
            ui.label(
                egui::RichText::new(text(snapshot, "workers"))
                    .size(12.0 * scale)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(12.0);
                if state.right_drawer && ui.small_button(text(snapshot, "close")).clicked() {
                    state.right_drawer = false;
                }
                if let Some(p) = p {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} {} / {}",
                            text(snapshot, "running"),
                            p.tasks
                                .iter()
                                .filter(|t| t.target.workspace_id == p.id
                                    && t.state == TaskState::Running)
                                .count(),
                            p.config.parallel_limit
                        ))
                        .size(10.0 * scale)
                        .color(c.muted),
                    );
                }
            });
        });
    });
    ui.painter().hline(
        header.x_range(),
        header.bottom(),
        egui::Stroke::new(1.0, c.line),
    );
    region(ui, list, c.bg, "cc-monitor-list", 16.0, |ui| {
        egui::ScrollArea::vertical()
            .id_salt(("cc-workers", p.map(|p| &p.id)))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    ui.spacing_mut().button_padding = egui::vec2(7.0, 4.0);
                    ui.spacing_mut().interact_size.y = 26.0 * scale;
                    for (filter, key) in [
                        (MonitorFilter::All, "all"),
                        (MonitorFilter::Attention, "attention"),
                        (MonitorFilter::Running, "running"),
                    ] {
                        if ui
                            .selectable_label(
                                state.monitor_filter == filter,
                                egui::RichText::new(text(snapshot, key)).size(10.0 * scale),
                            )
                            .clicked()
                        {
                            state.monitor_filter = filter;
                        }
                    }
                });
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
                    ui.add_space(12.0);
                    ui.push_id((&t.target.workspace_id, &t.target.task_id), |ui| {
                        ui.horizontal(|ui| {
                            egui::Frame::NONE
                                .stroke(egui::Stroke::new(1.0, c.line))
                                .corner_radius(3)
                                .inner_margin(egui::Margin::symmetric(4, 2))
                                .show(ui, |ui| {
                                    ui.label(
                                        egui::RichText::new(format!("{} · {}", t.provider, t.role))
                                            .size(10.0 * scale),
                                    );
                                });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "● {}",
                                            state_label(snapshot, t.state)
                                        ))
                                        .size(10.0 * scale)
                                        .color(state_color(snapshot, t.state)),
                                    );
                                },
                            );
                        });
                        ui.add_space(5.0);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&t.title).size(12.0 * scale).strong(),
                            )
                            .wrap(),
                        );
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&t.scope)
                                    .size(10.0 * scale)
                                    .color(c.muted),
                            )
                            .wrap(),
                        );
                        ui.add_space(3.0);
                        ui.add(
                            egui::Label::new(egui::RichText::new(&t.activity).size(11.0 * scale))
                                .wrap(),
                        );
                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(&t.device)
                                    .size(10.0 * scale)
                                    .color(c.muted),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(&t.observed_at)
                                            .size(10.0 * scale)
                                            .color(c.muted),
                                    );
                                },
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.spacing_mut().interact_size.y = 28.0 * scale;
                            ui.spacing_mut().button_padding = egui::vec2(7.0, 4.0);
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
                            if ui
                                .small_button(
                                    egui::RichText::new(text(snapshot, key)).size(10.0 * scale),
                                )
                                .clicked()
                                && let Some(id) = &t.target.task_id
                            {
                                state.select_task(id.clone());
                                state.detail_tab = if key == "result_check" {
                                    DetailTab::Result
                                } else {
                                    DetailTab::Work
                                };
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let r = ui.add_enabled(
                                        t.terminal.enabled,
                                        egui::Button::new(
                                            egui::RichText::new(text(snapshot, "terminal"))
                                                .size(10.0 * scale),
                                        ),
                                    );
                                    if !t.terminal.enabled {
                                        r.clone().on_disabled_hover_text(&t.terminal.reason);
                                    }
                                    if r.clicked() {
                                        state.open_terminal(t.target.clone());
                                        intents.push(Intent::OpenSession(t.target.clone()));
                                    }
                                },
                            );
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
            });
    });
    region(ui, footer, c.bg, "cc-monitor-footer", 0.0, |ui| {
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.add_space(16.0);
            ui.label(
                egui::RichText::new(format!(
                    "기기 {}대 연결",
                    snapshot.devices.iter().filter(|d| d.connected).count()
                ))
                .size(10.0 * scale)
                .color(c.muted),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(12.0);
                ui.spacing_mut().button_padding = egui::vec2(7.0, 4.0);
                if action(
                    ui,
                    snapshot,
                    text(snapshot, "manage_devices"),
                    &snapshot.manage_devices,
                ) {
                    state.view = View::Devices;
                    state.right_drawer = false;
                    intents.push(Intent::ManageDevices {
                        workspace_id: p.map(|p| p.id.clone()),
                    });
                }
            });
        });
    });
    ui.painter().hline(
        footer.x_range(),
        footer.top(),
        egui::Stroke::new(1.0, c.line),
    );
}
