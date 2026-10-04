use super::{ACTION_GAP, BUTTON_HEIGHT, BUTTON_PADDING, divider, palette};

#[derive(Clone, Copy)]
pub enum ActionTone {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

pub(super) fn button_width(ui: &mut egui::Ui, label: &str) -> f32 {
    ui.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(
                label.into(),
                egui::FontId::proportional(13.0),
                egui::Color32::WHITE,
            )
            .size()
            .x
    }) + BUTTON_PADDING.x * 2.0
}

pub fn action_button(
    ui: &mut egui::Ui,
    label: &str,
    tone: ActionTone,
    enabled: bool,
) -> egui::Response {
    let colors = palette(ui);
    let (fill, hover, stroke, text, hover_text) = match tone {
        ActionTone::Danger => (
            colors.danger,
            colors.danger_hover,
            colors.danger,
            colors.danger_text,
            colors.danger_text,
        ),
        ActionTone::Primary => (
            colors.accent,
            colors.accent_hover,
            colors.accent,
            colors.primary_text,
            colors.primary_text,
        ),
        ActionTone::Secondary => (
            colors.button,
            colors.hover,
            colors.line,
            colors.text,
            colors.text,
        ),
        ActionTone::Ghost => (
            egui::Color32::TRANSPARENT,
            colors.button,
            egui::Color32::TRANSPARENT,
            colors.muted,
            colors.text,
        ),
    };
    // Keep widgets in the footer's wrapping layout: child scopes inherit the
    // remaining row width and can squeeze even a short label into a tall button.
    let previous_style = ui.style().clone();
    ui.spacing_mut().button_padding = BUTTON_PADDING;
    ui.spacing_mut().interact_size.y = BUTTON_HEIGHT;
    ui.visuals_mut().override_text_color = None;
    let widgets = &mut ui.visuals_mut().widgets;
    for (visuals, bg, fg) in [
        (&mut widgets.inactive, fill, text),
        (&mut widgets.hovered, hover, hover_text),
        (&mut widgets.active, hover, hover_text),
        (&mut widgets.noninteractive, fill, text),
    ] {
        visuals.bg_fill = bg;
        visuals.weak_bg_fill = bg;
        visuals.bg_stroke = egui::Stroke::new(1.0, stroke);
        visuals.fg_stroke = egui::Stroke::new(1.0, fg);
        visuals.corner_radius = egui::CornerRadius::same(3);
        visuals.expansion = 0.0;
    }
    let text = egui::RichText::new(label).size(13.0);
    let text = if matches!(tone, ActionTone::Primary | ActionTone::Danger) {
        text.strong()
    } else {
        text
    };
    // Measure against the complete row, not its remaining width. The parent
    // moves the whole button to the next row; only an oversized label wraps.
    let galley = egui::WidgetText::from(text).into_galley(
        ui,
        Some(egui::TextWrapMode::Wrap),
        (ui.max_rect().width() - BUTTON_PADDING.x * 2.0).max(1.0),
        egui::FontId::proportional(13.0),
    );
    let response = ui.add_enabled(
        enabled,
        egui::Button::new(galley).min_size(egui::vec2(0.0, BUTTON_HEIGHT)),
    );
    if response.has_focus() {
        ui.painter().rect_stroke(
            response.rect,
            3,
            egui::Stroke::new(2.0, colors.accent),
            egui::StrokeKind::Inside,
        );
    }
    ui.set_style(previous_style);
    response
}

pub fn footer<T>(
    ui: &mut egui::Ui,
    hint: Option<&str>,
    actions: impl FnOnce(&mut egui::Ui) -> T,
) -> T {
    let colors = palette(ui);
    divider(ui);
    let height_key = ui
        .id()
        .with(("popup_footer_height", ui.ctx().viewport_id()));
    let previous_height = footer_height(ui);
    let response = egui::Frame::NONE
        .fill(colors.footer)
        .inner_margin(egui::Margin {
            left: 22,
            right: 22,
            top: 12,
            bottom: 12,
        })
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(ACTION_GAP, ACTION_GAP);
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), BUTTON_HEIGHT),
                egui::Layout::right_to_left(egui::Align::Center).with_main_wrap(true),
                |ui| {
                    let result = actions(ui);
                    if let Some(hint) = hint {
                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width().max(0.0), BUTTON_HEIGHT),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(hint).size(11.0).color(colors.muted),
                                    )
                                    .truncate(),
                                );
                            },
                        );
                    }
                    result
                },
            )
            .inner
        });
    let height = response.response.rect.height() + 1.0;
    ui.ctx()
        .data_mut(|data| data.insert_temp(height_key, height));
    if (height - previous_height).abs() > 0.5 {
        ui.ctx().request_repaint();
    }
    response.inner
}

pub(super) fn footer_height(ui: &egui::Ui) -> f32 {
    let key = ui
        .id()
        .with(("popup_footer_height", ui.ctx().viewport_id()));
    ui.ctx()
        .data(|data| data.get_temp::<f32>(key))
        .unwrap_or(BUTTON_HEIGHT + 25.0)
}
