pub mod activity;
pub mod agent_launcher;
pub mod agent_sessions;
pub mod agent_terminal;
pub mod agent_visuals;
pub mod agents;
pub mod approvals;
pub mod clipboard_image;
pub mod composer;
pub mod credentials;
pub(crate) mod cross_workspace;
pub mod designall;
pub mod diff_panel;
pub mod env_profiles;
pub mod env_project_list;
pub mod file_tree;
pub mod fleet;
pub mod inbox_approvals;
pub mod inbox_waiting;
pub mod notifications;
pub(crate) mod ports;
pub mod prompt_palette;
pub(crate) mod resource_manager;
pub mod settings;
pub mod workspace;

/// 픽셀 스냅된 1px 가로 헤어라인. egui 기본 `ui.separator()`는 좌표가 물리픽셀에
/// 정렬되지 않아 안티에일리어싱(feathering)으로 흐릿하게 번진다 — `round_to_pixel_center`
/// 로 라인 중심을 픽셀 중심에 맞춰 또렷한 1px로 그린다 (egui 0.35 픽셀 완벽 라인 API).
/// 색은 기본 separator와 동일한 noninteractive bg_stroke를 쓴다.
pub fn hairline(ui: &mut egui::Ui) {
    let color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    hairline_colored(ui, color);
}

/// 좌표를 물리 픽셀 **경계**로 반올림한다 (`ppp`는 `ctx.pixels_per_point()`).
///
/// `hairline`이 선 중심을 픽셀 중심에 맞추는 것과 같은 이유다. epaint는 갤리 **안의**
/// 글리프 위치만 픽셀에 맞추고(`text_layout.rs`의 `round_to_pixel`), 갤리 원점과 행
/// 사각형은 호출부가 준 좌표를 그대로 쓴다. 사이드바 행 높이가 29.19처럼 소수라 행이
/// 쌓일수록 원점이 물리 픽셀에서 밀리고(2x에서 행마다 0.38px), 그 결과 어떤 행은
/// 글자가 또렷하고 어떤 행은 반 픽셀 흐려서 목록을 훑을 때 선명도가 출렁인다.
/// 행 배경·레일도 같은 이유로 테두리가 뭉개진다.
///
/// 선 중심용인 `Painter::round_to_pixel_center`(x.5로 맞춤)와 용도가 다르다 —
/// 채움 사각형과 텍스트 원점은 픽셀 경계(정수)에 맞춰야 한다.
pub fn snap_to_pixel(ppp: f32, v: f32) -> f32 {
    (v * ppp).round() / ppp
}

/// [`snap_to_pixel`]의 좌표 버전.
pub fn snap_pos_to_pixel(ppp: f32, p: egui::Pos2) -> egui::Pos2 {
    egui::pos2(snap_to_pixel(ppp, p.x), snap_to_pixel(ppp, p.y))
}

/// [`snap_to_pixel`]의 사각형 버전. min/max를 각각 맞춰 폭·높이가 정수 픽셀이 된다.
pub fn snap_rect_to_pixel(ppp: f32, r: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(snap_pos_to_pixel(ppp, r.min), snap_pos_to_pixel(ppp, r.max))
}

/// 색 지정 버전.
pub fn hairline_colored(ui: &mut egui::Ui, color: egui::Color32) {
    // 기본 `ui.separator()`와 동일한 세로 공간을 차지한다(레이아웃 밀림 방지) —
    // separator 높이는 spacing().item_spacing.y가 아니라 spacing().separator... 가 아닌
    // egui 기본 6.0(separator widget의 spacing). visuals에서 못 읽으므로 상수로 맞춘다.
    let space = 6.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), space),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    let y = painter.round_to_pixel_center(rect.center().y);
    painter.hline(rect.x_range(), y, egui::Stroke::new(1.0, color));
}

