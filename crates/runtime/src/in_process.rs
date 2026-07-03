//! v0 구현체 (설계문서 2.3). worker thread가 세션들을 소유한다.
//! 세션 로직(PTY+terminal+lifecycle)은 session crate 소관 (PR-08).

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deppy_core::{MuxPaneId, MuxTabId};
use mux::{FocusManager, MuxPane, MuxSnapshot, MuxTab, MuxWindow, PaneSnapshot, TabSnapshot};
use std::path::PathBuf;

use pty::CommandSpec;
use secret::{RedactionService, SecretStore, StreamRedactor};
use session::{Session, StatusDetector, StatusPatterns};
use storage::SessionLogWriter;

use crate::client::{RuntimeClient, RuntimeCommandSink, RuntimeEventReceiver, RuntimeEventStream};
use crate::command::{RuntimeCommand, SessionId};
use crate::event::{RuntimeEvent, SpawnKind};

/// 구독자 한 명의 송신측. 상태 이벤트(unbounded — 세션 수명당 상수 개수의
/// 제어 이벤트라 누적 위험 없음)와 세션별 Viewport slot(최신본만 유지 — 14.5의
/// output bounded 요구를 "누적 불가" 구조로 충족)을 분리한다 (8.2).
struct Subscriber {
    events: Sender<RuntimeEvent>,
    viewports: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>>,
}

pub struct InProcessRuntimeClient {
    /// shutdown 시 None — drop되면 worker가 Disconnected로 종료한다
    command_tx: Option<Sender<RuntimeCommand>>,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl InProcessRuntimeClient {
    /// `output_batch_ms`: Viewport push 주기 (설계문서 10.1, config.performance 소비).
    /// 시작 시점에 고정 — 변경은 앱 재시작 필요.
    /// `secret_store`: SpawnAgent의 secret env를 spawn 직전에 resolve할 때만 사용 (6.3).
    /// `logs_root`: 세션별 redacted 로그 디렉터리 (7장). `redaction`: 공유 레지스트리 —
    /// UI(credential 저장)와 worker(spawn 주입)가 같은 인스턴스에 등록한다.
    pub fn new(
        output_batch_ms: u64,
        secret_store: Arc<dyn SecretStore>,
        logs_root: PathBuf,
        redaction: RedactionService,
    ) -> Self {
        Self::with_shell(
            output_batch_ms,
            secret_store,
            logs_root,
            redaction,
            pty::default_shell(),
        )
    }

    /// 테스트용: 셸 대신 임의 명령을 spawn한다.
    pub fn with_shell(
        output_batch_ms: u64,
        secret_store: Arc<dyn SecretStore>,
        logs_root: PathBuf,
        redaction: RedactionService,
        shell: CommandSpec,
    ) -> Self {
        // 세션 id(u64)는 실행마다 1부터 다시 시작한다 — 이전 실행 로그에
        // append되지 않도록 실행(run) 단위 하위 디렉터리로 격리한다.
        // (영속 세션 id 도입은 PR-14)
        let run_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        // 같은 ms의 다중 인스턴스/테스트 충돌 방지: pid + 프로세스 내 카운터
        static RUN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = RUN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let logs_root = logs_root.join(format!("run-{run_ms}-{}-{seq}", std::process::id()));
        let (command_tx, command_rx) = channel();
        let subscribers: Arc<Mutex<Vec<Subscriber>>> = Arc::default();
        let worker_subscribers = Arc::clone(&subscribers);
        let worker = std::thread::Builder::new()
            .name("runtime-worker".into())
            .spawn(move || {
                Worker {
                    command_rx,
                    subscribers: worker_subscribers,
                    batch: Duration::from_millis(output_batch_ms.max(1)),
                    shell,
                    next_id: 1,
                    sessions: std::collections::HashMap::new(),
                    logs: std::collections::HashMap::new(),
                    detectors: std::collections::HashMap::new(),
                    logs_root,
                    redaction,
                    secret_store,
                    mux: MuxState::new(),
                    tab_counter: 0,
                }
                .run();
            })
            .expect("runtime worker thread 생성");
        Self {
            command_tx: Some(command_tx),
            subscribers,
            worker: Some(worker),
        }
    }

    /// worker를 종료시키고 세션 정리(PtySession Drop)까지 동기적으로 기다린다.
    /// 앱 종료 경로(on_exit)에서 호출 — main 리턴과 worker 정리 사이의
    /// 스케줄링 경합으로 자식 프로세스가 reap되지 않는 문제 방지.
    pub fn shutdown(&mut self) {
        self.command_tx = None; // Disconnected → worker 루프 break
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::warn!("runtime worker join 실패 (panic)");
        }
    }
}

impl Drop for InProcessRuntimeClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl RuntimeCommandSink for InProcessRuntimeClient {
    fn send_command(&self, command: RuntimeCommand) -> anyhow::Result<()> {
        self.command_tx
            .as_ref()
            .and_then(|tx| tx.send(command).ok())
            .ok_or_else(|| anyhow::anyhow!("runtime worker가 종료됨"))
    }
}

impl RuntimeEventStream for InProcessRuntimeClient {
    fn subscribe(&self) -> RuntimeEventReceiver {
        let (tx, rx) = channel();
        let viewports: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>> =
            Arc::default();
        self.subscribers
            .lock()
            .expect("subscribers lock")
            .push(Subscriber {
                events: tx,
                viewports: Arc::clone(&viewports),
            });
        RuntimeEventReceiver {
            events: rx,
            viewports,
        }
    }
}

impl RuntimeClient for InProcessRuntimeClient {}

