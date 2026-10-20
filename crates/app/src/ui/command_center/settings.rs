//! Inline project settings. Drafts and persistence are owned by the caller.

use super::super::popup;
use super::*;

pub(super) fn show(
    ui: &mut egui::Ui,
    state: &mut UiState,
    snapshot: &Snapshot,
    p: &Project,
    intents: &mut Vec<Intent>,
) {
    let width = ui.available_width().min(960.0);
    let bounds = egui::Rect::from_min_size(
        ui.next_widget_position(),
        egui::vec2(width, ui.available_height()),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(bounds), |ui| {
        ui.set_width(width);
        ui.spacing_mut().interact_size.y = 36.0 * snapshot.font_scale.clamp(1.0, 2.0);
        form(ui, state, snapshot, p, intents);
    });
}

fn form(
    ui: &mut egui::Ui,
    state: &mut UiState,
    snapshot: &Snapshot,
    p: &Project,
    intents: &mut Vec<Intent>,
) {
    ui.heading(text(snapshot, "settings"));
    weak(
        ui,
        snapshot,
        "메인과 작업 폴더를 정하고, 필요할 때 실행 규칙을 조정하세요.",
    );
    if state
        .settings_draft
        .as_ref()
        .is_none_or(|(id, _)| id != &p.id)
    {
        state.settings_draft = Some((p.id.clone(), p.config.clone()));
        state.settings_error = None;
    }
    let (_, draft) = state.settings_draft.as_mut().unwrap();
    field(ui, snapshot, "name", |ui| {
        input(ui, snapshot, &mut draft.name);
    });
    field(ui, snapshot, "project_directory", |ui| {
        input(ui, snapshot, &mut draft.directory);
    });
    weak(
        ui,
        snapshot,
        "다른 컴퓨터의 폴더는 기기 연결 시 확인합니다.",
    );
    inline(ui, |ui, index| {
        if index == 0 {
            let prior = draft.provider.clone();
            choices(
                ui,
                snapshot,
                "provider",
                &mut draft.provider,
                &p.available_providers,
            );
            if prior != draft.provider
                && let Some(option) = p
                    .available_agents
                    .iter()
                    .find(|a| a.provider == draft.provider)
            {
                draft.model.clone_from(&option.model);
                draft.effort.clone_from(&option.effort);
            }
        } else {
            let prior = draft.model.clone();
            choices(
                ui,
                snapshot,
                "model",
                &mut draft.model,
                &models_for(p, &draft.provider),
            );
            if prior != draft.model
                && let Some(option) = p
                    .available_agents
                    .iter()
                    .find(|a| a.provider == draft.provider && a.model == draft.model)
            {
                draft.effort.clone_from(&option.effort);
            }
        }
    });
    ui.add_space(24.0);
    let rules = egui::CollapsingHeader::new(text(snapshot, "rules"))
        .id_salt("cc-rules")
        .show(ui, |ui| {
            weak(ui, snapshot, "이 프로젝트의 다음 배정부터 적용합니다.");
            inline(ui, |ui, index| {
                if index == 0 {
                    choices(
                        ui,
                        snapshot,
                        "effort",
                        &mut draft.effort,
                        &efforts_for(p, &draft.provider, &draft.model),
                    );
                } else {
                    field(ui, snapshot, "parallel_limit", |ui| {
                        ui.add_sized(
                            [ui.available_width(), 36.0 * snapshot.font_scale.max(1.0)],
                            egui::DragValue::new(&mut draft.parallel_limit).range(1..=8),
                        );
                    });
                }
            });
            field(ui, snapshot, "worker_providers", |ui| {
                ui.horizontal_wrapped(|ui| {
                    for provider in &p.available_providers {
                        let mut allowed = draft.worker_providers.contains(provider);
                        if ui.checkbox(&mut allowed, provider).changed() {
                            if allowed {
                                draft.worker_providers.push(provider.clone());
                            } else {
                                draft.worker_providers.retain(|p| p != provider);
                            }
                        }
                    }
                });
            });
            choices(
                ui,
                snapshot,
                "automation",
                &mut draft.automation,
                &p.available_automation,
            );
            ui.checkbox(&mut draft.use_worktrees, text(snapshot, "worktrees"));
            field(ui, snapshot, "main_binding", |ui| {
                let selected = p
                    .sessions
                    .iter()
                    .find(|s| {
                        s.target.session_id == draft.main_session_id
                            && s.target.workspace_id == p.id
                    })
                    .map(|s| s.title.as_str())
                    .unwrap_or(text(snapshot, "unbound"));
                egui::ComboBox::from_id_salt("main-binding")
                    .selected_text(selected)
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut draft.main_session_id,
                            None,
                            text(snapshot, "unbound"),
                        );
                        for session in p.sessions.iter().filter(|s| {
                            s.target.workspace_id == p.id && s.target.session_id.is_some()
                        }) {
                            ui.selectable_value(
                                &mut draft.main_session_id,
                                session.target.session_id.clone(),
                                format!(
                                    "{} · {} · {}",
                                    session.title, session.provider, session.device
                                ),
                            );
                        }
                    });
            });
            field(ui, snapshot, "scope", |ui| {
                let mut roots = draft.allowed_scope_roots.join("\n");
                if ui
                    .add(
                        egui::TextEdit::multiline(&mut roots)
                            .hint_text("src\ntests")
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    )
                    .changed()
                {
                    draft.allowed_scope_roots = roots
                        .lines()
                        .map(str::trim)
                        .filter(|r| !r.is_empty())
                        .map(str::to_owned)
                        .collect();
                }
            });
        });
    ui.painter().hline(
        ui.max_rect().x_range(),
        rules.header_response.rect.top() - 12.0,
        egui::Stroke::new(1.0, colors(snapshot).line),
    );
    ui.add_space(18.0);
    egui::Frame::NONE
        .fill(colors(snapshot).bg)
        .stroke(egui::Stroke::new(1.0, colors(snapshot).line))
        .inner_margin(egui::Margin::symmetric(13, 10))
        .show(ui, |ui| {
            ui.set_width((ui.available_width() - 26.0).max(1.0));
            ui.add(
                egui::Label::new(
                    egui::RichText::new(text(snapshot, "settings_note"))
                        .size(12.0 * snapshot.font_scale)
                        .color(colors(snapshot).muted),
                )
                .wrap(),
            );
        });
    if let Some(error) = &state.settings_error {
        popup::notice(ui, error, popup::NoticeTone::Error);
    }
    if !p.save_settings.enabled && !p.save_settings.reason.is_empty() {
        weak(ui, snapshot, &p.save_settings.reason);
    }
    ui.add_space(18.0);
    let mut cancel = false;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                p.save_settings.enabled,
                egui::Button::new(egui::RichText::new(text(snapshot, "save")).color(
                    if snapshot.dark_mode {
                        egui::Color32::from_rgb(7, 25, 31)
                    } else {
                        egui::Color32::WHITE
                    },
                ))
                .fill(colors(snapshot).accent),
            )
            .on_disabled_hover_text(&p.save_settings.reason)
            .clicked()
        {
            if draft.name.trim().is_empty()
                || draft.directory.trim().is_empty()
                || !(1..=8).contains(&draft.parallel_limit)
            {
                state.settings_error = Some(text(snapshot, "invalid_settings").into());
            } else {
                intents.push(Intent::SaveProject {
                    workspace_id: p.id.clone(),
                    config: draft.clone(),
                });
                state.settings_error = None;
            }
        }
        cancel = ghost_button(ui, snapshot, text(snapshot, "cancel"), 34.0, 13.0).clicked();
    });
    if cancel {
        state.settings_draft = None;
        state.settings_error = None;
        state.view = View::Main;
    }
}

