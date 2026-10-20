//! Independently editable composer; submission remains host-authorized.

use super::*;
use unicode_normalization::UnicodeNormalization as _;

impl UiState {
    /// Used by the host to distinguish an intentional empty draft from no local edit.
    pub(crate) fn has_draft(&self, target: &RouteTarget) -> bool {
        self.drafts.contains_key(target)
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

#[cfg(test)]
mod tests;

mod picker;
use picker::main_picker;

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

#[derive(Clone)]
struct MainDraftOwner {
    target: RouteTarget,
    alternate: bool,
}

/// Only the project's confirmed main target can inherit its pre-binding draft.
fn preserve_main_draft(ctx: &egui::Context, state: &mut UiState, p: &Project) -> bool {
    let key = egui::Id::new(("cc-main-draft-owner", &p.id));
    let previous = ctx.data_mut(|data| data.get_temp::<MainDraftOwner>(key));
    let unbound = RouteTarget {
        workspace_id: p.id.clone(),
        ..Default::default()
    };
    let mut alternate = previous
        .as_ref()
        .is_some_and(|old| old.target == p.main_target && old.alternate);
    if p.main_target.workspace_id == p.id
        && p.main_target.task_id.is_none()
        && p.main_target.session_id.is_some()
        && previous.as_ref().is_none_or(|old| old.target == unbound)
        && let Some(text) = state.drafts.remove(&unbound)
    {
        if let Some(existing) = state.drafts.insert(p.main_target.clone(), text)
            && !existing.is_empty()
        {
            state.drafts.insert(unbound, existing);
            alternate = true;
        }
    }
    ctx.data_mut(|data| {
        data.insert_temp(
            key,
            MainDraftOwner {
                target: p.main_target.clone(),
                alternate,
            },
        )
    });
    alternate
}

pub(super) fn composer(
    ui: &mut egui::Ui,
    state: &mut UiState,
    snapshot: &Snapshot,
    p: &Project,
    intents: &mut Vec<Intent>,
) {
    let terminal_target = (state.view == View::Terminal)
        .then(|| state.terminal_target.clone())
        .flatten();
    let task = if state.view == View::Task {
        selected_task(state, p)
    } else {
        terminal_target
            .as_ref()
            .and_then(|target| p.tasks.iter().find(|t| t.target == *target))
    };
    let target = terminal_target
        .unwrap_or_else(|| task.map(|t| &t.target).unwrap_or(&p.main_target).clone());
    let session = p.sessions.iter().find(|s| s.target == target);
    let is_main = target == p.main_target && state.view != View::Task;
    let unavailable = Capability::default();
    let cap = if is_main {
        &p.main_input
    } else {
        task.map(|t| &t.input)
            .or_else(|| session.map(|s| &s.input))
            .unwrap_or(&unavailable)
    };
    let valid = target.workspace_id == p.id
        && (state.view != View::Task || task.is_some())
        && (is_main || task.is_some() || session.is_some());
    let relay_readonly = target
        .session_id
        .as_deref()
        .is_some_and(|id| id.starts_with("remote:"));
    let editable = valid && !relay_readonly;
    let enabled = valid && cap.enabled && !relay_readonly;
    let unbound = is_main && target.session_id.is_none();
    let alternate = is_main && preserve_main_draft(ui.ctx(), state, p);
    let target_label = task
        .map(|t| format!("{} · {} · {}", t.provider, t.role, t.device))
        .or_else(|| {
            session
                .filter(|_| !is_main)
                .map(|s| format!("{} · {} · {}", s.title, s.provider, s.device))
        })
        .unwrap_or_else(|| {
            if unbound {
                "메인 에이전트 선택".into()
            } else {
                format!(
                    "{} / {} {}",
                    p.name,
                    text(snapshot, "main"),
                    p.main_provider
                )
            }
        });
    let mobile = ui.ctx().content_rect().width() <= 760.0;
    let horizontal = if mobile {
        12.0
    } else if ui.ctx().content_rect().width() <= 1250.0 {
        20.0
    } else {
        24.0
    };
    let available = (ui.available_width() - horizontal * 2.0)
        .min(1000.0)
        .max(1.0);
    let inset = ((ui.available_width() - available) * 0.5).max(0.0);
    let origin = egui::pos2(
        ui.max_rect().left() + inset,
        ui.cursor().top() + if mobile { 9.0 } else { 12.0 },
    );
    let scale = snapshot.font_scale.clamp(1.0, 2.0);
    let text_height = (if mobile {
        if state.composer_focused { 88.0 } else { 60.0 }
    } else if state.composer_focused {
        100.0
    } else {
        62.0
    }) + (scale - 1.0) * 56.0;
    let frame_rect = egui::Rect::from_min_size(origin, egui::vec2(available, text_height + 44.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(frame_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    child.spacing_mut().item_spacing = egui::Vec2::ZERO;
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
        .corner_radius(7)
        .show(&mut child, |ui| {
            ui.set_width((available - 2.0).max(1.0));
            ui.set_min_height(text_height + 42.0);
            if state.ime_target.as_ref() != Some(&target) {
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
                        egui::Event::Ime(egui::ImeEvent::Commit(_)) => state.ime_composing = false,
                        _ => {}
                    }
                }
            });
            let draft = state.drafts.entry(target.clone()).or_default();
            let editor = egui::ScrollArea::vertical()
                .id_salt(composer_id(&target).with("scroll"))
                .max_height(text_height)
                .min_scrolled_height(text_height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_enabled(
                        editable,
                        egui::TextEdit::multiline(draft)
                            .id(composer_id(&target))
                            .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(14, 12)))
                            .desired_width(f32::INFINITY)
                            .desired_rows(2)
                            .font(egui::FontId::proportional(
                                if mobile { 16.0 } else { 14.0 } * scale,
                            ))
                            .hint_text(text(
                                snapshot,
                                if task.is_some() {
                                    "composer_task"
                                } else {
                                    "composer_main"
                                },
                            ))
                            .return_key(egui::KeyboardShortcut::new(
                                egui::Modifiers::SHIFT,
                                egui::Key::Enter,
                            )),
                    )
                })
                .inner;
            editor.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::TextEdit,
                    editable,
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
            let can_attempt =
                editable && !draft.trim().is_empty() && !too_long && (enabled || unbound);
            let enter_key = editor.has_focus()
                && ui.input_mut(|i| {
                    !i.modifiers.shift && i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                });
            let enter = enter_key && !was_composing && !frame_has_ime && !state.ime_composing;
            let footer_top = origin.y + 1.0 + text_height;
            let footer_rect = egui::Rect::from_min_size(
                egui::pos2(origin.x + 14.0, footer_top),
                egui::vec2((available - 24.0).max(1.0), 34.0),
            );
            let mut footer = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(footer_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            footer.spacing_mut().item_spacing.x = 8.0;
            let send_rect = egui::Rect::from_min_size(
                egui::pos2(footer_rect.right() - 34.0, footer_rect.top()),
                egui::vec2(34.0, 34.0),
            );
            let mut send_ui = footer.new_child(egui::UiBuilder::new().max_rect(send_rect));
            send_ui.spacing_mut().button_padding = egui::Vec2::ZERO;
            send_ui.spacing_mut().interact_size = egui::vec2(34.0, 34.0);
            let send = send_ui.add_enabled(
                can_attempt,
                egui::Button::new(egui::RichText::new("↑").size(22.0).color(
                    if snapshot.dark_mode {
                        colors(snapshot).bg
                    } else {
                        egui::Color32::WHITE
                    },
                ))
                .fill(colors(snapshot).accent)
                .min_size(egui::vec2(34.0, 34.0)),
            );
            send.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    can_attempt,
                    text(snapshot, "send"),
                )
            });
            let label_rect = egui::Rect::from_min_max(
                footer_rect.min,
                egui::pos2(send_rect.left() - 8.0, footer_rect.bottom()),
            );
            let mut label_ui = footer.new_child(
                egui::UiBuilder::new()
                    .max_rect(label_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            let caption = if too_long {
                text(snapshot, "input_limit")
            } else {
                &target_label
            };
            let caption_width = (label_rect.width()
                - if alternate {
                    106.0
                } else if !mobile && label_rect.width() >= 430.0 {
                    224.0
                } else {
                    0.0
                })
            .max(1.0);
            let anchor = label_ui.add_sized(
                [caption_width, 34.0],
                egui::Button::new(
                    egui::RichText::new(caption)
                        .size(if mobile { 10.0 } else { 11.0 })
                        .color(if too_long {
                            colors(snapshot).red
                        } else {
                            colors(snapshot).muted
                        }),
                )
                .frame(false)
                .truncate()
                .sense(if is_main {
                    egui::Sense::click()
                } else {
                    egui::Sense::hover()
                }),
            );
            if is_main {
                state.composer_focused |= main_picker(
                    ui,
                    &anchor,
                    snapshot,
                    p,
                    anchor.clicked() || ((send.clicked() || enter) && can_attempt && unbound),
                    intents,
                );
            }
            if !mobile && !too_long && !alternate && label_rect.width() >= 430.0 {
                label_ui.add(
                    egui::Label::new(
                        egui::RichText::new(text(snapshot, "send_hint"))
                            .size(11.0)
                            .color(colors(snapshot).muted),
                    )
                    .truncate(),
                );
            }
            if alternate && label_ui.small_button("다른 초안 보기").clicked() {
                let unbound = RouteTarget {
                    workspace_id: p.id.clone(),
                    ..Default::default()
                };
                if let Some(other) = state.drafts.remove(&unbound) {
                    let current = state
                        .drafts
                        .insert(target.clone(), other.clone())
                        .unwrap_or_default();
                    state.drafts.insert(unbound, current);
                    intents.push(Intent::DraftChanged {
                        target: target.clone(),
                        text: other,
                    });
                }
            }
            if (send.clicked() || enter)
                && can_attempt
                && !unbound
                && let Some(intent) = submit(state, &target, enabled)
            {
                intents.push(intent);
            }
        });
}
