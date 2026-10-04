use super::palette;

#[derive(Clone, Copy)]
pub enum NoticeTone {
    Info,
    Error,
}

pub fn notice(ui: &mut egui::Ui, text: &str, tone: NoticeTone) {
    let colors = palette(ui);
    let (fill, border, text_color, icon) = match tone {
        NoticeTone::Info => (colors.accent_low, colors.info_line, colors.info_text, "ⓘ"),
        NoticeTone::Error => (colors.error_low, colors.error_line, colors.error_text, "!"),
    };
    let inner_width = (ui.available_width() - 26.0).max(1.0);
    egui::Frame::NONE
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, border))
        .corner_radius(egui::CornerRadius::same(3))
        .inner_margin(egui::Margin::symmetric(12, 11))
        .show(ui, |ui| {
            ui.set_width(inner_width);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 9.0;
                ui.label(egui::RichText::new(icon).size(15.0).color(text_color));
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(text)
                            .size(12.0)
                            .line_height(Some(18.0))
                            .color(text_color),
                    )
                    .wrap(),
                );
            });
        });
}
