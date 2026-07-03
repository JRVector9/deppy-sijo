use std::path::PathBuf;

use runtime::{InProcessRuntimeClient, RuntimeCommandSink, RuntimeEventReceiver};

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
    frame_stats: crate::perf::FrameStats,
    /// worker에 마지막으로 보낸 render 활성 상태 (§14.1 Active↔Warm) — 전이 시에만 전송
    render_active: bool,
    /// logic()에서 drain했지만 아직 ui()가 렌더에 소비하지 않은 이벤트 (§14.1 Warm:
    /// 알림은 logic()에서 처리하고 렌더는 Active 복귀 시 ui()가 몰아서 소비).
    pending_events: Vec<runtime::RuntimeEvent>,
    /// 세션→제목 캐시 (MuxUpdated에서 누적) — Warm 동안 mux가 안 갱신돼도 알림 제목을
    /// 해석하기 위함. exit 처리 후 제거해 live 세션으로 유계.
    session_titles: std::collections::HashMap<runtime::SessionId, String>,
}

impl App {
    pub fn new(
        config: Config,
        config_path: PathBuf,
        db: Db,
        workspace_id: String,
        logs_root: PathBuf,
        db_path: PathBuf,
        egui_ctx: egui::Context,
    ) -> Self {
        // output_batch_ms는 시작 시 고정, scrollback_lines는 세션 spawn 시점에 전달
        let redaction = secret::RedactionService::new();
        let runtime = InProcessRuntimeClient::new(
            config.performance.output_batch_ms,
            Arc::new(KeyringSecretStore),
            logs_root,
            redaction.clone(),
            // 세션/mux 영속 파이프라인 (runtime↔persist 배선)
            Some(runtime::PersistConfig {
                db_path,
                workspace_id: workspace_id.clone(),
            }),
        );
        // 상태 이벤트 도착 시 UI를 깨운다 — 창이 숨겨져(Warm) 프레임이 멈춰도
        // logic()이 돌아 알림/상태를 처리하도록 (§14.1). egui Context는 스레드 안전.
        let runtime_events = runtime.subscribe_with_wake(std::sync::Arc::new({
            let ctx = egui_ctx.clone();
            move || ctx.request_repaint()
        }));
        // 이전 실행의 mux layout 복원 (PR-14) — subscribe 직후 1회 보내
        // subscribe→restore 순서와 "빈 상태" 전제를 코드로 보장한다. perf 하네스가
        // 세션을 만들기 전에 보내야 worker가 복원을 skip하지 않는다.
        if let Err(e) = runtime.send_command(runtime::RuntimeCommand::RestoreWorkspace) {
            tracing::warn!("workspace 복원 명령 전송 실패: {e:#}");
        }
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
        // PR-21 부하 하네스 (env로만 활성): hidden 10개 시나리오 자동 구성
        if crate::perf::harness_enabled() {
            for i in 0..crate::perf::HARNESS_SESSIONS {
                let (command, args) = crate::perf::harness_command(i);
                let _ = runtime.send_command(runtime::RuntimeCommand::SpawnAgent {
                    agent_config_id: None,
                    cols: 120,
                    rows: 40,
                    scrollback_lines: config.terminal.scrollback_lines as usize,
                    command,
                    args,
                    env_plain: Vec::new(),
                    env_secrets: Vec::new(),
                    waiting_regex: None,
                    approval_regex: None,
                    error_regex: None,
                    done_regex: None,
                });
            }
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
            frame_stats: crate::perf::FrameStats::new(),
            render_active: true,
            pending_events: Vec::new(),
            session_titles: std::collections::HashMap::new(),
        }
    }

