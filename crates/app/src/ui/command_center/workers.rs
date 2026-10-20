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
    let panel = ui.available_rect_before_wrap();
    let c = colors(snapshot);
    let footer_h = 132.0 * snapshot.font_scale.max(1.0);
    let header = egui::Rect::from_min_max(panel.min, egui::pos2(panel.right(), panel.top() + 50.0));
    let footer = egui::Rect::from_min_max(
        egui::pos2(
            panel.left() + 8.0,
            (panel.bottom() - footer_h).max(header.bottom()),
        ),
        panel.max - egui::vec2(8.0, 0.0),
    );
    let list = egui::Rect::from_min_max(
        egui::pos2(panel.left() + 8.0, header.bottom()),
        egui::pos2(panel.right() - 8.0, footer.top()),
    );
    region(ui, header, c.bg, "cc-project-heading", 0.0, |ui| {
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.add_space(17.0);
            ui.label(
                egui::RichText::new(text(snapshot, "workspaces"))
                    .size(11.0 * snapshot.font_scale)
                    .color(c.muted),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                if ui
                    .add_enabled(
                        snapshot.add_project.enabled,
                        egui::Button::new(egui::RichText::new("＋").size(12.0)).frame(false),
                    )
                    .on_disabled_hover_text(&snapshot.add_project.reason)
                    .clicked()
                {
                    intents.push(Intent::AddProject);
                }
            });
        });
    });
    region(ui, list, c.bg, "cc-project-list-region", 0.0, |ui| {
        egui::ScrollArea::vertical()
            .id_salt("cc-project-list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                if snapshot.projects.is_empty() {
                    weak(ui, snapshot, text(snapshot, "no_projects"));
                }
                for p in &snapshot.projects {
                    let active = state.active_workspace.as_ref() == Some(&p.id);
                    let (r, response) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 34.0 * snapshot.font_scale.max(1.0)),
                        egui::Sense::click(),
                    );
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            ui.is_enabled(),
                            &p.name,
                        )
                    });
                    if response.hovered() {
                        ui.painter().rect_filled(r, 3.0, c.raised);
                    }
                    let glyph = egui::Rect::from_center_size(
                        egui::pos2(r.left() + 18.0, r.center().y),
                        egui::vec2(18.0, 18.0),
                    );
                    ui.painter().rect_stroke(
                        glyph,
                        3.0,
                        egui::Stroke::new(1.0, c.muted),
                        egui::StrokeKind::Inside,
                    );
                    ui.painter().text(
                        glyph.center(),
                        egui::Align2::CENTER_CENTER,
                        p.name
                            .chars()
                            .next()
                            .map(|ch| ch.to_uppercase().to_string())
                            .unwrap_or_default(),
                        egui::FontId::monospace(11.0),
                        c.muted,
                    );
                    let count = p
                        .tasks
                        .iter()
                        .filter(|t| t.target.workspace_id == p.id)
                        .count();
                    let mut name_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(egui::Rect::from_min_max(
                                egui::pos2(r.left() + 36.0, r.top()),
                                egui::pos2(r.right() - 28.0, r.bottom()),
                            ))
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    );
                    name_ui.add(
                        egui::Label::new(
                            egui::RichText::new(&p.name)
                                .size(12.0 * snapshot.font_scale)
                                .strong()
                                .color(if active { c.accent } else { c.text }),
                        )
                        .truncate(),
                    );
                    ui.painter().text(
                        egui::pos2(r.right() - 9.0, r.center().y),
                        egui::Align2::RIGHT_CENTER,
                        count.to_string(),
                        egui::FontId::proportional(11.0 * snapshot.font_scale),
                        c.muted,
                    );
                    if response.clicked() {
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
                        if session_row(
                            ui,
                            snapshot,
                            &t.title,
                            &t.provider,
                            &t.device,
                            t.state,
                            selected,
                        )
                        .clicked()
                        {
                            if !active {
                                intents.push(Intent::SelectProject {
                                    workspace_id: p.id.clone(),
                                });
                            }
                            state.open_task(t.target.clone());
                        }
                    }
                    for session in independent_sessions(p) {
                        if session_row(
                            ui,
                            snapshot,
                            &session.title,
                            &session.provider,
                            &session.device,
                            session.state,
                            false,
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
                        egui::Frame::NONE
                            .inner_margin(egui::Margin {
                                left: 18,
                                right: 9,
                                top: 8,
                                bottom: 8,
                            })
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new("배정된 작업이 없습니다.")
                                        .size(11.0 * snapshot.font_scale)
                                        .color(c.muted),
                                );
                            });
                    }
                    ui.add_space(18.0);
                }
            });
    });
    ui.painter().hline(
        footer.x_range(),
        footer.top(),
        egui::Stroke::new(1.0, c.line),
    );
    region(ui, footer, c.bg, "cc-project-footer", 8.0, |ui| {
        ui.add_space(6.0);
        ui.spacing_mut().item_spacing.y = 5.0;
        if ghost_button(ui, snapshot, text(snapshot, "settings"), 28.0, 12.0).clicked() {
            state.view = View::Settings;
            state.settings_draft = None;
            state.left_drawer = false;
        }
        if ghost_button(ui, snapshot, "메인에게 작업 맡기기", 28.0, 12.0).clicked() {
            state.view = View::Main;
            state.left_drawer = false;
        }
        ui.add(
            egui::Label::new(
                egui::RichText::new(text(snapshot, "project_independent"))
                    .size(11.0 * snapshot.font_scale)
                    .color(c.muted),
            )
            .wrap(),
        );
    });
}

