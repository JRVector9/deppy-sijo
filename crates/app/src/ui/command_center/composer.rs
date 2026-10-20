//! Independently editable composer; submission remains host-authorized.

use super::*;
use unicode_normalization::UnicodeNormalization as _;

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
                            .frame(egui::Frame::NONE)
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
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            clicked = ui
                                .add_enabled(
                                    can_send,
                                    egui::Button::new(text(snapshot, "send"))
                                        .fill(colors(snapshot).accent),
                                )
                                .clicked();
                            ui.with_layout(
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(caption).small().color(
                                                if too_long {
                                                    colors(snapshot).red
                                                } else {
                                                    colors(snapshot).muted
                                                },
                                            ),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(caption);
                                },
                            );
                        });
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
