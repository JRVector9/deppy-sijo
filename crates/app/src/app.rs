use std::path::PathBuf;

use runtime::{InProcessRuntimeClient, RuntimeCommandSink, RuntimeEventReceiver};

use crate::config::Config;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::storage::Db;
use crate::ui;
use mcp_store::PendingApprovalRow;
use secret::KeyringSecretStore;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ApprovalPendingSignature(Vec<String>);

impl ApprovalPendingSignature {
    fn from_rows(rows: &[PendingApprovalRow]) -> Self {
        Self(rows.iter().map(|row| row.id.clone()).collect())
    }
}

struct ApprovalWatcher {
    stop_tx: Option<std::sync::mpsc::Sender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ApprovalWatcher {
    fn spawn(
        db_path: PathBuf,
        ctx: egui::Context,
        poll_requested: Arc<AtomicBool>,
        interval: std::time::Duration,
    ) -> Self {
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("approval-watcher".to_owned())
            .spawn(move || {
                let db = match Db::open(&db_path) {
                    Ok(db) => db,
                    Err(e) => {
                        tracing::warn!("approval watcher DB 열기 실패: {e:#}");
                        return;
                    }
                };
                let mut last = ApprovalPendingSignature::default();
                loop {
                    match stop_rx.recv_timeout(interval) {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    match db.list_pending_approvals() {
                        Ok(rows) => {
                            let next = ApprovalPendingSignature::from_rows(&rows);
                            if next != last {
                                last = next;
                                poll_requested.store(true, Ordering::Release);
                                ctx.request_repaint();
                            }
                        }
                        Err(e) => tracing::warn!("approval watcher 조회 실패: {e:#}"),
                    }
                }
            })
            .expect("approval watcher thread spawn");
        Self {
            stop_tx: Some(stop_tx),
            handle: Some(handle),
        }
    }

    fn stop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ApprovalWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

struct AppCredentialService<'a> {
    db: &'a Db,
    secret_store: &'a dyn secret::SecretStore,
    redaction: &'a secret::RedactionService,
}

impl ui::credentials::CredentialService for AppCredentialService<'_> {
    fn list_credentials(&self) -> anyhow::Result<Vec<ui::credentials::CredentialListItem>> {
        Ok(self
            .db
            .list_credentials()?
            .into_iter()
            .map(|meta| ui::credentials::CredentialListItem {
                id: meta.id,
                provider: meta.provider,
                label: meta.label,
                credential_kind: meta.credential_kind,
                masked_hint: meta.masked_hint,
            })
            .collect())
    }

    fn add_credential(&self, credential: ui::credentials::NewCredential) -> anyhow::Result<()> {
        let secret = secret::SecretString::new(credential.secret);
        // 새 credential은 즉시 로그 redaction 대상이다. JSON service account 형태도
        // 필드 단위로 등록해 후속 session/MCP output에서 마스킹된다.
        self.redaction.register(&secret);
        self.redaction.register_json_fields(&secret);
        let id = uuid::Uuid::new_v4().to_string();
        self.secret_store.set_secret(&id, &secret)?;
        let meta = crate::storage::CredentialMeta {
            id: id.clone(),
            provider: credential.provider,
            label: credential.label,
            credential_kind: credential.credential_kind,
            masked_hint: Some(secret::masked_hint(secret.expose())),
        };
        if let Err(e) = self.db.insert_credential(&meta) {
            if let Err(rollback) = self.secret_store.delete_secret(&id) {
                tracing::warn!(credential_id = %id, "rollback 실패 — 고아 keyring entry: {rollback:#}");
            }
            return Err(e);
        }
        tracing::info!(credential_id = %id, "credential 추가");
        Ok(())
    }

    fn delete_credential(&self, id: &str) -> anyhow::Result<()> {
        // 순서 근거:
        // 1) 참조 검사 — 참조 중이면 아무것도 건드리지 않는다.
        // 2) keyring 삭제 먼저 — 실패하면 metadata가 남아 사용자가 재시도할 수 있다.
        // 3) 조건부 DB 삭제 — 참조 race가 생기면 행이 남고, secret은 이미 지워진다.
        if self.db.credential_in_use(id)? {
            anyhow::bail!("env var가 참조 중인 credential입니다 — 해당 변수를 먼저 삭제하세요");
        }
        self.secret_store.delete_secret(id)?;
        self.secret_store
            .delete_secret(&auth::refresh_entry_id(id))?;
        if !self.db.delete_credential_if_unused(id)? {
            anyhow::bail!(
                "삭제 중 env var 참조가 생겼습니다 — secret은 지워졌으니 변수 정리 후 다시 삭제하세요"
            );
        }
        tracing::info!(credential_id = %id, "credential 삭제");
        Ok(())
    }
}

struct AppOAuthCredentialStore<'a> {
    secret_store: &'a dyn secret::SecretStore,
    redaction: &'a secret::RedactionService,
}

impl ui::connectors::OAuthCredentialStore for AppOAuthCredentialStore<'_> {
    fn store_oauth_token(
        &self,
        token: &auth::OAuthToken,
    ) -> anyhow::Result<ui::connectors::StoredOAuthCredential> {
        let id = uuid::Uuid::new_v4().to_string();
        auth::store_token(self.secret_store, &id, token)?;
        self.redaction.register(&token.access_token);
        if let Some(refresh) = &token.refresh_token {
            self.redaction.register(refresh);
        }
        Ok(ui::connectors::StoredOAuthCredential {
            id,
            masked_hint: secret::masked_hint(token.access_token.expose()),
        })
    }

    fn delete_oauth_token(&self, id: &str) -> anyhow::Result<()> {
        self.secret_store.delete_secret(id)?;
        self.secret_store
            .delete_secret(&auth::refresh_entry_id(id))?;
        Ok(())
    }
}

struct AppMcpScopedEnvResolver<'a> {
    secret_store: &'a dyn secret::SecretStore,
    redaction: &'a secret::RedactionService,
}

impl ui::connectors::McpScopedEnvResolver for AppMcpScopedEnvResolver<'_> {
    fn resolve_mcp_env(
        &self,
        env_plain: &[(String, String)],
        env_secrets: &[(String, String)],
    ) -> anyhow::Result<Vec<(String, String)>> {
        mcp_store::validate_server_env_for_persistence(env_plain, env_secrets)?;
        let mut env = env_plain.to_vec();
        for (key, credential_id) in env_secrets {
            let secret = self
                .secret_store
                .get_secret(credential_id)
                .map_err(|e| anyhow::anyhow!("MCP env '{}' credential 조회 실패: {e:#}", key))?;
            self.redaction.register(&secret);
            self.redaction.register_json_fields(&secret);
            env.push((key.clone(), secret.expose().to_owned()));
        }
        Ok(env)
    }
}

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
    /// 마지막 worker resource sample. PR-U25 activity view 표시용.
    resource_usage: Option<runtime::ProcessResourceSnapshot>,
    /// 마지막 worker child-process resource samples. Runtime이 집계한 값만 보관한다.
    session_resource_usage: Vec<runtime::SessionResourceUsage>,
    /// 마지막 PTY input pressure signal. UI는 런타임 이벤트만 보관한다.
    input_pressure: Option<runtime::PtyInputPressure>,
    /// Warm으로 내려간 시각. 일정 시간 이후 자동 Suspended(워커 shutdown)로 내린다.
    backgrounded_at: Option<std::time::Instant>,
    /// live 세션 추적 (suspend 보호 — 이벤트 스트림에서 갱신).
    live: LiveSessionTracker,
    /// 워커 생성 시각 — 첫 MuxUpdated 관측 전 suspend 유예(RESTORE 관측 창) 판정용.
    created: std::time::Instant,
    /// 응답(AgentSpawned/SpawnFailed) 대기 중인 agent spawn 수 — 전환 시 전역
    /// AgentsUi에서 이관받는다 (agent spawn 직후 전환 race의 live 판정).
    pending_agent_spawns: u32,
}

impl WorkspaceRuntime {
    /// 아직 종료(Exited)되지 않은 세션이 pane에 하나라도 있으면 true — 셸이든
    /// 에이전트든 떠 있는 것 자체가 실행 중이다. 이런 workspace는 Suspended(워커
    /// shutdown = PTY kill)로 내리면 안 된다 (2026-07-05 사용자 요구: 진행 중인
    /// 에이전트 작업이 경고 없이 죽는 문제).
    ///
    /// tracker 외 두 가지를 추가로 live 취급한다 (codex High — spawn/restore race):
    /// - 응답 대기 중인 셸 spawn (명령이 큐/워커에 있고 MuxUpdated가 아직 안 옴)
    /// - 워커 생성 직후 첫 MuxUpdated 관측 전의 유예 창 (RestoreWorkspace 복원 세션이
    ///   아직 이벤트로 안 왔을 수 있다 — 빈 workspace는 restore가 emit하지 않으므로
    ///   유예가 끝나면 정상적으로 suspend 가능해진다)
    fn has_live_sessions(&self) -> bool {
        workspace_is_live(
            self.live.has_live(),
            self.live.seen_mux,
            self.workspace_ui.pending_spawns() + self.pending_agent_spawns,
            self.created.elapsed(),
        )
    }
}

/// suspend 보호의 live 판정 (순수 함수 — 테스트 용이).
fn workspace_is_live(
    tracker_live: bool,
    seen_mux: bool,
    pending_spawns: u32,
    age: std::time::Duration,
) -> bool {
    /// 첫 MuxUpdated 관측 전 suspend를 미루는 유예 — restore 이벤트 전파(수 ms)보다
    /// 넉넉히. 빈 workspace는 이 유예만 지나면 suspend 대상이 된다.
    const RESTORE_GRACE: std::time::Duration = std::time::Duration::from_secs(10);
    tracker_live || pending_spawns > 0 || (!seen_mux && age < RESTORE_GRACE)
}

