use std::path::PathBuf;

use runtime::{
    InProcessRuntimeClient, RuntimeCommandSink, RuntimeEventReceiver, RuntimeEventStream,
};

use crate::config::Config;
use std::sync::Arc;

use crate::storage::Db;
use crate::ui;
use secret::KeyringSecretStore;

pub struct App {
    config: Config,
    config_path: PathBuf,
    settings_open: bool,
    db: Db,
    workspace_id: String,
    secret_store: KeyringSecretStore,
    agents_ui: ui::agents::AgentsUi,
    connectors_ui: ui::connectors::ConnectorsUi,
    credentials_ui: ui::credentials::CredentialsUi,
    env_profiles_ui: ui::env_profiles::EnvProfilesUi,
    workspace_ui: ui::workspace::WorkspaceUi,
    notifications_ui: ui::notifications::NotificationsUi,
    runtime: InProcessRuntimeClient,
    runtime_events: RuntimeEventReceiver,
}

impl App {
    pub fn new(
        config: Config,
        config_path: PathBuf,
        db: Db,
        workspace_id: String,
        logs_root: PathBuf,
    ) -> Self {
        // output_batch_ms는 시작 시 고정, scrollback_lines는 세션 spawn 시점에 전달
        let redaction = secret::RedactionService::new();
        let runtime = InProcessRuntimeClient::new(
            config.performance.output_batch_ms,
            Arc::new(KeyringSecretStore),
            logs_root,
            redaction.clone(),
        );
        let runtime_events = runtime.subscribe();
        // 이전 실행에서 저장한 credential도 로그 redaction 대상으로 시드
        // (값 resolve는 worker에서 — UI는 metadata의 id만 읽는다)
        match db.list_credentials() {
            Ok(credentials) => {
                let mut ids: Vec<String> = Vec::with_capacity(credentials.len());
                for c in credentials {
                    // OAuth credential은 refresh token entry도 redaction 대상 (PR-18)
                    if c.credential_kind == "oauth_token" {
                        ids.push(auth::refresh_entry_id(&c.id));
                    }
                    ids.push(c.id);
                }
                if !ids.is_empty()
                    && let Err(e) = runtime.send_command(runtime::RuntimeCommand::SeedRedaction {
                        credential_ids: ids,
                    })
                {
                    tracing::warn!("redaction 시드 전송 실패: {e:#}");
                }
            }
            Err(e) => tracing::warn!("credential 목록 조회 실패 (redaction 시드 생략): {e:#}"),
        }
        Self {
            config,
            config_path,
            settings_open: false,
            db,
            workspace_id,
            secret_store: KeyringSecretStore,
            agents_ui: ui::agents::AgentsUi::new(),
            connectors_ui: ui::connectors::ConnectorsUi::new(redaction.clone()),
            credentials_ui: ui::credentials::CredentialsUi::new(redaction),
            env_profiles_ui: ui::env_profiles::EnvProfilesUi::new(),
            workspace_ui: ui::workspace::WorkspaceUi::new(),
            notifications_ui: ui::notifications::NotificationsUi::new(),
            runtime,
            runtime_events,
        }
    }
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        // worker join까지 동기 대기 — 셸 자식 프로세스 정리(reap) 보장
        self.runtime.shutdown();
    }

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
                if ui.button("연결").clicked() {
                    self.connectors_ui.toggle();
                }
                if ui.button("환경").clicked() {
                    self.env_profiles_ui.toggle();
                }
                if ui.button("에이전트").clicked() {
                    self.agents_ui.toggle();
                }
                let unread = self.notifications_ui.unread();
                let label = if unread > 0 {
                    format!("알림 ({unread})")
                } else {
                    "알림".to_owned()
                };
                if ui.button(label).clicked() {
                    // 열면 모두 읽음 → 배지가 이미 그려진 뒤라 다음 프레임에 갱신
                    self.notifications_ui.toggle();
                    ui.ctx().request_repaint();
                }
            });
        });

        // 이벤트는 한 번 drain해서 agents/workspace가 같은 슬라이스를 본다
        let events = self.runtime_events.drain();
        self.agents_ui.show(
            ui.ctx(),
            &self.db,
            &self.workspace_id,
            &self.config.terminal,
            &self.runtime,
            &events,
        );
        self.credentials_ui
            .show(ui.ctx(), &self.db, &self.secret_store);
        if self.connectors_ui.show(ui.ctx(), &mut self.db) {
            // OAuth로 credential이 추가됨 — 자격증명 창은 이번 프레임에 이미
            // 그려졌으므로 캐시 무효화 후 다음 프레임을 예약해 즉시 반영한다
            self.credentials_ui.invalidate_cache();
            ui.ctx().request_repaint();
        }
        self.env_profiles_ui
            .show(ui.ctx(), &mut self.db, &self.workspace_id);
        egui::CentralPanel::default().show(ui, |ui| {
            self.workspace_ui
                .show(ui, &self.config.terminal, &self.runtime, &events);
        });

        // 알림 센터: workspace가 mux를 갱신한 뒤 상태 이벤트를 알림으로 만든다.
        // 배지는 이번 프레임 상단에서 이미 그려졌으므로, unread가 바뀌면 재도장한다.
        let mux = self.workspace_ui.mux().cloned();
        let unread_before = self.notifications_ui.unread();
        for event in &events {
            // 이미 사라진 세션(닫기 직전 큐된 상태)은 유령 알림을 만들지 않는다
            let title_of = |session| mux.as_ref().and_then(|mux| session_title(mux, session));
            match event {
                runtime::RuntimeEvent::SessionStatusChanged { session, status } => {
                    if let Some(title) = title_of(*session) {
                        self.notifications_ui.on_status(*session, *status, &title);
                    }
                }
                // regex 없는 agent는 결과가 SessionExited로만 온다 (완료 기준: done/error)
                runtime::RuntimeEvent::SessionExited { session, exit_code } => {
                    if let Some(title) = title_of(*session) {
                        self.notifications_ui.on_exit(*session, *exit_code, &title);
                    }
                }
                _ => {}
            }
        }
        if let Some(mux) = &mux {
            let alive: Vec<_> = mux
                .tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .filter_map(|pane| pane.session_id)
                .collect();
            self.notifications_ui.retain_sessions(&alive);
        }
        let focused = self
            .notifications_ui
            .show(ui.ctx(), &self.runtime, mux.as_deref());
        // 배지 변화(추가/pruning)·focus 명령 응답은 다음 프레임 반영 — 즉시 repaint
        if focused || self.notifications_ui.unread() != unread_before {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }

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

/// 세션이 속한 pane의 제목 (알림 표시용).
fn session_title(mux: &runtime::MuxSnapshot, session: runtime::SessionId) -> Option<String> {
    mux.tabs
        .iter()
        .flat_map(|tab| &tab.panes)
        .find(|pane| pane.session_id == Some(session))
        .map(|pane| pane.title.clone())
}
