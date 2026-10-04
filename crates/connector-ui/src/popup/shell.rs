use super::{FIELD_GAP, divider, palette, palette_for};

pub struct PopupSpec<'a> {
    pub id: egui::Id,
    pub width: f32,
    pub title: &'a str,
    pub subtitle: &'a str,
    pub close_label: &'a str,
    pub close_enabled: bool,
}

#[derive(Clone, Copy)]
struct TargetState {
    target: egui::Id,
    slot: bool,
}

/// Keep one target and two reusable action scopes per case/viewport.
pub fn prepare_target(ctx: &egui::Context, case: egui::Id, target: egui::Id) -> egui::Id {
    let key = case.with(("target", ctx.viewport_id()));
    let (changed, slot) = ctx.data_mut(|data| {
        let previous = data.get_temp::<TargetState>(key);
        let changed = previous.is_none_or(|previous| previous.target != target);
        let slot = previous.is_some_and(|previous| previous.slot ^ changed);
        data.insert_temp(key, TargetState { target, slot });
        (changed, slot)
    });
    if changed {
        ctx.memory_mut(|memory| {
            if let Some(focused) = memory.focused() {
                memory.surrender_focus(focused);
            }
        });
    }
    case.with(("actions", slot))
}

/// Render the fixed header and modal frame. Width is capped to the viewport;
/// the caller supplies body and footer through the functions below.
pub fn show(
    ctx: &egui::Context,
    spec: PopupSpec<'_>,
    contents: impl FnOnce(&mut egui::Ui),
) -> bool {
    super::set_pending_modal(ctx, true);
    let width = spec.width.min((ctx.content_rect().width() - 34.0).max(1.0));
    let previous_size = ctx.memory(|memory| memory.area_rect(spec.id).map(|rect| rect.size()));
    let mut close_clicked = false;
    let area = egui::Modal::default_area(spec.id).default_width(width);
    let response = egui::Modal::new(spec.id)
        .area(area)
        .backdrop_color(egui::Color32::from_black_alpha(160))
        .frame(popover_frame(ctx))
        .show(ctx, |ui| {
            ui.set_width(width);
            apply_style(ui);
            close_clicked = header(ui, width, &spec);
            divider(ui);
            contents(ui)
        });
    // egui's anchored Area stores the new size at frame end but only repaints its
    // initial sizing pass. Settle the centered position after a form changes size
    // so the visible controls and the next pointer hit test use the same rects.
    if previous_size
        .is_some_and(|size| (size - response.response.rect.size()).abs().max_elem() > 0.5)
    {
        ctx.request_repaint();
    }
    let requested_close = close_clicked || response.should_close();
    spec.close_enabled && requested_close
}

/// Shared presentation only: anchored popovers retain egui's menu ownership.
pub fn popover(
    ui: &mut egui::Ui,
    spec: PopupSpec<'_>,
    contents: impl FnOnce(&mut egui::Ui),
) -> bool {
    let width = spec
        .width
        .min((ui.ctx().content_rect().width() - 34.0).max(1.0));
    ui.push_id(spec.id, |ui| {
        ui.set_width(width);
        apply_style(ui);
        let close = header(ui, width, &spec);
        divider(ui);
        contents(ui);
        close
    })
    .inner
}

pub fn popover_frame(ctx: &egui::Context) -> egui::Frame {
    let colors = palette_for(ctx.style_of(ctx.theme()).visuals.dark_mode);
    egui::Frame::NONE
        .fill(colors.surface)
        .stroke(egui::Stroke::new(1.0, colors.border))
        .corner_radius(egui::CornerRadius::same(3))
        .shadow(egui::epaint::Shadow {
            offset: [0, 20],
            blur: 60,
            spread: 0,
            color: egui::Color32::from_black_alpha(100),
        })
}

pub(super) fn apply_style(ui: &mut egui::Ui) {
    ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
    ui.style_mut().override_font_id = None;
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
}

pub(super) fn header(ui: &mut egui::Ui, width: f32, spec: &PopupSpec<'_>) -> bool {
    let colors = palette(ui);
    let mut close_clicked = false;
    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: 22,
            right: 22,
            top: 19,
            bottom: 16,
        })
        .show(ui, |ui| {
            ui.set_width((width - 44.0).max(1.0));
            let header_top = ui.cursor().min;
            let close_rect = egui::Rect::from_min_size(
                egui::pos2(ui.max_rect().right() - 24.0, header_top.y - 4.0),
                egui::vec2(30.0, 30.0),
            );
            ui.allocate_ui_with_layout(
                egui::vec2((ui.available_width() - 28.0).max(1.0), 23.4),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(spec.title)
                                .size(18.0)
                                .line_height(Some(23.4))
                                .strong()
                                .color(colors.text),
                        )
                        .wrap(),
                    );
                },
            );
            // Independent child: the 30pt close target must not enlarge the title row.
            let mut close_ui = ui.new_child(egui::UiBuilder::new().max_rect(close_rect).layout(
                egui::Layout::centered_and_justified(egui::Direction::TopDown),
            ));
            let close = close_ui
                .add_enabled(
                    spec.close_enabled,
                    egui::Button::new(egui::RichText::new("×").size(19.0).color(colors.muted))
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
            if !spec.subtitle.is_empty() {
                ui.add_space(7.0);
                ui.label(
                    egui::RichText::new(spec.subtitle)
                        .size(12.0)
                        .line_height(Some(18.0))
                        .color(colors.muted),
                );
            }
        });
    close_clicked
}

/// Content area stays scrollable while the header and footer remain visible.
pub fn body<T>(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui) -> T) -> T {
    body_with_max_height(ui, f32::INFINITY, contents)
}

pub fn body_with_max_height<T>(
    ui: &mut egui::Ui,
    limit: f32,
    contents: impl FnOnce(&mut egui::Ui) -> T,
) -> T {
    // A wrapped subtitle changes the header height. Reserve the actual header,
    // body margins, footer (button + padding + divider), frame and screen gutter.
    let header_height = ui.cursor().top() - ui.min_rect().top();
    let max_height = (ui.ctx().content_rect().height()
        - header_height
        - 39.0
        - super::actions::footer_height(ui)
        - 34.0)
        .max(1.0)
        .min(limit);
    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: 22,
            right: 22,
            top: 18,
            bottom: 21,
        })
        .show(ui, |ui| {
            // The anchored area's previous size must not limit a newly expanded
            // form; otherwise it grows a footer-height per frame before settling.
            ui.set_max_height(max_height);
            egui::ScrollArea::vertical()
                .max_height(max_height)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = FIELD_GAP;
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
                                    let _ = super::super::text_input(ui, &mut name, "");
                                },
                            );
                            super::super::notice(
                                ui,
                                "The destination folder already contains another repository and cannot be replaced.",
                                super::super::NoticeTone::Error,
                            );
                        });
                        super::super::footer(ui, None, |ui| {
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
        assert!(rect.top() >= 0.0 && rect.bottom() <= 360.0, "{rect:?}");
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