/// 이벤트 스트림에서 "pane에 붙어 있고 아직 Exited 안 된 세션"을 추적한다.
/// MuxUpdated가 세션 집합의 근거, SessionExited가 종료 마킹 — 이벤트 순서대로
/// 갱신해 한 drain 안의 Exited → pane 제거 MuxUpdated 시퀀스도 정확히 반영된다.
#[derive(Default)]
struct LiveSessionTracker {
    /// 최신 MuxUpdated 기준 pane에 붙은 세션 집합.
    mux_sessions: std::collections::HashSet<runtime::SessionId>,
    /// SessionExited를 관측한 세션 (mux_sessions에 남은 것만 유지해 유계).
    exited_sessions: std::collections::HashSet<runtime::SessionId>,
    /// MuxUpdated를 한 번이라도 관측했다 — 관측 전에는 restore 유예가 적용된다.
    seen_mux: bool,
}

impl LiveSessionTracker {
    fn observe(&mut self, event: &runtime::RuntimeEvent) {
        match event {
            runtime::RuntimeEvent::MuxUpdated { snapshot } => {
                self.seen_mux = true;
                self.mux_sessions = snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .filter_map(|pane| pane.session_id)
                    .collect();
                self.exited_sessions
                    .retain(|s| self.mux_sessions.contains(s));
            }
            runtime::RuntimeEvent::SessionExited { session, .. } => {
                self.exited_sessions.insert(*session);
            }
            _ => {}
        }
    }

    fn has_live(&self) -> bool {
        self.mux_sessions
            .iter()
            .any(|s| !self.exited_sessions.contains(s))
    }
}

/// 실행 중인 remote TLS 서버 + 그 신원 지문(attach 클라이언트 대조용).
/// 원격 worker는 server가 소유(move)한다 — 활성 workspace worker와 별개의 전용 worker라
/// 수명이 서로 얽히지 않는다. Drop/shutdown이 accept 루프·접속·worker를 모두 정리한다.
struct RemoteTlsState {
    server: runtime::RemoteRuntimeServer,
    fingerprint: String,
}

