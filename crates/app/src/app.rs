use std::path::PathBuf;

use runtime::{InProcessRuntimeClient, RuntimeEventReceiver, RuntimeEventStream};

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
    runtime: InProcessRuntimeClient,
    runtime_events: RuntimeEventReceiver,
}

impl App {
    pub fn new(config: Config, config_path: PathBuf, db: Db, workspace_id: String) -> Self {
        // output_batch_ms는 시작 시 고정, scrollback_lines는 세션 spawn 시점에 전달
        let runtime = InProcessRuntimeClient::new(config.performance.output_batch_ms);
        let runtime_events = runtime.subscribe();
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
            runtime,
            runtime_events,
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
                    self.shell_ui.toggle(&self.runtime);
                }
            });
        });
        egui::CentralPanel::default().show(ui, |_ui| {});

        self.credentials_ui
            .show(ui.ctx(), &self.db, &self.secret_store);
        self.env_profiles_ui
            .show(ui.ctx(), &mut self.db, &self.workspace_id);
        let events: Vec<_> = self.runtime_events.try_iter().collect();
        self.shell_ui
            .show(ui.ctx(), &self.config.terminal, &self.runtime, &events);

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
