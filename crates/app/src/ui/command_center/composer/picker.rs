//! Anchored agent configuration; all effects remain explicit intents.

use super::*;

#[derive(Clone)]
struct PickerState {
    config: ProjectConfig,
    error: Option<String>,
    saved_revision: Option<String>,
}

fn same_config(desired: &ProjectConfig, current: &ProjectConfig) -> bool {
    let mut desired = desired.clone();
    desired.revision = current.revision.clone();
    desired.worker_providers.sort();
    desired.worker_providers.dedup();
    let mut current = current.clone();
    current.worker_providers.sort();
    current.worker_providers.dedup();
    desired == current
}

fn agent_matches(option: &AgentOption, config: &ProjectConfig) -> bool {
    option.provider == config.provider
        && option.model == config.model
        && option.effort == config.effort
}

fn choose_session(config: &mut ProjectConfig, session: &Session) {
    config.main_session_id = session.target.session_id.clone();
    config.provider = session.provider.clone();
    config.model = session.model.clone().unwrap_or_default();
    config.effort = session.effort.clone().unwrap_or_default();
}

fn option_choice(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    key: &str,
    value: &mut String,
    options: Vec<String>,
) -> bool {
    use crate::ui::popup;
    let mut changed = false;
    popup::field(ui, text(snapshot, key), None, |ui| {
        let selected = if value == "unbound" {
            "에이전트 선택".into()
        } else if value.is_empty() {
            "현재 설정 유지".into()
        } else {
            value.clone()
        };
        let choice = popup::choice_input(ui, key, &selected, |ui| {
            let mut options = options;
            options.sort();
            options.dedup();
            for option in options {
                let label = if option.is_empty() {
                    "현재 설정 유지"
                } else {
                    &option
                };
                changed |= ui.selectable_value(value, option.clone(), label).changed();
            }
        });
        choice.response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::ComboBox,
                ui.is_enabled(),
                text(snapshot, key),
            )
        });
    });
    changed
}