pub struct App {
    config: Config,
    config_path: PathBuf,
    settings_open: bool,
    /// 통합 설정 창의 선택된 카테고리.
    settings_category: ui::settings::Category,
    settings_search: String,
    db: Db,
    secret_store: KeyringSecretStore,
    agents_ui: ui::agents::AgentsUi,
    connectors_ui: ui::connectors::ConnectorsUi,
    credentials_ui: ui::credentials::CredentialsUi,
    env_profiles_ui: ui::env_profiles::EnvProfilesUi,
    activity_ui: ui::activity::ActivityUi,
    notifications_ui: ui::notifications::NotificationsUi,
    /// agent-proxy 승인 팝업 (option 1.5). proxy가 DB에 쓴 pending 행을 폴링해 표시한다.
    approvals_ui: ui::approvals::ApprovalsUi,
    /// watcher가 pending approval 목록 변화를 감지하면 logic()이 한 번만 DB를 읽게 하는 플래그.
    approval_poll_requested: Arc<AtomicBool>,
    /// 외부 proxy가 DB에 쓴 pending approval 변화를 감지해 UI를 깨운다.
    approval_watcher: ApprovalWatcher,
    /// 마지막 오프스크린 창 위치 보정 시각 (쿨다운용)
    last_offscreen_fix: std::time::Instant,
    /// 시작 시 창을 주 화면으로 1회 이동했다 (centered의 macOS 좌표 문제 우회)
    startup_positioned: bool,
    frame_stats: crate::perf::FrameStats,
    i18n: i18n::Catalog,
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
    /// 새 workspace 생성 시 함께 넣을 프로젝트 경로 (FT-0 — 빈 값 허용).
    new_workspace_path: String,
    /// 경로 편집 중인 workspace (id, 입력 버퍼) — 한 번에 한 행만.
    edit_ws_path: Option<(String, String)>,
    /// 삭제 확인 대기 중인 workspace id (2단계 확인 — 실수 방지)
    confirm_delete_ws: Option<String>,
    /// 알림 클릭으로 다른 workspace 전환 후, mux 재구성되면 이동할 (workspace, session).
    pending_focus: Option<(String, runtime::SessionId)>,
    /// 전환으로 background 정리 중인 옛 워커 shutdown 스레드들 (workspace_id, handle).
    /// 앱 종료 시 join(자식 reap 보장) + 같은 workspace 재오픈 전 직렬화(layout 경합 방지).
    pending_shutdowns: Vec<(String, std::thread::JoinHandle<()>)>,
    /// remote TLS 서버 (켜져 있을 때만 Some). 활성 workspace worker와 별개의 전용 worker를 노출.
    remote: Option<RemoteTlsState>,
    /// remote 시작 실패 시 settings에 표시할 에러 (best-effort — 앱은 계속, 크래시 금지).
    remote_error: Option<String>,
    /// settings의 토큰 표시(reveal) 토글. 토큰은 민감이라 기본 마스킹.
    remote_reveal_token: bool,
    /// known_hosts 표시 캐시 (settings 열 때 lazily 로드, 닫으면 None으로 리셋해 재로드).
    known_hosts_cache: Option<Vec<(String, String)>>,
    /// 폴더 트리 사이드바 (file-tree-design §6). OFF면 None — Panel 미생성 + 상태 drop(리소스 0).
    file_tree: Option<ui::file_tree::FileTreeUi>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mut config: Config,
        config_path: PathBuf,
        db: Db,
        workspace_id: String,
        logs_base: PathBuf,
        db_path: PathBuf,
        egui_ctx: egui::Context,
    ) -> Self {
        // output_batch_ms는 시작 시 고정, scrollback_lines는 세션 spawn 시점에 전달
        config.ui.last_workspace_id = Some(workspace_id.clone());
        let redaction = secret::RedactionService::new();
        let i18n = load_catalog(&config.i18n.locale);
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

        let approval_poll_requested = Arc::new(AtomicBool::new(false));
        let approval_watcher = ApprovalWatcher::spawn(
            db_path.clone(),
            egui_ctx.clone(),
            approval_poll_requested.clone(),
            std::time::Duration::from_millis(Self::APPROVAL_POLL_MS),
        );

        let mut app = Self {
            config,
            config_path,
            settings_open: false,
            settings_category: ui::settings::Category::default(),
            settings_search: String::new(),
            db,
            secret_store: KeyringSecretStore,
            agents_ui: ui::agents::AgentsUi::new(),
            connectors_ui: ui::connectors::ConnectorsUi::new(redaction.clone()),
            credentials_ui: ui::credentials::CredentialsUi::new(),
            env_profiles_ui: ui::env_profiles::EnvProfilesUi::new(),
            activity_ui: ui::activity::ActivityUi::new(),
            notifications_ui: ui::notifications::NotificationsUi::new(),
            approvals_ui: ui::approvals::ApprovalsUi::new(),
            approval_poll_requested,
            approval_watcher,
            last_offscreen_fix: std::time::Instant::now(),
            startup_positioned: false,
            active,
            warm: std::collections::HashMap::new(),
            warm_order: Vec::new(),
            frame_stats: crate::perf::FrameStats::new(),
            i18n,
            egui_ctx,
            db_path,
            logs_base,
            redaction,
            workspaces: Vec::new(),
            workspaces_open: false,
            new_workspace_name: String::new(),
            new_workspace_path: String::new(),
            edit_ws_path: None,
            confirm_delete_ws: None,
            pending_focus: None,
            pending_shutdowns: Vec::new(),
            remote: None,
            remote_error: None,
            remote_reveal_token: false,
            known_hosts_cache: None,
            file_tree: None,
        };
        app.prune_resolved_approvals();
        app.poll_pending_approvals();
        // 파일 트리 헤더(workspace 이름) 표시용 — 시작 시 1회 로드
        app.refresh_workspaces();
        if app.config.ui.file_tree_enabled {
            app.file_tree = Some(app.make_file_tree());
        }
        // 시작 시 config가 remote를 켜 뒀으면 best-effort로 기동한다 (실패는 log + settings 표시,
        // config는 그대로 두어 다음 실행에 재시도). 자동 시작은 config 저장을 유발하지 않는다.
        if app.config.remote.tls_enabled {
            match app.start_remote() {
                Ok(state) => app.remote = Some(state),
                Err(e) => {
                    tracing::warn!("remote TLS 자동 시작 실패: {e:#}");
                    app.remote_error = Some(format!("{e:#}"));
                }
            }
        }
        app
    }

    /// warm 상태로 유지할 최대 workspace 수 (활성 제외). 저-RAM 정책상 작게 — 초과분은
    /// Suspended(워커 shutdown). 단 **live 세션(미종료 셸/에이전트)이 있는 workspace는
    /// 상한과 무관하게 warm으로 유지**된다(작업 보호 > 메모리) — 그 경우 동시 워커 수는
    /// 사용자가 실제로 실행 중인 workspace 수까지 늘 수 있다.
    const MAX_WARM: usize = 2;
    /// Warm workspace가 이 시간 동안 재활성화되지 않으면 Suspended로 내린다. 세션/PTY는
    /// 종료되고 layout/session metadata만 DB에 남는다 (§14.1). live 세션이 있으면
    /// 시간이 지나도 내리지 않는다.
    const WARM_AUTO_SUSPEND_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 60);

    /// 승인 watcher 폴링 간격(ms). frame 예약은 하지 않고, pending 상태 변화 때만 UI를 깨운다.
    const APPROVAL_POLL_MS: u64 = 500;
    const RESOLVED_APPROVAL_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

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
        Self::seed_redaction(&runtime, db);
        WorkspaceRuntime {
            id: workspace_id.to_owned(),
            runtime,
            events: runtime_events,
            workspace_ui: ui::workspace::WorkspaceUi::new(),
            render_active: true,
            pending_events: Vec::new(),
            session_titles: std::collections::HashMap::new(),
            resource_usage: None,
            session_resource_usage: Vec::new(),
            input_pressure: None,
            backgrounded_at: None,
            live: LiveSessionTracker::default(),
            created: std::time::Instant::now(),
            pending_agent_spawns: 0,
        }
    }

    /// 저장된 credential id를 worker의 로그 redaction 대상으로 시드한다 (값 resolve는 worker).
    /// 활성 workspace worker와 remote 전용 worker가 공유하는 시드 로직.
    fn seed_redaction(runtime: &InProcessRuntimeClient, db: &Db) {
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
    }

    /// 앱 데이터 디렉터리 (db_path = `<data>/metadata.sqlite3` → parent). remote cert/known_hosts의 기준.
    fn data_dir(&self) -> &std::path::Path {
        self.db_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
    }

    /// remote TLS 서버 신원 인증서 경로 (`<data>/remote-tls.crt` — tls_identity 관례, 키는 keyring).
    fn cert_path(&self) -> PathBuf {
        self.data_dir().join("remote-tls.crt")
    }

    /// 클라이언트 측 known_hosts 파일 경로 (`<data>/known_hosts`).
    fn known_hosts_path(&self) -> PathBuf {
        self.data_dir().join("known_hosts")
    }

    /// remote TLS 서버를 기동한다: 신원 로드/생성 → 전용 원격 worker(비영속) → loopback bind.
    /// **원격 worker는 fresh empty 런타임**(원격 클라가 스스로 세션을 만든다) + PersistConfig=None
    /// (원격 세션은 영속하지 않는다). 실패는 Err — 호출측이 표시하고 앱은 계속(크래시 금지).
    fn start_remote(&self) -> anyhow::Result<RemoteTlsState> {
        let identity =
            runtime::tls_identity::get_or_create_identity(&self.secret_store, &self.cert_path())?;
        let fingerprint = identity.fingerprint();
        // 전용 원격 worker — logs는 logs_base/remote/ 하위(활성 workspace 로그와 분리).
        let worker = InProcessRuntimeClient::new(
            self.config.performance.output_batch_ms,
            Arc::new(KeyringSecretStore),
            self.logs_base.join("remote"),
            self.redaction.clone(),
            None, // 원격 세션은 영속하지 않는다
        );
        Self::seed_redaction(&worker, &self.db);
        let addr =
            std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, self.config.remote.port));
        // loopback 전용(allow_non_loopback=false) — 비-loopback 개방은 후속 UI(C-4 가드 유지).
        let server = runtime::RemoteRuntimeServer::serve_tls(worker, addr, identity, false)?;
        Ok(RemoteTlsState {
            server,
            fingerprint,
        })
    }

    /// settings 체크 on: 서버를 켜고 성공 시 config에 의도를 영속한다(다음 실행 자동 시작).
    fn remote_enable(&mut self) {
        match self.start_remote() {
            Ok(state) => {
                self.remote = Some(state);
                self.remote_error = None;
                self.config.remote.tls_enabled = true;
                if let Err(e) = self.config.save(&self.config_path) {
                    tracing::warn!("config 저장 실패: {e:#}");
                    // 서버는 켜졌지만 자동시작이 영속되지 않음 — 사용자에게 알린다.
                    self.remote_error = Some(format!(
                        "설정 저장 실패 — 다음 실행엔 자동시작 안 됨: {e:#}"
                    ));
                }
            }
            Err(e) => {
                tracing::warn!("remote TLS 시작 실패: {e:#}");
                self.remote_error = Some(format!("{e:#}"));
            }
        }
    }

    /// settings 체크 off: 서버를 정지(Drop이 accept/접속/worker 정리)하고 config에 영속한다.
    fn remote_disable(&mut self) {
        if let Some(state) = self.remote.take() {
            state.server.shutdown();
        }
        self.remote_error = None;
        self.config.remote.tls_enabled = false;
        if let Err(e) = self.config.save(&self.config_path) {
            tracing::warn!("config 저장 실패: {e:#}");
            // 저장 실패를 조용히 넘기면 config.toml에 tls_enabled=true가 남아, 사용자가 껐다고
            // 생각한 원격 서버(셸 접근 동등)가 다음 실행에 다시 자동시작된다 — 표면화 (codex P2).
            self.remote_error = Some(format!(
                "서버는 껐지만 설정 저장 실패 — 다음 실행에 다시 켜질 수 있습니다: {e:#}"
            ));
        }
    }

    /// known_hosts 파일을 (host, 지문) 목록으로 로드한다 (표시 전용 — 파일 없으면 빈 목록).
    fn load_known_hosts(&self) -> Vec<(String, String)> {
        let path = self.known_hosts_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => parse_known_hosts(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                tracing::warn!("known_hosts 읽기 실패: {e:#}");
                Vec::new()
            }
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
        new_active.backgrounded_at = None;
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
        old.backgrounded_at = Some(std::time::Instant::now());
        let old_id = old.id.clone();
        self.warm.insert(old_id.clone(), old);
        self.warm_order.push(old_id.clone());

        // pending 상태 정리 (이전 워커 응답 못 받음, 교차-ws 감사 방지).
        // notifications는 리셋하지 않는다 — (ws, session)로 namespacing돼 전역 센터가
        // 모든 workspace 알림을 유지한다 (background 완료 통지·클릭 이동, codex 리뷰).
        // agent spawn 대기는 버리지 않고 물러난 workspace로 이관 — 응답이 오기 전까지
        // 그 workspace를 live로 취급해 suspend가 새 PTY를 죽이는 창을 막는다 (codex).
        let pending_agents = self.agents_ui.take_pending();
        if let Some(old_rt) = self.warm.get_mut(&old_id) {
            old_rt.pending_agent_spawns += pending_agents;
        }
        self.connectors_ui.clear_invoke();
        // 파일 트리 루트를 새 workspace path로 갱신 (FT-1)
        self.config.ui.last_workspace_id = Some(target_id.to_owned());
        if let Err(e) = self.config.save(&self.config_path) {
            tracing::warn!("마지막 workspace 저장 실패: {e:#}");
        }
        self.refresh_file_tree_root();
        self.egui_ctx.request_repaint();

        self.evict_warm();
    }

    /// warm 풀이 MAX_WARM을 넘으면 가장 오래된 것부터 Suspended로 내린다 (워커 shutdown,
    /// 세션 종료 — §14.1 Suspended). background 스레드에서 정리하고 on_exit에서 join.
    /// **live 세션(미종료 셸/에이전트)이 있는 workspace는 축출하지 않는다** — 진행 중
    /// 작업을 경고 없이 kill하지 않기 위해 상한 초과를 허용한다 (메모리 < 작업 보호).
    fn evict_warm(&mut self) {
        let evictable = warm_eviction_candidates(&self.warm_order, Self::MAX_WARM, |id| {
            self.warm.get(id).is_some_and(|rt| rt.has_live_sessions())
        });
        for evict_id in evictable {
            self.warm_order.retain(|id| id != &evict_id);
            self.suspend_warm_workspace(&evict_id);
        }
    }

    fn evict_idle_warm(&mut self, now: std::time::Instant) {
        let expired = expired_warm_workspace_ids(
            &self.warm_order,
            |id| self.warm.get(id).and_then(|rt| rt.backgrounded_at),
            now,
            Self::WARM_AUTO_SUSPEND_AFTER,
        );
        for id in expired {
            // live 세션이 있으면 시간이 지나도 suspend하지 않는다 (작업 보호).
            if self.warm.get(&id).is_some_and(|rt| rt.has_live_sessions()) {
                continue;
            }
            self.warm_order.retain(|warm_id| warm_id != &id);
            self.suspend_warm_workspace(&id);
        }
    }

    fn suspend_warm_workspace(&mut self, workspace_id: &str) {
        if let Some(mut rt) = self.warm.remove(workspace_id) {
            // 마지막으로 큐에 남은 이벤트를 처리해 방금 끝난 background 작업의 완료/오류
            // 알림을 놓치지 않는다 (codex 리뷰 — 축출 시 receiver drop으로 유실되던 것).
            let events = rt.events.drain();
            Self::record_activity_events(&mut rt, &events);
            Self::process_ws_notifications(
                &mut self.notifications_ui,
                workspace_id,
                &events,
                &mut rt.session_titles,
                &self.i18n,
            );
            // 최종 방어: 마지막 drain에서 새 spawn이 관측됐을 수 있다 — live 세션이
            // 있으면 suspend를 취소하고 warm으로 되돌린다 (워커/PTY 유지).
            if rt.has_live_sessions() {
                tracing::info!(
                    workspace_id,
                    "suspend 취소 — 실행 중 세션이 있어 warm 유지 (작업 보호)"
                );
                // drain한 lifecycle 이벤트를 replay 큐에 보존 — 버리면 재활성 시
                // exit/status 상태가 UI에 재구성되지 않는다 (codex Medium).
                rt.pending_events.extend(events);
                self.warm.insert(workspace_id.to_owned(), rt);
                self.warm_order.push(workspace_id.to_owned());
                return;
            }
            // 축출 = Suspended(워커 종료) — 그 workspace의 진행형 알림은 더는 조치
            // 불가하므로 정리한다 (결과 알림은 기록이라 유지, codex 리뷰).
            self.notifications_ui.prune_transient(workspace_id);
            self.pending_shutdowns.retain(|(_, h)| !h.is_finished());
            let evict_id = workspace_id.to_owned();
            let handle = std::thread::spawn(move || {
                let mut runtime = rt.runtime;
                let _ = runtime.send_command(runtime::RuntimeCommand::SetWorkspaceState(
                    runtime::WorkspaceRuntimeState::Suspended,
                ));
                runtime.shutdown();
            });
            self.pending_shutdowns.push((evict_id, handle));
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

    /// 활성 workspace의 트리 루트 (path 미설정/조회 실패 → None → 안내 표시 §9-2).
    fn active_tree_root(&self) -> Option<PathBuf> {
        match self.db.workspace_path(&self.active.id) {
            Ok(path) => Self::workspace_path_to_tree_root(path),
            Err(e) => {
                tracing::warn!("workspace 경로 조회 실패: {e:#}");
                None
            }
        }
    }

    fn workspace_path_to_tree_root(path: Option<String>) -> Option<PathBuf> {
        path.and_then(|path| (!path.trim().is_empty()).then(|| PathBuf::from(path)))
    }

    /// 활성 workspace 기준으로 파일 트리 상태를 새로 만든다 (ON 전환/루트 변경 시).
    fn make_file_tree(&self) -> ui::file_tree::FileTreeUi {
        let mut tree = ui::file_tree::FileTreeUi::new(self.egui_ctx.clone());
        // 앱 자신의 data dir(로그·DB·cert 등) 이벤트는 무시 — 로그 쓰기가 워처로 돌아와
        // 리페인트를 유발하는 자기-루프 차단 (workspace 루트가 홈 등 넓은 경로일 때).
        if let Some(data_dir) = self.db_path.parent() {
            tree.set_watch_ignore(vec![data_dir.to_path_buf()]);
        }
        tree.set_root(self.active_tree_root());
        tree
    }

    /// 활성 workspace의 트리 루트가 바뀌었을 수 있을 때 (전환/경로 저장) 트리를 재구성한다.
    fn refresh_file_tree_root(&mut self) {
        if self.file_tree.is_some() {
            self.file_tree = Some(self.make_file_tree());
        }
    }

    fn refresh_workspaces(&mut self) {
        match self.db.list_workspaces() {
            Ok(list) => self.workspaces = list,
            Err(e) => tracing::warn!("workspace 목록 조회 실패: {e:#}"),
        }
    }

    fn activity_rows(&self) -> Vec<ui::activity::ActivityWorkspaceRow> {
        let now = std::time::Instant::now();
        self.workspaces
            .iter()
            .map(|ws| {
                if ws.id == self.active.id {
                    return ui::activity::ActivityWorkspaceRow {
                        id: ws.id.clone(),
                        name: ws.name.clone(),
                        state: ui::activity::ActivityWorkspaceState::Active,
                        session_count: self.active.workspace_ui.session_entries(&self.i18n).len(),
                        pending_events: self.active.pending_events.len(),
                        input_pressure: self.active.input_pressure.clone(),
                        backgrounded_for_secs: None,
                        auto_suspend_remaining_secs: None,
                        resource: self.active.resource_usage,
                        session_resources: self.active.session_resource_usage.clone(),
                    };
                }
                if let Some(rt) = self.warm.get(&ws.id) {
                    let elapsed = rt
                        .backgrounded_at
                        .map(|at| now.saturating_duration_since(at));
                    let remaining = elapsed.map(|duration| {
                        Self::WARM_AUTO_SUSPEND_AFTER
                            .as_secs()
                            .saturating_sub(duration.as_secs())
                    });
                    return ui::activity::ActivityWorkspaceRow {
                        id: ws.id.clone(),
                        name: ws.name.clone(),
                        state: ui::activity::ActivityWorkspaceState::Warm,
                        session_count: rt.session_titles.len(),
                        pending_events: rt.pending_events.len(),
                        input_pressure: rt.input_pressure.clone(),
                        backgrounded_for_secs: elapsed.map(|duration| duration.as_secs()),
                        auto_suspend_remaining_secs: remaining,
                        resource: rt.resource_usage,
                        session_resources: rt.session_resource_usage.clone(),
                    };
                }
                ui::activity::ActivityWorkspaceRow {
                    id: ws.id.clone(),
                    name: ws.name.clone(),
                    state: ui::activity::ActivityWorkspaceState::Suspended,
                    session_count: 0,
                    pending_events: 0,
                    input_pressure: None,
                    backgrounded_for_secs: None,
                    auto_suspend_remaining_secs: None,
                    resource: None,
                    session_resources: Vec::new(),
                }
            })
            .collect()
    }

    /// 워크스페이스 목록 창: 전환/생성. 전환은 워커 shutdown+recreate라 창 closure 밖에서.
    fn workspaces_window(&mut self, ctx: &egui::Context, catalog: &i18n::Catalog) {
        if !self.workspaces_open {
            return;
        }
        let mut open = true;
        let mut switch_to: Option<String> = None;
        let mut delete_id: Option<String> = None;
        let mut set_confirm: Option<String> = None;
        let mut cancel_confirm = false;
        let mut create = false;
        // 경로 편집 상태를 잠시 꺼내 로컬로 다룬다 (workspaces 순회와 동시 &mut 회피)
        let mut edit_ws_path = self.edit_ws_path.take();
        let mut save_path: Option<(String, String)> = None;
        let deletable = self.workspaces.len() > 1; // 마지막 workspace는 삭제 불가
        egui::Window::new(catalog.t("workspace.manager.title", &[]))
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                for ws in &self.workspaces {
                    ui.horizontal(|ui| {
                        if ws.id == self.active.id {
                            ui.strong(&ws.name);
                            ui.weak(catalog.t("workspace.manager.current", &[]));
                        } else {
                            ui.label(&ws.name);
                            if ui
                                .button(catalog.t("workspace.manager.switch", &[]))
                                .clicked()
                            {
                                switch_to = Some(ws.id.clone());
                            }
                            // 삭제 (활성/마지막 제외) — 2단계 확인
                            if deletable {
                                if self.confirm_delete_ws.as_deref() == Some(ws.id.as_str()) {
                                    ui.colored_label(
                                        egui::Color32::RED,
                                        catalog.t("workspace.manager.delete_confirm", &[]),
                                    );
                                    if ui.button(catalog.t("action.delete", &[])).clicked() {
                                        delete_id = Some(ws.id.clone());
                                    }
                                    if ui.button(catalog.t("action.cancel", &[])).clicked() {
                                        cancel_confirm = true;
                                    }
                                } else if ui.button(catalog.t("action.delete", &[])).clicked() {
                                    set_confirm = Some(ws.id.clone());
                                }
                            }
                        }
                    });
                    // 프로젝트 경로 (FT-0): 표시 + 인라인 편집. 파일 트리의 루트가 된다.
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        match &mut edit_ws_path {
                            Some((id, buffer)) if *id == ws.id => {
                                ui.label(catalog.t("workspace.manager.path", &[]));
                                ui.add(
                                    egui::TextEdit::singleline(buffer)
                                        .hint_text("/path/to/project")
                                        .desired_width(260.0),
                                );
                                if ui.button(catalog.t("action.save", &[])).clicked() {
                                    save_path = Some((id.clone(), buffer.trim().to_owned()));
                                    edit_ws_path = None;
                                } else if ui.button(catalog.t("action.cancel", &[])).clicked() {
                                    edit_ws_path = None;
                                }
                            }
                            _ => {
                                if ws.path.is_empty() {
                                    ui.weak(catalog.t("workspace.manager.path_unset", &[]));
                                } else {
                                    ui.weak(&ws.path);
                                }
                                if ui
                                    .small_button(catalog.t("workspace.manager.edit_path", &[]))
                                    .clicked()
                                {
                                    edit_ws_path = Some((ws.id.clone(), ws.path.clone()));
                                }
                            }
                        }
                    });
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(catalog.t("workspace.manager.new_workspace", &[]));
                    ui.text_edit_singleline(&mut self.new_workspace_name);
                    if ui.button(catalog.t("action.new", &[])).clicked() {
                        create = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(catalog.t("workspace.manager.project_path", &[]));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_workspace_path)
                            .hint_text(catalog.t("workspace.manager.project_path_hint", &[]))
                            .desired_width(260.0),
                    );
                });
            });
        self.workspaces_open = open;
        self.edit_ws_path = edit_ws_path;

        if let Some((id, path)) = save_path {
            match self.db.set_workspace_path(&id, &path) {
                Ok(()) => {
                    self.refresh_workspaces();
                    // 활성 workspace의 경로가 바뀌면 트리 루트도 갱신 (FT-1)
                    if id == self.active.id {
                        self.refresh_file_tree_root();
                    }
                }
                Err(e) => tracing::warn!("workspace 경로 저장 실패: {e:#}"),
            }
        }

        if create {
            let name = self.new_workspace_name.trim().to_owned();
            if !name.is_empty() {
                match self.db.create_workspace(&name) {
                    Ok(id) => {
                        // 경로가 입력됐으면 함께 저장 (FT-0 — 실패해도 생성은 유지)
                        let path = self.new_workspace_path.trim();
                        if !path.is_empty()
                            && let Err(e) = self.db.set_workspace_path(&id, path)
                        {
                            tracing::warn!("workspace 경로 저장 실패: {e:#}");
                        }
                        self.new_workspace_name.clear();
                        self.new_workspace_path.clear();
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
        catalog: &i18n::Catalog,
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
                            session_titles.insert(
                                session,
                                ui::workspace::display_pane_title(&pane.title, catalog),
                            );
                        }
                    }
                }
                runtime::RuntimeEvent::SessionStatusChanged { session, status } => {
                    if let Some(title) = session_titles.get(session).cloned() {
                        notifications.on_status(workspace_id, *session, *status, &title, catalog);
                    }
                }
                // regex 없는 agent는 결과가 SessionExited로만 온다 (완료 기준: done/error)
                runtime::RuntimeEvent::SessionExited { session, exit_code } => {
                    if let Some(title) = session_titles.get(session).cloned() {
                        notifications.on_exit(workspace_id, *session, *exit_code, &title, catalog);
                    }
                    session_titles.remove(session);
                }
                _ => {}
            }
        }
    }

    fn record_activity_events(rt: &mut WorkspaceRuntime, events: &[runtime::RuntimeEvent]) {
        for event in events {
            if let runtime::RuntimeEvent::ResourceUsage {
                snapshot,
                session_usage,
            } = event
            {
                rt.resource_usage = Some(*snapshot);
                rt.session_resource_usage = session_usage.clone();
            }
            if let runtime::RuntimeEvent::PtyInputPressure { pressure, .. } = event {
                rt.input_pressure = Some(pressure.clone());
            }
            // live 세션 추적 (suspend 보호)
            rt.live.observe(event);
            // 이관받은 agent spawn 대기 해소 (성공/실패 어느 쪽이든 응답 도착)
            if matches!(
                event,
                runtime::RuntimeEvent::AgentSpawned { .. }
                    | runtime::RuntimeEvent::SpawnFailed {
                        kind: runtime::SpawnKind::Agent,
                        ..
                    }
            ) {
                rt.pending_agent_spawns = rt.pending_agent_spawns.saturating_sub(1);
            }
        }
    }

    fn poll_pending_approvals(&mut self) {
        match self.db.list_pending_approvals() {
            Ok(rows) => self.approvals_ui.set_pending(rows),
            Err(e) => tracing::warn!("승인 목록 조회 실패: {e:#}"),
        }
    }

    fn prune_resolved_approvals(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        match self
            .db
            .prune_resolved_approvals(now.saturating_sub(Self::RESOLVED_APPROVAL_RETENTION_SECS))
        {
            Ok(n) if n > 0 => tracing::info!("resolved MCP approval {n}건 정리"),
            Ok(_) => {}
            Err(e) => tracing::warn!("resolved MCP approval 정리 실패: {e:#}"),
        }
    }
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        self.approval_watcher.stop();
        // remote TLS 서버를 먼저 정지 — accept 루프·접속·전용 worker(그 세션들 reap)를 정리한다.
        if let Some(state) = self.remote.take() {
            state.server.shutdown();
        }
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
        // macOS 네이티브 메뉴 이벤트 (main.rs install_macos_menu)
        #[cfg(target_os = "macos")]
        while let Ok(event) = muda::MenuEvent::receiver().try_recv() {
            if event.id() == "settings" {
                self.settings_open = true;
            }
        }

        // 오프스크린 방어: 실행 중 외부 모니터가 분리되면 창이 존재하지 않는 좌표에
        // 남아 "죽은 것처럼" 보인다 (2026-07-05 실증). 창이 어느 모니터에도 속하지
        // 않으면(monitor_size None — macOS는 완전 오프스크린 창의 screen이 nil)
        // 주 화면 안으로 옮긴다. 쿨다운 2s — 이동 반영 전 재발사 방지.
        let offscreen =
            ctx.input(|i| i.viewport().outer_rect.is_some() && i.viewport().monitor_size.is_none());
        if offscreen && self.last_offscreen_fix.elapsed() >= std::time::Duration::from_secs(2) {
            self.last_offscreen_fix = std::time::Instant::now();
            tracing::warn!("창이 화면 밖 — 주 화면으로 이동");
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(80.0, 80.0)));
        }
        // 시작 위치 강제: 항상 주 화면에 뜬다. NativeOptions.centered는 주 화면 크기로
        // 계산한 좌표를 macOS winit이 보조 모니터 로컬 좌표로 적용하는 문제가 있어
        // (2026-07-05 실증: (255,137) 지정 → 왼쪽 모니터 -2303) 창 생성 후 런타임
        // 명령으로 1회 이동한다 — 이 경로는 글로벌 좌표로 동작한다. 이후 사용자가
        // 옮기는 위치는 존중(1회뿐, persist_window=false라 다음 시작도 여기부터).
        if !self.startup_positioned && ctx.input(|i| i.viewport().outer_rect.is_some()) {
            self.startup_positioned = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                120.0, 60.0,
            )));
            // 이동 직후 key window 상태가 흔들려 키 입력이 일시적으로 안 먹는 사례
            // (2026-07-05 사용자 보고) — 창 포커스를 명시 재요청한다.
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

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
                Self::record_activity_events(rt, &events);
                // warm workspace도 알림은 만든다 (background 완료/승인 통지) — (ws, session)로
                // 식별해 워커 간 SessionId 충돌을 피한다. 렌더용으로는 pending에 누적.
                Self::process_ws_notifications(
                    &mut self.notifications_ui,
                    &rt.id,
                    &events,
                    &mut rt.session_titles,
                    &self.i18n,
                );
                rt.pending_events.extend(events);
                // MuxUpdated는 매번 전체 스냅샷이라 오래된 건 최신에 완전히 대체된다.
                // chatty한 warm 워커가 pending_events를 무한 누적하지 않도록 최신 하나만
                // 남기고 합친다 (lifecycle/Viewport는 순서대로 보존 — replay 정확성).
                // 새 이벤트가 들어온 이 분기에서만 호출돼 프레임마다 도는 걸 피한다.
                coalesce_mux_updated(&mut rt.pending_events);
            }
        }
        self.evict_idle_warm(std::time::Instant::now());

        // 이벤트 drain + 알림 생성은 non-render 경로인 여기서 한다 (§14.1 Warm:
        // ui()가 스킵돼도 승인/완료/실패 알림은 유지). worker의 wake가 숨겨진 UI를
        // 깨워 이 logic()을 돌린다. 렌더용으로는 pending_events에 쌓아 ui()가 소비한다.
        let new_events = self.active.events.drain();
        if !new_events.is_empty() {
            Self::record_activity_events(&mut self.active, &new_events);
            Self::process_ws_notifications(
                &mut self.notifications_ui,
                &self.active.id,
                &new_events,
                &mut self.active.session_titles,
                &self.i18n,
            );
            self.active.pending_events.extend(new_events);
            // 창이 숨겨져(render_active=false) ui()가 스킵되면 active의 pending도 warm처럼
            // 무한 누적된다 — 동일하게 coalesce로 유계화한다. 보일 때는 ui()가 매 프레임
            // take()로 소비해 자라지 않으므로 coalesce가 불필요하다.
            if !self.active.render_active {
                coalesce_mux_updated(&mut self.active.pending_events);
            }
            // 보이는 idle 상태에서도 새 출력/상태를 즉시 렌더하도록 프레임 예약
            ctx.request_repaint();
        }

        if self.approval_poll_requested.swap(false, Ordering::AcqRel) {
            self.poll_pending_approvals();
        }
    }

    // egui 0.35부터 update(&Context) 대신 ui(&mut Ui) 시그니처를 쓴다.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame_stats.begin();
        let text = self.i18n.clone();
        let mut unread_before = 0;
        egui::Panel::top("top_bar").resizable(false).show(ui, |ui| {
            // 타이틀바 영역 통합 (2026-07-06): fullsize content view로 이 바가 macOS
            // 타이틀바까지 확장된다. 패널 크기는 버튼 높이에 맞춰 자동 — exact_size로
            // 강제하면 버튼이 얇은 띠에 클리핑됐다(2026-07-06 사용자 화면). 위아래
            // 여백으로 신호등(y~14) 높이에 맞춰 세로 중앙 정렬한다.
            // 빈 곳을 잡으면 창을 드래그로 옮긴다. auto-sized Panel의 max_rect는
            // content 측정 전 매우 커질 수 있으므로 실제 titlebar 높이만 hit-test한다.
            let bar_rect =
                egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), 34.0));
            let drag = ui.interact(
                bar_rect,
                egui::Id::new("titlebar_drag"),
                egui::Sense::click_and_drag(),
            );
            if drag.drag_started_by(egui::PointerButton::Primary) {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            ui.add_space(7.0);
            ui.horizontal(|ui| {
                // 신호등(닫기/최소화/전체화면) 폭만큼 왼쪽 여백 — macOS.
                #[cfg(target_os = "macos")]
                ui.add_space(72.0);
                // 프레임 없는 텍스트 버튼 — 버튼마다 박스가 생기면 "라인 여러 개"로
                // 보인다 (2026-07-06 사용자). hover 시에만 옅은 배경.
                let tbtn = |ui: &mut egui::Ui, label: String| {
                    ui.add(egui::Button::new(label).frame(false)).clicked()
                };
                if tbtn(ui, text.t("top.settings", &[])) {
                    self.settings_open = !self.settings_open;
                }
                if tbtn(ui, text.t("top.credentials", &[])) {
                    self.credentials_ui.toggle();
                }
                if tbtn(ui, text.t("top.connectors", &[])) {
                    self.connectors_ui.toggle();
                }
                if tbtn(ui, text.t("top.environment", &[])) {
                    self.env_profiles_ui.toggle();
                }
                if tbtn(ui, text.t("top.agents", &[])) {
                    self.agents_ui.toggle();
                }
                if tbtn(ui, text.t("top.workspaces", &[])) {
                    self.workspaces_open = !self.workspaces_open;
                    if self.workspaces_open {
                        self.refresh_workspaces();
                    }
                }
                if tbtn(ui, text.t("top.activity", &[])) {
                    self.activity_ui.toggle();
                    self.refresh_workspaces();
                }
                let unread = self.notifications_ui.unread();
                unread_before = unread;
                let label = if unread > 0 {
                    let count = unread.to_string();
                    text.t("top.notifications.unread", &[("count", &count)])
                } else {
                    text.t("top.notifications", &[])
                };
                if tbtn(ui, label) {
                    // 열면 모두 읽음 → 배지가 이미 그려진 뒤라 다음 프레임에 갱신
                    self.notifications_ui.toggle();
                    ui.ctx().request_repaint();
                }
                // 우측: 로케일 · 메모리 (목업의 'ko · 113MB').
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let locale_short = self
                        .config
                        .i18n
                        .locale
                        .split('-')
                        .next()
                        .unwrap_or(&self.config.i18n.locale);
                    let label = match self.active.resource_usage {
                        Some(r) => format!("{locale_short} · {}MB", r.rss_bytes / (1024 * 1024)),
                        None => locale_short.to_owned(),
                    };
                    ui.weak(label);
                });
            });
            ui.add_space(6.0);
            // 툴바-본문 경계선 하나 (픽셀 스냅 헤어라인).
            crate::ui::hairline(ui);
        });

        // 폴더 트리 사이드바 (FT-1) — CentralPanel보다 먼저 배치해야 한다 (§9-1).
        // OFF(None)면 Panel 자체를 만들지 않는다 (§6 리소스 0).
        if self.file_tree.is_some() {
            let sessions = self.active.workspace_ui.session_entries(&text);
            let sidebar_action = self
                .file_tree
                .as_mut()
                .and_then(|tree| tree.panel(ui, &sessions, &text));
            match sidebar_action {
                // "터미널에 경로 삽입" (FT-3): 포커스된 pane의 세션에 WriteInput —
                // 파일 트리의 유일한 runtime 접점 (§6).
                Some(ui::file_tree::SidebarAction::InsertPath(path)) => {
                    let session = self.active.workspace_ui.mux().and_then(|mux| {
                        mux.focused_pane.as_ref().and_then(|focused| {
                            mux.tabs
                                .iter()
                                .flat_map(|tab| &tab.panes)
                                .find(|pane| &pane.id == focused)
                                .and_then(|pane| pane.session_id)
                        })
                    });
                    match session {
                        Some(session) => {
                            let bracketed =
                                self.active.workspace_ui.session_bracketed_paste(session);
                            let shell_kind = self.active.workspace_ui.session_shell_kind(session);
                            let bytes = ui::workspace::path_insert_paste_bytes(
                                &path, shell_kind, bracketed,
                            );
                            if let Err(e) = self.active.runtime.send_command(
                                runtime::RuntimeCommand::WriteInput { session, bytes },
                            ) {
                                tracing::warn!("경로 삽입 실패: {e:#}");
                            }
                        }
                        None => tracing::info!("경로 삽입: 활성 터미널 세션 없음 — 무시"),
                    }
                }
                // 세션 목록 클릭 — 해당 tab/pane으로 전환 (workspace 사이드바)
                Some(ui::file_tree::SidebarAction::FocusSession { tab, pane }) => {
                    let is_active_tab = self
                        .active
                        .workspace_ui
                        .mux()
                        .and_then(|m| m.active_tab.clone())
                        == Some(tab.clone());
                    if !is_active_tab
                        && let Err(e) = self
                            .active
                            .runtime
                            .send_command(runtime::RuntimeCommand::SelectTab { tab })
                    {
                        tracing::warn!("탭 전환 실패: {e:#}");
                    }
                    if let Err(e) = self
                        .active
                        .runtime
                        .send_command(runtime::RuntimeCommand::FocusPane { pane })
                    {
                        tracing::warn!("pane 포커스 실패: {e:#}");
                    }
                }
                // 사이드바 + 버튼 — 새 셸 (탭바 제거 후 대체 진입점)
                Some(ui::file_tree::SidebarAction::NewShell) => {
                    self.active.workspace_ui.spawn_shell(
                        &self.active.runtime,
                        self.config.terminal.scrollback_lines as usize,
                    );
                }
                None => {}
            }
        }

        // 워크스페이스 전환/생성 (switch는 워커 shutdown+recreate라 window closure 밖에서)
        self.workspaces_window(ui.ctx(), &text);
        let activity_rows = self.activity_rows();
        if let Some(action) = self.activity_ui.show(ui.ctx(), &text, &activity_rows) {
            match action {
                ui::activity::ActivityAction::SwitchWorkspace(id) if id != self.active.id => {
                    self.switch_workspace(&id);
                    self.refresh_workspaces();
                }
                ui::activity::ActivityAction::SwitchWorkspace(_) => {}
            }
        }

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
            &self.db_path,
            &text,
        );
        {
            let credential_service = AppCredentialService {
                db: &self.db,
                secret_store: &self.secret_store,
                redaction: &self.redaction,
            };
            self.credentials_ui
                .show(ui.ctx(), &credential_service, &text);
        }
        let credential_added = {
            let oauth_store = AppOAuthCredentialStore {
                secret_store: &self.secret_store,
                redaction: &self.redaction,
            };
            let mcp_env_resolver = AppMcpScopedEnvResolver {
                secret_store: &self.secret_store,
                redaction: &self.redaction,
            };
            self.connectors_ui.show(
                ui.ctx(),
                &mut self.db,
                &self.active.id,
                &oauth_store,
                &mcp_env_resolver,
                &text,
            )
        };
        if credential_added {
            // OAuth로 credential이 추가됨 — 자격증명 창은 이번 프레임에 이미
            // 그려졌으므로 캐시 무효화 후 다음 프레임을 예약해 즉시 반영한다
            self.credentials_ui.invalidate_cache();
            ui.ctx().request_repaint();
        }
        self.env_profiles_ui
            .show(ui.ctx(), &mut self.db, &self.active.id, &text);
        egui::CentralPanel::default().show(ui, |ui| {
            self.active.workspace_ui.show(
                ui,
                &self.config.terminal,
                &self.active.runtime,
                &events,
                &text,
            );
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
            // retain_sessions가 배지 그리기 이후 unread를 줄였다면 다음 프레임에 재반영
            if self.notifications_ui.unread() != unread_before {
                ui.ctx().request_repaint();
            }
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
        if let Some((ws_id, session)) = self.notifications_ui.show(ui.ctx(), &text) {
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

        // agent-proxy 승인 팝업 (option 1.5). logic()이 폴링해 넣어둔 pending 중 가장
        // 오래된 하나를 모달로 띄운다. 버튼을 누르면 결정을 DB에 되쓴다.
        if let Some(decision) = self.approvals_ui.show(ui.ctx(), &text) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            // proxy가 타임아웃으로 먼저 해소했으면 행이 사라졌을 수 있다 —
            // resolve_approval은 first-writer-wins라 그 경우 조용한 no-op(안전).
            if let Err(e) =
                self.db
                    .resolve_approval(&decision.id, decision.allowed, decision.remember, now)
            {
                tracing::warn!("승인 해소 실패: {e:#}");
            }
            self.prune_resolved_approvals();
            // 해소 직후 목록을 갱신해 다음 항목이 바로 뜨게 한다 (다음 폴링을 기다리지 않음).
            match self.db.list_pending_approvals() {
                Ok(rows) => self.approvals_ui.set_pending(rows),
                Err(e) => tracing::warn!("승인 목록 조회 실패: {e:#}"),
            }
            ui.ctx().request_repaint();
        }

        // known_hosts는 settings 열 때 lazily 로드한다 (닫으면 아래서 None으로 리셋 → 재로드).
        if self.settings_open && self.known_hosts_cache.is_none() {
            let kh = self.load_known_hosts();
            self.known_hosts_cache = Some(kh);
        }
        // Remote 뷰모델을 현재 상태에서 구성 (UI는 서버를 직접 만지지 않는다 — disjoint 필드 차용).
        let remote_view = {
            let (running, addr, fp, token) = match &self.remote {
                Some(s) => (
                    true,
                    Some(s.server.local_addr().to_string()),
                    Some(s.fingerprint.as_str()),
                    Some(s.server.auth_token()),
                ),
                None => (false, None, None, None),
            };
            ui::settings::RemoteView {
                running,
                addr,
                fingerprint: fp,
                token,
                error: self.remote_error.as_deref(),
                known_hosts_path: self.known_hosts_path().display().to_string(),
                known_hosts: self.known_hosts_cache.as_deref().unwrap_or(&[]),
            }
        };
        let notif_unread = self.notifications_ui.unread() as u32;
        let out = ui::settings::show(
            ui.ctx(),
            &mut self.settings_open,
            &mut self.settings_category,
            &mut self.config,
            &remote_view,
            &mut self.remote_reveal_token,
            notif_unread,
            &mut self.settings_search,
            &text,
        );
        // 관리/모니터 네비 항목 클릭 — 아직 별도 패널을 여는 기존 기능들 (전체 인라인화 후속).
        if let Some(panel) = out.open_panel {
            match panel {
                ui::settings::OpenPanel::Credentials => self.credentials_ui.toggle(),
                ui::settings::OpenPanel::Connectors => self.connectors_ui.toggle(),
                ui::settings::OpenPanel::Environment => self.env_profiles_ui.toggle(),
                ui::settings::OpenPanel::Agents => self.agents_ui.toggle(),
                ui::settings::OpenPanel::Workspaces => {
                    self.workspaces_open = !self.workspaces_open;
                    if self.workspaces_open {
                        self.refresh_workspaces();
                    }
                }
                ui::settings::OpenPanel::Activity => {
                    self.activity_ui.toggle();
                    self.refresh_workspaces();
                }
                ui::settings::OpenPanel::Notifications => {
                    self.notifications_ui.toggle();
                    ui.ctx().request_repaint();
                }
            }
        }
        if out.config_changed {
            self.config.i18n.locale = i18n::normalize_locale(&self.config.i18n.locale);
            if self.i18n.locale() != self.config.i18n.locale {
                self.i18n = load_catalog(&self.config.i18n.locale);
            }
            // hot reload: 테마는 즉시 적용
            ui.ctx().set_theme(self.config.ui.theme.to_egui());
            // 폴더 트리 hot toggle (§6): OFF → 상태 drop(리소스 0), ON → 즉시 생성
            if self.config.ui.file_tree_enabled != self.file_tree.is_some() {
                self.file_tree = self
                    .config
                    .ui
                    .file_tree_enabled
                    .then(|| self.make_file_tree());
            }
            if let Err(e) = self.config.save(&self.config_path) {
                tracing::warn!("config 저장 실패: {e:#}");
                // remote 포트 등은 접근 표면에 영향 — 저장 실패를 UI에도 남긴다 (codex xhigh Low).
                self.remote_error = Some(format!("설정 저장 실패: {e:#}"));
            }
        }
        match out.remote_action {
            ui::settings::RemoteAction::Start => self.remote_enable(),
            ui::settings::RemoteAction::Stop => self.remote_disable(),
            ui::settings::RemoteAction::Forget(host) => {
                let path = self.known_hosts_path();
                match runtime::known_hosts::KnownHosts::load(&path)
                    .and_then(|mut kh| kh.forget(&host))
                {
                    Ok(()) => {}
                    Err(e) => tracing::warn!("known_hosts forget 실패: {e:#}"),
                }
                self.known_hosts_cache = Some(self.load_known_hosts());
            }
            ui::settings::RemoteAction::None => {}
        }
        // settings가 닫혔으면 표시 상태를 리셋 — 다음에 열 때 known_hosts를 fresh 로드하고
        // 토큰은 다시 마스킹한다.
        if !self.settings_open {
            self.known_hosts_cache = None;
            self.remote_reveal_token = false;
        }
        self.frame_stats.end();
    }
}

