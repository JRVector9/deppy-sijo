use crate::config::Config;

/// 빈 창. UI 골격은 PR-01부터 채운다.
pub struct App {
    _config: Config,
}

impl App {
    pub fn new(config: Config) -> Self {
        Self { _config: config }
    }
}

impl eframe::App for App {
    // egui 0.35부터 update(&Context) 대신 ui(&mut Ui) 시그니처를 쓴다.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |_ui| {});
    }
}
