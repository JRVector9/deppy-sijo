use std::path::PathBuf;

use runtime::{InProcessRuntimeClient, RuntimeCommandSink, RuntimeEventReceiver};

use crate::config::Config;
use std::sync::Arc;

use crate::storage::Db;
use crate::ui;
use secret::KeyringSecretStore;

/// 한 workspace의 런타임 상태 묶음 (워커-per-workspace §14.1 준비 — Stage A).
/// 활성 workspace는 렌더되고, (후속) warm workspace는 이벤트만 드레인된다.
struct WorkspaceRuntime {
    id: String,
    runtime: InProcessRuntimeClient,
    events: RuntimeEventReceiver,
    workspace_ui: ui::workspace::WorkspaceUi,
    /// worker에 마지막으로 보낸 render 활성 상태 (§14.1 Active↔Warm) — 전이 시에만 전송
    render_active: bool,
    /// logic()에서 drain했지만 아직 ui()가 렌더에 소비하지 않은 이벤트 (§14.1 Warm:
    /// 알림은 logic()에서 처리하고 렌더는 Active 복귀 시 ui()가 몰아서 소비).
    pending_events: Vec<runtime::RuntimeEvent>,
    /// 세션→제목 캐시 (MuxUpdated에서 누적) — Warm 동안 mux가 안 갱신돼도 알림 제목을
    /// 해석하기 위함. exit 처리 후 제거해 live 세션으로 유계.
    session_titles: std::collections::HashMap<runtime::SessionId, String>,
}

pub struct App {
    config: Config,
    config_path: PathBuf,
    settings_open: bool,
    db: Db,
    secret_store: KeyringSecretStore,
    agents_ui: ui::agents::AgentsUi,
    connectors_ui: ui::connectors::ConnectorsUi,
    credentials_ui: ui::credentials::CredentialsUi,
    env_profiles_ui: ui::env_profiles::EnvProfilesUi,
    notifications_ui: ui::notifications::NotificationsUi,
    frame_stats: crate::perf::FrameStats,
    /// 현재 활성(렌더되는) workspace의 런타임 상태.
    active: WorkspaceRuntime,
    /// warm workspace들 (전환으로 물러났지만 워커는 계속 실행 — §14.1 Warm). 이벤트는
    /// drain만 하고(채널 backup 방지) 렌더/알림은 안 한다. 재활성 시 즉시 복귀.
    warm: std::collections::HashMap<String, WorkspaceRuntime>,
    /// warm LRU 순서 (앞이 가장 오래됨) — MAX_WARM 초과 시 앞에서부터 Suspended(shutdown).
    warm_order: Vec<String>,
    egui_ctx: egui::Context,
    db_path: PathBuf,
    logs_base: PathBuf,
    redaction: secret::RedactionService,
    workspaces: Vec<crate::storage::WorkspaceRow>,
    workspaces_open: bool,
    new_workspace_name: String,
    /// 삭제 확인 대기 중인 workspace id (2단계 확인 — 실수 방지)
    confirm_delete_ws: Option<String>,
    /// 알림 클릭으로 다른 workspace 전환 후, mux 재구성되면 이동할 (workspace, session).
    pending_focus: Option<(String, runtime::SessionId)>,
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
        let active = Self::make_runtime(
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
                let _ = active
                    .runtime
                    .send_command(runtime::RuntimeCommand::SpawnAgent {
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
            secret_store: KeyringSecretStore,
            agents_ui: ui::agents::AgentsUi::new(),
            connectors_ui: ui::connectors::ConnectorsUi::new(redaction.clone()),
            credentials_ui: ui::credentials::CredentialsUi::new(redaction.clone()),
            env_profiles_ui: ui::env_profiles::EnvProfilesUi::new(),
            notifications_ui: ui::notifications::NotificationsUi::new(),
            active,
            warm: std::collections::HashMap::new(),
            warm_order: Vec::new(),
            frame_stats: crate::perf::FrameStats::new(),
            egui_ctx,
            db_path,
            logs_base,
            redaction,
            workspaces: Vec::new(),
            workspaces_open: false,
            new_workspace_name: String::new(),
            confirm_delete_ws: None,
            pending_focus: None,
            pending_shutdowns: Vec::new(),
        }
    }

    /// warm 상태로 유지할 최대 workspace 수 (활성 제외). 저-RAM 정책상 작게 — 초과분은
    /// Suspended(워커 shutdown). active + MAX_WARM개까지 워커가 동시 실행될 수 있다.
    const MAX_WARM: usize = 2;

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
    ) -> WorkspaceRuntime {
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
        WorkspaceRuntime {
            id: workspace_id.to_owned(),
            runtime,
            events: runtime_events,
            workspace_ui: ui::workspace::WorkspaceUi::new(),
            render_active: true,
            pending_events: Vec::new(),
            session_titles: std::collections::HashMap::new(),
        }
    }