/// known_hosts 파일 텍스트를 (host, 지문) 목록으로 파싱한다 (settings 표시 전용 —
/// forget/pin은 runtime::known_hosts API로 처리). 포맷은 한 줄에 `host 지문`, `#` 주석·빈
/// 줄은 스킵 (known_hosts 파일 계약과 동일). 파일 순서를 보존한다.
fn parse_known_hosts(text: &str) -> Vec<(String, String)> {
    // 표시도 KnownHosts::load와 같은 **effective view**를 쓴다 — host 중복은 last-wins,
    // 지문은 소문자 정규화. 수동 편집으로 duplicate가 생겨도 실제 신뢰 판단과 다른 낡은
    // 지문을 "신뢰 기록"처럼 보여주지 않는다 (codex xhigh Low).
    let mut rows: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        if let (Some(host), Some(fp)) = (parts.next(), parts.next()) {
            let fp = fp.to_ascii_lowercase();
            match rows.iter_mut().find(|(h, _)| h == host) {
                Some(row) => row.1 = fp, // last-wins (KnownHosts HashMap과 동일)
                None => rows.push((host.to_owned(), fp)),
            }
        }
    }
    rows
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

fn load_catalog(locale: &str) -> i18n::Catalog {
    i18n::Catalog::load(locale).unwrap_or_else(|e| {
        tracing::warn!("locale catalog 로드 실패({locale}): {e:#}");
        i18n::Catalog::load(i18n::FALLBACK_LOCALE).expect("fallback locale catalog must load")
    })
}

