//! Compact, title-free context menus. Callers own targets and effects.
use egui::{Atom, Button, InnerResponse, Response, Ui, WidgetText};

pub(crate) const MIN_WIDTH: f32 = 192.0;
pub(crate) const ROW_GAP: f32 = 3.0;

#[derive(Clone, Copy)]
pub(crate) enum Icon {
    Copy,
    Paste,
    File,
    Folder,
    Edit,
    Plus,
    Close,
    Trash,
    Send,
    Settings,
    Split,
    Bottom,
    Clock,
    Terminal,
}

fn compact_style(style: &mut egui::Style) {
    egui::containers::menu::menu_style(style);
    style.wrap_mode = Some(egui::TextWrapMode::Extend);
    style.spacing.item_spacing = egui::vec2(6.0, ROW_GAP);
    style.spacing.menu_margin = egui::Margin::same(4);
}

pub(crate) fn show<R>(
    response: &Response,
    contents: impl FnOnce(&mut Ui) -> R,
) -> Option<InnerResponse<R>> {
    let mut popup = egui::Popup::context_menu(response).style(compact_style);
    // DnD/header backdrops may only sense hover. Explicit secondary-click routing
    // opens their context menu without adding a click layer over primary controls.
    if !response.sense.senses_click()
        && response.contains_pointer()
        && response
            .ctx
            .input(|input| input.pointer.button_clicked(egui::PointerButton::Secondary))
    {
        popup = popup.open_memory(Some(egui::SetOpenCommand::Bool(true)));
    }
    popup.show(|ui| {
        ui.set_min_width(MIN_WIDTH);
        contents(ui)
    })
}

pub(crate) fn scope<R>(ui: &mut Ui, contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    ui.scope(|ui| {
        compact_style(ui.style_mut());
        ui.set_min_width(MIN_WIDTH);
        contents(ui)
    })
}

fn icon_atom(ui: &Ui) -> Atom<'static> {
    Atom::custom(
        ui.next_auto_id().with("context_icon"),
        egui::Vec2::splat(14.0),
    )
}

pub(crate) fn button(ui: &mut Ui, text: impl Into<WidgetText>, icon: Icon) -> Response {
    let response = ui.add(Button::new((icon_atom(ui), text.into())));
    paint_icon(ui, &response, icon, false);
    response
}

pub(crate) fn enabled_button(
    ui: &mut Ui,
    enabled: bool,
    text: impl Into<WidgetText>,
    icon: Icon,
) -> Response {
    ui.add_enabled_ui(enabled, |ui| button(ui, text, icon))
        .inner
}

pub(crate) fn danger_button(ui: &mut Ui, text: impl Into<WidgetText>, icon: Icon) -> Response {
    ui.scope(|ui| {
        let error = super::designall::tokens(ui.visuals()).error;
        ui.visuals_mut().override_text_color = Some(error);
        ui.visuals_mut().widgets.hovered.weak_bg_fill = error.gamma_multiply(0.14);
        let response = ui.add(Button::new((icon_atom(ui), text.into())));
        paint_icon(ui, &response, icon, true);
        response
    })
    .inner
}

pub(crate) fn submenu<R>(
    ui: &mut Ui,
    text: impl Into<WidgetText>,
    icon: Icon,
    contents: impl FnOnce(&mut Ui) -> R,
) {
    let (response, _) = egui::containers::menu::SubMenuButton::new((icon_atom(ui), text.into()))
        .config(egui::containers::menu::MenuConfig::new().style(compact_style))
        .ui(ui, |ui| {
            ui.set_min_width(MIN_WIDTH);
            contents(ui)
        });
    paint_icon(ui, &response, icon, false);
}

