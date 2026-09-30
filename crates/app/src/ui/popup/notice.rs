use super::palette;

#[derive(Clone, Copy)]
pub enum NoticeTone {
    Info,
    Error,
}

pub fn notice(ui: &mut egui::Ui, text: &str, tone: NoticeTone) {
    let colors = palette(ui);
    let (fill, border, text_color, icon) = match tone {
        NoticeTone::Info => (colors.accent_low, colors.accent, colors.text, "ⓘ"),
        NoticeTone::Error => (colors.error_low, colors.error, colors.error, "!"),
    };
    egui::Frame::NONE
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, border))
        .corner_radius(egui::CornerRadius::same(3))
        .inner_margin(egui::Margin::symmetric(12, 11))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 9.0;
                ui.label(egui::RichText::new(icon).size(15.0).color(text_color));
                ui.add(
                    egui::Label::new(egui::RichText::new(text).size(12.0).color(text_color)).wrap(),
                );
            });
        });
}
