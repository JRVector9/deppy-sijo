use super::{INPUT_HEIGHT, palette};

/// Same height, border and type scale as a shared text input.
pub fn choice_input<R>(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    selected: &str,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<Option<R>> {
    ui.scope(|ui| {
        let colors = palette(ui);
        ui.spacing_mut().interact_size.y = INPUT_HEIGHT;
        ui.spacing_mut().button_padding = egui::vec2(10.0, 8.0);
        let widgets = &mut ui.visuals_mut().widgets;
        for visual in [
            &mut widgets.inactive,
            &mut widgets.hovered,
            &mut widgets.active,
        ] {
            visual.bg_fill = colors.input;
            visual.weak_bg_fill = colors.input;
            visual.bg_stroke = egui::Stroke::new(1.0, colors.border);
            visual.corner_radius = egui::CornerRadius::same(3);
            visual.expansion = 0.0;
        }
        let response = egui::ComboBox::from_id_salt(id)
            .selected_text(egui::RichText::new(selected).size(13.0))
            .width(ui.available_width())
            .show_ui(ui, contents);
        // Popup handles closing before returning, but leaves Esc in the input
        // batch. A chooser rendered this pass owns it ahead of its parent modal.
        if response.inner.is_some() {
            ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        }
        response
    })
    .inner
}

/// A complete input control, not just a preferred size on the parent Ui.
pub fn text_input(ui: &mut egui::Ui, draft: &mut String, hint: &str) -> egui::Response {
    ui.scope(|ui| {
        let colors = palette(ui);
        ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
        let widgets = &mut ui.visuals_mut().widgets;
        for visuals in [&mut widgets.inactive, &mut widgets.hovered] {
            visuals.bg_stroke = egui::Stroke::new(1.0, colors.border);
            visuals.corner_radius = egui::CornerRadius::same(3);
            visuals.expansion = 0.0;
        }
        widgets.active.corner_radius = egui::CornerRadius::same(3);
        widgets.active.expansion = 0.0;
        ui.visuals_mut().selection.stroke = egui::Stroke::new(1.0, colors.accent);
        ui.add_sized(
            [ui.available_width(), INPUT_HEIGHT],
            egui::TextEdit::singleline(draft)
                .font(egui::FontId::proportional(13.0))
                .margin(egui::Margin::symmetric(10, 8))
                .background_color(colors.input)
                .text_color(colors.text)
                .vertical_align(egui::Align::Center)
                .hint_text(hint),
        )
    })
    .inner
}

pub struct PathInputResponse {
    pub input: egui::Response,
    pub browse: egui::Response,
}

/// Same input and action components, with the path occupying the remaining row.
pub fn path_input(ui: &mut egui::Ui, draft: &mut String, browse_label: &str) -> PathInputResponse {
    let button_width = super::actions::button_width(ui, browse_label);
    if ui.available_width() < button_width + 7.0 + 24.0 {
        let input = text_input(ui, draft, "");
        let browse = ui
            .with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                super::action_button(ui, browse_label, super::ActionTone::Secondary, true)
            })
            .inner;
        return PathInputResponse { input, browse };
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        let input_width = (ui.available_width() - button_width - 7.0).max(24.0);
        let input = ui
            .allocate_ui_with_layout(
                egui::vec2(input_width, INPUT_HEIGHT),
                egui::Layout::top_down(egui::Align::Min),
                |ui| text_input(ui, draft, ""),
            )
            .inner;
        let browse = super::action_button(ui, browse_label, super::ActionTone::Secondary, true);
        PathInputResponse { input, browse }
    })
    .inner
}

pub fn field<T>(
    ui: &mut egui::Ui,
    label: &str,
    hint: Option<&str>,
    control: impl FnOnce(&mut egui::Ui) -> T,
) -> T {
    let colors = palette(ui);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(
            egui::RichText::new(label)
                .size(12.0)
                .strong()
                .line_height(Some(17.4))
                .color(colors.text),
        );
        ui.add_space(7.0);
        let result = control(ui);
        if let Some(hint) = hint {
            ui.add_space(6.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(hint)
                        .size(11.0)
                        .line_height(Some(16.0))
                        .color(colors.muted),
                )
                .wrap(),
            );
        }
        result
    })
    .inner
}

pub fn segmented_choice<T: Copy + Eq>(
    ui: &mut egui::Ui,
    selected: &mut T,
    choices: [(T, &str); 2],
) {
    let colors = palette(ui);
    egui::Frame::NONE
        .fill(colors.input)
        .stroke(egui::Stroke::new(1.0, colors.line))
        .corner_radius(egui::CornerRadius::same(3))
        .inner_margin(3)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                ui.spacing_mut().interact_size.y = 31.0;
                let width = (ui.available_width() - 3.0) / 2.0;
                for (value, label) in choices {
                    let is_selected = *selected == value;
                    let response = ui.add_sized(
                        [width, 31.0],
                        egui::Button::new(egui::RichText::new(label).size(13.0).color(
                            if is_selected {
                                colors.text
                            } else {
                                colors.muted
                            },
                        ))
                        .selected(is_selected)
                        .fill(if is_selected {
                            colors.button
                        } else {
                            egui::Color32::TRANSPARENT
                        })
                        .stroke(egui::Stroke::new(
                            1.0,
                            if is_selected {
                                colors.line
                            } else {
                                egui::Color32::TRANSPARENT
                            },
                        ))
                        .corner_radius(egui::CornerRadius::same(2)),
                    );
                    if response.has_focus() {
                        ui.painter().rect_stroke(
                            response.rect,
                            2,
                            egui::Stroke::new(2.0, colors.accent),
                            egui::StrokeKind::Inside,
                        );
                    } else if response.hovered() {
                        ui.painter().rect_stroke(
                            response.rect,
                            2,
                            egui::Stroke::new(1.0, colors.line),
                            egui::StrokeKind::Inside,
                        );
                    }
                    if response.clicked() {
                        *selected = value;
                    }
                }
            });
        });
}
