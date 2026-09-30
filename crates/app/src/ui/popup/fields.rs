use super::palette;

pub fn field<T>(
    ui: &mut egui::Ui,
    label: &str,
    hint: Option<&str>,
    control: impl FnOnce(&mut egui::Ui) -> T,
) -> T {
    let colors = palette(ui);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 5.0;
        ui.label(egui::RichText::new(label).size(12.0).color(colors.text));
        let result = ui.scope(|ui| {
            ui.spacing_mut().interact_size.y = 36.0;
            control(ui)
        });
        if let Some(hint) = hint {
            ui.label(egui::RichText::new(hint).size(11.0).color(colors.muted));
        }
        result.inner
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
        .inner_margin(2)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let width = (ui.available_width() - 2.0) / 2.0;
                for (value, label) in choices {
                    if ui
                        .add_sized(
                            [width, 34.0],
                            egui::Button::new(label).selected(*selected == value),
                        )
                        .clicked()
                    {
                        *selected = value;
                    }
                }
            });
        });
}