pub(super) fn main_picker(
    ui: &mut egui::Ui,
    anchor: &egui::Response,
    snapshot: &Snapshot,
    p: &Project,
    open_requested: bool,
    intents: &mut Vec<Intent>,
) -> bool {
    use crate::ui::popup;
    let key = egui::Id::new(("cc-composer-main-picker", &p.id));
    let mut picker = ui.ctx().data_mut(|data| data.get_temp::<PickerState>(key));
    let was_open = picker.is_some();
    if open_requested && picker.is_none() {
        picker = Some(PickerState {
            config: p.config.clone(),
            error: None,
            saved_revision: None,
        });
    }
    let Some(mut picker) = picker else {
        return false;
    };
    let child_menu_open = ui.ctx().any_popup_open();
    let mut close = false;
    let response = egui::Popup::from_response(anchor)
        .id(key)
        .open(true)
        .close_behavior(egui::PopupCloseBehavior::IgnoreClicks)
        .frame(popup::popover_frame(ui.ctx()))
        .show(|ui| {
            close |= popup::popover(
                ui,
                popup::PopupSpec {
                    id: key,
                    width: 360.0,
                    title: "메인 에이전트 선택",
                    subtitle: "설정한 뒤 전송을 다시 눌러 주세요.",
                    close_label: text(snapshot, "close"),
                    close_enabled: true,
                },
                |ui| {
                    popup::body(ui, |ui| {
                        let config = &mut picker.config;
                        popup::field(ui, text(snapshot, "main_binding"), None, |ui| {
                            let selected = p
                                .sessions
                                .iter()
                                .find(|s| {
                                    s.target.workspace_id == p.id
                                        && s.target.session_id == config.main_session_id
                                })
                                .map(|s| s.title.as_str())
                                .unwrap_or("새 메인 에이전트");
                            let choice =
                                popup::choice_input(ui, "composer-binding", selected, |ui| {
                                    ui.selectable_value(
                                        &mut config.main_session_id,
                                        None,
                                        "새 메인 에이전트",
                                    );
                                    for session in p.sessions.iter().filter(|s| {
                                        s.target.workspace_id == p.id
                                            && s.target.task_id.is_none()
                                            && s.target.session_id.is_some()
                                            && s.input.enabled
                                    }) {
                                        if ui
                                            .selectable_label(
                                                config.main_session_id == session.target.session_id,
                                                format!(
                                                    "{} · {} · {}",
                                                    session.title, session.provider, session.device
                                                ),
                                            )
                                            .clicked()
                                        {
                                            choose_session(config, session);
                                        }
                                    }
                                });
                            choice.response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::ComboBox,
                                    ui.is_enabled(),
                                    text(snapshot, "main_binding"),
                                )
                            });
                        });
                        if config.main_session_id.is_none() {
                            if option_choice(
                                ui,
                                snapshot,
                                "provider",
                                &mut config.provider,
                                p.available_agents
                                    .iter()
                                    .map(|a| a.provider.clone())
                                    .collect(),
                            ) && let Some(first) = p
                                .available_agents
                                .iter()
                                .find(|a| a.provider == config.provider)
                            {
                                config.model = first.model.clone();
                                config.effort = first.effort.clone();
                            }
                            if option_choice(
                                ui,
                                snapshot,
                                "model",
                                &mut config.model,
                                p.available_agents
                                    .iter()
                                    .filter(|a| a.provider == config.provider)
                                    .map(|a| a.model.clone())
                                    .collect(),
                            ) && let Some(first) = p
                                .available_agents
                                .iter()
                                .find(|a| a.provider == config.provider && a.model == config.model)
                            {
                                config.effort = first.effort.clone();
                            }
                            option_choice(
                                ui,
                                snapshot,
                                "effort",
                                &mut config.effort,
                                p.available_agents
                                    .iter()
                                    .filter(|a| {
                                        a.provider == config.provider && a.model == config.model
                                    })
                                    .map(|a| a.effort.clone())
                                    .collect(),
                            );
                        } else {
                            weak(
                                ui,
                                snapshot,
                                format!(
                                    "{} · {} · {}",
                                    config.provider, config.model, config.effort
                                ),
                            );
                        }
                        popup::field(ui, text(snapshot, "worker_providers"), None, |ui| {
                            let mut providers: Vec<_> = p
                                .available_agents
                                .iter()
                                .map(|a| a.provider.clone())
                                .collect();
                            providers.sort();
                            providers.dedup();
                            ui.horizontal_wrapped(|ui| {
                                for provider in providers {
                                    let mut allowed = config.worker_providers.contains(&provider);
                                    if ui.checkbox(&mut allowed, &provider).changed() {
                                        if allowed {
                                            config.worker_providers.push(provider);
                                        } else {
                                            config
                                                .worker_providers
                                                .retain(|value| value != &provider);
                                        }
                                    }
                                }
                            });
                        });
                        if p.config.allowed_scope_roots.is_empty() || picker.error.is_some() {
                            popup::field(ui, text(snapshot, "scope"), None, |ui| {
                                let mut scopes = config.allowed_scope_roots.join("\n");
                                if ui
                                    .add(
                                        egui::TextEdit::multiline(&mut scopes)
                                            .desired_rows(2)
                                            .hint_text("src\ntests")
                                            .desired_width(f32::INFINITY),
                                    )
                                    .changed()
                                {
                                    config.allowed_scope_roots = scopes
                                        .lines()
                                        .map(str::trim)
                                        .filter(|s| !s.is_empty())
                                        .map(str::to_owned)
                                        .collect();
                                }
                            });
                        }
                        if p.available_agents.is_empty() && p.sessions.is_empty() {
                            popup::notice(
                                ui,
                                "사용 가능한 에이전트 설정을 확인하고 있습니다. 초안은 보존됩니다.",
                                popup::NoticeTone::Info,
                            );
                        }
                        if let Some(error) = &picker.error {
                            popup::notice(ui, error, popup::NoticeTone::Error);
                        }
                        if picker.saved_revision.is_some() && !snapshot.status.is_empty() {
                            popup::notice(ui, &snapshot.status, popup::NoticeTone::Info);
                        }
                        if !p.save_settings.enabled {
                            popup::notice(ui, &p.save_settings.reason, popup::NoticeTone::Info);
                        }
                    });
                    popup::footer(ui, None, |ui| {
                        if ui.button(text(snapshot, "cancel")).clicked() {
                            close = true;
                        }
                        if ui
                            .add_enabled(
                                p.save_settings.enabled,
                                egui::Button::new(text(snapshot, "save")),
                            )
                            .clicked()
                        {
                            let binding_valid =
                                picker.config.main_session_id.as_ref().is_some_and(|id| {
                                    p.sessions.iter().any(|s| {
                                        s.target.workspace_id == p.id
                                            && s.target.session_id.as_ref() == Some(id)
                                            && s.provider == picker.config.provider
                                            && s.model.as_deref().unwrap_or_default()
                                                == picker.config.model
                                            && s.effort.as_deref().unwrap_or_default()
                                                == picker.config.effort
                                    })
                                });
                            if picker.config.allowed_scope_roots.is_empty() {
                                picker.error =
                                    Some("작업할 수정 범위를 직접 입력해 주세요.".into());
                            } else if picker.config.worker_providers.is_empty() {
                                picker.error = Some(
                                    "허용할 작업자 에이전트를 하나 이상 선택해 주세요.".into(),
                                );
                            } else if !binding_valid
                                && !p
                                    .available_agents
                                    .iter()
                                    .any(|a| agent_matches(a, &picker.config))
                            {
                                picker.error = Some(
                                    "사용 가능한 에이전트·모델·추론 강도를 선택해 주세요.".into(),
                                );
                            } else {
                                // Save is independent of launch and input. Acknowledgement never submits.
                                intents.push(Intent::SaveProject {
                                    workspace_id: p.id.clone(),
                                    config: picker.config.clone(),
                                });
                                picker.saved_revision = Some(picker.config.revision.clone());
                                picker.error = None;
                            }
                        }
                        let saved_selection = same_config(&picker.config, &p.config);
                        if ui
                            .add_enabled(
                                p.create_main.enabled
                                    && saved_selection
                                    && p.main_target.session_id.is_none(),
                                egui::Button::new("메인 생성"),
                            )
                            .on_disabled_hover_text("에이전트 설정을 저장한 뒤 생성해 주세요.")
                            .clicked()
                        {
                            intents.push(Intent::CreateMain {
                                workspace_id: p.id.clone(),
                            });
                        }
                    });
                },
            );
        });
    // Child ComboBoxes own their popup clicks and first Escape. The parent
    // ignores their close response, which egui otherwise propagates up its stack.
    let nested_open = child_menu_open || ui.ctx().any_popup_open();
    let outside = was_open
        && !nested_open
        && ui.input(|input| {
            input.pointer.any_click()
                && input.pointer.interact_pos().is_some_and(|point| {
                    response
                        .as_ref()
                        .is_some_and(|response| !response.response.rect.contains(point))
                })
        });
    let escape = !nested_open
        && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    if outside || escape || close {
        ui.ctx().data_mut(|data| data.remove::<PickerState>(key));
        ui.ctx()
            .memory_mut(|memory| memory.request_focus(composer_id(&p.main_target)));
    } else {
        if picker
            .saved_revision
            .as_ref()
            .is_some_and(|old| old != &p.config.revision)
            && same_config(&picker.config, &p.config)
        {
            picker.config.revision = p.config.revision.clone();
            picker.saved_revision = None;
        }
        ui.ctx().data_mut(|data| data.insert_temp(key, picker));
    }
    true
}
