/// Reusable list surface; callers retain row contents and action ownership.
pub fn list_row<T>(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui) -> T) -> T {
    let colors = super::palette(ui);
    let width = (ui.available_width() - 24.0).max(1.0);
    egui::Frame::NONE
        .fill(colors.input)
        .stroke(egui::Stroke::new(1.0, colors.line))
        .corner_radius(3)
        .inner_margin(egui::Margin::symmetric(11, 10))
        .show(ui, |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
            contents(ui)
        })
        .inner
}

/// A wrapping action row with a measured minimum height, independent of scroll space.
pub fn list_actions<T>(ui: &mut egui::Ui, actions: impl FnOnce(&mut egui::Ui) -> T) -> T {
    ui.spacing_mut().item_spacing = egui::vec2(super::ACTION_GAP, super::ACTION_GAP);
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), super::BUTTON_HEIGHT),
        egui::Layout::right_to_left(egui::Align::Center).with_main_wrap(true),
        actions,
    )
    .inner
}
