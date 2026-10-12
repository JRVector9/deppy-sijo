//! Compact, title-free context menus. Callers own targets and effects.
use egui::{Atom, Button, InnerResponse, Response, Ui, WidgetText};

// 메뉴 폭은 고정 최소값 없이 가장 긴 항목에 맞춘다(2026-10-12 사용자: 짧은 메뉴에 빈 여백).
pub(crate) const ROW_GAP: f32 = 7.0;
const CORNER_RADIUS: u8 = 2;

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
    style.visuals.menu_corner_radius = egui::CornerRadius::same(CORNER_RADIUS);
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
    popup.show(contents)
}

pub(crate) fn scope<R>(ui: &mut Ui, contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    ui.scope(|ui| {
        compact_style(ui.style_mut());
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
        .ui(ui, contents);
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

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    fn open_menu(labels: &'static [&'static str]) -> (egui::Rect, Vec<egui::Rect>) {
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            let target = ui.add(egui::Label::new("target").sense(egui::Sense::click()));
            show(&target, |ui| {
                for label in labels {
                    let _ = button(ui, *label, Icon::Folder);
                }
            });
        });
        super::super::context_menu_audit::prepare(&harness.ctx);
        harness.run();
        let target = harness.get_by_label("target").rect().center();
        harness.event(egui::Event::PointerMoved(target));
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton {
                pos: target,
                button: egui::PointerButton::Secondary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
            harness.step();
        }
        harness.run();
        let menu = harness.ctx.memory(|memory| {
            let layer = memory
                .areas()
                .top_layer_id(egui::Order::Foreground)
                .expect("menu layer");
            memory.area_rect(layer.id).expect("menu bounds")
        });
        let rows = labels
            .iter()
            .map(|label| harness.get_by_label(label).rect())
            .collect();
        (menu, rows)
    }

    /// 2026-10-12 사용자: 짧은 메뉴에 고정 최소 폭(192pt)만큼 빈 여백이 남았다. 메뉴는 가장 긴
    /// 항목에 맞추고, 행 사이는 이전(3pt)보다 4pt 넓힌다.
    #[test]
    fn 메뉴_폭은_가장_긴_항목에_맞추고_행_간격을_넓힌다() {
        let (menu, rows) = open_menu(&["새 폴더"]);
        // 좌우 여백(menu_margin 4)과 테두리(1)만 남는다.
        assert!(
            (menu.width() - rows[0].width() - 10.0).abs() <= 1.0,
            "menu {menu:?} row {:?}",
            rows[0]
        );
        assert!(
            menu.width() < 150.0,
            "short menu kept a fixed width: {menu:?}"
        );

        let (wide, wide_rows) = open_menu(&["새 폴더", "파일 트리를 이 폴더로 이동"]);
        assert!(wide.width() > menu.width());
        assert!(wide_rows.iter().all(|row| wide.contains_rect(*row)));
        let gap = wide_rows[1].top() - wide_rows[0].bottom();
        assert!((gap - ROW_GAP).abs() < 0.5, "row gap {gap}");
        assert_eq!(ROW_GAP, 7.0);
    }

    #[test]
    fn 메뉴_모서리는_2pt로_둥글린다() {
        let mut style = egui::Style::default();
        compact_style(&mut style);
        assert_eq!(
            style.visuals.menu_corner_radius,
            egui::CornerRadius::same(2)
        );
    }
}