fn paint_icon(ui: &Ui, response: &Response, icon: Icon, danger: bool) {
    let color = if danger {
        super::designall::tokens(ui.visuals()).error
    } else {
        ui.style().interact(response).text_color()
    };
    let stroke = egui::Stroke::new(1.1, color);
    let origin = egui::pos2(
        response.rect.left() + ui.spacing().button_padding.x,
        response.rect.center().y - 7.0,
    );
    let point = |x: f32, y: f32| origin + egui::vec2(x, y) * 0.7;
    let line = |a: (f32, f32), b: (f32, f32)| {
        ui.painter()
            .line_segment([point(a.0, a.1), point(b.0, b.1)], stroke);
    };
    let rect = |x: f32, y: f32, w: f32, h: f32| {
        ui.painter().rect_stroke(
            egui::Rect::from_min_max(point(x, y), point(x + w, y + h)),
            1,
            stroke,
            egui::StrokeKind::Inside,
        );
    };
    match icon {
        Icon::Copy => {
            rect(7.0, 7.0, 10.0, 10.0);
            line((13.0, 7.0), (13.0, 3.0));
            line((13.0, 3.0), (3.0, 3.0));
            line((3.0, 3.0), (3.0, 13.0));
            line((3.0, 13.0), (7.0, 13.0));
        }
        Icon::Paste => {
            rect(3.0, 4.0, 14.0, 14.0);
            rect(6.0, 2.0, 8.0, 4.0);
            line((6.0, 10.0), (14.0, 10.0));
            line((6.0, 14.0), (12.0, 14.0));
        }
        Icon::Folder => {
            for (a, b) in [
                ((2.0, 6.0), (2.0, 4.0)),
                ((2.0, 4.0), (8.0, 4.0)),
                ((8.0, 4.0), (10.0, 6.0)),
                ((10.0, 6.0), (18.0, 6.0)),
                ((18.0, 6.0), (18.0, 17.0)),
                ((18.0, 17.0), (2.0, 17.0)),
                ((2.0, 17.0), (2.0, 6.0)),
                ((2.0, 9.0), (18.0, 9.0)),
            ] {
                line(a, b);
            }
        }
        Icon::File => {
            for (a, b) in [
                ((4.0, 2.0), (12.0, 2.0)),
                ((12.0, 2.0), (16.0, 6.0)),
                ((16.0, 6.0), (16.0, 18.0)),
                ((16.0, 18.0), (4.0, 18.0)),
                ((4.0, 18.0), (4.0, 2.0)),
                ((12.0, 2.0), (12.0, 6.0)),
                ((12.0, 6.0), (16.0, 6.0)),
                ((7.0, 10.0), (13.0, 10.0)),
                ((7.0, 14.0), (11.0, 14.0)),
            ] {
                line(a, b);
            }
        }
        Icon::Edit => {
            for (a, b) in [
                ((4.0, 12.0), (13.0, 3.0)),
                ((13.0, 3.0), (17.0, 7.0)),
                ((17.0, 7.0), (8.0, 16.0)),
                ((8.0, 16.0), (3.0, 17.0)),
                ((3.0, 17.0), (4.0, 12.0)),
                ((11.0, 5.0), (15.0, 9.0)),
            ] {
                line(a, b);
            }
        }
        Icon::Plus => {
            line((10.0, 3.0), (10.0, 17.0));
            line((3.0, 10.0), (17.0, 10.0));
        }
        Icon::Close => {
            line((5.0, 5.0), (15.0, 15.0));
            line((15.0, 5.0), (5.0, 15.0));
        }
        Icon::Trash => {
            for (a, b) in [
                ((3.0, 5.0), (17.0, 5.0)),
                ((7.0, 5.0), (7.0, 2.0)),
                ((7.0, 2.0), (13.0, 2.0)),
                ((13.0, 2.0), (13.0, 5.0)),
                ((5.0, 5.0), (6.0, 18.0)),
                ((6.0, 18.0), (14.0, 18.0)),
                ((14.0, 18.0), (15.0, 5.0)),
                ((8.0, 8.0), (8.0, 15.0)),
                ((12.0, 8.0), (12.0, 15.0)),
            ] {
                line(a, b);
            }
        }
        Icon::Send => {
            for (a, b) in [
                ((2.0, 3.0), (18.0, 10.0)),
                ((18.0, 10.0), (2.0, 17.0)),
                ((2.0, 17.0), (5.0, 10.0)),
                ((5.0, 10.0), (2.0, 3.0)),
                ((5.0, 10.0), (18.0, 10.0)),
            ] {
                line(a, b);
            }
        }
        Icon::Settings => {
            for (y, x) in [(5.0, 7.0), (10.0, 13.0), (15.0, 8.0)] {
                line((3.0, y), (17.0, y));
                ui.painter()
                    .circle(point(x, y), 1.4, ui.visuals().window_fill(), stroke);
            }
        }
        Icon::Split => {
            rect(2.0, 3.0, 16.0, 14.0);
            line((10.0, 3.0), (10.0, 17.0));
        }
        Icon::Bottom => {
            line((10.0, 3.0), (10.0, 16.0));
            line((5.0, 11.0), (10.0, 16.0));
            line((15.0, 11.0), (10.0, 16.0));
            line((4.0, 18.0), (16.0, 18.0));
        }
        Icon::Clock => {
            ui.painter().circle_stroke(point(10.0, 10.0), 5.6, stroke);
            line((10.0, 5.0), (10.0, 10.0));
            line((10.0, 10.0), (14.0, 12.0));
        }
        Icon::Terminal => {
            rect(2.0, 3.0, 16.0, 14.0);
            line((5.0, 7.0), (8.0, 10.0));
            line((8.0, 10.0), (5.0, 13.0));
            line((10.0, 13.0), (14.0, 13.0));
        }
    }
}