fn input(ui: &mut egui::Ui, snapshot: &Snapshot, value: &mut String) {
    ui.add_sized(
        [ui.available_width(), 36.0 * snapshot.font_scale.max(1.0)],
        egui::TextEdit::singleline(value)
            .desired_width(f32::INFINITY)
            .vertical_align(egui::Align::Center),
    );
}
fn field(ui: &mut egui::Ui, snapshot: &Snapshot, key: &str, control: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(19.0);
    popup::field(ui, text(snapshot, key), None, control);
}
fn inline(ui: &mut egui::Ui, mut draw: impl FnMut(&mut egui::Ui, usize)) {
    if ui.ctx().viewport_rect().width() <= 760.0 {
        draw(ui, 0);
        draw(ui, 1);
    } else {
        let width = (ui.available_width() - 16.0) * 0.5;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            ui.allocate_ui_with_layout(
                egui::vec2(width, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| draw(ui, 0),
            );
            ui.allocate_ui_with_layout(
                egui::vec2(width, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| draw(ui, 1),
            );
        });
    }
}

fn choices(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    key: &str,
    value: &mut String,
    choices: &[String],
) {
    field(ui, snapshot, key, |ui| {
        ui.add_enabled_ui(!choices.is_empty(), |ui| {
            egui::ComboBox::from_id_salt(key)
                .selected_text(if value.is_empty() {
                    text(snapshot, "unbound")
                } else {
                    value.as_str()
                })
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for choice in choices {
                        ui.selectable_value(value, choice.clone(), choice);
                    }
                });
        });
    });
}
fn models_for(p: &Project, provider: &str) -> Vec<String> {
    if p.available_agents.is_empty() {
        return p.available_models.clone();
    }
    let mut values = Vec::new();
    for agent in p.available_agents.iter().filter(|a| a.provider == provider) {
        if !values.contains(&agent.model) {
            values.push(agent.model.clone());
        }
    }
    values
}
fn efforts_for(p: &Project, provider: &str, model: &str) -> Vec<String> {
    if p.available_agents.is_empty() {
        return p.available_efforts.clone();
    }
    let mut values = Vec::new();
    for agent in p
        .available_agents
        .iter()
        .filter(|a| a.provider == provider && a.model == model)
    {
        if !values.contains(&agent.effort) {
            values.push(agent.effort.clone());
        }
    }
    values
}
