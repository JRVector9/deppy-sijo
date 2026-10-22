//! Settings geometry shared with the approved HTML; controller-free UI helpers.
use crate::ui::settings::{SETTINGS_DETAIL as METRICS, SETTINGS_TYPE as TYPE};
use egui::{Color32, FontId, RichText, Stroke, Ui};

pub(super) const CONTROL_HEIGHT: f32 = METRICS.control_height;
pub(super) const RADIUS: u8 = 5;
pub(super) const PERMISSION_COLUMN: f32 = 88.0;
pub(super) const CONTROL_COLUMN: f32 = 132.0;
pub(super) const SESSION_ROW_HEIGHT: f32 = 48.0;
pub(super) const SESSION_TABLE_MIN: f32 = 660.0;
// HTML의 전체 viewport 대신 사이드바를 제외한 설정 본문 폭을 사용한다.
pub(super) const COMPACT_WIDTH: f32 = 600.0;
pub(super) const MEDIUM_WIDTH: f32 = 800.0;

/// Approved settings palette; shared typography and geometry stay inherited.
pub(super) fn apply_palette(ui: &mut Ui) {
    let dark = ui.visuals().dark_mode;
    let rgb = |dark_rgb: [u8; 3], light_rgb: [u8; 3]| {
        let c = if dark { dark_rgb } else { light_rgb };
        Color32::from_rgb(c[0], c[1], c[2])
    };
    let bg = rgb([22, 25, 30], [249, 250, 251]);
    let panel = rgb([27, 30, 37], [241, 242, 245]);
    let surface = rgb([31, 34, 41], [238, 239, 242]);
    let hover = rgb([37, 42, 51], [229, 231, 235]);
    let selected = rgb([33, 45, 56], [226, 237, 243]);
    let line = rgb([52, 56, 64], [188, 193, 204]);
    let text = rgb([220, 222, 226], [35, 38, 44]);
    let accent = rgb([57, 184, 232], [8, 113, 142]);
    ui.spacing_mut().icon_width = 16.0;
    ui.spacing_mut().button_padding = egui::vec2(12.0, 5.0);
    let v = ui.visuals_mut();
    v.override_text_color = Some(text);
    v.extreme_bg_color = bg;
    v.faint_bg_color = panel;
    v.hyperlink_color = accent;
    v.selection.bg_fill = selected;
    v.selection.stroke = Stroke::new(1.0, accent);
    v.widgets.noninteractive.bg_fill = surface;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, line);
    for (widget, fill) in [
        (&mut v.widgets.inactive, surface),
        (&mut v.widgets.hovered, hover),
        (&mut v.widgets.active, selected),
    ] {
        widget.corner_radius = egui::CornerRadius::same(RADIUS);
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = Stroke::new(1.0, line);
        widget.fg_stroke = Stroke::new(1.0, text);
    }
}
pub(super) fn secondary(ui: &Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(175, 181, 191)
    } else {
        Color32::from_rgb(83, 91, 105)
    }
}
pub(super) fn accent(ui: &Ui) -> Color32 {
    ui.visuals().hyperlink_color
}
pub(super) fn good(ui: &Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(120, 214, 173)
    } else {
        Color32::from_rgb(23, 108, 67)
    }
}
pub(super) fn warn(ui: &Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(232, 181, 105)
    } else {
        Color32::from_rgb(130, 83, 11)
    }
}
pub(super) fn hint(ui: &mut Ui, text: impl Into<String>) -> egui::Response {
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .size(TYPE.row_description)
                .color(secondary(ui)),
        )
        .wrap(),
    )
}
pub(super) fn title(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text).size(TYPE.section_title).strong());
}
pub(super) fn detail_title(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text).size(TYPE.page_title).strong());
}
pub(super) fn button(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).size(TYPE.control))
            .min_size(egui::vec2(0.0, METRICS.control_height)),
    )
}
/// TextEdit::min_size ignores its y component in egui 0.36.1.
/// A justified allocation sizes the frame and its actual hit area together.
pub(super) fn text_input(ui: &mut Ui, width: f32, edit: egui::TextEdit<'_>) -> egui::Response {
    ui.add_sized(egui::vec2(width, CONTROL_HEIGHT), edit)
}
pub(super) fn primary(ui: &mut Ui, text: &str) -> egui::Response {
    let color = if ui.visuals().dark_mode {
        Color32::from_rgb(16, 23, 28)
    } else {
        Color32::WHITE
    };
    ui.add(
        egui::Button::new(RichText::new(text).size(TYPE.control).color(color).strong())
            .fill(accent(ui))
            .stroke(Stroke::new(1.0, accent(ui)))
            .min_size(egui::vec2(0.0, METRICS.control_height)),
    )
}
pub(super) fn notice(ui: &mut Ui, color: Color32, contents: impl FnOnce(&mut Ui)) {
    let frame = egui::Frame::NONE
        .fill(ui.visuals().faint_bg_color)
        .inner_margin(egui::Margin::symmetric(12, 11))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            contents(ui);
        });
    ui.painter().vline(
        frame.response.rect.left(),
        frame.response.rect.y_range(),
        Stroke::new(2.0, color),
    );
}
pub(super) fn line(ui: &mut Ui) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
}
pub(super) fn child(
    ui: &mut Ui,
    rect: egui::Rect,
    id: impl std::hash::Hash + std::fmt::Debug,
) -> Ui {
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(id)
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    child
}
pub(super) struct RowContent<'a> {
    pub(super) title: &'a str,
    pub(super) subtitle: &'a str,
    pub(super) status: &'a str,
    pub(super) icon: Option<char>,
}
/// Paint a compact accessible selectable row; IDs are supplied by the caller.
pub(super) fn list_row(
    ui: &mut Ui,
    selected: bool,
    accessible: &str,
    content: RowContent<'_>,
    color: Color32,
) -> egui::Response {
    let RowContent {
        title,
        subtitle,
        status,
        icon,
    } = content;
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), METRICS.row_height),
        egui::Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            accessible,
        )
    });
    let painter = ui.painter();
    if selected {
        painter.rect_filled(
            rect,
            0.0,
            if ui.visuals().dark_mode {
                Color32::from_rgb(33, 45, 56)
            } else {
                Color32::from_rgb(226, 237, 243)
            },
        );
        painter.vline(rect.left(), rect.y_range(), Stroke::new(2.0, accent(ui)));
    } else if response.hovered() {
        painter.rect_filled(rect, 0.0, ui.visuals().widgets.hovered.bg_fill);
    }
    let mut text = rect.shrink2(egui::vec2(10.0, 10.0));
    if let Some(icon) = icon {
        let icon_rect = egui::Rect::from_min_size(
            egui::pos2(text.left(), rect.center().y - 15.0),
            egui::vec2(30.0, 30.0),
        );
        painter.rect(
            icon_rect,
            0.0,
            ui.visuals().widgets.noninteractive.bg_fill,
            Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            egui::StrokeKind::Inside,
        );
        painter.text(
            icon_rect.center(),
            egui::Align2::CENTER_CENTER,
            icon,
            FontId::proportional(13.0),
            ui.visuals().text_color(),
        );
        text.min.x += 40.0;
    }
    let status_galley = painter.layout_no_wrap(status.into(), FontId::proportional(12.0), color);
    let status_width = status_galley.size().x.min(text.width() * 0.44);
    let main_rect = egui::Rect::from_min_max(
        text.min,
        egui::pos2(text.right() - status_width - 10.0, text.bottom()),
    );
    let label = painter.with_clip_rect(main_rect.intersect(ui.clip_rect()));
    label.text(
        main_rect.left_top(),
        egui::Align2::LEFT_TOP,
        title,
        FontId::proportional(TYPE.row_title),
        ui.visuals().text_color(),
    );
    // The subtitle spans the row while status stays on the first line.
    painter.with_clip_rect(text.intersect(ui.clip_rect())).text(
        egui::pos2(text.left(), text.top() + 25.0),
        egui::Align2::LEFT_TOP,
        subtitle,
        FontId::proportional(12.0),
        secondary(ui),
    );
    painter
        .with_clip_rect(
            egui::Rect::from_min_max(
                egui::pos2(text.right() - status_width, text.top()),
                text.max,
            )
            .intersect(ui.clip_rect()),
        )
        .galley(
            egui::pos2(text.right() - status_width, text.top() + 2.0),
            status_galley,
            color,
        );
    painter.hline(
        rect.x_range(),
        rect.bottom(),
        Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
    response
}
pub(super) fn fact(ui: &mut Ui, label: &str, value: &str) -> egui::Response {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        let rect = ui
            .allocate_exact_size(egui::vec2(86.0, 28.0), egui::Sense::hover())
            .0;
        let mut label_ui = child(ui, rect, ("cloud-fact", label));
        hint(&mut label_ui, label);
        ui.add(egui::Label::new(RichText::new(value).size(TYPE.row_description)).wrap())
    })
    .inner
}
/// 68px standard settings row, wrapping control below on a narrow pane.
pub(super) fn setting_row(
    ui: &mut Ui,
    label: &str,
    description: &str,
    control: impl FnOnce(&mut Ui),
) {
    let width = ui.available_width();
    let start = ui.cursor().min;
    if width < 480.0 {
        title(ui, label);
        hint(ui, description);
        control(ui);
        ui.add_space(9.0);
        line(ui);
        return;
    }
    let control_width = METRICS.control_column.min(width * 0.48);
    let label_width = width - control_width - METRICS.column_gap;
    let title_height = ui
        .painter()
        .layout(
            label.to_owned(),
            FontId::proportional(TYPE.section_title),
            ui.visuals().text_color(),
            label_width,
        )
        .size()
        .y;
    let description_height = ui
        .painter()
        .layout(
            description.to_owned(),
            FontId::proportional(TYPE.row_description),
            secondary(ui),
            label_width,
        )
        .size()
        .y;
    // The HTML row uses min-height, allowing wrapped descriptions to grow.
    let row_height = METRICS
        .row_height
        .max(20.0 + title_height + description_height);
    let rect = egui::Rect::from_min_size(start, egui::vec2(width, row_height));
    let mut label_ui = child(
        ui,
        egui::Rect::from_min_max(
            rect.min,
            egui::pos2(
                rect.right() - control_width - METRICS.column_gap,
                rect.bottom(),
            ),
        ),
        ("cloud-setting-label", label),
    );
    label_ui.add_space(10.0);
    title(&mut label_ui, label);
    hint(&mut label_ui, description);
    let mut controls = child(
        ui,
        egui::Rect::from_min_max(
            egui::pos2(
                rect.right() - control_width,
                rect.top() + (row_height - CONTROL_HEIGHT) / 2.0,
            ),
            rect.max,
        ),
        ("cloud-setting-control", label),
    );
    controls.with_layout(egui::Layout::right_to_left(egui::Align::Min), control);
    ui.allocate_rect(rect, egui::Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.bottom(),
        Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
}
/// List/detail on a wide pane, vertical sections below 700 logical pixels.
pub(super) fn list_detail(
    ui: &mut Ui,
    id: &str,
    ratio: f32,
    left: impl FnOnce(&mut Ui),
    right: impl FnOnce(&mut Ui),
) {
    let width = ui.available_width();
    if width < 700.0 {
        egui::ScrollArea::vertical()
            .id_salt((id, "narrow-list"))
            .max_height(220.0)
            .auto_shrink([false, true])
            .show(ui, left);
        ui.add_space(12.0);
        line(ui);
        ui.add_space(18.0);
        right(ui);
        return;
    }
    let start = ui.cursor().min;
    let split = width * ratio;
    let height = (ui.clip_rect().bottom() - start.y).clamp(220.0, 420.0);
    let mut list = child(
        ui,
        egui::Rect::from_min_size(start, egui::vec2(split - 14.0, height)),
        (id, "list"),
    );
    egui::ScrollArea::vertical()
        .id_salt((id, "list-scroll"))
        .max_height(height)
        .auto_shrink([false, true])
        .show(&mut list, left);
    let mut detail = child(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(start.x + split + 23.0, start.y + 19.0),
            egui::vec2(width - split - 23.0, height),
        ),
        (id, "detail"),
    );
    // The body scrolls the entire measured detail. Keep its viewport clip,
    // rather than freezing a 420px child clip that moves above the viewport.
    detail.set_clip_rect(
        egui::Rect::from_x_y_ranges(detail.max_rect().x_range(), ui.clip_rect().y_range())
            .intersect(ui.clip_rect()),
    );
    right(&mut detail);
    let used = egui::Rect::from_min_size(
        start,
        egui::vec2(
            width,
            list.min_rect()
                .height()
                .max(detail.min_rect().height() + 19.0)
                .max(362.0),
        ),
    );
    ui.allocate_rect(used, egui::Sense::hover());
    ui.painter().vline(
        start.x + split,
        used.y_range(),
        Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
}

pub(super) fn button_named(ui: &mut Ui, text: &str, accessible: &str) -> egui::Response {
    let response = button(ui, text);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), accessible)
    });
    response
}
pub(super) fn toggle(ui: &mut Ui, on: &mut bool, label: &str) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(33.0, 19.0), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, label)
    });
    let changed = response.clicked();
    if changed {
        *on = !*on;
    }
    ui.painter().rect(
        rect,
        10.0,
        if *on {
            accent(ui)
        } else {
            ui.visuals().widgets.hovered.bg_fill
        },
        Stroke::new(1.0, secondary(ui)),
        egui::StrokeKind::Inside,
    );
    let x = if *on {
        rect.right() - 9.0
    } else {
        rect.left() + 9.0
    };
    ui.painter().circle_filled(
        egui::pos2(x, rect.center().y),
        5.5,
        if *on {
            Color32::from_rgb(7, 26, 34)
        } else {
            secondary(ui)
        },
    );
    changed
}

/// 표시와 접근성이 같은 셀 사각형을 사용하는 고정 열 라벨.
pub(super) fn cell_label(
    ui: &mut Ui,
    rect: egui::Rect,
    text: &str,
    size: f32,
    color: Color32,
    centered: bool,
) -> egui::Response {
    let response = ui.allocate_rect(rect, egui::Sense::hover());
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), text));
    ui.painter()
        .with_clip_rect(rect.intersect(ui.clip_rect()))
        .text(
            if centered {
                rect.center()
            } else {
                rect.left_center()
            },
            if centered {
                egui::Align2::CENTER_CENTER
            } else {
                egui::Align2::LEFT_CENTER
            },
            text,
            FontId::proportional(size),
            color,
        );
    response
}