struct Worker {
    command_rx: Receiver<RuntimeCommand>,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
    batch: Duration,
    shell: CommandSpec,
    next_id: u64,
    /// 다중 세션 (PR-08 Session Runtime). 세션 로직은 session crate 소관.
    sessions: std::collections::HashMap<SessionId, Session>,
    /// spawn 직전 secret resolve 전용 (6.3). worker 단일 스레드 접근 (1.4).
    secret_store: Arc<dyn SecretStore>,
    /// 세션별 redacted 로그 (7장). raw 평문 로그는 만들지 않는다.
    logs: std::collections::HashMap<SessionId, SessionLog>,
    /// 세션별 status detector (PR-12) — regex 있는 agent만
    detectors: std::collections::HashMap<SessionId, StatusDetector>,
    logs_root: PathBuf,
    redaction: RedactionService,
    /// mux 상태 (PR-10) — layout source of truth. UI는 MuxUpdated 스냅샷만 본다.
    mux: MuxState,
    tab_counter: u64,
}

/// 세션 하나의 redaction 상태 + 로그 파일 (설계문서 7장).
struct SessionLog {
    redactor: StreamRedactor,
    writer: SessionLogWriter,
}

/// worker가 소유하는 mux 상태 (설계문서 3장 Mux Runtime).
struct MuxState {
    window: MuxWindow,
    tabs: std::collections::HashMap<MuxTabId, MuxTab>,
    panes: std::collections::HashMap<MuxPaneId, MuxPane>,
    focus: FocusManager,
}

impl MuxState {
    fn new() -> Self {
        Self {
            window: MuxWindow::new(deppy_core::MuxWindowId::new()),
            tabs: std::collections::HashMap::new(),
            panes: std::collections::HashMap::new(),
            focus: FocusManager::new(),
        }
    }

