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
    workspace_id: String,
    secret_store: KeyringSecretStore,
    credentials_ui: ui::credentials::CredentialsUi,
    env_profiles_ui: ui::env_profiles::EnvProfilesUi,
    shell_ui: ui::shell::ShellUi,
}

impl App {
    pub fn new(config: Config, config_path: PathBuf, db: Db, workspace_id: String) -> Self {
        Self {
            config,
            config_path,
            settings_open: false,
            db,
            workspace_id,
            secret_store: KeyringSecretStore,
            credentials_ui: ui::credentials::CredentialsUi::new(),
            env_profiles_ui: ui::env_profiles::EnvProfilesUi::new(),
            shell_ui: ui::shell::ShellUi::new(),
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
                if ui.button("환경").clicked() {
                    self.env_profiles_ui.toggle();
                }
                if ui.button("셸").clicked() {
                    self.shell_ui.toggle();
                }
            });
        });
        egui::CentralPanel::default().show(ui, |_ui| {});

        self.credentials_ui
            .show(ui.ctx(), &self.db, &self.secret_store);
        self.env_profiles_ui
            .show(ui.ctx(), &mut self.db, &self.workspace_id);
        self.shell_ui.show(ui.ctx());

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
