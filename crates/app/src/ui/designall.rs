#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tokens {
    pub app_background: egui::Color32,
    pub workspace_background: egui::Color32,
    pub folder_tree_background: egui::Color32,
    pub input_background: egui::Color32,
    pub separator: egui::Color32,
    pub text: egui::Color32,
    pub muted_text: egui::Color32,
    pub accent: egui::Color32,
    /// **현재 사용처 없음** (2026-08-07). 원래 workspace.rs가 "포커스된 pane 점"과
    /// "닫기 버튼 hover"에 썼는데 둘 다 성공이 아니라 각각 accent·error가 맞는 의미였다.
    ///
    /// 새로 초록이 필요하면 여기부터 집지 말 것 — 세션/에이전트의 "완료"는
    /// `agent_visuals::status_color(AgentVisualState::Complete)`가 소유한다. 이 토큰은
    /// 그 체계 밖의 일반 성공 표시가 생길 때 쓴다.
    pub success: egui::Color32,
    /// `apply_workspace_visuals`가 egui의 `warn_fg_color`/`error_fg_color`로 흘려보낸다.
    pub warning: egui::Color32,
    pub error: egui::Color32,
    pub selected_background: egui::Color32,
    pub hover_background: egui::Color32,
}

pub const STRUCTURAL_CORNER_RADIUS: u8 = 0;
#[allow(dead_code)]
pub const INTERACTION_CORNER_RADIUS: u8 = 4;
pub const NAV_RAIL_WIDTH: f32 = 88.0;
pub const NAV_RAIL_MIN_WIDTH: f32 = 20.0;
pub const NAV_RAIL_MAX_WIDTH: f32 = 180.0;
pub const SEPARATOR_WIDTH: f32 = 1.0;

pub const DARK: Tokens = Tokens {
    app_background: egui::Color32::from_rgb(0x18, 0x1b, 0x20),
    // 사이드바 본문은 nav rail(app_background)과 **같은 면**이다. 원래는 한 단 위라
    // 레일과 패널 사이에 계단이 보였는데, 사이드바는 하나의 탐색 영역이고 레일과는 이미
    // 구분선이 나누므로 합쳤다(2026-08-06 사용자).
    // 토큰은 분리해 둔다 — 나중에 다시 갈라야 할 수 있고, 칠하는 코드가 서로 다르다
    // (structural_frame vs 각 섹션의 rect_filled).
    workspace_background: egui::Color32::from_rgb(0x18, 0x1b, 0x20),
    folder_tree_background: egui::Color32::from_rgb(0x18, 0x1b, 0x20),
    input_background: egui::Color32::from_rgb(0x0f, 0x11, 0x15),
    separator: egui::Color32::from_rgb(0x32, 0x36, 0x3e),
    text: egui::Color32::from_rgb(0xdc, 0xde, 0xe2),
    muted_text: egui::Color32::from_rgb(0x82, 0x88, 0x93),
    accent: egui::Color32::from_rgb(0x39, 0xb8, 0xe8),
    success: egui::Color32::from_rgb(0x4a, 0xcb, 0x82),
    warning: egui::Color32::from_rgb(0xe0, 0xa4, 0x3a),
    error: egui::Color32::from_rgb(0xef, 0x66, 0x71),
    selected_background: egui::Color32::from_rgb(0x21, 0x24, 0x2c),
    hover_background: egui::Color32::from_rgb(0x1d, 0x20, 0x26),
};

pub const LIGHT: Tokens = Tokens {
    app_background: egui::Color32::from_rgb(0xf1, 0xf2, 0xf5),
    workspace_background: egui::Color32::from_rgb(0xf1, 0xf2, 0xf5),
    folder_tree_background: egui::Color32::from_rgb(0xf1, 0xf2, 0xf5),
    input_background: egui::Color32::from_rgb(0xfd, 0xfd, 0xfd),
    separator: egui::Color32::from_rgb(0xc9, 0xcd, 0xd6),
    text: egui::Color32::from_rgb(0x23, 0x26, 0x2c),
    muted_text: egui::Color32::from_rgb(0x65, 0x6a, 0x74),
    accent: egui::Color32::from_rgb(0x1c, 0x93, 0xaa),
    success: egui::Color32::from_rgb(0x27, 0x91, 0x5b),
    warning: egui::Color32::from_rgb(0xb2, 0x70, 0x16),
    error: egui::Color32::from_rgb(0xc8, 0x3d, 0x49),
    selected_background: egui::Color32::from_rgb(0xe5, 0xe8, 0xec),
    hover_background: egui::Color32::from_rgb(0xeb, 0xed, 0xf0),
};

pub fn tokens(visuals: &egui::Visuals) -> Tokens {
    if visuals.dark_mode { DARK } else { LIGHT }
}

pub fn row_fill(tokens: Tokens, selected: bool, hovered: bool) -> Option<egui::Color32> {
    selected
        .then_some(tokens.selected_background)
        .or_else(|| hovered.then_some(tokens.hover_background))
}

pub fn separator_stroke(visuals: &egui::Visuals) -> egui::Stroke {
    egui::Stroke::new(SEPARATOR_WIDTH, tokens(visuals).separator)
}