fn session_row(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    title: &str,
    provider: &str,
    device: &str,
    state: TaskState,
    selected: bool,
) -> egui::Response {
    let c = colors(snapshot);
    let scale = snapshot.font_scale;
    let (r, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 58.0 * scale.max(1.0)),
        egui::Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), title)
    });
    if selected || response.hovered() {
        ui.painter().rect_filled(r, 2.0, c.raised);
    }
    if selected {
        ui.painter()
            .vline(r.left(), r.y_range(), egui::Stroke::new(2.0, c.accent));
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(
                r.shrink2(egui::vec2(9.0, 9.0))
                    .translate(egui::vec2(9.0, 0.0)),
            )
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    child.set_clip_rect(r.intersect(ui.clip_rect()));
    child.spacing_mut().item_spacing.y = 3.0;
    child.add(egui::Label::new(egui::RichText::new(title).size(12.0 * scale).strong()).truncate());
    child.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let (dot, _) = ui.allocate_exact_size(egui::vec2(5.0, 12.0), egui::Sense::hover());
        ui.painter()
            .circle_filled(dot.center(), 2.5, state_color(snapshot, state));
        ui.label(
            egui::RichText::new(state_label(snapshot, state))
                .size(10.0 * scale)
                .color(state_color(snapshot, state)),
        );
        ui.add(
            egui::Label::new(
                egui::RichText::new(format!("· {} · {}", provider, device))
                    .size(10.0 * scale)
                    .color(c.muted),
            )
            .truncate(),
        );
    });
    response
}

fn independent_sessions(p: &Project) -> impl Iterator<Item = &Session> {
    p.sessions.iter().filter(|session| {
        if session.target.workspace_id != p.id {
            return false;
        }
        let Some(id) = session.target.session_id.as_deref() else {
            return true;
        };
        let represented_by = |target: &RouteTarget| {
            target.workspace_id == p.id && target.session_id.as_deref() == Some(id)
        };
        !represented_by(&p.main_target) && !p.tasks.iter().any(|task| represented_by(&task.target))
    })
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
                if state.right_drawer
                    && ghost_button(ui, snapshot, text(snapshot, "close"), 28.0, 10.0).clicked()
                {
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
    region(
        ui,
        list,
        c.bg,
        "cc-monitor-list",
        if ui.ctx().viewport_rect().width() <= 1250.0 {
            14.0
        } else {
            18.0
        },
        |ui| {
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
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new(text(snapshot, key)).size(10.0 * scale),
                                    )
                                    .min_size(egui::vec2(0.0, 26.0))
                                    .fill(if state.monitor_filter == filter {
                                        c.raised
                                    } else {
                                        egui::Color32::TRANSPARENT
                                    })
                                    .stroke(egui::Stroke::new(
                                        1.0,
                                        if state.monitor_filter == filter {
                                            egui::Color32::from_rgb(108, 132, 149)
                                        } else {
                                            c.line
                                        },
                                    )),
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
                        ui.add_space(17.0);
                        let background = ui.painter().add(egui::Shape::Noop);
                        let worker =
                            ui.push_id((&t.target.workspace_id, &t.target.task_id), |ui| {
                                ui.spacing_mut().interact_size.y = 0.0;
                                ui.horizontal(|ui| {
                                    egui::Frame::NONE
                                        .stroke(egui::Stroke::new(1.0, c.line))
                                        .corner_radius(3)
                                        .inner_margin(egui::Margin::symmetric(4, 2))
                                        .show(ui, |ui| {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "{} · {}",
                                                    t.provider, t.role
                                                ))
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
                                ui.add_space(12.0);
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
                                ui.add_space(6.0);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&t.activity)
                                            .size(11.0 * scale)
                                            .color(c.muted),
                                    )
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
                                        TaskState::ResultSubmitted
                                            | TaskState::AwaitingUserConfirmation
                                    ) {
                                        "result_check"
                                    } else {
                                        "task_detail"
                                    };
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                egui::RichText::new(text(snapshot, key))
                                                    .size(10.0 * scale),
                                            )
                                            .min_size(egui::vec2(0.0, 28.0)),
                                        )
                                        .clicked()
                                        && let Some(id) = &t.target.task_id
                                    {
                                        state.select_task(id.clone());
                                        state.detail_tab = DetailTab::Work;
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            let r = ui.add_enabled(
                                                t.terminal.enabled,
                                                egui::Button::new(
                                                    egui::RichText::new(format!(
                                                        "{} ↗",
                                                        text(snapshot, "terminal")
                                                    ))
                                                    .size(10.0 * scale),
                                                )
                                                .min_size(egui::vec2(0.0, 28.0)),
                                            );
                                            if !t.terminal.enabled {
                                                r.clone()
                                                    .on_disabled_hover_text(&t.terminal.reason);
                                            }
                                            if r.clicked() {
                                                state.open_terminal(t.target.clone());
                                                intents.push(Intent::OpenSession(t.target.clone()));
                                            }
                                        },
                                    );
                                });
                            });
                        if state.view == View::Task && state.selected_task_id == t.target.task_id {
                            ui.painter().set(
                                background,
                                egui::Shape::rect_filled(
                                    worker.response.rect.expand2(egui::vec2(10.0, 0.0)),
                                    3.0,
                                    c.raised,
                                ),
                            );
                        }
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
                    if ghost_button(ui, snapshot, "실행 규칙 설정 ↗", 28.0, 10.0).clicked()
                    {
                        state.view = View::Settings;
                        state.settings_draft = None;
                        state.right_drawer = false;
                    }
                });
        },
    );
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
                if ui
                    .add_enabled_ui(snapshot.manage_devices.enabled, |ui| {
                        ghost_button(ui, snapshot, "기기 관리 ↗", 26.0, 10.0)
                    })
                    .inner
                    .clicked()
                {
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