/// 레이아웃을 소비하지 않고 지정 y에 긋는 픽셀 스냅 1px 라인 — 행 배경 위에
/// 테두리를 복원하는 테이블/카드 계열용 (hairline과 달리 painter 직접 호출).
pub fn hairline_at(painter: &egui::Painter, x_range: egui::Rangef, y: f32, color: egui::Color32) {
    let y = painter.round_to_pixel_center(y);
    painter.hline(x_range, y, egui::Stroke::new(1.0, color));
}

/// 기본 구분선 색·현재 min_rect 폭으로 hairline_at을 긋는다.
pub fn hairline_row(ui: &egui::Ui, y: f32) {
    let color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    hairline_at(ui.painter(), ui.min_rect().x_range(), y, color);
}

/// 섹션 헤더: 제목 + (옵션) 카운트 배지 + (옵션) 우측 액션 버튼.
/// 액션 버튼 클릭 시 true를 반환한다 (credentials/env_profiles 공용).
pub fn section_header(
    ui: &mut egui::Ui,
    title: &str,
    count: Option<usize>,
    action_label: Option<&str>,
) -> bool {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 36.0), egui::Sense::hover());
    let painter = ui.painter();
    let y = rect.center().y;
    let title_font = egui::FontId::monospace(14.0);
    painter.text(
        egui::pos2(rect.left(), y),
        egui::Align2::LEFT_CENTER,
        title,
        title_font.clone(),
        ui.visuals().text_color(),
    );
    if let Some(count) = count {
        let title_w = painter
            .layout_no_wrap(title.to_owned(), title_font, ui.visuals().text_color())
            .rect
            .width();
        let count_rect = egui::Rect::from_center_size(
            egui::pos2(rect.left() + title_w + 16.0, y),
            egui::vec2(20.0, 20.0),
        );
        let tag = if ui.visuals().dark_mode {
            egui::Color32::from_rgb(0x2a, 0x3a, 0x44)
        } else {
            egui::Color32::from_rgb(0xd0, 0xe8, 0xf4)
        };
        painter.rect_filled(count_rect, 0.0, tag);
        painter.text(
            count_rect.center(),
            egui::Align2::CENTER_CENTER,
            count.to_string(),
            egui::FontId::monospace(12.0),
            ui.visuals().hyperlink_color,
        );
    }

    let mut clicked = false;
    if let Some(action_label) = action_label {
        let label_font = egui::FontId::monospace(13.0);
        let label_width = painter
            .layout_no_wrap(
                action_label.to_owned(),
                label_font.clone(),
                ui.visuals().weak_text_color(),
            )
            .rect
            .width();
        let button_w = (label_width + 16.0).max(58.0);
        let button_rect = egui::Rect::from_min_size(
            egui::pos2(rect.right() - button_w, y - 13.0),
            egui::vec2(button_w, 26.0),
        );
        let response = ui.interact(
            button_rect,
            ui.id().with(("section_header_action", title)),
            egui::Sense::click(),
        );
        let hovered = response.hovered();
        let fill = if hovered {
            ui.visuals().selection.bg_fill
        } else {
            ui.visuals().extreme_bg_color
        };
        let stroke = if hovered {
            ui.visuals().selection.bg_fill
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke.color
        };
        painter.rect_filled(button_rect, 0.0, fill);
        painter.rect_stroke(
            button_rect,
            0.0,
            egui::Stroke::new(1.0, stroke),
            egui::StrokeKind::Inside,
        );
        painter.text(
            button_rect.center(),
            egui::Align2::CENTER_CENTER,
            action_label,
            label_font,
            if hovered {
                egui::Color32::WHITE
            } else {
                ui.visuals().weak_text_color()
            },
        );
        clicked = response.clicked();
    }
    hairline_row(ui, rect.bottom());
    clicked
}

/// 사람이 읽는 바이트 단위 표기 (GiB/MiB/KiB/B).
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

pub fn render_message(catalog: &i18n::Catalog, message: &runtime::MessagePayload) -> String {
    let args: Vec<(&str, &str)> = message
        .args
        .iter()
        .map(|arg| (arg.key.as_str(), arg.value.as_str()))
        .collect();
    catalog.t(&message.message_id, &args)
}