    /// workspace 전환 (워커-per-workspace §14.1 Warm): 현재 활성 workspace는 Warm으로
    /// 내려 워커를 계속 살려 둔다(에이전트 유지). 대상이 warm 풀에 있으면 재사용(즉시 복귀),
    /// 없으면 새로 만든다. warm 풀이 MAX_WARM을 넘으면 가장 오래된 것을 Suspended(shutdown).
    fn switch_workspace(&mut self, target_id: &str) {
        if target_id == self.active.id {
            return;
        }
        // 대상이 background 정리 중이면 먼저 끝낸다 (같은 window 행 경합 방지 — codex 리뷰).
        self.join_pending_shutdown(target_id);

        // 대상 준비: warm 풀에 있으면 재사용, 없으면 새 워커.
        let mut new_active = match self.warm.remove(target_id) {
            Some(rt) => {
                self.warm_order.retain(|id| id != target_id);
                rt
            }
            None => {
                // 새 워커는 SessionId를 1부터 다시 시작한다 — 이 workspace의 옛 워커
                // lifetime에서 남은 알림을 지운다. 안 그러면 재사용된 SessionId의 완료
                // 알림이 옛 항목과 dup으로 취급돼 안 뜬다 (codex 리뷰).
                self.notifications_ui.prune_workspace(target_id);
                Self::make_runtime(
                    &self.config,
                    &self.logs_base,
                    target_id,
                    &self.db_path,
                    &self.redaction,
                    &self.db,
                    &self.egui_ctx,
                )
            }
        };
        // UI 상태는 리셋하지 않는다 — warm 재사용이면 그동안 누적된 pending_events(=lifecycle
        // 이벤트 포함)를 그대로 ui()가 처리해 exit/status 상태를 재구성해야 하고, workspace_ui는
        // 마지막 active 상태 + 아래 Active 재emit(전체 mux 스냅샷)으로 최신화된다. (새 워커는
        // 이미 fresh + RestoreWorkspace라 리셋 불필요.)
        new_active.render_active = true;
        let _ = new_active
            .runtime
            .send_command(runtime::RuntimeCommand::SetWorkspaceState(
                runtime::WorkspaceRuntimeState::Active,
            ));

        // 현재 활성을 Warm으로 내리고 warm 풀에 보관 (워커·세션 계속 실행).
        let mut old = std::mem::replace(&mut self.active, new_active);
        let _ = old
            .runtime
            .send_command(runtime::RuntimeCommand::SetWorkspaceState(
                runtime::WorkspaceRuntimeState::Warm,
            ));
        old.render_active = false;
        let old_id = old.id.clone();
        self.warm.insert(old_id.clone(), old);
        self.warm_order.push(old_id);

        // pending 상태 정리 (이전 워커 응답 못 받음, 교차-ws 감사 방지).
        // notifications는 리셋하지 않는다 — (ws, session)로 namespacing돼 전역 센터가
        // 모든 workspace 알림을 유지한다 (background 완료 통지·클릭 이동, codex 리뷰).
        self.agents_ui.clear_pending();
        self.connectors_ui.clear_invoke();
        self.egui_ctx.request_repaint();

        self.evict_warm();
    }

