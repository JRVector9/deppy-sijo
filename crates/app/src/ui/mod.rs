pub mod activity;
pub mod agent_launcher;
pub mod agent_sessions;
pub mod agent_terminal;
pub mod agent_visuals;
pub mod agents;
pub mod approvals;
pub mod aux_search;
pub mod clipboard_image;
pub mod composer;
pub mod credentials;
pub(crate) mod cross_workspace;
pub mod designall;
pub mod diff_panel;
pub mod diff_viewer;
pub mod env_profiles;
pub mod env_project_list;
pub mod file_drop;
pub mod file_tree;
pub mod fleet;
pub mod git_panel;
pub mod inbox_approvals;
pub mod inbox_waiting;
pub mod markdown_viewer;
pub mod notes;
pub mod notifications;
pub(crate) mod ports;
pub mod prompt_palette;
pub(crate) mod resource_manager;
pub mod settings;
pub mod transcript_viewer;
pub mod work_history;
pub mod workspace;

/// 픽셀 스냅된 1px 가로 헤어라인. egui 기본 `ui.separator()`는 좌표가 물리픽셀에
/// 정렬되지 않아 안티에일리어싱(feathering)으로 흐릿하게 번진다 — `snap_line_to_pixel`로
/// 굵기의 물리픽셀 패리티에 맞춰 스냅해 또렷하게 그린다.
/// 색은 기본 separator와 동일한 noninteractive bg_stroke를 쓴다.
pub fn hairline(ui: &mut egui::Ui) {
    let color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    hairline_colored(ui, color);
}

/// 좌표를 물리 픽셀 **경계**로 반올림한다 (`ppp`는 `ctx.pixels_per_point()`).
///
/// `hairline`이 선 중심을 픽셀 중심에 맞추는 것과 같은 이유다. epaint는 갤리 **안의**
/// 글리프 위치만 픽셀에 맞추고(`text_layout.rs`의 `round_to_pixel`), 갤리 원점과 행
/// 사각형은 호출부가 준 좌표를 그대로 쓴다. 원점이 물리 픽셀에서 밀리면 어떤 행은
/// 글자가 또렷하고 어떤 행은 반 픽셀 흐려서, 목록을 훑을 때 선명도가 출렁인다.
/// 행 배경·레일도 같은 이유로 테두리가 뭉개진다.
///
/// 소수 좌표의 출처는 두 가지다. 하나는 상수 자체가 안 맞는 경우(29.19 같은 퍼센트
/// 축소 잔재) — 이건 상수를 정렬값으로 고치는 게 근본 해결이다. 다른 하나는 스크롤
/// 오프셋·리사이즈 드래그·텍스트 실측 높이처럼 **실행 중에 정해지는** 값이라 상수로
/// 고칠 수 없는 경우 — 이 헬퍼가 필요한 건 후자 때문이다.
///
/// 참고: 2x에서는 0.5 단위가 이미 정렬돼 있다(12.5 × 2 = 25px). 소수라고 다 어긋난
/// 게 아니므로, 상수를 손볼 때 무작정 정수화하지 말고 실제 정렬 여부를 확인할 것.
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

/// 선(hline/vline)의 좌표를 물리픽셀 격자에 맞춘다. 기준이 **굵기의 물리픽셀 패리티**에
/// 달려 있다 — 홀수면 픽셀 **중심**, 짝수면 픽셀 **경계**여야 양끝이 안티에일리어싱으로
/// 번지지 않는다.
///
/// egui의 `Painter::round_to_pixel_center`는 문서가 밝히듯 홀수 폭 전용이다
/// ("lines that are one pixel wide (or any odd number of pixels)"). 그런데 이 앱의 선은
/// 전부 `Stroke::new(1.0, ..)` — **1.0 포인트**라 Retina(ppp 2)에서 2 물리픽셀(짝수)이다.
/// 그대로 중심에 맞추면 반 픽셀씩 걸쳐 3개 행에 잉크가 퍼지고, 또렷한 1px을 의도한
/// `hairline`이 오히려 흐려진다(2026-08-07).
pub fn snap_line_to_pixel(coord: f32, stroke_width: f32, pixels_per_point: f32) -> f32 {
    let ppp = pixels_per_point.max(1.0);
    let physical_width = (stroke_width * ppp).round().max(1.0);
    let physical = coord * ppp;
    let snapped = if (physical_width as i64) % 2 == 0 {
        physical.round()
    } else {
        (physical - 0.5).round() + 0.5
    };
    snapped / ppp
}

