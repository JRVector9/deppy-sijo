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
    pub success: egui::Color32,
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
    app_background: egui::Color32::from_rgb(0x0e, 0x0f, 0x12),
    workspace_background: egui::Color32::from_rgb(0x13, 0x15, 0x19),
    folder_tree_background: egui::Color32::from_rgb(0x13, 0x15, 0x19),
    input_background: egui::Color32::from_rgb(0x15, 0x17, 0x1c),
    separator: egui::Color32::from_rgb(0x2b, 0x2e, 0x35),
    text: egui::Color32::from_rgb(0xdc, 0xde, 0xe2),
    muted_text: egui::Color32::from_rgb(0x82, 0x88, 0x93),
    accent: egui::Color32::from_rgb(0x39, 0xb8, 0xe8),
    success: egui::Color32::from_rgb(0x4a, 0xcb, 0x82),
    warning: egui::Color32::from_rgb(0xe0, 0xa4, 0x3a),
    error: egui::Color32::from_rgb(0xef, 0x66, 0x71),
    selected_background: egui::Color32::from_rgb(0x17, 0x1a, 0x20),
    hover_background: egui::Color32::from_rgb(0x14, 0x17, 0x1c),
};

pub const LIGHT: Tokens = Tokens {
    app_background: egui::Color32::from_rgb(0xfb, 0xfc, 0xfd),
    workspace_background: egui::Color32::from_rgb(0xf5, 0xf7, 0xf9),
    folder_tree_background: egui::Color32::from_rgb(0xf5, 0xf7, 0xf9),
    input_background: egui::Color32::from_rgb(0xf1, 0xf3, 0xf5),
    separator: egui::Color32::from_rgb(0xd5, 0xd9, 0xdf),
    text: egui::Color32::from_rgb(0x23, 0x26, 0x2c),
    muted_text: egui::Color32::from_rgb(0x65, 0x6b, 0x74),
    accent: egui::Color32::from_rgb(0x1c, 0x93, 0xaa),
    success: egui::Color32::from_rgb(0x27, 0x91, 0x5b),
    warning: egui::Color32::from_rgb(0xb2, 0x70, 0x16),
    error: egui::Color32::from_rgb(0xc8, 0x3d, 0x49),
    selected_background: egui::Color32::from_rgb(0xed, 0xf1, 0xf4),
    hover_background: egui::Color32::from_rgb(0xf3, 0xf5, 0xf6),
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

pub fn vertical_separator(ui: &mut egui::Ui, height: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, height), egui::Sense::hover());
    let x = ui.painter().round_to_pixel_center(rect.center().x);
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
            egui::Color32::from_rgb(0x0e, 0x0f, 0x12)
        );
        assert_eq!(
            DARK.input_background,
            egui::Color32::from_rgb(0x15, 0x17, 0x1c)
        );
        assert_eq!(DARK.separator, egui::Color32::from_rgb(0x2b, 0x2e, 0x35));
        assert_eq!(DARK.accent, egui::Color32::from_rgb(0x39, 0xb8, 0xe8));
        assert_eq!(
            DARK.selected_background,
            egui::Color32::from_rgb(0x17, 0x1a, 0x20)
        );
        assert_eq!(
            LIGHT.selected_background,
            egui::Color32::from_rgb(0xed, 0xf1, 0xf4)
        );
        assert_eq!(STRUCTURAL_CORNER_RADIUS, 0);
        assert_eq!(NAV_RAIL_WIDTH, 88.0);
    }

    #[test]
    fn inactive_structure_has_no_fill_but_interactions_may_have_one() {
        assert_eq!(row_fill(DARK, false, false), None);
        assert_eq!(row_fill(DARK, true, false), Some(DARK.selected_background));
        assert_eq!(row_fill(DARK, false, true), Some(DARK.hover_background));
    }
}
