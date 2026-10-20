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
    ui.heading(text(snapshot, "settings"));
    ui.add_space(8.0);
    popup::notice(ui, text(snapshot, "settings_note"), popup::NoticeTone::Info);
    ui.add_space(14.0);
    if state
        .settings_draft
        .as_ref()
        .is_none_or(|(id, _)| id != &p.id)
    {
        state.settings_draft = Some((p.id.clone(), p.config.clone()));
        state.settings_error = None;
    }
    let (_, draft) = state.settings_draft.as_mut().unwrap();
    ui.set_max_width(ui.available_width().min(620.0));
    field(ui, snapshot, "name", |ui| {
        ui.add(egui::TextEdit::singleline(&mut draft.name).desired_width(f32::INFINITY));
    });
    field(ui, snapshot, "directory", |ui| {
        ui.add(egui::TextEdit::singleline(&mut draft.directory).desired_width(f32::INFINITY));
    });
    field(ui, snapshot, "main_binding", |ui| {
        let selected = p
            .sessions
            .iter()
            .find(|s| s.target.session_id == draft.main_session_id && s.target.workspace_id == p.id)
            .map(|s| s.title.as_str())
            .unwrap_or(text(snapshot, "unbound"));
        egui::ComboBox::from_id_salt("main-binding")
            .selected_text(selected)
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut draft.main_session_id, None, text(snapshot, "unbound"));
                for session in p
                    .sessions
                    .iter()
                    .filter(|s| s.target.workspace_id == p.id && s.target.session_id.is_some())
                {
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
    choices(
        ui,
        snapshot,
        "provider",
        &mut draft.provider,
        &p.available_providers,
    );
    choices(ui, snapshot, "model", &mut draft.model, &p.available_models);
    ui.add_space(10.0);
    egui::CollapsingHeader::new(text(snapshot, "rules"))
        .id_salt("cc-rules")
        .show(ui, |ui| {
            choices(
                ui,
                snapshot,
                "effort",
                &mut draft.effort,
                &p.available_efforts,
            );
            field(ui, snapshot, "worker_providers", |ui| {
                if p.available_providers.is_empty() {
                    weak(ui, snapshot, text(snapshot, "unsupported"));
                }
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
            field(ui, snapshot, "parallel_limit", |ui| {
                ui.add(egui::DragValue::new(&mut draft.parallel_limit).range(1..=8));
            });
            choices(
                ui,
                snapshot,
                "automation",
                &mut draft.automation,
                &p.available_automation,
            );
            ui.checkbox(&mut draft.use_worktrees, text(snapshot, "worktrees"));
        });
    if let Some(error) = &state.settings_error {
        popup::notice(ui, error, popup::NoticeTone::Error);
    }
    if !p.save_settings.enabled {
        popup::notice(
            ui,
            if p.save_settings.reason.is_empty() {
                text(snapshot, "unsupported")
            } else {
                &p.save_settings.reason
            },
            popup::NoticeTone::Info,
        );
    }
    ui.add_space(14.0);
    let mut cancel = false;
    popup::footer(ui, None, |ui| {
        if action(ui, snapshot, text(snapshot, "save"), &p.save_settings) {
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
        cancel = ui.button(text(snapshot, "cancel")).clicked();
    });
    if cancel {
        state.settings_draft = None;
        state.settings_error = None;
        state.view = View::Main;
    }
}

fn field(ui: &mut egui::Ui, snapshot: &Snapshot, key: &str, control: impl FnOnce(&mut egui::Ui)) {
    popup::field(ui, text(snapshot, key), None, control);
    ui.add_space(12.0);
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
                    text(snapshot, "unsupported")
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
