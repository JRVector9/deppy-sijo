use super::palette;

#[derive(Clone, Copy)]
pub enum ActionTone {
    Primary,
    Secondary,
}

pub fn action_button(
    ui: &mut egui::Ui,
    label: &str,
    tone: ActionTone,
    enabled: bool,
) -> egui::Response {
    let colors = palette(ui);
    let (fill, stroke, text) = match tone {
        ActionTone::Primary => (colors.accent, colors.accent, colors.input),
        ActionTone::Secondary => (colors.button, colors.line, colors.text),
    };
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(label).size(13.0).color(text))
            .fill(fill)
            .stroke(egui::Stroke::new(1.0, stroke))
            .corner_radius(egui::CornerRadius::same(3))
            .min_size(egui::vec2(0.0, 34.0)),
    )
}

pub fn footer<T>(ui: &mut egui::Ui, actions: impl FnOnce(&mut egui::Ui) -> T) -> T {
    let colors = palette(ui);
    crate::ui::hairline_colored(ui, colors.line);
    egui::Frame::NONE
        .fill(colors.footer)
        .inner_margin(egui::Margin {
            left: 22,
            right: 22,
            top: 12,
            bottom: 12,
        })
        .show(ui, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), actions)
                .inner
        })
        .inner
}