/// 패널 경계에 세로 구분선을 놓을 x — 경계 좌표에서 **안쪽으로 1물리픽셀** 민 값.
/// 호출부는 이 값을 `Painter::round_to_pixel_center`로 감싸 쓴다.
///
/// 두 가지를 동시에 만족해야 해서 규칙을 여기 한 곳에 둔다.
/// 1. 경계 좌표(`rect.right()`)에 그리면 **다음 패널이 배경으로 덮어** 선이 사라진다
///    (egui는 나중에 그린 것이 위에 온다).
/// 2. 덮일 일이 없는 상단 바도 같은 규칙을 써야 한다 — 한쪽만 경계 좌표를 쓰면 1픽셀
///    어긋나 세로선이 상단 바에서 꺾인다(2026-08-07 사용자: "탑헤드쪽 경계가 비뚤어").
pub fn panel_edge_separator_x(right: f32, pixels_per_point: f32) -> f32 {
    right - 1.0 / pixels_per_point.max(1.0)
}

pub fn vertical_separator(ui: &mut egui::Ui, height: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, height), egui::Sense::hover());
    let x = crate::ui::snap_line_to_pixel(
        rect.center().x,
        SEPARATOR_WIDTH,
        ui.ctx().pixels_per_point(),
    );
    ui.painter()
        .vline(x, rect.y_range(), separator_stroke(ui.visuals()));
}

pub fn structural_frame(visuals: &egui::Visuals) -> egui::Frame {
    egui::Frame::NONE
        .fill(tokens(visuals).app_background)
        .inner_margin(egui::Margin::ZERO)
        .corner_radius(egui::CornerRadius::same(STRUCTURAL_CORNER_RADIUS))
}

pub fn apply_workspace_visuals(ui: &mut egui::Ui) {
    let tokens = tokens(ui.visuals());
    let visuals = ui.visuals_mut();
    visuals.override_text_color = Some(tokens.text);
    visuals.weak_text_color = Some(tokens.muted_text);
    visuals.panel_fill = tokens.app_background;
    visuals.window_fill = tokens.app_background;
    visuals.extreme_bg_color = tokens.input_background;
    visuals.faint_bg_color = tokens.hover_background;
    visuals.hyperlink_color = tokens.accent;
    visuals.warn_fg_color = tokens.warning;
    visuals.error_fg_color = tokens.error;
    visuals.selection.bg_fill = tokens.accent;
    visuals.selection.stroke = egui::Stroke::new(1.0, tokens.accent);
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, tokens.separator);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_tokens_match_the_approved_design_freeze() {
        assert_eq!(
            DARK.app_background,
            egui::Color32::from_rgb(0x18, 0x1b, 0x20)
        );
        assert_eq!(
            DARK.input_background,
            egui::Color32::from_rgb(0x0f, 0x11, 0x15)
        );
        assert_eq!(DARK.separator, egui::Color32::from_rgb(0x32, 0x36, 0x3e));
        assert_eq!(DARK.accent, egui::Color32::from_rgb(0x39, 0xb8, 0xe8));
        assert_eq!(
            DARK.selected_background,
            egui::Color32::from_rgb(0x21, 0x24, 0x2c)
        );
        assert_eq!(
            LIGHT.selected_background,
            egui::Color32::from_rgb(0xe5, 0xe8, 0xec)
        );
        assert_eq!(STRUCTURAL_CORNER_RADIUS, 0);
        assert_eq!(NAV_RAIL_WIDTH, 88.0);
    }

    /// 상단 바와 사이드바가 같은 경계에 그리는 세로선은 **같은 물리픽셀**에 놓여야 한다.
    /// 두 호출부가 각자 계산하다 1픽셀 어긋나 선이 상단 바에서 꺾였다(2026-08-07).
    #[test]
    fn 패널_경계_세로선은_안쪽_마지막_픽셀에_놓인다() {
        for ppp in [1.0, 2.0, 3.0] {
            let right = 288.0;
            let x = panel_edge_separator_x(right, ppp);
            assert!(x < right, "ppp {ppp}: 경계 위에 그리면 다음 패널이 덮는다");
            // f32는 288 근처에서 간격이 3e-5쯤이라 EPSILON으로는 못 잰다.
            assert!(
                (right - x - 1.0 / ppp).abs() < 1e-3,
                "ppp {ppp}: 정확히 1물리픽셀만 안쪽이어야 한다 (x={x})"
            );
            // 같은 경계를 받은 두 호출부는 항상 같은 좌표를 얻는다.
            assert_eq!(x, panel_edge_separator_x(right, ppp));
        }
        // ppp가 비정상(0 이하)이어도 발산하지 않는다.
        assert!(panel_edge_separator_x(288.0, 0.0).is_finite());
    }

    #[test]
    fn inactive_structure_has_no_fill_but_interactions_may_have_one() {
        assert_eq!(row_fill(DARK, false, false), None);
        assert_eq!(row_fill(DARK, true, false), Some(DARK.selected_background));
        assert_eq!(row_fill(DARK, false, true), Some(DARK.hover_background));
    }
}