    /// warm 풀이 MAX_WARM을 넘으면 가장 오래된 것부터 Suspended로 내린다 (워커 shutdown,
    /// 세션 종료 — §14.1 Suspended). background 스레드에서 정리하고 on_exit에서 join.
    fn evict_warm(&mut self) {
        while self.warm_order.len() > Self::MAX_WARM {
            let evict_id = self.warm_order.remove(0);
            if let Some(mut rt) = self.warm.remove(&evict_id) {
                // 마지막으로 큐에 남은 이벤트를 처리해 방금 끝난 background 작업의 완료/오류
                // 알림을 놓치지 않는다 (codex 리뷰 — 축출 시 receiver drop으로 유실되던 것).
                let events = rt.events.drain();
                Self::process_ws_notifications(
                    &mut self.notifications_ui,
                    &evict_id,
                    &events,
                    &mut rt.session_titles,
                );
                // 축출 = Suspended(워커 종료) — 그 workspace의 진행형 알림은 더는 조치
                // 불가하므로 정리한다 (결과 알림은 기록이라 유지, codex 리뷰).
                self.notifications_ui.prune_transient(&evict_id);
                self.pending_shutdowns.retain(|(_, h)| !h.is_finished());
                let handle = std::thread::spawn(move || {
                    let mut runtime = rt.runtime;
                    runtime.shutdown();
                });
                self.pending_shutdowns.push((evict_id, handle));
            }
        }
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
                        if ws.id == self.active.id {
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
            if id != self.active.id {
                // 이 workspace의 background shutdown이 끝나길 먼저 기다린다 — 워커가
                // persist/로그를 쓰는 중에 삭제하면 DB 행 재생성·로그 파일 경합이 난다.
                // warm 풀에서 실행 중이면 먼저 동기 shutdown (워커 정지 후 삭제).
                if let Some(rt) = self.warm.remove(&id) {
                    self.warm_order.retain(|w| w != &id);
                    let mut runtime = rt.runtime;
                    runtime.shutdown();
                }
                self.join_pending_shutdown(&id);
                match self.db.delete_workspace(&id) {
                    Ok(()) => {
                        self.notifications_ui.prune_workspace(&id);
                        // 로그 디렉터리도 정리 (best-effort — redacted 로그, 실패해도 무해)
                        let log_dir = self.logs_base.join(&id);
                        if let Err(e) = std::fs::remove_dir_all(&log_dir)
                            && e.kind() != std::io::ErrorKind::NotFound
                        {
                            tracing::warn!("workspace 로그 삭제 실패 {}: {e:#}", log_dir.display());
                        }
                    }
                    Err(e) => tracing::warn!("workspace 삭제 실패: {e:#}"),
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

    /// 한 workspace의 이벤트에서 제목을 누적(session_titles)하고 상태/exit을 알림으로
    /// 만든다. 알림은 (workspace_id, SessionId)로 식별 — 워커마다 SessionId가 리셋돼
    /// 충돌하므로. 활성/warm 워커 모두 이걸 거쳐 background workspace 알림도 뜬다.
    fn process_ws_notifications(
        notifications: &mut ui::notifications::NotificationsUi,
        workspace_id: &str,
        events: &[runtime::RuntimeEvent],
        session_titles: &mut std::collections::HashMap<runtime::SessionId, String>,
    ) {
        for event in events {
            match event {
                runtime::RuntimeEvent::MuxUpdated { snapshot } => {
                    let present: std::collections::HashSet<runtime::SessionId> = snapshot
                        .tabs
                        .iter()
                        .flat_map(|tab| &tab.panes)
                        .filter_map(|pane| pane.session_id)
                        .collect();
                    session_titles.retain(|session, _| present.contains(session));
                    for pane in snapshot.tabs.iter().flat_map(|tab| &tab.panes) {
                        if let Some(session) = pane.session_id {
                            session_titles.insert(session, pane.title.clone());
                        }
                    }
                }
                runtime::RuntimeEvent::SessionStatusChanged { session, status } => {
                    if let Some(title) = session_titles.get(session).cloned() {
                        notifications.on_status(workspace_id, *session, *status, &title);
                    }
                }
                // regex 없는 agent는 결과가 SessionExited로만 온다 (완료 기준: done/error)
                runtime::RuntimeEvent::SessionExited { session, exit_code } => {
                    if let Some(title) = session_titles.get(session).cloned() {
                        notifications.on_exit(workspace_id, *session, *exit_code, &title);
                    }
                    session_titles.remove(session);
                }
                _ => {}
            }
        }
    }
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        // worker join까지 동기 대기 — 셸 자식 프로세스 정리(reap) 보장.
        self.active.runtime.shutdown();
        // warm 워커들도 종료 (계속 실행 중이던 세션들 reap).
        for (_, rt) in self.warm.drain() {
            let mut runtime = rt.runtime;
            runtime.shutdown();
        }
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
        if want_active != self.active.render_active {
            self.active.render_active = want_active;
            let state = if want_active {
                runtime::WorkspaceRuntimeState::Active
            } else {
                runtime::WorkspaceRuntimeState::Warm
            };
            let _ = self
                .active
                .runtime
                .send_command(runtime::RuntimeCommand::SetWorkspaceState(state));
            if want_active {
                // 재개된 Viewport push는 비동기 — 다음 프레임을 예약해 드레인한다.
                // (안 그러면 hidden 중 종료된 pane이 stale/"연결 중…"에 갇힐 수 있다)
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        // warm 워커의 이벤트는 drain해서 그 워커의 pending_events에 '누적'한다 (버리지
        // 않는다 — SessionExited/StatusChanged 같은 일회성 lifecycle 이벤트를 버리면
        // 재활성 시 종료된 pane이 실행 중으로 보인다, codex 리뷰). 재활성 시 fresh가 아닌
        // 이 누적분을 그대로 ui()가 처리해 상태를 재구성한다. 렌더/알림은 활성만.
        for rt in self.warm.values_mut() {
            let events = rt.events.drain();
            if !events.is_empty() {
                // warm workspace도 알림은 만든다 (background 완료/승인 통지) — (ws, session)로
                // 식별해 워커 간 SessionId 충돌을 피한다. 렌더용으로는 pending에 누적.
                Self::process_ws_notifications(
                    &mut self.notifications_ui,
                    &rt.id,
                    &events,
                    &mut rt.session_titles,
                );
                rt.pending_events.extend(events);
            }
        }

        // 이벤트 drain + 알림 생성은 non-render 경로인 여기서 한다 (§14.1 Warm:
        // ui()가 스킵돼도 승인/완료/실패 알림은 유지). worker의 wake가 숨겨진 UI를
        // 깨워 이 logic()을 돌린다. 렌더용으로는 pending_events에 쌓아 ui()가 소비한다.
        let new_events = self.active.events.drain();
        if !new_events.is_empty() {
            Self::process_ws_notifications(
                &mut self.notifications_ui,
                &self.active.id,
                &new_events,
                &mut self.active.session_titles,
            );
            self.active.pending_events.extend(new_events);
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
        let events = std::mem::take(&mut self.active.pending_events);
        self.agents_ui.show(
            ui.ctx(),
            &self.db,
            &self.active.id,
            &self.config.terminal,
            &self.active.runtime,
            &events,
        );
        self.credentials_ui
            .show(ui.ctx(), &self.db, &self.secret_store);
        if self
            .connectors_ui
            .show(ui.ctx(), &mut self.db, &self.active.id, &self.secret_store)
        {
            // OAuth로 credential이 추가됨 — 자격증명 창은 이번 프레임에 이미
            // 그려졌으므로 캐시 무효화 후 다음 프레임을 예약해 즉시 반영한다
            self.credentials_ui.invalidate_cache();
            ui.ctx().request_repaint();
        }
        self.env_profiles_ui
            .show(ui.ctx(), &mut self.db, &self.active.id);
        egui::CentralPanel::default().show(ui, |ui| {
            self.active
                .workspace_ui
                .show(ui, &self.config.terminal, &self.active.runtime, &events);
        });

        // 알림 센터 렌더 (생성은 logic()에서 끝났다). 활성 workspace의 사라진 세션의
        // 진행형 알림 정리 (다른 workspace 건 alive를 알 수 없어 유지).
        let mux = self.active.workspace_ui.mux().cloned();
        if let Some(mux) = &mux {
            let alive: Vec<_> = mux
                .tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .filter_map(|pane| pane.session_id)
                .collect();
            self.notifications_ui
                .retain_sessions(&self.active.id, &alive);
        }
        // 전환 후 대상 workspace의 mux가 재구성되면(재emit) 알림이 가리킨 세션 pane으로
        // 이동한다 — 전환은 즉시지만 mux는 다음 몇 프레임에 채워지므로 pending으로 둔다.
        if let Some((ws_id, session)) = self.pending_focus.clone() {
            if ws_id != self.active.id {
                self.pending_focus = None; // 다른 곳으로 전환됨 — 취소
            } else if let Some(pane) = mux.as_ref().and_then(|m| pane_of_session(m, session)) {
                let _ = self
                    .active
                    .runtime
                    .send_command(runtime::RuntimeCommand::FocusPane { pane });
                self.pending_focus = None;
            }
        }
        // 클릭한 알림 → 활성 workspace면 pane focus, 아니면 그 workspace로 전환 후 focus 예약.
        if let Some((ws_id, session)) = self.notifications_ui.show(ui.ctx()) {
            if ws_id == self.active.id {
                if let Some(pane) = mux.as_ref().and_then(|m| pane_of_session(m, session)) {
                    let _ = self
                        .active
                        .runtime
                        .send_command(runtime::RuntimeCommand::FocusPane { pane });
                }
            } else {
                // warm 재사용이면 워커·SessionId가 그대로라 그 세션으로 focus 예약.
                // 재생성(비-warm)이면 SessionId가 리셋돼 옛 id가 엉뚱한 셸을 잡을 수
                // 있으므로 focus를 예약하지 않는다 (전환만, codex 리뷰).
                let reused = self.warm.contains_key(&ws_id);
                self.switch_workspace(&ws_id);
                self.refresh_workspaces();
                if reused {
                    self.pending_focus = Some((ws_id, session));
                }
            }
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

/// 세션이 붙어 있는 pane id를 mux 스냅샷에서 찾는다 (알림 클릭 → focus용).
fn pane_of_session(
    mux: &runtime::MuxSnapshot,
    session: runtime::SessionId,
) -> Option<runtime::MuxPaneId> {
    mux.tabs
        .iter()
        .flat_map(|tab| &tab.panes)
        .find(|pane| pane.session_id == Some(session))
        .map(|pane| pane.id.clone())
}
