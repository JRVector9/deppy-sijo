use crate::config::{Config, Theme};

/// 설정 창. 값이 하나라도 바뀌면 true를 반환한다.
pub fn show(ctx: &egui::Context, open: &mut bool, config: &mut Config) -> bool {
    let mut changed = false;
    egui::Window::new("설정")
        .open(open)
        .resizable(false)
        .show(ctx, |ui| {
            ui.heading("UI");
            ui.horizontal(|ui| {
                ui.label("테마");
                for (theme, label) in [
                    (Theme::System, "시스템"),
                    (Theme::Light, "라이트"),
                    (Theme::Dark, "다크"),
                ] {
                    changed |= ui
                        .selectable_value(&mut config.ui.theme, theme, label)
                        .changed();
                }
            });

            ui.separator();
            ui.heading("Terminal");
            ui.horizontal(|ui| {
                ui.label("폰트 크기");
                changed |= ui
                    .add(egui::DragValue::new(&mut config.terminal.font_size).range(8.0..=32.0))
                    .changed();
            });
            ui.horizontal(|ui| {
                ui.label("스크롤백 줄 수");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut config.terminal.scrollback_lines)
                            .range(1_000..=100_000),
                    )
                    .changed();
            });

            ui.separator();
            ui.heading("Performance");
            ui.horizontal(|ui| {
                ui.label("출력 배치 간격(ms)");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut config.performance.output_batch_ms)
                            .range(16..=50),
                    )
                    .changed();
                ui.weak("(앱 재시작 후 적용)");
            });
        });
    changed
}
