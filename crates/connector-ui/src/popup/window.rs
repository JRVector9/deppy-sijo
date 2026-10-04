use super::{PopupSpec, shell};

pub struct WindowSpec<'a> {
    pub id: egui::Id,
    pub title: &'a str,
    pub subtitle: &'a str,
    pub close_label: &'a str,
    pub close_enabled: bool,
    pub default_size: egui::Vec2,
    pub min_size: egui::Vec2,
}

#[derive(Clone, Copy)]
struct WindowGeometry {
    last_frame: u64,
}

/// A stable, centered-on-open window; movement/resize remain native egui behavior.
/// The caller owns drafts, operations, input policy and Escape handling.
pub fn window<R>(
    ctx: &egui::Context,
    spec: WindowSpec<'_>,
    open: &mut bool,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> Option<egui::InnerResponse<Option<R>>> {
    if !*open {
        return None;
    }
    let viewport = ctx.content_rect().shrink(16.0);
    let max_size = viewport.size().max(egui::vec2(1.0, 1.0));
    let min_size = spec.min_size.min(max_size);
    let key = spec.id.with(("window_geometry", ctx.viewport_id()));
    let frame = ctx.cumulative_frame_nr();
    let previous = ctx.data(|data| data.get_temp::<WindowGeometry>(key));
    let opening = previous.is_none_or(|old| old.last_frame.saturating_add(1) < frame);
    let mut native = egui::Window::new(spec.title)
        .id(spec.id)
        .title_bar(false)
        .collapsible(false)
        .resizable(true)
        .default_size(spec.default_size.clamp(min_size, max_size))
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(viewport.center())
        .min_size(min_size)
        .max_size(max_size)
        .constrain_to(viewport)
        .frame(super::popover_frame(ctx));
    if opening {
        native = native.current_pos(viewport.center());
    }
    let mut close = false;
    let result = native.show(ctx, |ui| {
        shell::apply_style(ui);
        let width = ui.available_width();
        close = shell::header(
            ui,
            width,
            &PopupSpec {
                id: spec.id,
                width,
                title: spec.title,
                subtitle: spec.subtitle,
                close_label: spec.close_label,
                close_enabled: spec.close_enabled,
            },
        );
        super::divider(ui);
        contents(ui)
    });
    if result.is_some() {
        ctx.data_mut(|data| data.insert_temp(key, WindowGeometry { last_frame: frame }));
    }
    if close {
        *open = false;
    }
    result
}

/// Reserve measured footer height inside a user-resized window, not the whole screen.
pub fn window_body<R>(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let height = (ui.available_height() - super::actions::footer_height(ui) - 39.0).max(1.0);
    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: 22,
            right: 22,
            top: 18,
            bottom: 21,
        })
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("resizable_popup_body")
                .max_height(height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = super::FIELD_GAP;
                    contents(ui)
                })
                .inner
        })
        .inner
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;
    struct State {
        open: bool,
        rect: egui::Rect,
        text: String,
    }
    fn draw(ui: &mut egui::Ui, state: &mut State) {
        if let Some(response) = window(
            ui.ctx(),
            WindowSpec {
                id: egui::Id::new("centered_resize_test"),
                title: "Next task",
                subtitle: "",
                close_label: "Close",
                close_enabled: true,
                default_size: egui::vec2(600.0, 420.0),
                min_size: egui::vec2(300.0, 250.0),
            },
            &mut state.open,
            |ui| {
                window_body(ui, |ui| {
                    super::super::text_input(ui, &mut state.text, "Task");
                });
                super::super::footer(ui, None, |ui| {
                    super::super::action_button(
                        ui,
                        "Reserve",
                        super::super::ActionTone::Primary,
                        true,
                    );
                });
            },
        ) {
            state.rect = response.response.rect;
        }
    }
    #[test]
    fn window_moves_then_reopens_centered_with_preserved_draft() {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 800.0))
            .build_ui_state(
                draw,
                State {
                    open: true,
                    rect: egui::Rect::NOTHING,
                    text: "original".into(),
                },
            );
        harness.run();
        assert!((harness.state().rect.center() - egui::pos2(500.0, 400.0)).length() < 1.0);
        let start = harness.state().rect.min + egui::vec2(150.0, 25.0);
        for (pos, press) in [
            (start, Some(true)),
            (start + egui::vec2(70.0, 40.0), None),
            (start + egui::vec2(70.0, 40.0), Some(false)),
        ] {
            harness
                .input_mut()
                .events
                .push(egui::Event::PointerMoved(pos));
            if let Some(pressed) = press {
                harness.input_mut().events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                });
            }
            harness.step();
        }
        harness.run();
        assert!((harness.state().rect.center() - egui::pos2(500.0, 400.0)).length() > 20.0);
        harness.state_mut().open = false;
        harness.step();
        harness.step();
        harness.state_mut().open = true;
        harness.run();
        assert!((harness.state().rect.center() - egui::pos2(500.0, 400.0)).length() < 1.0);
        assert_eq!(harness.state().text, "original");
    }
    #[test]
    fn native_resize_preserves_size_on_reopen() {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1100.0, 900.0))
            .build_ui_state(
                draw,
                State {
                    open: true,
                    rect: egui::Rect::NOTHING,
                    text: "kept".into(),
                },
            );
        harness.run();
        let before = harness.state().rect;
        let start = before.max - egui::vec2(2.0, 2.0);
        for (pos, press) in [
            (start, Some(true)),
            (start + egui::vec2(80.0, 60.0), None),
            (start + egui::vec2(80.0, 60.0), Some(false)),
        ] {
            harness
                .input_mut()
                .events
                .push(egui::Event::PointerMoved(pos));
            if let Some(pressed) = press {
                harness.input_mut().events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                });
            }
            harness.step();
        }
        harness.run();
        let resized = harness.state().rect.size();
        assert!(resized.x > before.width() + 40.0 && resized.y > before.height() + 30.0);
        harness.state_mut().open = false;
        harness.step();
        harness.step();
        harness.state_mut().open = true;
        harness.run();
        assert!((harness.state().rect.size() - resized).length() < 2.0);
        assert!((harness.state().rect.center() - egui::pos2(550.0, 450.0)).length() < 1.0);
    }

    #[test]
    fn narrow_window_keeps_reserve_button_inside_viewport() {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(380.0, 320.0))
            .build_ui_state(
                draw,
                State {
                    open: true,
                    rect: egui::Rect::NOTHING,
                    text: String::new(),
                },
            );
        harness.run();
        assert!(
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(380.0, 320.0))
                .contains_rect(harness.get_by_label("Reserve").rect())
        );
    }
}
