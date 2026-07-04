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
    // multi-workspace (Track 2): 활성 workspace 하나만 런타임 보유 — 전환 시 현재 워커를
    // shutdown하고 대상 워커를 새로 만든다. 비활성 workspace는 DB metadata만(§14.1
    // Suspended/Closed). Warm(비활성 계속 실행)은 워커-per-workspace 필요 — 후속.
    egui_ctx: egui::Context,
    db_path: PathBuf,
    logs_base: PathBuf,
    redaction: secret::RedactionService,
    workspaces: Vec<crate::storage::WorkspaceRow>,
    workspaces_open: bool,
    new_workspace_name: String,
    /// 삭제 확인 대기 중인 workspace id (2단계 확인 — 실수 방지)
    confirm_delete_ws: Option<String>,
    /// 전환으로 background 정리 중인 옛 워커 shutdown 스레드들 (workspace_id, handle).
    /// 앱 종료 시 join(자식 reap 보장) + 같은 workspace 재오픈 전 직렬화(layout 경합 방지).
    pending_shutdowns: Vec<(String, std::thread::JoinHandle<()>)>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        config_path: PathBuf,
        db: Db,
        workspace_id: String,
        logs_base: PathBuf,
        db_path: PathBuf,
        egui_ctx: egui::Context,
    ) -> Self {
        // output_batch_ms는 시작 시 고정, scrollback_lines는 세션 spawn 시점에 전달
        let redaction = secret::RedactionService::new();
        let (runtime, runtime_events) = Self::make_runtime(
            &config,
            &logs_base,
            &workspace_id,
            &db_path,
            &redaction,
            &db,
            &egui_ctx,
        );
        // PR-21 부하 하네스 (env로만 활성): hidden 10개 시나리오 자동 구성 (기본 workspace만)
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
            credentials_ui: ui::credentials::CredentialsUi::new(redaction.clone()),
            env_profiles_ui: ui::env_profiles::EnvProfilesUi::new(),
            workspace_ui: ui::workspace::WorkspaceUi::new(),
            notifications_ui: ui::notifications::NotificationsUi::new(),
            runtime,
            runtime_events,
            frame_stats: crate::perf::FrameStats::new(),
            render_active: true,
            pending_events: Vec::new(),
            session_titles: std::collections::HashMap::new(),
            egui_ctx,
            db_path,
            logs_base,
            redaction,
            workspaces: Vec::new(),
            workspaces_open: false,
            new_workspace_name: String::new(),
            confirm_delete_ws: None,
            pending_shutdowns: Vec::new(),
        }
    }

    /// 한 workspace의 런타임 워커를 만든다: 생성 → wake 구독 → 저장 layout 복원 →
    /// credential redaction 시드. (perf 하네스는 제외 — new()에서 기본 workspace만.)
    #[allow(clippy::too_many_arguments)]
    fn make_runtime(
        config: &Config,
        logs_base: &std::path::Path,
        workspace_id: &str,
        db_path: &std::path::Path,
        redaction: &secret::RedactionService,
        db: &Db,
        egui_ctx: &egui::Context,
    ) -> (InProcessRuntimeClient, RuntimeEventReceiver) {
        // 세션 로그 루트: logs/<workspace_id>/ (설계문서 7장)
        let logs_root = logs_base.join(workspace_id);
        let runtime = InProcessRuntimeClient::new(
            config.performance.output_batch_ms,
            Arc::new(KeyringSecretStore),
            logs_root,
            redaction.clone(),
            Some(runtime::PersistConfig {
                db_path: db_path.to_path_buf(),
                workspace_id: workspace_id.to_owned(),
            }),
        );
        // 상태 이벤트 도착 시 UI를 깨운다 (§14.1 Warm 알림 유지). subscribe→restore 순서
        // 를 코드로 보장하려 subscribe 직후 복원 명령을 보낸다.
        let runtime_events = runtime.subscribe_with_wake(std::sync::Arc::new({
            let ctx = egui_ctx.clone();
            move || ctx.request_repaint()
        }));
        if let Err(e) = runtime.send_command(runtime::RuntimeCommand::RestoreWorkspace) {
            tracing::warn!("workspace 복원 명령 전송 실패: {e:#}");
        }
        // 저장된 credential을 로그 redaction 대상으로 시드 (값 resolve는 worker에서)
        match db.list_credentials() {
            Ok(credentials) => {
                let mut ids: Vec<String> = Vec::with_capacity(credentials.len());
                for c in credentials {
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
        (runtime, runtime_events)
    }

    /// workspace 전환: 대상 워커를 즉시 만들어 UI 응답성을 유지하고, **옛 워커는
    /// 백그라운드 스레드에서** shutdown(join+PTY reap)한다 — UI 스레드가 수초 멈추지
    /// 않게 (codex 리뷰). 비활성 workspace는 런타임을 갖지 않는다 (§14.1 Suspended/Closed).
    fn switch_workspace(&mut self, target_id: &str) {
        if target_id == self.workspace_id {
            return;
        }
        // 대상 workspace의 이전 워커가 아직 background 정리 중이면 먼저 끝낸다 — 안 그러면
        // 옛 target 워커와 새 target 워커가 같은 window 행에 동시 save_layout해 layout이
        // stale로 덮일 수 있다 (codex 리뷰). 다른 workspace 정리는 기다리지 않는다.
        self.join_pending_shutdown(target_id);
        let old_workspace_id = self.workspace_id.clone();
        // 대상 워커를 먼저 만든다 (make_runtime은 블록하지 않음). 옛 워커와 잠시 공존하나
        // persist workspace_id/PTY가 서로 달라 충돌 없음. 옛 워커의 세션 영속 상태는
        // 워커가 shutdown 정리 시 스스로 exited로 마감한다 (자기 UUID 행만 — race 없음).
        let (runtime, runtime_events) = Self::make_runtime(
            &self.config,
            &self.logs_base,
            target_id,
            &self.db_path,
            &self.redaction,
            &self.db,
            &self.egui_ctx,
        );
        // 옛 런타임을 꺼내 background 스레드에서 정리한다 (shutdown이 join으로 블록).
        let old_runtime = std::mem::replace(&mut self.runtime, runtime);
        self.runtime_events = runtime_events;
        self.workspace_id = target_id.to_owned();
        // 런타임에 묶인 pending 상태 정리 (이전 워커의 응답을 못 받음).
        // MCP invoke도 비운다 — A에서 연 실행/승인이 B의 workspace_id로 감사되면 안 된다.
        self.agents_ui.clear_pending();
        self.connectors_ui.clear_invoke();
        // 워크스페이스별 UI 상태 초기화 (새 워커의 이벤트로 다시 채워진다)
        self.workspace_ui = ui::workspace::WorkspaceUi::new();
        self.notifications_ui = ui::notifications::NotificationsUi::new();
        self.pending_events.clear();
        self.session_titles.clear();
        self.render_active = true;
        self.egui_ctx.request_repaint();

        // 옛 워커는 background에서 shutdown(join+PTY reap + 세션 영속 마감)한다 —
        // UI freeze의 원인이던 부분. 핸들을 보관해 앱 종료 시 join한다(자식 reap 보장).
        self.pending_shutdowns.retain(|(_, h)| !h.is_finished());
        let handle = std::thread::spawn(move || {
            let mut old_runtime = old_runtime;
            old_runtime.shutdown();
        });
        self.pending_shutdowns.push((old_workspace_id, handle));
    }

    /// 주어진 workspace의 대기 중 background shutdown들을 join한다 (같은 workspace 워커가
    /// 동시에 두 개 살아 layout 행을 경합하지 않도록). 다른 workspace 것은 남겨 둔다.
    fn join_pending_shutdown(&mut self, workspace_id: &str) {
        let mut i = 0;
        while i < self.pending_shutdowns.len() {
            if self.pending_shutdowns[i].0 == workspace_id {
                let (_, handle) = self.pending_shutdowns.remove(i);
                let _ = handle.join();
            } else {
                i += 1;
            }
        }
    }

    fn refresh_workspaces(&mut self) {
        match self.db.list_workspaces() {
            Ok(list) => self.workspaces = list,
            Err(e) => tracing::warn!("workspace 목록 조회 실패: {e:#}"),
        }
    }

    /// 워크스페이스 목록 창: 전환/생성. 전환은 워커 shutdown+recreate라 창 closure 밖에서.
    fn workspaces_window(&mut self, ctx: &egui::Context) {
        if !self.workspaces_open {
            return;
        }
        let mut open = true;
        let mut switch_to: Option<String> = None;
        let mut delete_id: Option<String> = None;
        let mut set_confirm: Option<String> = None;
        let mut cancel_confirm = false;
        let mut create = false;
        let deletable = self.workspaces.len() > 1; // 마지막 workspace는 삭제 불가
        egui::Window::new("워크스페이스")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                for ws in &self.workspaces {
                    ui.horizontal(|ui| {
                        if ws.id == self.workspace_id {
                            ui.strong(&ws.name);
                            ui.weak("(현재)");
                        } else {
                            ui.label(&ws.name);
                            if ui.button("전환").clicked() {
                                switch_to = Some(ws.id.clone());
                            }
                            // 삭제 (활성/마지막 제외) — 2단계 확인
                            if deletable {
                                if self.confirm_delete_ws.as_deref() == Some(ws.id.as_str()) {
                                    ui.colored_label(egui::Color32::RED, "세션·env 삭제?");
                                    if ui.button("삭제").clicked() {
                                        delete_id = Some(ws.id.clone());
                                    }
                                    if ui.button("취소").clicked() {
                                        cancel_confirm = true;
                                    }
                                } else if ui.button("삭제").clicked() {
                                    set_confirm = Some(ws.id.clone());
                                }
                            }
                        }
                    });
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("새 워크스페이스");
                    ui.text_edit_singleline(&mut self.new_workspace_name);
                    if ui.button("생성").clicked() {
                        create = true;
                    }
                });
            });
        self.workspaces_open = open;

        if create {
            let name = self.new_workspace_name.trim().to_owned();
            if !name.is_empty() {
                match self.db.create_workspace(&name) {
                    Ok(id) => {
                        self.new_workspace_name.clear();
                        switch_to = Some(id); // 생성 후 바로 전환
                    }
                    Err(e) => tracing::warn!("workspace 생성 실패: {e:#}"),
                }
            }
        }
        if cancel_confirm {
            self.confirm_delete_ws = None;
        }
        if let Some(id) = set_confirm {
            self.confirm_delete_ws = Some(id);
        }
        if let Some(id) = delete_id {
            self.confirm_delete_ws = None;
            // 활성 workspace는 삭제 목록에 뜨지 않으므로 여기 도달하지 않는다 (이중 방어)
            if id != self.workspace_id {
                if let Err(e) = self.db.delete_workspace(&id) {
                    tracing::warn!("workspace 삭제 실패: {e:#}");
                }
                self.refresh_workspaces();
            }
        }
        if let Some(id) = switch_to {
            self.confirm_delete_ws = None;
            self.switch_workspace(&id);
            self.refresh_workspaces();
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
        // worker join까지 동기 대기 — 셸 자식 프로세스 정리(reap) 보장.
        self.runtime.shutdown();
        // 전환으로 background 정리 중이던 옛 워커들도 끝까지 join한다 — detached
        // 스레드는 프로세스 종료 시 join되지 않아 PTY reap이 중단될 수 있다 (codex 리뷰).
        for (_, handle) in self.pending_shutdowns.drain(..) {
            let _ = handle.join();
        }
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
                if ui.button("워크스페이스").clicked() {
                    self.workspaces_open = !self.workspaces_open;
                    if self.workspaces_open {
                        self.refresh_workspaces();
                    }
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

        // 워크스페이스 전환/생성 (switch는 워커 shutdown+recreate라 window closure 밖에서)
        self.workspaces_window(ui.ctx());

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
