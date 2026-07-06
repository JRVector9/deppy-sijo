pub mod activity;
pub mod agents;
pub mod approvals;
pub mod clipboard_image;
pub mod connectors;
pub mod credentials;
pub mod env_profiles;
pub mod file_tree;
pub mod notifications;
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

pub fn render_message(catalog: &i18n::Catalog, message: &runtime::MessagePayload) -> String {
    let args: Vec<(&str, &str)> = message
        .args
        .iter()
        .map(|arg| (arg.key.as_str(), arg.value.as_str()))
        .collect();
    catalog.t(&message.message_id, &args)
}
