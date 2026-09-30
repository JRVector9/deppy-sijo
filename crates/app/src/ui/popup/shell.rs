use super::{palette, palette_for};

pub struct PopupSpec<'a> {
    pub id: egui::Id,
    pub width: f32,
    pub title: &'a str,
    pub subtitle: &'a str,
    pub close_label: &'a str,
    pub close_enabled: bool,
}

/// Render the fixed header and modal frame. Width is capped to the viewport;
/// the caller supplies body and footer through the functions below.
pub fn show(
    ctx: &egui::Context,
    spec: PopupSpec<'_>,
    contents: impl FnOnce(&mut egui::Ui),
) -> bool {
    let width = spec.width.min((ctx.content_rect().width() - 34.0).max(1.0));
    let frame_colors = palette_for(ctx.style_of(ctx.theme()).visuals.dark_mode);
    let mut close_clicked = false;
    let area = egui::Modal::default_area(spec.id).default_width(width);
    let response = egui::Modal::new(spec.id)
        .area(area)
        .backdrop_color(egui::Color32::from_black_alpha(160))
        .frame(
            egui::Frame::NONE
                .fill(frame_colors.surface)
                .stroke(egui::Stroke::new(1.0, frame_colors.border))
                .corner_radius(egui::CornerRadius::same(3))
                .shadow(egui::epaint::Shadow {
                    offset: [0, 20],
                    blur: 60,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(100),
                }),
        )
        .show(ctx, |ui| {
            ui.set_width(width);
            let colors = palette(ui);
            ui.style_mut()
                .text_styles
                .insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
            ui.style_mut()
                .text_styles
                .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
            let visuals = ui.visuals_mut();
            visuals.override_text_color = Some(colors.text);
            visuals.weak_text_color = Some(colors.muted);
            visuals.extreme_bg_color = colors.input;
            visuals.selection.bg_fill = colors.accent;
            visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, colors.border);
            visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(3);
            egui::Frame::NONE
                .inner_margin(egui::Margin {
                    left: 22,
                    right: 22,
                    top: 19,
                    bottom: 16,
                })
                .show(ui, |ui| {
                    ui.set_width((width - 44.0).max(1.0));
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(spec.title)
                                .size(18.0)
                                .strong()
                                .color(colors.text),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let close = ui
                                .add_enabled(
                                    spec.close_enabled,
                                    egui::Button::new(
                                        egui::RichText::new("×").size(19.0).color(colors.muted),
                                    )
                                    .frame(false)
                                    .min_size(egui::vec2(30.0, 30.0)),
                                )
                                .on_hover_text(spec.close_label);
                            close.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    spec.close_enabled,
                                    spec.close_label,
                                )
                            });
                            close_clicked = close.clicked();
                        });
                    });
                    if !spec.subtitle.is_empty() {
                        ui.add_space(7.0);
                        ui.label(
                            egui::RichText::new(spec.subtitle)
                                .size(12.0)
                                .color(colors.muted),
                        );
                    }
                });
            crate::ui::hairline_colored(ui, colors.line);
            contents(ui)
        });
    let requested_close = close_clicked || response.should_close();
    spec.close_enabled && requested_close
}

/// Content area stays scrollable while the header and footer remain visible.
pub fn body<T>(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui) -> T) -> T {
    let max_height = (ui.ctx().content_rect().height() - 175.0).max(160.0);
    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: 22,
            right: 22,
            top: 18,
            bottom: 21,
        })
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(max_height)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 12.0;
                    contents(ui)
                })
                .inner
        })
        .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_stays_inside_a_narrow_viewport() {
        let mut name = String::new();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(280.0, 360.0))
            .build_ui(move |ui| {
                show(
                    ui.ctx(),
                    PopupSpec {
                        id: egui::Id::new("narrow_popup"),
                        width: 560.0,
                        title: "Create folder",
                        subtitle: "Choose a name",
                        close_label: "Close",
                        close_enabled: true,
                    },
                    |ui| {
                        body(ui, |ui| {
                            super::super::field(
                                ui,
                                "Folder name",
                                Some("Location: /very/long/project/path/without/spaces/assets"),
                                |ui| {
                                    let _ = ui.text_edit_singleline(&mut name);
                                },
                            );
                            super::super::notice(
                                ui,
                                "The destination folder already contains another repository and cannot be replaced.",
                                super::super::NoticeTone::Error,
                            );
                        });
                        super::super::footer(ui, |ui| {
                            let _ = ui.button("Create");
                        });
                    },
                );
            });
        harness.run();
        let rect = harness
            .ctx
            .memory(|memory| memory.area_rect(egui::Id::new("narrow_popup")))
            .expect("popup should be visible");
        assert!(rect.left() >= 0.0 && rect.right() <= 280.0, "{rect:?}");
    }

    #[test]
    fn disabled_close_consumes_escape_before_it_reaches_the_screen_behind() {
        let mut harness = egui_kittest::Harness::builder().build_ui_state(
            |ui, escaped_behind: &mut bool| {
                assert!(!show(
                    ui.ctx(),
                    PopupSpec {
                        id: egui::Id::new("busy_popup"),
                        width: 420.0,
                        title: "Busy",
                        subtitle: "Please wait",
                        close_label: "Close",
                        close_enabled: false,
                    },
                    |ui| {
                        body(ui, |ui| {
                            ui.label("Working");
                        });
                    },
                ));
                *escaped_behind = ui
                    .ctx()
                    .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            },
            false,
        );
        harness.run();
        harness.event(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
        assert!(!harness.state());
    }
}