/// 면의 **상단 경계**에 붙이는 선의 중심 y. 선이 첫 물리행부터 덮게 한다.
///
/// [`snap_line_to_pixel`]과 다르다 — 저건 "이 좌표를 지나는 선"을 스냅하고, 이건
/// "이 경계에서 시작하는 선"을 놓는다. 경계는 반올림이 아니라 **내림**이다. 경계가
/// 물리픽셀 중간에 걸릴 때 반올림이 위로 가면 그만큼 안쪽에 빈 띠가 남는다 — 살짝
/// 위로 겹치는 쪽이 낫다(2026-08-07).
pub fn snap_edge_line_to_pixel(edge: f32, stroke_width: f32, pixels_per_point: f32) -> f32 {
    let ppp = pixels_per_point.max(1.0);
    (edge * ppp).floor() / ppp + stroke_width * 0.5
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
    let ppp = ui.ctx().pixels_per_point();
    let painter = ui.painter();
    let y = snap_line_to_pixel(rect.center().y, 1.0, ppp);
    painter.hline(rect.x_range(), y, egui::Stroke::new(1.0, color));
}

/// 레이아웃을 소비하지 않고 지정 y에 긋는 픽셀 스냅 1px 라인 — 행 배경 위에
/// 테두리를 복원하는 테이블/카드 계열용 (hairline과 달리 painter 직접 호출).
pub fn hairline_at(painter: &egui::Painter, x_range: egui::Rangef, y: f32, color: egui::Color32) {
    let y = snap_line_to_pixel(y, 1.0, painter.pixels_per_point());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 선의 **양끝이 물리픽셀 경계**에 떨어져야 안티에일리어싱으로 번지지 않는다.
    /// 홀수 폭이면 중심 정렬, 짝수 폭이면 경계 정렬이라야 그렇게 된다.
    #[test]
    fn 선은_굵기_패리티에_맞게_스냅돼_양끝이_픽셀경계에_떨어진다() {
        let width = 1.0; // 이 앱의 모든 hairline/구분선
        for ppp in [1.0_f32, 2.0, 3.0] {
            for coord in [10.0_f32, 10.5, 38.25, 87.4, 200.75] {
                let y = snap_line_to_pixel(coord, width, ppp);
                for edge in [(y - width * 0.5) * ppp, (y + width * 0.5) * ppp] {
                    assert!(
                        (edge - edge.round()).abs() < 0.001,
                        "ppp {ppp}, coord {coord}: 끝점 {edge}가 픽셀 경계가 아니다"
                    );
                }
                // 원래 좌표에서 반 픽셀 넘게 밀리면 선이 엉뚱한 자리에 간다.
                assert!(
                    ((y - coord) * ppp).abs() <= 0.5 + 0.001,
                    "ppp {ppp}, coord {coord}: {:.2}물리픽셀이나 밀렸다",
                    (y - coord) * ppp
                );
            }
        }
    }

    /// 이 테스트가 실제로 옛 구현을 잡는지 확인한다 — 통과만 하는 테스트는 안 쓴 것과 같다.
    /// egui의 round_to_pixel_center(항상 픽셀 중심)는 ppp 2에서 1.0pt 선이 짝수 폭이라
    /// 끝점이 x.5로 떨어진다.
    #[test]
    fn 옛_중심정렬_규칙은_짝수폭에서_끝점이_어긋난다() {
        let (width, ppp, coord) = (1.0_f32, 2.0_f32, 10.0_f32);
        let old_y = ((coord * ppp - 0.5).round() + 0.5) / ppp; // round_to_pixel_center
        let old_edge = (old_y - width * 0.5) * ppp;
        assert!(
            (old_edge - old_edge.round()).abs() > 0.4,
            "옛 규칙이 이미 정렬돼 있으면 이 수정은 의미가 없다"
        );
        let new_edge = (snap_line_to_pixel(coord, width, ppp) - width * 0.5) * ppp;
        assert!((new_edge - new_edge.round()).abs() < 0.001);
    }

    /// ppp가 비정상이어도 발산하지 않는다.
    #[test]
    fn 선_스냅은_비정상_ppp에서도_유한하다() {
        assert!(snap_line_to_pixel(10.0, 1.0, 0.0).is_finite());
        assert!(snap_line_to_pixel(10.0, 0.0, 2.0).is_finite());
    }
}