    /// visible(= active tab의) pane 세션들 — Viewport push 대상.
    /// §14.4가 금지하는 것은 hidden pane snapshot이다: split로 화면에 보이는
    /// 비포커스 pane은 visible이므로 갱신한다 (리뷰 반영 — 얼어붙은 pane 방지).
    fn watched_sessions(&self) -> Vec<SessionId> {
        self.window
            .active_tab
            .as_ref()
            .and_then(|tab| self.tabs.get(tab))
            .map(|tab| {
                tab.panes()
                    .iter()
                    .filter_map(|pane| self.panes.get(pane))
                    .filter_map(|pane| pane.session_id)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn tab_of_pane(&self, pane: &MuxPaneId) -> Option<MuxTabId> {
        self.tabs
            .values()
            .find(|tab| tab.layout.contains(pane))
            .map(|tab| tab.id.clone())
    }

    /// tab 제거 후/포커스 이동 후 활성 tab의 pane들로 포커스를 보정한다.
    /// 드러난 tab이 기억하는 active_pane을 우선하고, 없으면 첫 pane.
    fn fix_focus(&mut self) {
        let active_tab = self
            .window
            .active_tab
            .as_ref()
            .and_then(|tab| self.tabs.get(tab));
        let panes = active_tab.map(|tab| tab.panes()).unwrap_or_default();
        let focused_valid = self
            .focus
            .focused()
            .is_some_and(|pane| panes.contains(pane));
        if !focused_valid
            && let Some(remembered) = active_tab.and_then(|tab| tab.active_pane.clone())
            && panes.contains(&remembered)
        {
            self.focus.focus(remembered);
            return;
        }
        self.focus.ensure_valid(&panes);
    }

    fn snapshot(&self) -> MuxSnapshot {
        let tabs = self
            .window
            .tabs
            .iter()
            .filter_map(|tab_id| self.tabs.get(tab_id))
            .map(|tab| TabSnapshot {
                id: tab.id.clone(),
                title: tab.title.clone(),
                layout: tab.layout.clone(),
                panes: tab
                    .panes()
                    .into_iter()
                    .filter_map(|pane_id| self.panes.get(&pane_id))
                    .map(|pane| PaneSnapshot {
                        id: pane.id.clone(),
                        session_id: pane.session_id,
                        title: pane.title.clone(),
                    })
                    .collect(),
            })
            .collect();
        MuxSnapshot {
            tabs,
            active_tab: self.window.active_tab.clone(),
            focused_pane: self.focus.focused().cloned(),
        }
    }
}

impl Worker {
    fn run(&mut self) {
        loop {
            // batch 간격으로 깨어나며 명령을 처리한다
            match self.command_rx.recv_timeout(self.batch) {
                Ok(command) => {
                    self.handle_command(command);
                    // 몰려온 명령은 한 번에 소화
                    while let Ok(command) = self.command_rx.try_recv() {
                        self.handle_command(command);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break, // client drop → 종료
            }
            self.pump_sessions();
        }
        // 앱 종료: 열려 있는 로그의 redaction carry를 flush하고 마감한다
        // (shutdown()이 join하므로 여기까지 동기 보장)
        let open_sessions: Vec<SessionId> = self.logs.keys().copied().collect();
        for session in open_sessions {
            self.close_session_log(session, "app-shutdown", None);
        }
        // sessions drop → PtySession Drop이 프로세스 정리
    }

    fn emit(&self, event: RuntimeEvent) {
        // Viewport는 최신본 slot 덮어쓰기 (누적/유실/blocking 없음 — 느린 소비자도
        // 재개 시 항상 최종 화면을 본다), 상태 이벤트는 채널 send.
        // receiver가 drop된 구독자는 제거: slot 경로는 Arc strong_count로 판별
        // (receiver도 slot Arc를 쥐므로 count 1이면 죽은 구독자), 채널 경로는 send 실패로.
        self.subscribers
            .lock()
            .expect("subscribers lock")
            .retain(|subscriber| {
                if let RuntimeEvent::Viewport { session, .. } = &event {
                    if Arc::strong_count(&subscriber.viewports) <= 1 {
                        return false;
                    }
                    subscriber
                        .viewports
                        .lock()
                        .expect("viewport slot lock")
                        .insert(*session, event.clone());
                    true
                } else {
                    subscriber.events.send(event.clone()).is_ok()
                }
            });
    }

    fn handle_command(&mut self, command: RuntimeCommand) {
        match command {
            RuntimeCommand::SpawnShell {
                cols,
                rows,
                scrollback_lines,
            } => {
                let id = SessionId(self.next_id);
                self.next_id += 1;
                match Session::spawn_with_spec(
                    id,
                    session::SessionKind::Shell,
                    &self.shell, // 테스트 주입 가능해야 하므로 default_shell 헬퍼 대신 spec 직접
                    cols,
                    rows,
                    scrollback_lines,
                ) {
                    Ok(new_session) => {
                        self.sessions.insert(id, new_session);
                        self.open_session_log(id);
                        self.attach_in_new_tab(id, "셸");
                        // MuxUpdated를 먼저 — Spawned 수신 시점에 스냅샷이 항상 앞서 있다
                        self.emit_mux_and_watched();
                        self.emit(RuntimeEvent::ShellSpawned { session: id });
                    }
                    Err(e) => self.emit(RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Shell,
                        message: format!("{e:#}"),
                    }),
                }
            }
            RuntimeCommand::SpawnAgent {
                cols,
                rows,
                scrollback_lines,
                command,
                args,
                env_plain,
                env_secrets,
                waiting_regex,
                approval_regex,
                error_regex,
                done_regex,
            } => {
                // secret은 여기(spawn 직전)에서만 resolve된다 — PR-09 완료 기준.
                // 실패 시 아무것도 spawn하지 않는다 (부분 주입 금지).
                let mut env = env_plain;
                let mut resolve_failed = None;
                for (key, credential_id) in env_secrets {
                    match self.secret_store.get_secret(&credential_id) {
                        Ok(value) => {
                            // 주입되는 secret은 로그 redaction 대상으로 등록 (6.3/7장)
                            self.redaction.register(&value);
                            env.push((key, value.expose().to_owned()));
                        }
                        Err(e) => {
                            // credential id만 로그 — secret 값/키 이름은 남기지 않는다
                            resolve_failed = Some(format!(
                                "secret resolve 실패 (credential {credential_id}): {e:#}"
                            ));
                            break;
                        }
                    }
                }
                if let Some(message) = resolve_failed {
                    self.emit(RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Agent,
                        message,
                    });
                    return;
                }
                let spec = CommandSpec {
                    program: command,
                    args,
                    env,
                };
                let id = SessionId(self.next_id);
                self.next_id += 1;
                match session::spawn_agent(id, &spec, cols, rows, scrollback_lines) {
                    Ok(new_session) => {
                        self.sessions.insert(id, new_session);
                        let patterns = StatusPatterns::compile(
                            waiting_regex.as_deref(),
                            approval_regex.as_deref(),
                            error_regex.as_deref(),
                            done_regex.as_deref(),
                        );
                        // regex가 없어도 idle heuristic(3단)은 동작해야 한다 — 상시 설치
                        self.detectors.insert(id, StatusDetector::new(patterns));
                        self.open_session_log(id);
                        self.attach_in_new_tab(id, "에이전트");
                        self.emit_mux_and_watched();
                        self.emit(RuntimeEvent::AgentSpawned { session: id });
                    }
                    Err(e) => self.emit(RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Agent,
                        message: format!("{e:#}"),
                    }),
                }
            }
            RuntimeCommand::WriteInput { session, bytes } => {
                if let Some(active) = self.sessions.get_mut(&session) {
                    active.write_input(&bytes);
                    // 사용자 입력 = 화면 프롬프트에 대한 응답 신호 (status detector)
                    if let Some(detector) = self.detectors.get_mut(&session) {
                        detector.on_input();
                    }
                }
            }
            RuntimeCommand::Resize {
                session,
                cols,
                rows,
            } => {
                if let Some(active) = self.sessions.get_mut(&session) {
                    active.resize(cols, rows);
                }
            }
            RuntimeCommand::Scroll { session, delta } => {
                if let Some(active) = self.sessions.get_mut(&session) {
                    active.scroll(delta);
                }
            }
            RuntimeCommand::SeedRedaction { credential_ids } => {
                // 기존 저장 credential을 로그 redaction 대상으로 등록 (7장).
                // resolve는 worker 단일 스레드에서만 (1.4) — UI는 id만 넘긴다 (2.1).
                for id in credential_ids {
                    match self.secret_store.get_secret(&id) {
                        Ok(value) => {
                            self.redaction.register(&value);
                            // OAuth 토큰처럼 JSON blob으로 저장된 credential은
                            // 개별 필드(access/refresh token)도 등록 (PR-18)
                            self.redaction.register_json_fields(&value);
                        }
                        Err(e) => {
                            tracing::warn!("redaction 시드 실패 (credential {id}): {e:#}")
                        }
                    }
                }
            }
            RuntimeCommand::KillSession { session } => {
                // Session drop → PtySession Drop이 process group 정리를 보장한다
                self.sessions.remove(&session);
                self.detectors.remove(&session);
                self.close_session_log(session, "killed", None);
                // 세션을 잃은 pane은 attach 해제 (pane/session 분리 — 5.2)
                for pane in self.mux.panes.values_mut() {
                    if pane.session_id == Some(session) {
                        pane.session_id = None;
                    }
                }
                self.emit_mux_and_watched();
            }
            RuntimeCommand::SplitPane {
                pane,
                direction,
                scrollback_lines,
            } => self.split_pane(pane, direction, scrollback_lines),
            RuntimeCommand::ClosePane { pane } => self.close_pane(pane),
            RuntimeCommand::CloseTab { tab } => self.close_tab(tab),
            RuntimeCommand::SelectTab { tab } => {
                if self.mux.tabs.contains_key(&tab) {
                    self.mux.window.active_tab = Some(tab.clone());
                    if let Some(active_pane) =
                        self.mux.tabs.get(&tab).and_then(|t| t.active_pane.clone())
                    {
                        self.mux.focus.focus(active_pane);
                    }
                    self.mux.fix_focus();
                    self.emit_mux_and_watched();
                }
            }
            RuntimeCommand::FocusPane { pane } => {
                if let Some(tab) = self.mux.tab_of_pane(&pane) {
                    self.mux.window.active_tab = Some(tab.clone());
                    if let Some(tab) = self.mux.tabs.get_mut(&tab) {
                        tab.active_pane = Some(pane.clone());
                    }
                    self.mux.focus.focus(pane);
                    self.emit_mux_and_watched();
                }
            }
        }
    }

    /// 세션 로그를 연다. 실패해도 세션은 계속 (로그만 없음 — warn).
    fn open_session_log(&mut self, session: SessionId) {
        match SessionLogWriter::open(&self.logs_root, session) {
            Ok(mut writer) => {
                let _ = writer.append_event("spawned", None);
                self.logs.insert(
                    session,
                    SessionLog {
                        redactor: self.redaction.stream_redactor(),
                        writer,
                    },
                );
            }
            Err(e) => tracing::warn!("세션 로그 열기 실패: {e:#}"),
        }
    }

    /// 세션 로그를 닫는다 — carry flush 후 종료 이벤트 기록.
    fn close_session_log(&mut self, session: SessionId, event: &str, detail: Option<&str>) {
        if let Some(mut log) = self.logs.remove(&session) {
            let tail = log.redactor.flush();
            let _ = log.writer.append_output(&tail);
            let _ = log.writer.append_event(event, detail);
            log.writer.flush();
        }
    }

    /// 새 tab에 pane 하나를 만들어 세션을 attach하고 포커스한다.
    fn attach_in_new_tab(&mut self, session: SessionId, title_prefix: &str) {
        self.tab_counter += 1;
        let title = format!("{title_prefix} {}", self.tab_counter);
        let pane_id = MuxPaneId::new();
        let mut pane = MuxPane::new(pane_id.clone(), title.clone());
        pane.session_id = Some(session);
        self.mux.panes.insert(pane_id.clone(), pane);
        let tab = MuxTab::new(MuxTabId::new(), title, pane_id.clone());
        self.mux.window.add_tab(tab.id.clone());
        self.mux.tabs.insert(tab.id.clone(), tab);
        self.mux.focus.focus(pane_id);
    }

    fn split_pane(
        &mut self,
        target: MuxPaneId,
        direction: mux::SplitDirection,
        scrollback_lines: usize,
    ) {
        let Some(tab_id) = self.mux.tab_of_pane(&target) else {
            // stale 요청(이미 닫힌 pane)에도 응답한다 — UI의 pending 폴링이 끝나도록
            self.emit(RuntimeEvent::SpawnFailed {
                kind: SpawnKind::Shell,
                message: "분할 대상 pane이 이미 없음".into(),
            });
            return;
        };
        let id = SessionId(self.next_id);
        self.next_id += 1;
        match Session::spawn_with_spec(
            id,
            session::SessionKind::Shell,
            &self.shell,
            80,
            24,
            scrollback_lines,
        ) {
            Ok(new_session) => {
                self.sessions.insert(id, new_session);
                self.open_session_log(id);
                self.tab_counter += 1;
                let pane_id = MuxPaneId::new();
                let mut pane = MuxPane::new(pane_id.clone(), format!("셸 {}", self.tab_counter));
                pane.session_id = Some(id);
                self.mux.panes.insert(pane_id.clone(), pane);
                let split_ok = self
                    .mux
                    .tabs
                    .get_mut(&tab_id)
                    .is_some_and(|tab| tab.split_pane(&target, direction, pane_id.clone()));
                if !split_ok {
                    // 방어: split 실패 시 고아 pane/세션을 남기지 않는다
                    self.mux.panes.remove(&pane_id);
                    self.sessions.remove(&id);
                    self.close_session_log(id, "killed", None);
                    self.emit(RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Shell,
                        message: "분할 실패 (대상 pane 소실)".into(),
                    });
                    return;
                }
                self.mux.focus.focus(pane_id);
                self.emit_mux_and_watched();
                self.emit(RuntimeEvent::ShellSpawned { session: id });
            }
            Err(e) => self.emit(RuntimeEvent::SpawnFailed {
                kind: SpawnKind::Shell,
                message: format!("{e:#}"),
            }),
        }
    }

    fn close_pane(&mut self, pane_id: MuxPaneId) {
        let Some(tab_id) = self.mux.tab_of_pane(&pane_id) else {
            return;
        };
        // 세션 kill
        if let Some(session) = self
            .mux
            .panes
            .get(&pane_id)
            .and_then(|pane| pane.session_id)
        {
            self.sessions.remove(&session);
            self.detectors.remove(&session);
            self.close_session_log(session, "killed", None);
        }
        self.mux.panes.remove(&pane_id);
        let last_pane = match self.mux.tabs.get_mut(&tab_id) {
            Some(tab) => matches!(tab.close_pane(&pane_id), mux::ClosePane::LastPane),
            None => false,
        };
        if last_pane {
            self.mux.tabs.remove(&tab_id);
            self.mux.window.close_tab(&tab_id);
        }
        self.mux.fix_focus();
        self.emit_mux_and_watched();
    }

    fn close_tab(&mut self, tab_id: MuxTabId) {
        let Some(tab) = self.mux.tabs.remove(&tab_id) else {
            return;
        };
        for pane_id in tab.panes() {
            if let Some(pane) = self.mux.panes.remove(&pane_id)
                && let Some(session) = pane.session_id
            {
                self.sessions.remove(&session);
                self.detectors.remove(&session);
                self.close_session_log(session, "killed", None);
            }
        }
        self.mux.window.close_tab(&tab_id);
        self.mux.fix_focus();
        self.emit_mux_and_watched();
    }

    /// mux 스냅샷을 push하고, visible(active tab) 세션들의 화면도 즉시 push한다
    /// (tab/포커스 전환 직후 stale 화면 방지).
    fn emit_mux_and_watched(&mut self) {
        self.emit(RuntimeEvent::MuxUpdated {
            snapshot: Arc::new(self.mux.snapshot()),
        });
        let mut events = Vec::new();
        for session in self.mux.watched_sessions() {
            if let Some(active) = self.sessions.get_mut(&session)
                && let Some(snapshot) = active.take_snapshot()
            {
                events.push(RuntimeEvent::Viewport {
                    session,
                    snapshot: Arc::new(snapshot),
                    bracketed_paste: active.bracketed_paste(),
                });
            }
        }
        for event in events {
            self.emit(event);
        }
    }

    /// 모든 세션의 PTY 출력을 반영하고, active pane 세션만 Viewport를 push한다
    /// (14.4: hidden pane snapshot 생성 금지 — dirty는 유지되어 포커스 전환 시 따라잡는다).
    fn pump_sessions(&mut self) {
        let watched = self.mux.watched_sessions();
        let mut events = Vec::new();
        for active in self.sessions.values_mut() {
            let mut log = self.logs.get_mut(&active.id());
            let mut detector = self.detectors.get_mut(&active.id());
            let result = active.pump(|chunk| {
                if let Some(log) = log.as_mut() {
                    // redaction 후에만 디스크에 닿는다 (7장 — raw 평문 저장 금지)
                    let redacted = log.redactor.redact_chunk(chunk);
                    if let Err(e) = log.writer.append_output(&redacted) {
                        tracing::warn!("세션 로그 기록 실패: {e:#}");
                    }
                }
                if let Some(detector) = detector.as_mut() {
                    detector.on_output(chunk); // 1단: stream line regex
                }
            });
            // 2·3단: 화면 텍스트 패턴 + idle — batch 주기, 경량 grid 조회
            // (snapshot 미생성 — hidden 세션 규칙, PR-12).
            // 종료된 세션은 감지 중단 — 단, 종료 tick에서는 마지막 출력의
            // 상태(done/error 등)를 한 번 더 평가한다 (짧은 agent 대응).
            // idle 오발은 없다: 방금 출력이 왔으므로 last_output이 신선하다.
            if (active.lifecycle().is_running() || result.just_exited)
                && let Some(detector) = self.detectors.get_mut(&active.id())
            {
                // 게이트는 "이번 tick의 새 출력" — 누적 dirty를 쓰면 hidden 세션이
                // 매 tick 전체 grid를 스캔하게 된다 (hidden은 snapshot으로 dirty가 안 지워짐)
                let screen = (result.produced_output || detector.take_screen_scan_request())
                    .then(|| active.screen_text());
                if let Some(status) = detector.evaluate(screen.as_deref()) {
                    events.push(RuntimeEvent::SessionStatusChanged {
                        session: active.id(),
                        status,
                    });
                }
            }
            if result.dirty
                && watched.contains(&active.id())
                && let Some(snapshot) = active.take_snapshot()
            {
                events.push(RuntimeEvent::Viewport {
                    session: active.id(),
                    snapshot: Arc::new(snapshot),
                    bracketed_paste: active.bracketed_paste(),
                });
            }
            if result.just_exited
                && let session::SessionLifecycle::Exited { exit_code } = active.lifecycle()
            {
                events.push(RuntimeEvent::SessionExited {
                    session: active.id(),
                    exit_code,
                });
            }
        }
        // 종료 세션의 로그 마감 (carry flush + exited 이벤트)
        let exited: Vec<(SessionId, Option<u32>)> = events
            .iter()
            .filter_map(|e| match e {
                RuntimeEvent::SessionExited { session, exit_code } => Some((*session, *exit_code)),
                _ => None,
            })
            .collect();
        for (session, exit_code) in exited {
            let detail = exit_code.map(|c| format!("exit code {c}"));
            self.close_session_log(session, "exited", detail.as_deref());
            self.detectors.remove(&session);
        }
        for event in events {
            self.emit(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::SplitDirection;
    use std::time::Instant;

    fn test_store() -> Arc<dyn SecretStore> {
        Arc::new(secret::KeyringSecretStore)
    }

    fn test_logs_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deppy-rt-logs-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// mock keyring store는 test only (설계문서 1.4). 프로세스 전역 1회만 등록 —
    /// 테스트별 재등록은 병렬 실행에서 이전 등록분의 secret을 날린다.
    fn init_mock_store() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
        });
    }

    fn spec(program: &str, args: &[&str]) -> CommandSpec {
        CommandSpec {
            program: program.into(),
            args: args.iter().map(|s| (*s).into()).collect(),
            env: Vec::new(),
        }
    }

    /// 수신한 이벤트를 버리지 않고 모아두는 테스트 헬퍼 —
    /// 한 wait에서 드레인된 다른 이벤트를 다음 wait가 볼 수 있게 한다.
    struct Probe {
        rx: RuntimeEventReceiver,
        seen: Vec<RuntimeEvent>,
    }

    impl Probe {
        fn new(rx: RuntimeEventReceiver) -> Self {
            Self {
                rx,
                seen: Vec::new(),
            }
        }

        /// 조건을 만족하는 이벤트가 관측될 때까지 폴링 (timeout 시 panic).
        fn wait_for<T>(
            &mut self,
            timeout: Duration,
            mut pick: impl FnMut(&RuntimeEvent) -> Option<T>,
        ) -> T {
            let deadline = Instant::now() + timeout;
            loop {
                self.seen.extend(self.rx.drain());
                if let Some(value) = self.seen.iter().find_map(&mut pick) {
                    return value;
                }
                if Instant::now() >= deadline {
                    panic!("기다리던 이벤트가 오지 않음");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn snapshot_text(snapshot: &terminal::TerminalViewportSnapshot, row: usize) -> String {
        let cols = snapshot.cols as usize;
        snapshot.visible_cells[row * cols..(row + 1) * cols]
            .iter()
            .filter(|c| !c.wide_spacer)
            .map(|c| c.c)
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    #[test]
    #[cfg(unix)]
    fn spawn_출력_종료_이벤트_흐름() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("t"),
            RedactionService::new(),
            spec("/bin/echo", &["hi-runtime"]),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        // 출력이 Viewport로 push된다
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport { snapshot, .. }
                if snapshot_text(snapshot, 0).contains("hi-runtime") =>
            {
                Some(())
            }
            _ => None,
        });
        // echo 종료 → SessionExited
        let (exited, code) = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SessionExited {
                session, exit_code, ..
            } => Some((*session, *exit_code)),
            _ => None,
        });
        assert_eq!(exited, session);
        assert_eq!(code, Some(0));
    }

    #[test]
    #[cfg(unix)]
    fn 입력과_kill() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("t"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        client
            .send_command(RuntimeCommand::WriteInput {
                session,
                bytes: b"ping\r".to_vec(),
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport { snapshot, .. }
                if snapshot_text(snapshot, 0).contains("ping") =>
            {
                Some(())
            }
            _ => None,
        });
        // kill → 세션 제거 (KillSession은 이벤트 없이 조용히 정리)
        client
            .send_command(RuntimeCommand::KillSession { session })
            .unwrap();
        // 새 세션 spawn이 정상 동작하면 정리가 끝난 것
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let new_session = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::ShellSpawned { session: s } if *s != session => Some(*s),
            _ => None,
        });
        assert_ne!(new_session, session);
    }

    #[test]
    #[cfg(unix)]
    fn 종료_후에도_scrollback_열람_가능() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("t"),
            RedactionService::new(),
            spec("/bin/echo", &["done"]),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SessionExited { .. } => Some(()),
            _ => None,
        });
        // backend가 유지되어 Scroll에 Viewport로 응답해야 한다
        // (종료 전 Viewport와 구분하기 위해 관측 버퍼를 비운다)
        probe.seen.clear();
        client
            .send_command(RuntimeCommand::Scroll { session, delta: 1 })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport { snapshot, .. }
                if snapshot_text(snapshot, 0).contains("done") =>
            {
                Some(())
            }
            _ => None,
        });
    }

    #[test]
    #[cfg(unix)]
    fn 다중_세션_동시_생존과_독립_입출력() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("t"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
        );
        let mut probe = Probe::new(client.subscribe());
        let mut ids = Vec::new();
        for _ in 0..3 {
            client
                .send_command(RuntimeCommand::SpawnShell {
                    cols: 80,
                    rows: 24,
                    scrollback_lines: 100,
                })
                .unwrap();
            let known = ids.clone();
            let id = probe.wait_for(Duration::from_secs(15), move |e| match e {
                RuntimeEvent::ShellSpawned { session } if !known.contains(session) => {
                    Some(*session)
                }
                _ => None,
            });
            ids.push(id);
        }
        // 각 세션에 서로 다른 입력 (백그라운드 pane 포함)
        for (i, id) in ids.iter().enumerate() {
            client
                .send_command(RuntimeCommand::WriteInput {
                    session: *id,
                    bytes: format!("mark-{i}\r").into_bytes(),
                })
                .unwrap();
        }
        // active pane만 Viewport가 온다 (14.4) — pane 포커스를 옮겨가며 각자 확인
        let mux = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.len() == 3 => {
                Some(snapshot.clone())
            }
            _ => None,
        });
        for (i, id) in ids.iter().enumerate() {
            let pane = mux
                .tabs
                .iter()
                .flat_map(|t| &t.panes)
                .find(|p| p.session_id == Some(*id))
                .unwrap()
                .id
                .clone();
            client
                .send_command(RuntimeCommand::FocusPane { pane })
                .unwrap();
            let expect = format!("mark-{i}");
            let id = *id;
            probe.wait_for(Duration::from_secs(15), move |e| match e {
                RuntimeEvent::Viewport {
                    session, snapshot, ..
                } if *session == id && snapshot_text(snapshot, 0).contains(&expect) => Some(()),
                _ => None,
            });
        }
    }

    #[test]
    #[cfg(unix)]
    fn 비활성_pane은_viewport_push_안됨() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("t"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
        );
        let mut probe = Probe::new(client.subscribe());
        for _ in 0..2 {
            client
                .send_command(RuntimeCommand::SpawnShell {
                    cols: 80,
                    rows: 24,
                    scrollback_lines: 100,
                })
                .unwrap();
        }
        let mux = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.len() == 2 => {
                Some(snapshot.clone())
            }
            _ => None,
        });
        // 두 번째 spawn이 focused — 첫 세션은 비활성
        let inactive = mux.tabs[0].panes[0].session_id.unwrap();
        let active = mux.tabs[1].panes[0].session_id.unwrap();
        client
            .send_command(RuntimeCommand::WriteInput {
                session: inactive,
                bytes: b"hidden\r".to_vec(),
            })
            .unwrap();
        client
            .send_command(RuntimeCommand::WriteInput {
                session: active,
                bytes: b"visible\r".to_vec(),
            })
            .unwrap();
        // active 세션 화면은 오고
        probe.wait_for(Duration::from_secs(15), move |e| match e {
            RuntimeEvent::Viewport {
                session, snapshot, ..
            } if *session == active && snapshot_text(snapshot, 0).contains("visible") => Some(()),
            _ => None,
        });
        // 비활성 기간의 출력("hidden")이 Viewport로 push되면 안 된다 (14.4).
        // (spawn 직후 잠깐 active였을 때의 초기 화면 push는 정당하다)
        assert!(
            !probe.seen.iter().any(|e| matches!(
                e,
                RuntimeEvent::Viewport { session, snapshot, .. }
                    if *session == inactive && snapshot_text(snapshot, 0).contains("hidden")
            )),
            "비활성 pane의 출력이 Viewport로 push됨"
        );
    }

    #[test]
    #[cfg(unix)]
    fn split과_close_pane_mux_흐름() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("t"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let mux = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.len() == 1 => {
                Some(snapshot.clone())
            }
            _ => None,
        });
        let first_pane = mux.tabs[0].panes[0].id.clone();
        client
            .send_command(RuntimeCommand::SplitPane {
                pane: first_pane.clone(),
                direction: SplitDirection::Horizontal,
                scrollback_lines: 100,
            })
            .unwrap();
        let mux = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot.tabs.len() == 1 && snapshot.tabs[0].panes.len() == 2 =>
            {
                Some(snapshot.clone())
            }
            _ => None,
        });
        // split 후 새 pane이 focused
        let new_pane = mux.tabs[0].panes[1].id.clone();
        assert_eq!(mux.focused_pane, Some(new_pane.clone()));
        // 새 pane 닫기 → 첫 pane으로 복귀
        client
            .send_command(RuntimeCommand::ClosePane { pane: new_pane })
            .unwrap();
        let mux = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot.tabs.len() == 1 && snapshot.tabs[0].panes.len() == 1 =>
            {
                Some(snapshot.clone())
            }
            _ => None,
        });
        assert_eq!(mux.focused_pane, Some(first_pane.clone()));
        // 마지막 pane 닫기 → tab도 제거
        client
            .send_command(RuntimeCommand::ClosePane { pane: first_pane })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.is_empty() => Some(()),
            _ => None,
        });
    }

    #[test]
    fn spawn_실패_이벤트() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("nofail"),
            RedactionService::new(),
            spec("/nonexistent-deppy-test-cmd", &[]),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SpawnFailed {
                kind: SpawnKind::Shell,
                ..
            } => Some(()),
            _ => None,
        });
    }

    #[test]
    #[cfg(unix)]
    fn spawn_agent_secret_env_주입() {
        init_mock_store();
        let store = test_store();
        store
            .set_secret(
                "cred-agent-test",
                &secret::SecretString::new("s3cret-value".into()),
            )
            .unwrap();

        let client = InProcessRuntimeClient::with_shell(
            5,
            store,
            test_logs_root("agent"),
            RedactionService::new(),
            pty::default_shell(),
        );
        let mut probe = Probe::new(client.subscribe());
        // sh가 env를 출력 — plain + secret(spawn 직전 resolve) 주입 검증
        client
            .send_command(RuntimeCommand::SpawnAgent {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "echo P=$PLAIN_K S=$SECRET_K".into()],
                env_plain: vec![("PLAIN_K".into(), "plain-v".into())],
                env_secrets: vec![("SECRET_K".into(), "cred-agent-test".into())],
                waiting_regex: None,
                approval_regex: None,
                error_regex: None,
                done_regex: None,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport { snapshot, .. }
                if snapshot_text(snapshot, 0).contains("P=plain-v S=s3cret-value") =>
            {
                Some(())
            }
            _ => None,
        });
    }

    #[test]
    fn spawn_agent_resolve_실패시_spawn_안함() {
        init_mock_store();
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("fail"),
            RedactionService::new(),
            pty::default_shell(),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnAgent {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                command: "/bin/echo".into(),
                args: vec!["누출되면 안 됨".into()],
                env_plain: Vec::new(),
                env_secrets: vec![("K".into(), "cred-없음".into())],
                waiting_regex: None,
                approval_regex: None,
                error_regex: None,
                done_regex: None,
            })
            .unwrap();
        // resolve 실패 → SpawnFailed, 메시지에 secret 값 없음 (credential id만)
        let message = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SpawnFailed { message, .. } => Some(message.clone()),
            _ => None,
        });
        assert!(message.contains("cred-없음"));
        // 실패 시 아무것도 spawn되지 않아야 한다 (부분 주입 금지)
        std::thread::sleep(Duration::from_millis(150));
        probe.seen.extend(probe.rx.drain());
        assert!(
            !probe
                .seen
                .iter()
                .any(|e| matches!(e, RuntimeEvent::AgentSpawned { .. })),
            "resolve 실패 후 AgentSpawned가 발행됨"
        );
    }

    #[test]
    #[cfg(unix)]
    fn 로그_secret_scan_평문_없음() {
        // 완료 기준 (PR-11): 세션 로그 어디에도 secret 평문이 없어야 한다
        init_mock_store();
        let store = test_store();
        store
            .set_secret(
                "cred-log-scan",
                &secret::SecretString::new("scan-me-s3cret-XYZ".into()),
            )
            .unwrap();
        let logs_root = test_logs_root("scan");
        let redaction = RedactionService::new();
        let client = InProcessRuntimeClient::with_shell(
            5,
            store,
            logs_root.clone(),
            redaction,
            pty::default_shell(),
        );
        let mut probe = Probe::new(client.subscribe());
        // secret env를 stdout으로 두 번 출력 (chunk 분할 가능성 포함)
        client
            .send_command(RuntimeCommand::SpawnAgent {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                command: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "echo leak=$SECRET_K; printf '%s' $SECRET_K; echo".into(),
                ],
                env_plain: Vec::new(),
                env_secrets: vec![("SECRET_K".into(), "cred-log-scan".into())],
                waiting_regex: None,
                approval_regex: None,
                error_regex: None,
                done_regex: None,
            })
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::AgentSpawned { session } => Some(*session),
            _ => None,
        });
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SessionExited { .. } => Some(()),
            _ => None,
        });
        // exit 처리(로그 flush)까지 잠시 대기
        std::thread::sleep(Duration::from_millis(200));
        // with_shell이 run-<ms> 하위 디렉터리를 만든다 — 그 안에서 세션 디렉터리를 찾는다
        let run_dir = std::fs::read_dir(&logs_root)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().starts_with("run-"))
            .expect("run 디렉터리 없음")
            .path();
        let dir = storage::SessionLogWriter::session_dir(&run_dir, session);
        for file in [
            "redacted.ansi.log",
            "redacted.plain.txt",
            "events.redacted.jsonl",
        ] {
            let bytes = std::fs::read(dir.join(file)).unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                !text.contains("scan-me-s3cret-XYZ"),
                "{file}에 secret 평문 존재: {text}"
            );
        }
        // 출력 자체는 기록되었고 치환 마커가 있어야 한다
        let ansi = std::fs::read(dir.join("redacted.ansi.log")).unwrap();
        let text = String::from_utf8_lossy(&ansi);
        assert!(text.contains("[REDACTED]"), "치환 마커 없음: {text}");
        std::fs::remove_dir_all(&logs_root).ok();
    }

    #[test]
    #[cfg(unix)]
    fn select_tab과_close_tab_흐름() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("tabs"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
        );
        let mut probe = Probe::new(client.subscribe());
        for _ in 0..2 {
            client
                .send_command(RuntimeCommand::SpawnShell {
                    cols: 80,
                    rows: 24,
                    scrollback_lines: 100,
                })
                .unwrap();
        }
        let mux = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.len() == 2 => {
                Some(snapshot.clone())
            }
            _ => None,
        });
        let first_tab = mux.tabs[0].id.clone();
        let second_tab = mux.tabs[1].id.clone();
        assert_eq!(mux.active_tab, Some(second_tab.clone()));
        // SelectTab → active 전환 + 그 tab의 pane으로 포커스
        client
            .send_command(RuntimeCommand::SelectTab {
                tab: first_tab.clone(),
            })
            .unwrap();
        let expect_pane = mux.tabs[0].panes[0].id.clone();
        probe.wait_for(Duration::from_secs(15), move |e| match e {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot.active_tab == Some(first_tab.clone())
                    && snapshot.focused_pane == Some(expect_pane.clone()) =>
            {
                Some(())
            }
            _ => None,
        });
        // CloseTab → tab 제거 + 남은 tab으로 활성 전환
        client
            .send_command(RuntimeCommand::CloseTab {
                tab: mux.tabs[0].id.clone(),
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), move |e| match e {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot.tabs.len() == 1 && snapshot.active_tab == Some(second_tab.clone()) =>
            {
                Some(())
            }
            _ => None,
        });
    }

    /// agent spawn 공통 헬퍼 (status regex 지정)
    #[cfg(unix)]
    fn spawn_agent_cmd(script: &str, waiting: Option<&str>, done: Option<&str>) -> RuntimeCommand {
        RuntimeCommand::SpawnAgent {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            waiting_regex: waiting.map(str::to_owned),
            approval_regex: None,
            error_regex: None,
            done_regex: done.map(str::to_owned),
        }
    }

    #[test]
    #[cfg(unix)]
    fn status_stream_regex_감지() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("status-stream"),
            RedactionService::new(),
            pty::default_shell(),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(spawn_agent_cmd(
                "echo START; echo WAITING_FOR_INPUT; sleep 2",
                Some("WAITING_FOR_INPUT"),
                None,
            ))
            .unwrap();
        // 일반 출력의 line regex로 Waiting 감지 (완료 기준 1)
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SessionStatusChanged {
                status: session::SessionStatus::Waiting,
                ..
            } => Some(()),
            _ => None,
        });
    }

    #[test]
    #[cfg(unix)]
    fn status_화면_패턴_hidden에서_snapshot_없이_감지() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("status-screen"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
        );
        let mut probe = Probe::new(client.subscribe());
        // agent 먼저 spawn (개행 없는 TUI식 프롬프트 → stream 단계 미감지)
        // 프롬프트는 1초 뒤 — 그 사이 셸 tab을 열어 agent를 hidden으로 만든다
        client
            .send_command(spawn_agent_cmd(
                "sleep 1; printf 'PRESS_ANY_KEY'; sleep 3",
                Some("PRESS_ANY_KEY"),
                None,
            ))
            .unwrap();
        let agent = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::AgentSpawned { session } => Some(*session),
            _ => None,
        });
        // 셸 tab을 하나 더 열어 agent tab을 hidden으로 만든다
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::ShellSpawned { .. } => Some(()),
            _ => None,
        });
        probe.seen.clear(); // 이후 이벤트만 관찰
        // hidden 상태에서 화면 텍스트 패턴으로 감지 (완료 기준 2)
        let agent_id = agent;
        probe.wait_for(Duration::from_secs(15), move |e| match e {
            RuntimeEvent::SessionStatusChanged {
                session,
                status: session::SessionStatus::Waiting,
            } if *session == agent_id => Some(()),
            _ => None,
        });
        // 감지 기간 동안 hidden 세션의 Viewport는 미발행 (완료 기준 3)
        assert!(
            !probe
                .seen
                .iter()
                .any(|e| matches!(e, RuntimeEvent::Viewport { session, .. } if *session == agent)),
            "hidden 세션의 Viewport가 발행됨 (snapshot 생성 규칙 위반)"
        );
    }

    #[test]
    #[cfg(unix)]
    fn status_done과_exit_반영() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("status-done"),
            RedactionService::new(),
            pty::default_shell(),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(spawn_agent_cmd(
                "echo ALL_TASKS_DONE; sleep 1",
                None,
                Some("ALL_TASKS_DONE"),
            ))
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SessionStatusChanged {
                status: session::SessionStatus::Done,
                ..
            } => Some(()),
            _ => None,
        });
        // process exit status 반영 (완료 기준 4 — portable-pty ExitStatus 경유)
        let code = probe.wait_for(Duration::from_secs(10), |e| match e {
            RuntimeEvent::SessionExited { exit_code, .. } => Some(*exit_code),
            _ => None,
        });
        assert_eq!(code, Some(0));
    }

    #[test]
    #[cfg(unix)]
    fn 다중_구독자() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("t"),
            RedactionService::new(),
            spec("/bin/echo", &["multi"]),
        );
        let mut probe1 = Probe::new(client.subscribe());
        let mut probe2 = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        for probe in [&mut probe1, &mut probe2] {
            probe.wait_for(Duration::from_secs(15), |e| match e {
                RuntimeEvent::ShellSpawned { .. } => Some(()),
                _ => None,
            });
        }
    }
}