/// warm 풀 상한 초과분 중 축출 가능한(live 세션 없는) workspace를 앞(가장 오래됨)에서부터
/// 고른다. live workspace는 건너뛰며, 그만큼 상한 초과가 허용된다 (작업 보호 우선).
fn warm_eviction_candidates(
    warm_order: &[String],
    max_warm: usize,
    has_live: impl Fn(&str) -> bool,
) -> Vec<String> {
    let overflow = warm_order.len().saturating_sub(max_warm);
    warm_order
        .iter()
        .filter(|id| !has_live(id))
        .take(overflow)
        .cloned()
        .collect()
}

fn expired_warm_workspace_ids(
    warm_order: &[String],
    backgrounded_at: impl Fn(&str) -> Option<std::time::Instant>,
    now: std::time::Instant,
    timeout: std::time::Duration,
) -> Vec<String> {
    warm_order
        .iter()
        .filter(|id| {
            backgrounded_at(id).is_some_and(|at| now.saturating_duration_since(at) >= timeout)
        })
        .cloned()
        .collect()
}

/// warm workspace의 pending_events를 합쳐(coalesce) 재활성 replay를 정확+유계로 만든다.
///
/// replay 규칙(중요): pending_events는 재활성 시 workspace_ui.show()로 렌더 상태를
/// 재구성한다. workspace_ui는 SessionExited/SessionStatusChanged/SessionStatusViewChanged를
/// "현재 mux에 그 세션이 있을 때만" 적용하고(session_alive 체크), MuxUpdated는 pane
/// 구조(session_id/title)만 담아 status/exit은 lifecycle 이벤트로만 반영된다.
///
/// 그래서:
/// 1) 최신 MuxUpdated 하나만 남기고 **맨 앞으로 옮긴다**(나머지 MuxUpdated 제거). replay가
///    최신 mux로 현재 세션/pane을 먼저 확립한 뒤 lifecycle 이벤트가 자기 세션을 찾아
///    적용된다 — 최신 mux 뒤에 남은 SessionExited가 session_alive를 통과해 종료 pane이
///    running으로 남는 버그를 막는다. (mux에 없는 detach된 세션의 잔여 이벤트는 무시돼도
///    화면에 안 나오니 무해.)
/// 2) SessionStatusChanged/SessionStatusViewChanged는 세션별 최신 1개만 유지한다(status는
///    last-wins). detector의 Running↔Waiting churn으로 무계 누적되던 것을 O(세션수)로
///    유계화. 유지분 상대 순서는 보존.
/// 3) SessionExited/ShellSpawned/AgentSpawned/SpawnFailed/Viewport는 전량 순서 보존.
///
/// 알림은 coalesce 전에 process_ws_notifications가 전량 소비하므로(렌더 replay 전용)
/// 공격적으로 줄여도 알림엔 영향이 없다.
fn coalesce_mux_updated(events: &mut Vec<runtime::RuntimeEvent>) {
    // 남길 최신 MuxUpdated(있으면) — 뽑아서 나중에 맨 앞에 재삽입.
    let latest_mux = events
        .iter()
        .rposition(|e| matches!(e, runtime::RuntimeEvent::MuxUpdated { .. }))
        .map(|i| events[i].clone());

    // 세션별 마지막 StatusChanged의 원 인덱스 (나중 것이 이김 → 그 인덱스만 유지).
    let mut latest_status_idx: std::collections::HashMap<runtime::SessionId, usize> =
        std::collections::HashMap::new();
    let mut latest_status_view_idx: std::collections::HashMap<runtime::SessionId, usize> =
        std::collections::HashMap::new();
    for (i, e) in events.iter().enumerate() {
        if let runtime::RuntimeEvent::SessionStatusChanged { session, .. } = e {
            latest_status_idx.insert(*session, i);
        }
        if let runtime::RuntimeEvent::SessionStatusViewChanged { session, .. } = e {
            latest_status_view_idx.insert(*session, i);
        }
    }

    let mut idx = 0;
    events.retain(|e| {
        // retain은 원소를 원래 순서대로 한 번씩 방문 → idx로 원 위치를 추적한다.
        let keep = match e {
            // 모든 MuxUpdated 제거 (최신 하나는 아래서 맨 앞에 재삽입).
            runtime::RuntimeEvent::MuxUpdated { .. } => false,
            // 세션별 마지막 StatusChanged만 유지.
            runtime::RuntimeEvent::SessionStatusChanged { session, .. } => {
                latest_status_idx.get(session) == Some(&idx)
            }
            runtime::RuntimeEvent::SessionStatusViewChanged { session, .. } => {
                latest_status_view_idx.get(session) == Some(&idx)
            }
            _ => true,
        };
        idx += 1;
        keep
    });

    if let Some(mux) = latest_mux {
        events.insert(0, mux);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// 구분 가능한 최소 MuxUpdated 이벤트 (active_tab 태그로 스냅샷을 식별).
    fn mux_event(tag: &str) -> runtime::RuntimeEvent {
        runtime::RuntimeEvent::MuxUpdated {
            snapshot: std::sync::Arc::new(runtime::MuxSnapshot {
                tabs: Vec::new(),
                active_tab: Some(runtime::MuxTabId(tag.to_owned())),
                focused_pane: None,
            }),
        }
    }

    fn mux_tag(e: &runtime::RuntimeEvent) -> Option<&str> {
        match e {
            runtime::RuntimeEvent::MuxUpdated { snapshot } => {
                snapshot.active_tab.as_ref().map(|t| t.0.as_str())
            }
            _ => None,
        }
    }

    fn temp_db_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "deppy-sijo-{name}-{}-{nanos}.sqlite3",
            std::process::id()
        ))
    }

    fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut file_name = path.as_os_str().to_owned();
        file_name.push(suffix);
        PathBuf::from(file_name)
    }

    fn remove_sqlite_files(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(sqlite_sidecar(path, "-wal"));
        let _ = std::fs::remove_file(sqlite_sidecar(path, "-shm"));
    }

    struct MemSecretStore(Mutex<HashMap<String, String>>);

    impl MemSecretStore {
        fn new() -> Self {
            Self(Mutex::new(HashMap::new()))
        }

        fn contains(&self, id: &str) -> bool {
            self.0.lock().unwrap().contains_key(id)
        }
    }

    impl secret::SecretStore for MemSecretStore {
        fn set_secret(&self, id: &str, secret: &secret::SecretString) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(id.to_owned(), secret.expose().to_owned());
            Ok(())
        }

        fn get_secret(&self, id: &str) -> anyhow::Result<secret::SecretString> {
            let value = self
                .0
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing secret: {id}"))?;
            Ok(secret::SecretString::new(value))
        }

        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }

        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            Ok(self.contains(id))
        }
    }

    #[test]
    fn credentials_service_add_delete_owns_db_and_secret_side_effects() {
        let path = temp_db_path("credentials-service");
        let db = storage::Db::open(&path).unwrap();
        let store = MemSecretStore::new();
        let redaction = secret::RedactionService::new();
        let service = AppCredentialService {
            db: &db,
            secret_store: &store,
            redaction: &redaction,
        };

        ui::credentials::CredentialService::add_credential(
            &service,
            ui::credentials::NewCredential {
                provider: "test".to_owned(),
                label: "unit".to_owned(),
                credential_kind: "api_key".to_owned(),
                secret: "sk-test-boundary-secret".to_owned(),
            },
        )
        .unwrap();

        let rows = db.list_credentials().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(store.contains(&rows[0].id));

        ui::credentials::CredentialService::delete_credential(&service, &rows[0].id).unwrap();
        assert!(db.list_credentials().unwrap().is_empty());
        assert!(!store.contains(&rows[0].id));

        drop(db);
        remove_sqlite_files(&path);
    }

    #[test]
    fn connectors_oauth_secret_adapter_stores_access_and_refresh_tokens() {
        let store = MemSecretStore::new();
        let redaction = secret::RedactionService::new();
        let service = AppOAuthCredentialStore {
            secret_store: &store,
            redaction: &redaction,
        };
        let token = auth::OAuthToken {
            access_token: secret::SecretString::new("access-token-secret".to_owned()),
            refresh_token: Some(secret::SecretString::new("refresh-token-secret".to_owned())),
            expires_in_secs: Some(3600),
        };

        let stored =
            ui::connectors::OAuthCredentialStore::store_oauth_token(&service, &token).unwrap();

        assert!(store.contains(&stored.id));
        assert!(store.contains(&auth::refresh_entry_id(&stored.id)));
        assert_eq!(stored.masked_hint, "****cret");

        ui::connectors::OAuthCredentialStore::delete_oauth_token(&service, &stored.id).unwrap();
        assert!(!store.contains(&stored.id));
        assert!(!store.contains(&auth::refresh_entry_id(&stored.id)));
    }

    #[test]
    fn workspace_path_to_tree_root는_빈_경로를_desktop_fallback하지_않는다() {
        assert_eq!(App::workspace_path_to_tree_root(None), None);
        assert_eq!(App::workspace_path_to_tree_root(Some(String::new())), None);
        assert_eq!(
            App::workspace_path_to_tree_root(Some("   \t ".to_owned())),
            None
        );
        assert_eq!(
            App::workspace_path_to_tree_root(Some(" /tmp/project ".to_owned())),
            Some(PathBuf::from(" /tmp/project "))
        );
    }

    #[test]
    fn parse_known_hosts_주석_빈줄_손상행_스킵하고_순서보존() {
        let text = "# deppy remote TLS known_hosts\n\
                    127.0.0.1:7777 aa:bb:cc\n\
                    \n\
                    host-only-no-fp\n\
                    [::1]:9000 dd:ee:ff\n";
        let rows = parse_known_hosts(text);
        assert_eq!(
            rows,
            vec![
                ("127.0.0.1:7777".to_owned(), "aa:bb:cc".to_owned()),
                ("[::1]:9000".to_owned(), "dd:ee:ff".to_owned()),
            ]
        );
    }

    #[test]
    fn parse_known_hosts_중복host는_last_wins_소문자정규화() {
        // KnownHosts::load(HashMap)와 동일한 effective view — 낡은 지문을 표시하지 않는다
        let text = "h:1 AA:BB
h:2 cc:dd
h:1 EE:FF
";
        let rows = parse_known_hosts(text);
        assert_eq!(rows.len(), 2);
        assert!(rows.contains(&("h:1".to_owned(), "ee:ff".to_owned())));
        assert!(rows.contains(&("h:2".to_owned(), "cc:dd".to_owned())));
    }

    #[test]
    fn approval_watcher_empty_db는_repaint를_예약하지_않는다() {
        let path = temp_db_path("approval-empty");
        let db = storage::Db::open(&path).unwrap();
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        ctx.set_request_repaint_callback(move |info| {
            let _ = tx.send(info.delay);
        });
        let poll_requested = Arc::new(AtomicBool::new(false));
        let mut watcher = ApprovalWatcher::spawn(
            path.clone(),
            ctx,
            poll_requested.clone(),
            std::time::Duration::from_millis(20),
        );

        std::thread::sleep(std::time::Duration::from_millis(90));

        assert!(rx.try_recv().is_err());
        assert!(!poll_requested.load(Ordering::Acquire));
        watcher.stop();
        drop(db);
        remove_sqlite_files(&path);
    }

    #[test]
    fn approval_watcher_pending_삽입시_ui를_깨운다() {
        let path = temp_db_path("approval-pending");
        let db = storage::Db::open(&path).unwrap();
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        ctx.set_request_repaint_callback(move |info| {
            let _ = tx.send(info.delay);
        });
        let poll_requested = Arc::new(AtomicBool::new(false));
        let mut watcher = ApprovalWatcher::spawn(
            path.clone(),
            ctx,
            poll_requested.clone(),
            std::time::Duration::from_millis(20),
        );

        db.insert_pending_approval("req-1", "srv", "tool", "{}", None, 100)
            .unwrap();

        let delay = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(delay, std::time::Duration::ZERO);
        assert!(poll_requested.load(Ordering::Acquire));
        watcher.stop();
        drop(db);
        remove_sqlite_files(&path);
    }

    #[test]
    fn coalesce_moves_latest_mux_to_front() {
        let mut events = vec![
            mux_event("a"),
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(1),
                status: runtime::SessionStatus::Running,
            },
            mux_event("b"),
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(1),
                exit_code: None,
            },
            mux_event("c"),
        ];

        coalesce_mux_updated(&mut events);

        // 최신 mux(c)만 남아 맨 앞으로. 이전 mux(a, b) 제거. lifecycle는 순서 보존.
        assert_eq!(events.len(), 3);
        assert_eq!(mux_tag(&events[0]), Some("c"));
        assert!(matches!(
            events[1],
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(1),
                status: runtime::SessionStatus::Running,
            }
        ));
        assert!(matches!(
            events[2],
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(1),
                exit_code: None,
            }
        ));
    }

    #[test]
    fn coalesce_dedups_status_per_session() {
        // 같은 세션의 status churn → 세션별 최신 1개만. 유지분 상대 순서 보존.
        let mut events = vec![
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(1),
                status: runtime::SessionStatus::Running,
            },
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(1),
                status: runtime::SessionStatus::Waiting,
            },
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(2),
                status: runtime::SessionStatus::Running,
            },
        ];

        coalesce_mux_updated(&mut events);

        assert_eq!(events.len(), 2);
        // 세션1은 최신(Waiting)만, 세션2는 그대로. 순서: 세션1 → 세션2.
        assert!(matches!(
            events[0],
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(1),
                status: runtime::SessionStatus::Waiting,
            }
        ));
        assert!(matches!(
            events[1],
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(2),
                status: runtime::SessionStatus::Running,
            }
        ));
    }

    #[test]
    fn coalesce_dedups_status_view_per_session() {
        let view = |status| {
            runtime::SessionStatusView::detected(
                status,
                runtime::StatusSource::StreamRegex,
                None,
                None,
            )
        };
        let mut events = vec![
            runtime::RuntimeEvent::SessionStatusViewChanged {
                session: runtime::SessionId(1),
                view: view(runtime::SessionStatus::Running),
            },
            runtime::RuntimeEvent::SessionStatusViewChanged {
                session: runtime::SessionId(1),
                view: view(runtime::SessionStatus::Waiting),
            },
        ];

        coalesce_mux_updated(&mut events);

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            runtime::RuntimeEvent::SessionStatusViewChanged {
                session: runtime::SessionId(1),
                view
            } if view.status == runtime::SessionStatus::Waiting
        ));
    }

    #[test]
    fn coalesce_exit_stays_after_mux_for_replay() {
        // [MuxUpdated(세션X 도입), SessionExited(X)] → coalesce 후에도 exit이 mux 뒤에.
        // (mux가 맨 앞으로 가므로 replay 시 X를 먼저 확립하고 exit이 적용됨.)
        let mut events = vec![
            mux_event("x"),
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(9),
                exit_code: Some(0),
            },
        ];

        coalesce_mux_updated(&mut events);

        assert_eq!(events.len(), 2);
        assert_eq!(mux_tag(&events[0]), Some("x"));
        assert!(matches!(
            events[1],
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(9),
                exit_code: Some(0),
            }
        ));
    }

    #[test]
    fn coalesce_noop_without_mux() {
        let mut events = vec![
            runtime::RuntimeEvent::ShellSpawned {
                session: runtime::SessionId(7),
            },
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(7),
                exit_code: Some(0),
            },
        ];
        coalesce_mux_updated(&mut events);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn warm_auto_suspend_candidates_respect_timeout_and_order() {
        let now = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(60);
        let ids = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let mut backgrounded = HashMap::new();
        backgrounded.insert("a".to_owned(), now - std::time::Duration::from_secs(61));
        backgrounded.insert("b".to_owned(), now - std::time::Duration::from_secs(59));
        backgrounded.insert("c".to_owned(), now - std::time::Duration::from_secs(120));

        assert_eq!(
            expired_warm_workspace_ids(&ids, |id| backgrounded.get(id).copied(), now, timeout),
            vec!["a".to_owned(), "c".to_owned()]
        );
    }

    #[test]
    fn warm_eviction은_live_workspace를_건너뛴다() {
        let ids = vec![
            "a".to_owned(),
            "b".to_owned(),
            "c".to_owned(),
            "d".to_owned(),
        ];
        // 상한 2, 초과 2 — 가장 오래된 a부터 고르되 live(a, c)는 건너뛴다
        let live: std::collections::HashSet<&str> = ["a", "c"].into();
        assert_eq!(
            warm_eviction_candidates(&ids, 2, |id| live.contains(id)),
            vec!["b".to_owned(), "d".to_owned()]
        );
        // 전부 live면 아무것도 축출하지 않는다 (상한 초과 허용 — 작업 보호)
        assert_eq!(
            warm_eviction_candidates(&ids, 2, |_| true),
            Vec::<String>::new()
        );
        // 초과 없음 → 빈 결과
        assert_eq!(
            warm_eviction_candidates(&ids, 4, |_| false),
            Vec::<String>::new()
        );
        // live 아닌 것이 초과분보다 많아도 초과분만큼만 축출
        assert_eq!(
            warm_eviction_candidates(&ids, 3, |_| false),
            vec!["a".to_owned()]
        );
    }

    #[test]
    fn workspace_is_live는_spawn대기와_초기유예를_존중한다() {
        let d = std::time::Duration::from_secs;
        // tracker가 live면 무조건 live
        assert!(workspace_is_live(true, true, 0, d(999)));
        // spawn 응답 대기 중이면 live (mux 관측과 무관)
        assert!(workspace_is_live(false, true, 1, d(999)));
        // 첫 MuxUpdated 관측 전 + 유예 내 → live (restore 이벤트 미도착 창)
        assert!(workspace_is_live(false, false, 0, d(1)));
        // 유예가 지나면 빈 workspace로 취급 — suspend 가능
        assert!(!workspace_is_live(false, false, 0, d(11)));
        // mux 관측 후 세션 없음 → suspend 가능
        assert!(!workspace_is_live(false, true, 0, d(1)));
    }

    #[test]
    fn live_세션_추적은_mux와_exited를_반영한다() {
        use std::sync::Arc;
        let mut tracker = LiveSessionTracker::default();
        assert!(!tracker.has_live(), "빈 workspace는 live 아님");

        let s1 = runtime::SessionId(1);
        let mux = |sessions: &[runtime::SessionId]| runtime::RuntimeEvent::MuxUpdated {
            snapshot: Arc::new(runtime::MuxSnapshot {
                tabs: vec![runtime::TabSnapshot {
                    id: runtime::MuxTabId::new(),
                    title: "t".into(),
                    layout: runtime::LayoutNode::Pane(runtime::MuxPaneId::new()),
                    panes: sessions
                        .iter()
                        .map(|s| runtime::PaneSnapshot {
                            id: runtime::MuxPaneId::new(),
                            session_id: Some(*s),
                            title: "p".into(),
                        })
                        .collect(),
                }],
                active_tab: None,
                focused_pane: None,
            }),
        };

        // 세션 attach → live
        tracker.observe(&mux(&[s1]));
        assert!(tracker.has_live());

        // Exited → live 아님 (pane은 남아 있어도 프로세스는 죽음 — agent 결과 pane)
        tracker.observe(&runtime::RuntimeEvent::SessionExited {
            session: s1,
            exit_code: Some(0),
        });
        assert!(!tracker.has_live());

        // pane 제거 MuxUpdated → exited 집합도 정리(유계)
        tracker.observe(&mux(&[]));
        assert!(tracker.exited_sessions.is_empty());
        assert!(!tracker.has_live());
    }
}