    /// MuxUpdated에서 제목을 누적하고, 상태/exit 이벤트를 알림으로 만든다.
    /// logic()에서만 호출 — Warm 동안에도 알림이 즉시 생성된다 (§14.1).
    fn process_notifications(&mut self, events: &[runtime::RuntimeEvent]) {
        for event in events {
            match event {
                runtime::RuntimeEvent::MuxUpdated { snapshot } => {
                    // MuxUpdated는 전체 mux 스냅샷 — 사라진 세션(수동 close 등 exit
                    // 이벤트 없이 제거된 것 포함)을 정리해 캐시를 live 세션으로 유계.
                    // exit은 detach MuxUpdated보다 먼저 emit되므로(archival) 종료 알림
                    // 제목이 이보다 앞서 해석돼 안전하다.
                    let present: std::collections::HashSet<runtime::SessionId> = snapshot
                        .tabs
                        .iter()
                        .flat_map(|tab| &tab.panes)
                        .filter_map(|pane| pane.session_id)
                        .collect();
                    self.session_titles
                        .retain(|session, _| present.contains(session));
                    for pane in snapshot.tabs.iter().flat_map(|tab| &tab.panes) {
                        if let Some(session) = pane.session_id {
                            self.session_titles.insert(session, pane.title.clone());
                        }
                    }
                }
                runtime::RuntimeEvent::SessionStatusChanged { session, status } => {
                    if let Some(title) = self.session_titles.get(session).cloned() {
                        self.notifications_ui.on_status(*session, *status, &title);
                    }
                }
                // regex 없는 agent는 결과가 SessionExited로만 온다 (완료 기준: done/error)
                runtime::RuntimeEvent::SessionExited { session, exit_code } => {
                    if let Some(title) = self.session_titles.get(session).cloned() {
                        self.notifications_ui.on_exit(*session, *exit_code, &title);
                    }
                    // 종료된 세션은 더 알림이 없다 — 캐시에서 제거해 유계 유지
                    self.session_titles.remove(session);
                }
                _ => {}
            }
        }
    }
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        // worker join까지 동기 대기 — 셸 자식 프로세스 정리(reap) 보장
        self.runtime.shutdown();
    }

    // §14.1 Active↔Warm: 창이 안 보이면(최소화/완전 가림) worker가 snapshot 생성을
    // 멈추게 한다(세션은 유지). logic()은 창이 안 보여 ui()가 스킵될 때도 호출되므로
    // 여기서 감지해야 전이를 놓치지 않는다 (eframe 0.35). `visible()`은 eframe이 ui()
    // 스킵 판단에 쓰는 바로 그 신호(minimized OR occluded — macOS는 occluded로 갱신되어
    // minimized 미갱신 문제를 피한다). None(미보고)이면 안전하게 Active 유지.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let want_active = ctx.input(|i| i.viewport().visible()) != Some(false);
        if want_active != self.render_active {
            self.render_active = want_active;
            let state = if want_active {
                runtime::WorkspaceRuntimeState::Active
            } else {
                runtime::WorkspaceRuntimeState::Warm
            };
            let _ = self
                .runtime
                .send_command(runtime::RuntimeCommand::SetWorkspaceState(state));
            if want_active {
                // 재개된 Viewport push는 비동기 — 다음 프레임을 예약해 드레인한다.
                // (안 그러면 hidden 중 종료된 pane이 stale/"연결 중…"에 갇힐 수 있다)
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        // 이벤트 drain + 알림 생성은 non-render 경로인 여기서 한다 (§14.1 Warm:
        // ui()가 스킵돼도 승인/완료/실패 알림은 유지). worker의 wake가 숨겨진 UI를
        // 깨워 이 logic()을 돌린다. 렌더용으로는 pending_events에 쌓아 ui()가 소비한다.
        let new_events = self.runtime_events.drain();
        if !new_events.is_empty() {
            self.process_notifications(&new_events);
            self.pending_events.extend(new_events);
            // 보이는 idle 상태에서도 새 출력/상태를 즉시 렌더하도록 프레임 예약
            ctx.request_repaint();
        }
    }

    // egui 0.35부터 update(&Context) 대신 ui(&mut Ui) 시그니처를 쓴다.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame_stats.begin();
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

        // logic()이 drain해 쌓아둔 이벤트를 렌더에 소비한다 (알림은 logic()에서 이미 처리).
        // Warm 동안 쌓였다면 Active 복귀 시 여기서 몰아 처리된다.
        let events = std::mem::take(&mut self.pending_events);
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
        if self.connectors_ui.show(
            ui.ctx(),
            &mut self.db,
            &self.workspace_id,
            &self.secret_store,
        ) {
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

        // 알림 센터 렌더 (생성은 logic()에서 끝났다). 사라진 세션의 진행형 알림 정리.
        let mux = self.workspace_ui.mux().cloned();
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
        // focus 명령 응답은 다음 프레임 반영 — 즉시 repaint
        if focused {
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
        self.frame_stats.end();
    }
}
