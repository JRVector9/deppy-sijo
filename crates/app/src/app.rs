use std::path::PathBuf;

use crate::config::Config;
use crate::secret::KeyringSecretStore;
use crate::storage::Db;
use crate::ui;

pub struct App {
    config: Config,
    config_path: PathBuf,
    settings_open: bool,
    db: Db,
    secret_store: KeyringSecretStore,
    credentials_ui: ui::credentials::CredentialsUi,
}

impl App {
    pub fn new(config: Config, config_path: PathBuf, db: Db) -> Self {
        Self {
            config,
            config_path,
            settings_open: false,
            db,
            secret_store: KeyringSecretStore,
            credentials_ui: ui::credentials::CredentialsUi::new(),
        }
    }
}

impl eframe::App for App {
    // egui 0.35부터 update(&Context) 대신 ui(&mut Ui) 시그니처를 쓴다.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("설정").clicked() {
                    self.settings_open = !self.settings_open;
                }
                if ui.button("자격증명").clicked() {
                    self.credentials_ui.toggle();
                }
            });
        });
        egui::CentralPanel::default().show(ui, |_ui| {});

        self.credentials_ui
            .show(ui.ctx(), &self.db, &self.secret_store);

        let changed = ui::settings::show(ui.ctx(), &mut self.settings_open, &mut self.config);
        if changed {
            // hot reload: 테마는 즉시 적용
            ui.ctx().set_theme(self.config.ui.theme.to_egui());
            if let Err(e) = self.config.save(&self.config_path) {
                tracing::warn!("config 저장 실패: {e:#}");
            }
        }
    }
}
