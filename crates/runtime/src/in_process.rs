//! v0 구현체 (설계문서 2.3). worker thread가 세션들을 소유한다.
//! 세션 로직(PTY+terminal+lifecycle)은 session crate 소관 (PR-08).

use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deppy_core::{MuxPaneId, MuxTabId};
use mux::{FocusManager, MuxPane, MuxSnapshot, MuxTab, MuxWindow, PaneSnapshot, TabSnapshot};
use std::path::PathBuf;

use pty::CommandSpec;
use secret::{RedactionService, SecretStore, StreamRedactor};
use session::{Session, StatusDetector, StatusPatterns};
use storage::SessionLogWriter;
use terminal::{TERMINAL_GLOBAL_CACHE_BUDGET_BYTES, TerminalCacheClass, TerminalCacheEvent};

use crate::client::{
    LOCAL_EVENT_QUEUE_CAP, RuntimeClient, RuntimeCommandSink, RuntimeEventReceiver,
    RuntimeEventStream,
};
use crate::command::{RuntimeCommand, SessionId};
use crate::event::{MessagePayload, RuntimeEvent, SpawnKind};
use crate::resource_monitor::{
    ProcessResourceMonitor, ProcessResourceMonitorConfig, SessionResourceTarget,
};

/// 재시작 시 한 세션에서 terminal parser로 다시 읽는 ANSI tail 상한. 전체 audit 로그는
/// append-only로 보존하되 시작 I/O/CPU는 세션당 유계로 유지한다.
const MAX_ANSI_REPLAY_BYTES: u64 = 16 * 1024 * 1024;
/// `terminal.size`가 없던 구버전 로그에서 마지막 zsh ZLE redraw 너비를 찾는 tail 상한.
/// geometry 복구는 최초 한 번뿐이고 이후 resize가 sidecar를 기록한다.
const MAX_ANSI_GEOMETRY_SCAN_BYTES: u64 = 256 * 1024;
const DEFAULT_TERMINAL_COLS: u16 = 80;
const DEFAULT_TERMINAL_ROWS: u16 = 24;
/// 연속 출력 중 viewport snapshot을 만들 수 있는 최소 간격. 8ms는 120Hz 화면을
/// 따라가면서도 token/chunk마다 전체 grid snapshot을 만드는 폭주를 막는다.
const ACTIVE_VIEWPORT_FRAME_INTERVAL: Duration = Duration::from_millis(8);

/// 구독자 한 명의 송신측. 상태 이벤트(unbounded — 세션 수명당 상수 개수의
/// 제어 이벤트라 누적 위험 없음)와 세션별 Viewport slot(최신본만 유지 — 14.5의
/// output bounded 요구를 "누적 불가" 구조로 충족)을 분리한다 (8.2).
struct Subscriber {
    events: SyncSender<RuntimeEvent>,
    overflowed: Arc<std::sync::atomic::AtomicBool>,
    viewports: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>>,
    input_pressures: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>>,
    /// ResourceUsage 최신본 slot — 주기 샘플이 느린 소비자 채널에 무한 누적되지
    /// 않게 latest-value 덮어쓰기(안정성 감사 High #1).
    resource_usage: Arc<Mutex<Option<RuntimeEvent>>>,
    /// 상태 이벤트(채널) 도착 시 소비자를 깨우는 콜백 — UI가 숨겨져(Warm) repaint가
    /// 없을 때도 알림/상태를 처리하도록 (§14.1). Viewport(slot)도 깨운다 — push가
    /// dirty 게이트라 출력이 있을 때만 울리므로 idle 리페인트를 유발하지 않는다.
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
}

fn enqueue_durable_event(
    subscriber: &Subscriber,
    event: RuntimeEvent,
    wakes: &mut Vec<Arc<dyn Fn() + Send + Sync>>,
) -> bool {
    match subscriber.events.try_send(event) {
        Ok(()) => {
            if let Some(wake) = &subscriber.wake {
                wakes.push(Arc::clone(wake));
            }
            true
        }
        Err(TrySendError::Full(_)) => {
            subscriber
                .overflowed
                .store(true, std::sync::atomic::Ordering::Release);
            if let Some(wake) = &subscriber.wake {
                wakes.push(Arc::clone(wake));
            }
            false
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

pub struct InProcessRuntimeClient {
    /// shutdown 시 None — drop되면 worker가 Disconnected로 종료한다
    command_tx: Option<SyncSender<RuntimeCommand>>,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// command 송신과 PTY 출력 도착이 timeout을 기다리지 않고 worker를 깨우는 핸들.
    /// `Thread::unpark` 토큰은 1개로 coalesce되어 wake 폭주가 누적되지 않는다.
    worker_thread: Option<std::thread::Thread>,
}

impl InProcessRuntimeClient {
    /// `output_batch_ms`: 출력/명령이 없을 때 worker fallback poll 주기
    /// (설계문서 10.1, config.performance 소비). 실제 출력은 PTY reader wake로 즉시
    /// pump하고 연속 viewport만 8ms로 합친다. 시작 시점에 고정 — 변경은 앱 재시작 필요.
    /// `secret_store`: SpawnAgent의 secret env를 spawn 직전에 resolve할 때만 사용 (6.3).
    /// `logs_root`: 세션별 redacted 로그 디렉터리 (7장). `redaction`: 공유 레지스트리 —
    /// UI(credential 저장)와 worker(spawn 주입)가 같은 인스턴스에 등록한다.
    pub fn new(
        output_batch_ms: u64,
        secret_store: Arc<dyn SecretStore>,
        logs_root: PathBuf,
        redaction: RedactionService,
        persist: Option<crate::persistence::PersistConfig>,
        // 셸 작업 디렉터리(workspace 폴더). None이면 앱 cwd 상속 — 재시작 시 셸이 이 폴더에서
        // 떠서 claude/codex를 이어갈 수 있다(#2 루트로 튕김 수정).
        cwd: Option<PathBuf>,
        // 셸에 추가할 env (예: PATH 앞단에 deppy shim 디렉터리 — cmux식 hook 주입).
        extra_env: Vec<(String, String)>,
    ) -> Self {
        let mut shell = pty::default_shell();
        shell.cwd = cwd;
        shell.env.extend(extra_env);
        Self::with_shell(
            output_batch_ms,
            secret_store,
            logs_root,
            redaction,
            shell,
            persist,
        )
    }

    /// 테스트용: 셸 대신 임의 명령을 spawn한다.
    pub fn with_shell(
        output_batch_ms: u64,
        secret_store: Arc<dyn SecretStore>,
        logs_root: PathBuf,
        redaction: RedactionService,
        shell: CommandSpec,
        persist: Option<crate::persistence::PersistConfig>,
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
        let run_logs_root = logs_root.join(format!("run-{run_ms}-{}-{seq}", std::process::id()));
        let (command_tx, command_rx) = sync_channel(IN_PROCESS_CMD_QUEUE_CAP);
        let subscribers: Arc<Mutex<Vec<Subscriber>>> = Arc::default();
        let worker_subscribers = Arc::clone(&subscribers);
        let worker = std::thread::Builder::new()
            .name("runtime-worker".into())
            .spawn(move || {
                let persist_pipe =
                    persist.as_ref().and_then(
                        |config| match crate::persistence::PersistPipe::open(config) {
                            Ok(pipe) => Some(pipe),
                            Err(e) => {
                                tracing::warn!("세션 영속 비활성 (DB 열기 실패): {e:#}");
                                None
                            }
                        },
                    );
                Worker {
                    command_rx,
                    subscribers: worker_subscribers,
                    batch: Duration::from_millis(output_batch_ms.max(1)),
                    shell,
                    default_env_plain: Vec::new(),
                    default_env_secrets: Vec::new(),
                    // needsInput hook 키를 워크스페이스 스코프로 만들기 위해 workspace_id를
                    // 워커에 보관한다(SessionId는 워커마다 1부터라 전역 유일하지 않음 — codex High).
                    workspace_id: persist
                        .as_ref()
                        .map(|c| c.workspace_id.clone())
                        .unwrap_or_default(),
                    next_id: 1,
                    sessions: std::collections::HashMap::new(),
                    logs: std::collections::HashMap::new(),
                    detectors: std::collections::HashMap::new(),
                    status_overrides: std::collections::HashMap::new(),
                    logs_root,
                    run_logs_root,
                    redaction,
                    secret_store,
                    mux: MuxState::new(),
                    tab_counter: 0,
                    persist: persist_pipe,
                    exited_order: std::collections::VecDeque::new(),
                    max_exited_backends: DEFAULT_MAX_EXITED_BACKENDS,
                    cache_budget_bytes: TERMINAL_GLOBAL_CACHE_BUDGET_BYTES,
                    archived: std::collections::HashMap::new(),
                    archived_order: std::collections::VecDeque::new(),
                    archived_on_disk: std::collections::HashSet::new(),
                    hidden_scrollback: std::collections::HashSet::new(),
                    render_active: true,
                    suspended: false,
                    resource_monitor: ProcessResourceMonitor::new(
                        ProcessResourceMonitorConfig::default(),
                    ),
                    pressured_sessions: std::collections::HashSet::new(),
                }
                .run();
            })
            .expect("runtime worker thread 생성");
        let worker_thread = Some(worker.thread().clone());
        Self {
            command_tx: Some(command_tx),
            subscribers,
            worker: Some(worker),
            worker_thread,
        }
    }

    /// worker를 종료시키고 세션 정리(PtySession Drop)까지 동기적으로 기다린다.
    /// 앱 종료 경로(on_exit)에서 호출 — main 리턴과 worker 정리 사이의
    /// 스케줄링 경합으로 자식 프로세스가 reap되지 않는 문제 방지.
    pub fn shutdown(&mut self) {
        self.command_tx = None; // Disconnected → worker 루프 break
        if let Some(worker_thread) = &self.worker_thread {
            worker_thread.unpark();
        }
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::warn!("runtime worker join 실패 (panic)");
        }
        self.worker_thread = None;
    }
}

impl Drop for InProcessRuntimeClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl RuntimeCommandSink for InProcessRuntimeClient {
    fn send_command(&self, command: RuntimeCommand) -> anyhow::Result<()> {
        let Some(tx) = self.command_tx.as_ref() else {
            anyhow::bail!("runtime worker가 종료됨");
        };
        match tx.try_send(command) {
            Ok(()) => {
                if let Some(worker_thread) = &self.worker_thread {
                    worker_thread.unpark();
                }
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                anyhow::bail!("runtime 명령 큐 가득참 — local runtime backpressure")
            }
            Err(TrySendError::Disconnected(_)) => anyhow::bail!("runtime worker가 종료됨"),
        }
    }
}

impl RuntimeEventStream for InProcessRuntimeClient {
    fn subscribe(&self) -> RuntimeEventReceiver {
        let (tx, rx) = sync_channel(LOCAL_EVENT_QUEUE_CAP);
        let viewports: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>> =
            Arc::default();
        let input_pressures: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>> =
            Arc::default();
        let resource_usage: Arc<Mutex<Option<RuntimeEvent>>> = Arc::default();
        let overflowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.subscribers
            .lock()
            .expect("subscribers lock")
            .push(Subscriber {
                events: tx,
                overflowed: Arc::clone(&overflowed),
                viewports: Arc::clone(&viewports),
                input_pressures: Arc::clone(&input_pressures),
                resource_usage: Arc::clone(&resource_usage),
                wake: None,
            });
        RuntimeEventReceiver {
            events: rx,
            pending_durable: Mutex::new(None),
            overflowed,
            viewports,
            input_pressures,
            resource_usage,
        }
    }
}

impl InProcessRuntimeClient {
    /// 상태 이벤트 도착 시 `wake`를 호출하는 구독. UI가 숨겨져 프레임이 멈춰도
    /// worker가 UI 스레드를 깨워 알림/상태를 처리하게 한다 (§14.1 Warm 알림 유지).
    pub fn subscribe_with_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) -> RuntimeEventReceiver {
        let (tx, rx) = sync_channel(LOCAL_EVENT_QUEUE_CAP);
        let viewports: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>> =
            Arc::default();
        let input_pressures: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>> =
            Arc::default();
        let resource_usage: Arc<Mutex<Option<RuntimeEvent>>> = Arc::default();
        let overflowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.subscribers
            .lock()
            .expect("subscribers lock")
            .push(Subscriber {
                events: tx,
                overflowed: Arc::clone(&overflowed),
                viewports: Arc::clone(&viewports),
                input_pressures: Arc::clone(&input_pressures),
                resource_usage: Arc::clone(&resource_usage),
                wake: Some(wake),
            });
        RuntimeEventReceiver {
            events: rx,
            pending_durable: Mutex::new(None),
            overflowed,
            viewports,
            input_pressures,
            resource_usage,
        }
    }
}

impl RuntimeClient for InProcessRuntimeClient {}

/// exited 세션의 terminal backend(scrollback)를 유지하는 최대 개수 기본값 (§14.2/14.3).
/// 초과분은 가장 오래 전에 종료된 것부터 압축 아카이브로 내려 메모리를 유계로 만든다
/// (설정에서 변경 — SetTerminalCachePolicy).
const DEFAULT_MAX_EXITED_BACKENDS: usize = 64;
/// 압축 아카이브 총 바이트 예산 — 초과 시 오래된 아카이브부터 제거 (LRU).
/// 개당 압축 ANSI ~수십 KB라 넉넉한 개수를 담는다.
const ARCHIVED_SCROLLBACK_BUDGET_BYTES: usize = 16 * 1024 * 1024;
/// Local runtime command queue cap. Commands are ordered and cannot be coalesced
/// safely in the transport boundary, so overflow is surfaced to the caller.
const IN_PROCESS_CMD_QUEUE_CAP: usize = 1024;
const FINAL_DRAIN_MAX_BYTES: usize = 2 * 1024 * 1024;
const FINAL_DRAIN_MAX_PUMPS: usize = 32;
const SHELL_TITLE_ID: &str = "workspace.spawn.shell";
const AGENT_TITLE_ID: &str = "workspace.spawn.agent";

struct Worker {
    command_rx: Receiver<RuntimeCommand>,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
    batch: Duration,
    shell: CommandSpec,
    /// 워크스페이스 기본 env(.env 자동 주입 — 2026-07-07). 이후 SpawnShell에 적용된다.
    /// secret은 (key, credential_id)로 들고 spawn 직전에만 resolve한다(6.3).
    default_env_plain: Vec<(String, String)>,
    default_env_secrets: Vec<(String, String)>,
    /// 이 워커의 workspace id — needsInput hook 키(`{workspace_id}:{session_id}`)에 쓴다.
    workspace_id: String,
    next_id: u64,
    /// 다중 세션 (PR-08 Session Runtime). 세션 로직은 session crate 소관.
    sessions: std::collections::HashMap<SessionId, Session>,
    /// spawn 직전 secret resolve 전용 (6.3). worker 단일 스레드 접근 (1.4).
    secret_store: Arc<dyn SecretStore>,
    /// 세션별 redacted 로그 (7장). raw 평문 로그는 만들지 않는다.
    logs: std::collections::HashMap<SessionId, SessionLog>,
    /// 세션별 status detector (PR-12) — regex 있는 agent만
    detectors: std::collections::HashMap<SessionId, StatusDetector>,
    /// User status overrides. This affects `SessionStatusViewChanged` only;
    /// legacy `SessionStatusChanged` remains raw detector output.
    status_overrides: std::collections::HashMap<SessionId, session::SessionStatus>,
    logs_root: PathBuf,
    /// 영속 설정이 없는 테스트/원격 워커용 실행별 로그 루트. 숫자 SessionId가
    /// 재시작마다 재사용돼도 서로 append되지 않게 격리한다.
    run_logs_root: PathBuf,
    redaction: RedactionService,
    /// mux 상태 (PR-10) — layout source of truth. UI는 MuxUpdated 스냅샷만 본다.
    mux: MuxState,
    tab_counter: u64,
    /// 세션/mux 영속 파이프 (설정 시에만 — 실패는 best-effort warn)
    persist: Option<crate::persistence::PersistPipe>,
    /// backend를 유지 중인 exited 세션들 (종료 순서 — 오래된 것이 앞). §14.3 cap.
    exited_order: std::collections::VecDeque<SessionId>,
    /// exited 백엔드 LRU 상한 (SetTerminalCachePolicy로 변경 — 설정 UI).
    max_exited_backends: usize,
    /// 전역 터미널 캐시 바이트 예산 (SetTerminalCachePolicy로 변경).
    cache_budget_bytes: usize,
    /// 압축 아카이브 — 백엔드를 내린 exited 세션의 zlib(ANSI) 덤프. pane이 다시
    /// 보이면 복원(inflate)한다 (§14.3 확장, 2026-07-11).
    archived: std::collections::HashMap<SessionId, ArchivedScrollback>,
    /// 아카이브 삽입 순서 (오래된 것이 앞 — 총 바이트 예산 초과 시 제거 순서)
    archived_order: std::collections::VecDeque<SessionId>,
    /// 디스크 아카이브(scrollback.zlib)가 있는 세션들 (PR-A1) — 메모리 아카이브가
    /// 예산 축출돼도 디스크에서 복원 가능함을 fs stat 없이 판정한다.
    archived_on_disk: std::collections::HashSet<SessionId>,
    /// 현재 hidden scrollback cap이 적용된 running 세션들 (§14.3) — 전이 감지용.
    hidden_scrollback: std::collections::HashSet<SessionId>,
    /// Active면 visible pane snapshot 생성, false(Warm 등)면 중단 (§14.1). 세션은 유지.
    render_active: bool,
    /// Suspended로 전환됨 — 이후 큐에 남아 있던 spawn류 명령은 무시한다. suspend 직전
    /// UI가 못 본 SpawnShell/SpawnAgent가 shutdown 경로에서 처리되어 새 PTY가 만들어졌다
    /// 즉시 죽는 race 차단 (codex High, 2026-07-05 live 보호 후속).
    suspended: bool,
    /// PR-U12 process resource sampler. Low cadence and independent from UI frames.
    resource_monitor: ProcessResourceMonitor,
    /// backpressure를 emit한 세션들 — 큐가 비면 해소 이벤트(queued=0)를 보낸다(2026-07-09).
    pressured_sessions: std::collections::HashSet<SessionId>,
}

/// SessionKind ↔ 아카이브 헤더 kind 바이트 (0=shell, 1=agent).
fn archive_kind_to_u8(kind: session::SessionKind) -> u8 {
    match kind {
        session::SessionKind::Shell => 0,
        session::SessionKind::Agent => 1,
    }
}

fn archive_kind_from_u8(byte: u8) -> session::SessionKind {
    match byte {
        1 => session::SessionKind::Agent,
        _ => session::SessionKind::Shell,
    }
}

/// 압축 아카이브 항목 — 백엔드를 내린 exited 세션의 복원 재료 (§14.3 확장).
struct ArchivedScrollback {
    kind: session::SessionKind,
    cols: u16,
    rows: u16,
    scrollback_lines: usize,
    exit_code: Option<u32>,
    /// zlib 압축된 스타일 보존 ANSI 덤프
    compressed: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PumpActivity {
    /// frame pacing 때문에 아직 snapshot으로 내보내지 않은 visible dirty 화면이 있다.
    pending_viewport: bool,
    /// 이번 pump에서 visible viewport를 하나 이상 내보냈다.
    viewport_emitted: bool,
}

/// 세션 하나의 redaction 상태 + 로그 파일 (설계문서 7장).
struct SessionLog {
    redactor: StreamRedactor,
    writer: SessionLogWriter,
    last_log_offset: u64,
}

impl SessionLog {
    fn append_redacted_output(&mut self, redacted: &[u8]) -> anyhow::Result<u64> {
        self.writer.append_output(redacted)?;
        self.last_log_offset = self
            .last_log_offset
            .saturating_add(u64::try_from(redacted.len()).unwrap_or(u64::MAX));
        Ok(self.last_log_offset)
    }
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
    fn spawn_session(
        id: SessionId,
        kind: session::SessionKind,
        spec: &CommandSpec,
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
    ) -> anyhow::Result<Session> {
        let worker_thread = std::thread::current();
        let output_wake: pty::PtyOutputWake = Arc::new(move || worker_thread.unpark());
        Session::spawn_with_spec_and_output_wake(
            id,
            kind,
            spec,
            cols,
            rows,
            scrollback_lines,
            output_wake,
        )
    }

    fn run(&mut self) {
        // config batch는 출력/명령이 전혀 없을 때의 fallback poll 간격이다. 출력 reader와
        // command sender가 이 thread를 unpark하므로 첫 반응은 timeout과 무관하게 즉시다.
        // 연속 출력은 8ms(또는 더 작은 테스트 batch) frame pacing으로 snapshot만 합친다.
        let viewport_interval = self.batch.min(ACTIVE_VIEWPORT_FRAME_INTERVAL);
        let mut next_viewport_at = std::time::Instant::now();
        let mut pending_viewport = false;
        loop {
            let now = std::time::Instant::now();
            let wait = if pending_viewport {
                next_viewport_at
                    .saturating_duration_since(now)
                    .min(self.batch)
            } else {
                self.batch
            };
            std::thread::park_timeout(wait);

            // 몰려온 명령은 한 번에 소화하되 상한을 둔다 — 명령 폭주
            // (paste/resize 연타)가 PTY pump·로그·상태 감지를 굶기지 않게 한다.
            const COMMAND_BURST_CAP: usize = 128;
            let mut handled = 0;
            let mut disconnected = false;
            while handled < COMMAND_BURST_CAP {
                match self.command_rx.try_recv() {
                    Ok(command) => {
                        self.handle_command(command);
                        handled += 1;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected {
                // 기존 recv_timeout 루프처럼 마지막 command batch 뒤 한 번은 pump해
                // command 직후 도착한 PTY tail과 status/persistence를 반영하고 종료한다.
                let _ = self.pump_sessions(true);
                self.pump_resource_monitor();
                self.pump_input_pressure_resolution();
                break; // client drop → 종료
            }
            if handled == COMMAND_BURST_CAP {
                // unpark 토큰은 coalesced되므로 대량 명령이 이미 queue에 들어온 경우
                // 다음 tick을 스스로 예약해 command backlog를 timeout까지 방치하지 않는다.
                std::thread::current().unpark();
            }

            let allow_viewport = std::time::Instant::now() >= next_viewport_at;
            let activity = self.pump_sessions(allow_viewport);
            if activity.viewport_emitted {
                next_viewport_at = std::time::Instant::now() + viewport_interval;
            }
            pending_viewport = activity.pending_viewport;
            self.pump_resource_monitor();
            self.pump_input_pressure_resolution();
        }
        // 앱 종료: 남은 출력을 마지막으로 기록하고(로그 유실 방지 — codex 리뷰),
        // 열려 있는 로그의 redaction carry를 flush해 마감한다
        // (shutdown()이 join하므로 여기까지 동기 보장)
        let all_sessions: Vec<SessionId> = self.sessions.keys().copied().collect();
        for session in &all_sessions {
            self.final_drain(*session);
        }
        // 이 워커의 세션들을 영속 상태에서 exited로 마감한다 — 워커가 죽은 뒤(전환/종료)
        // running으로 남지 않도록. 각 세션은 자기 UUID 행만 건드리므로 다른 워커(같은
        // workspace를 다시 연 경우 포함)의 세션과 충돌하지 않는다 (codex 리뷰). buffered
        // 명령까지 위 루프에서 처리된 뒤이므로 spawn 누락도 없다.
        if let Some(pipe) = &mut self.persist {
            for session in &all_sessions {
                pipe.session_exited(*session);
            }
            if let Err(e) = pipe.flush_async_writes() {
                tracing::warn!("세션 영속 batch flush 실패 (shutdown): {e:#}");
            }
        }
        // 스크롤백 아카이브 flush (PR-A1): 미기록 exited + suspend로 죽는 running
        // agent의 최종 grid — 재시작/suspend 해제 후 열람 복원(PR-A2)의 원천.
        // running 셸은 제외 — 복원 시 respawn+로그 replay가 기대 동작이다.
        for session in &all_sessions {
            let skip = self.sessions.get(session).is_some_and(|live| {
                live.lifecycle().is_running() && live.kind() == session::SessionKind::Shell
            });
            if !skip {
                self.write_scrollback_archive(*session);
            }
        }
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
        // wake 콜백은 subscribers 락을 놓은 뒤에 호출한다 — 콜백이 임의 Fn(공개
        // subscribe_with_wake)이라 재진입 시 subscribers 락에서 데드락날 수 있다 (codex 리뷰).
        let mut wakes: Vec<Arc<dyn Fn() + Send + Sync>> = Vec::new();
        {
            let mut subscribers = self.subscribers.lock().expect("subscribers lock");
            subscribers.retain(|subscriber| {
                if let RuntimeEvent::Viewport { session, .. } = &event {
                    if Arc::strong_count(&subscriber.viewports) <= 1 {
                        return false;
                    }
                    subscriber
                        .viewports
                        .lock()
                        .expect("viewport slot lock")
                        .insert(*session, event.clone());
                    // Viewport(출력)도 wake — push는 dirty(이번 tick 새 출력) 게이트라
                    // idle엔 발생하지 않고, 출력 도착 시에만 UI를 깨운다. 이로써 UI측
                    // 50ms 상시 폴링(가시+running 시 20fps 리페인트 = idle CPU ~10%)을
                    // 제거할 수 있다 (가시 상태 상시 리페인트 원인 조사, 2026-07-04).
                    if let Some(wake) = &subscriber.wake {
                        wakes.push(Arc::clone(wake));
                    }
                    true
                } else if let RuntimeEvent::PtyInputPressure { session, .. } = &event {
                    if Arc::strong_count(&subscriber.input_pressures) <= 1 {
                        return false;
                    }
                    subscriber
                        .input_pressures
                        .lock()
                        .expect("input pressure slot lock")
                        .insert(*session, event.clone());
                    if let Some(wake) = &subscriber.wake {
                        wakes.push(Arc::clone(wake));
                    }
                    true
                } else if matches!(&event, RuntimeEvent::ResourceUsage { .. }) {
                    // 주기 샘플 — 최신본 slot 덮어쓰기(느린 소비자 채널 누적 방지).
                    if Arc::strong_count(&subscriber.resource_usage) <= 1 {
                        return false;
                    }
                    *subscriber
                        .resource_usage
                        .lock()
                        .expect("resource usage slot lock") = Some(event.clone());
                    if let Some(wake) = &subscriber.wake {
                        wakes.push(Arc::clone(wake));
                    }
                    true
                } else {
                    // 포화 시 false → retain에서 제거되어 느린 구독자가 disconnect된다.
                    enqueue_durable_event(subscriber, event.clone(), &mut wakes)
                }
            });
        }
        for wake in wakes {
            wake();
        }
    }

    fn pump_resource_monitor(&mut self) {
        let targets: Vec<_> = self
            .sessions
            .values()
            .map(|session| SessionResourceTarget {
                session: session.id(),
                identity: session.process_identity(),
            })
            .collect();
        if let Some((snapshot, session_usage)) =
            self.resource_monitor.sample_if_due_with_sessions(&targets)
        {
            self.emit(RuntimeEvent::ResourceUsage {
                snapshot,
                session_usage,
            });
        }
    }

    /// backpressure 해소 폴링 — pressure를 보냈던 세션의 입력 큐가 비면
    /// `queued_messages=0`인 PtyInputPressure를 한 번 보내 UI 뱃지를 내리게 한다
    /// (해소 전용 variant를 추가하지 않고 기존 이벤트의 0 값으로 표현 — wire 불변).
    fn pump_input_pressure_resolution(&mut self) {
        if self.pressured_sessions.is_empty() {
            return;
        }
        let resolved: Vec<SessionId> = self
            .pressured_sessions
            .iter()
            .copied()
            .filter(|s| {
                self.sessions
                    .get(s)
                    .map(|session| session.input_queue_idle())
                    // 세션이 사라졌으면 해소된 것으로 취급(추적 제거).
                    .unwrap_or(true)
            })
            .collect();
        for session in resolved {
            self.pressured_sessions.remove(&session);
            if self.sessions.contains_key(&session) {
                // spawn은 항상 기본 정책을 쓰므로 max 값도 기본에서 취한다.
                let policy = pty::PtyInputQueuePolicy::default();
                self.emit(RuntimeEvent::PtyInputPressure {
                    session,
                    pressure: pty::PtyInputPressure {
                        attempted_bytes: 0,
                        queued_bytes: 0,
                        queued_messages: 0,
                        max_bytes: policy.max_bytes,
                        max_messages: policy.max_messages,
                        reason: pty::PtyInputRejectReason::QueueFull,
                    },
                });
            }
        }
    }

    fn handle_command(&mut self, command: RuntimeCommand) {
        match command {
            RuntimeCommand::SpawnShell {
                cols,
                rows,
                scrollback_lines,
            } => {
                if self.suspended {
                    // suspend 이후 큐 잔여 spawn — 무시 (생성 즉시 죽는 것보다 안전)
                    let _ = (cols, rows, scrollback_lines);
                    self.emit(RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Shell,
                        message: MessagePayload::new("runtime.spawn_failed.suspended"),
                    });
                    return;
                }
                let id = SessionId(self.next_id);
                self.next_id += 1;
                match Self::spawn_session(
                    id,
                    session::SessionKind::Shell,
                    &self.shell_with_session(id), // 테스트 주입 가능해야 하므로 default_shell 헬퍼 대신 spec 직접
                    cols,
                    rows,
                    scrollback_lines,
                ) {
                    Ok(new_session) => {
                        self.sessions.insert(id, new_session);
                        // 셸도 status detector 설치 — regex 패턴은 없지만 idle heuristic
                        // (3단)이 Running/Idle을 감지해 레일에 상태가 반영된다(#2). agent와
                        // 달리 셸엔 감지 regex가 없어 그동안 상태가 아예 안 났다.
                        self.detectors.insert(
                            id,
                            StatusDetector::new(StatusPatterns::compile(None, None, None, None)),
                        );
                        self.attach_in_new_tab(id, SHELL_TITLE_ID);
                        if let Some(pipe) = &mut self.persist {
                            let args: Vec<String> = self.shell.args.clone();
                            pipe.session_spawned(
                                id,
                                "shell",
                                None,
                                SHELL_TITLE_ID,
                                &self.shell.program,
                                &args,
                                &Self::spawn_cwd_string(&self.shell.cwd),
                            );
                        }
                        self.open_session_log(id);
                        // MuxUpdated → Spawned → Viewport(slot) 순서 —
                        // drain의 happens-before 계약 (Viewport가 Spawned보다 먼저
                        // slot에 들어가면 안 된다)
                        self.emit_mux_snapshot();
                        self.emit(RuntimeEvent::ShellSpawned { session: id });
                        self.push_watched_viewports();
                    }
                    Err(e) => self.emit(RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Shell,
                        message: MessagePayload::new("runtime.spawn_failed.shell")
                            .arg("error", format!("{e:#}"))
                            .diagnostic(format!("{e:#}")),
                    }),
                }
            }
            RuntimeCommand::SpawnAgent {
                cols,
                rows,
                scrollback_lines,
                agent_config_id,
                command,
                args,
                env_plain,
                env_secrets,
                waiting_regex,
                approval_regex,
                error_regex,
                done_regex,
            } => {
                if self.suspended {
                    self.emit(RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Agent,
                        message: MessagePayload::new("runtime.spawn_failed.suspended"),
                    });
                    return;
                }
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
                            resolve_failed = Some((credential_id, format!("{e:#}")));
                            break;
                        }
                    }
                }
                if let Some((credential_id, error)) = resolve_failed {
                    self.emit(RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Agent,
                        message: MessagePayload::new("runtime.spawn_failed.agent_secret")
                            .arg("credential_id", credential_id)
                            .arg("error", error.clone())
                            .diagnostic(error),
                    });
                    return;
                }
                let id = SessionId(self.next_id);
                self.next_id += 1;
                // 앱이 직접 띄운 에이전트에도 needsInput hook 키를 주입한다(셸과 동일 —
                // 안 하면 Agents UI 실행 세션은 needsInput 미반영, codex Medium).
                env.push(("DEPPY_SESSION_ID".to_owned(), self.session_key(id)));
                let spec = CommandSpec {
                    program: command,
                    args,
                    env,
                    // 에이전트도 워크스페이스 폴더에서 실행 — 셸과 동일 cwd(agent 이어가기).
                    cwd: self.shell.cwd.clone(),
                };
                match Self::spawn_session(
                    id,
                    session::SessionKind::Agent,
                    &spec,
                    cols,
                    rows,
                    scrollback_lines,
                ) {
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
                        self.attach_in_new_tab(id, AGENT_TITLE_ID);
                        if let Some(pipe) = &mut self.persist {
                            // 스키마 CHECK: agent kind는 agent_id 필수 — config id가
                            // 없는 spawn(perf 하네스 등)은 shell kind로 기록한다
                            let kind = if agent_config_id.is_some() {
                                "agent"
                            } else {
                                "shell"
                            };
                            pipe.session_spawned(
                                id,
                                kind,
                                agent_config_id,
                                AGENT_TITLE_ID,
                                &spec.program,
                                &spec.args,
                                &Self::spawn_cwd_string(&spec.cwd),
                            );
                        }
                        self.open_session_log(id);
                        self.emit_mux_snapshot();
                        self.emit(RuntimeEvent::AgentSpawned { session: id });
                        self.push_watched_viewports();
                    }
                    Err(e) => self.emit(RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Agent,
                        message: MessagePayload::new("runtime.spawn_failed.agent")
                            .arg("error", format!("{e:#}"))
                            .diagnostic(format!("{e:#}")),
                    }),
                }
            }
            RuntimeCommand::SetSessionDefaultEnv {
                env_plain,
                env_secrets,
            } => {
                // 이후 SpawnShell부터 적용 — 기존 세션은 건드리지 않는다(.env 자동 주입).
                self.default_env_plain = env_plain;
                self.default_env_secrets = env_secrets;
            }
            RuntimeCommand::SetShellCwd(cwd) => {
                // 프로젝트 폴더 live 변경 — 이후 SpawnShell/SpawnAgent가 이 cwd에서 뜬다.
                self.shell.cwd = cwd;
            }
            RuntimeCommand::UpdateSessionCwd { session, cwd } => {
                // 감지 워커가 관측한 live cd — persist에 기록해 재시작 복원이 pane별
                // 원래 폴더에서 셸을 띄우게 한다(A안 2026-07-08).
                if let Some(pipe) = &mut self.persist {
                    pipe.update_session_cwd(session, &cwd);
                }
            }
            RuntimeCommand::SetTerminalCachePolicy {
                max_exited_backends,
                cache_budget_bytes,
            } => {
                // 원값 방어 (remote wire 포함) — 설정 UI clamp와 동일 기준.
                // 적용은 다음 pump tick의 archive_over_cap이 처리한다.
                self.max_exited_backends = max_exited_backends.clamp(4, 512);
                self.cache_budget_bytes =
                    cache_budget_bytes.clamp(32 * 1024 * 1024, 2048 * 1024 * 1024);
            }
            RuntimeCommand::WriteInput { session, bytes } => {
                if let Some(active) = self.sessions.get_mut(&session) {
                    match active.write_input(&bytes) {
                        Some(pty::PtyInputEnqueueResult::Accepted) => {
                            // 사용자 입력 = 화면 프롬프트에 대한 응답 신호 (status detector)
                            if let Some(detector) = self.detectors.get_mut(&session) {
                                detector.on_input();
                            }
                        }
                        Some(pty::PtyInputEnqueueResult::Backpressured { pressure }) => {
                            self.pressured_sessions.insert(session);
                            self.emit(RuntimeEvent::PtyInputPressure { session, pressure });
                        }
                        Some(pty::PtyInputEnqueueResult::Rejected { pressure }) => {
                            // PayloadTooLarge/closed/writer 없음은 큐가 빠지면 회복되는 상태가
                            // 아니다. resolution poll에 넣으면 즉시 QueueFull(queued=0)이
                            // 최신값 slot을 덮어 원래 거부 원인을 잃는다.
                            self.emit(RuntimeEvent::PtyInputPressure { session, pressure });
                        }
                        None => {}
                    }
                }
            }
            RuntimeCommand::SetUserStatusOverride { session, override_ } => {
                if !self.sessions.contains_key(&session) {
                    return;
                }
                match override_ {
                    session::UserStatusOverride::Mark(status) => {
                        self.status_overrides.insert(session, status);
                    }
                    session::UserStatusOverride::Clear => {
                        self.status_overrides.remove(&session);
                    }
                }
                self.emit(RuntimeEvent::SessionStatusViewChanged {
                    session,
                    view: self.session_status_view(session),
                });
            }
            RuntimeCommand::Resize {
                session,
                cols,
                rows,
            } => {
                if let Some(active) = self.sessions.get_mut(&session) {
                    if let Some(event) = active.resize(cols, rows) {
                        trace_terminal_cache_event(session, event);
                    }
                    self.save_terminal_size(session, cols, rows);
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
                self.final_drain(session);
                // Session drop → PtySession Drop이 process group 정리를 보장한다
                self.sessions.remove(&session);
                self.exited_order.retain(|s| *s != session);
                self.hidden_scrollback.remove(&session);
                self.detectors.remove(&session);
                self.status_overrides.remove(&session);
                self.close_session_log(session, "killed", None);
                if let Some(pipe) = &mut self.persist {
                    pipe.session_exited(session);
                }
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
            } => {
                if self.suspended {
                    let _ = (pane, direction, scrollback_lines);
                    self.emit(RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Shell,
                        message: MessagePayload::new("runtime.spawn_failed.suspended"),
                    });
                    return;
                }
                self.split_pane(pane, direction, scrollback_lines);
            }
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
            RuntimeCommand::SetWorkspaceState(state) => {
                // §14.1: Active만 render/snapshot. Warm/Suspended/Closed는 snapshot 중단.
                // (Suspended/Closed의 PTY 종료는 workspace-close 도입 시 — 지금은 유지)
                let active = matches!(state, crate::command::WorkspaceRuntimeState::Active);
                self.suspended = matches!(
                    state,
                    crate::command::WorkspaceRuntimeState::Suspended
                        | crate::command::WorkspaceRuntimeState::Closed
                );
                if active && !self.render_active {
                    // Warm→Active 복귀: 전체 mux 스냅샷 + 쌓인 화면을 즉시 다시 push.
                    // workspace 전환 복귀 시 UI가 fresh workspace_ui를 만들 수 있으므로
                    // tab/pane 구조(MuxUpdated)부터 다시 보내야 재구성된다 (워커-per-ws).
                    self.render_active = true;
                    self.emit_mux_snapshot();
                    self.push_watched_viewports();
                } else {
                    self.render_active = active;
                }
            }
            RuntimeCommand::RestoreWorkspace => {
                // "완전히 빈 상태(세션 0)"일 때만 복원한다 — 시작 직후 SpawnShell/
                // SpawnAgent가 먼저 처리돼 세션이 생겼으면 skip해 hybrid 상태를 막는다.
                // suspended면 복원하지 않는다 (shutdown 큐 잔여 — 복원 즉시 죽는 것 방지).
                if !self.suspended && self.sessions.is_empty() {
                    self.restore_saved_layout();
                }
            }
            RuntimeCommand::RenamePane { pane, title } => {
                if let Some(p) = self.mux.panes.get_mut(&pane) {
                    p.title = title;
                    self.emit_mux_snapshot(); // UI 반영 + 영속 저장
                }
            }
            RuntimeCommand::ResizeSplit { tab, path, ratio } => {
                if let Some(t) = self.mux.tabs.get_mut(&tab)
                    && t.layout.set_split_ratio(&path, ratio)
                {
                    // 새 배치(구조)만 UI에 반영 — 드래그 중 매 프레임 올 수 있으므로
                    // snapshot 강제 재생성(watched push)은 하지 않는다. pane 크기 변화에
                    // 따른 PTY Resize는 UI가 렌더 시 cols/rows 변화를 감지해 보낸다.
                    self.emit_mux_snapshot();
                }
            }
        }
    }

    /// 세션 로그를 연다. 실패해도 세션은 계속 (로그만 없음 — warn).
    fn open_session_log(&mut self, session: SessionId) {
        let persistent_key = self
            .persist
            .as_ref()
            .and_then(|pipe| pipe.session_log_key(session))
            .map(str::to_owned);
        let opened = match persistent_key.as_deref() {
            Some(key) => SessionLogWriter::open_key(&self.logs_root, key),
            None => SessionLogWriter::open(&self.run_logs_root, session),
        };
        match opened {
            Ok(mut writer) => {
                let last_log_offset = writer.ansi_len().unwrap_or(0);
                let _ = writer.append_event("spawned", None);
                self.logs.insert(
                    session,
                    SessionLog {
                        redactor: self.redaction.stream_redactor(),
                        writer,
                        last_log_offset,
                    },
                );
            }
            Err(e) => tracing::warn!("세션 로그 열기 실패: {e:#}"),
        }
    }

    /// 영속 sidecar가 있으면 정확한 마지막 grid를 사용한다. 구버전 세션은 zsh가
    /// PROMPT_EOL_MARK를 지울 때 남긴 width-dependent ANSI 패턴에서 열 수를 한 번
    /// 추론한다. 80열 로그를 실제 121열 pane처럼 잘못 재생하면 공백이 wrap되어
    /// 역상 `%`와 중복 프롬프트가 화면 곳곳에 남는다.
    fn restored_terminal_size(logs_root: &std::path::Path, persistent_id: &str) -> (u16, u16) {
        match SessionLogWriter::load_terminal_size(logs_root, persistent_id) {
            Ok(Some(size)) => return size,
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(persistent_id, "저장 터미널 크기 무시: {error:#}");
            }
        }

        let path = match SessionLogWriter::ansi_path(logs_root, persistent_id) {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(persistent_id, "복원 ANSI 로그 경로 거부: {error:#}");
                return (DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS);
            }
        };
        match infer_zsh_terminal_cols(&path, MAX_ANSI_GEOMETRY_SCAN_BYTES) {
            Ok(Some(cols)) => {
                tracing::info!(persistent_id, cols, "구버전 ANSI 로그에서 터미널 너비 복구");
                (cols, DEFAULT_TERMINAL_ROWS)
            }
            Ok(None) => (DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS)
            }
            Err(error) => {
                tracing::warn!(persistent_id, path = %path.display(), "복원 터미널 너비 탐색 실패: {error}");
                (DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS)
            }
        }
    }

    fn save_terminal_size(&self, session: SessionId, cols: u16, rows: u16) {
        let Some(persistent_id) = self
            .persist
            .as_ref()
            .and_then(|pipe| pipe.session_log_key(session))
        else {
            return;
        };
        if let Err(error) =
            SessionLogWriter::save_terminal_size(&self.logs_root, persistent_id, cols, rows)
        {
            tracing::warn!(
                persistent_id,
                cols,
                rows,
                "터미널 크기 영속 실패: {error:#}"
            );
        }
    }

    /// pane이 가리키는 이전 영속 세션의 redacted ANSI를 새 terminal backend에
    /// 스트리밍 재생한다. 파일이 없는 최초/legacy 세션은 정상적인 빈 복원이다.
    fn replay_saved_ansi(logs_root: &std::path::Path, persistent_id: &str, session: &mut Session) {
        let path = match SessionLogWriter::ansi_path(logs_root, persistent_id) {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!(persistent_id, "복원 ANSI 로그 경로 거부: {e:#}");
                return;
            }
        };
        let mut file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!(persistent_id, path = %path.display(), "복원 ANSI 로그 열기 실패: {e}");
                return;
            }
        };
        let replay_start = match seek_ansi_replay_tail(&mut file, MAX_ANSI_REPLAY_BYTES) {
            Ok(start) => start,
            Err(e) => {
                tracing::warn!(persistent_id, path = %path.display(), "복원 ANSI tail 탐색 실패: {e}");
                return;
            }
        };
        match session.replay_ansi(&mut file) {
            Ok(bytes) => {
                if bytes > 0
                    && let Err(error) = session.finish_ansi_replay()
                {
                    tracing::warn!(persistent_id, "ANSI 복원 경계 초기화 실패: {error:#}");
                }
                tracing::info!(
                    persistent_id,
                    bytes,
                    replay_start,
                    "이전 ANSI scrollback 복원"
                );
            }
            Err(e) => tracing::warn!(persistent_id, "이전 ANSI scrollback 복원 실패: {e:#}"),
        }
    }

    /// 세션 로그를 닫는다 — carry flush 후 종료 이벤트 기록.
    fn close_session_log(&mut self, session: SessionId, event: &str, detail: Option<&str>) {
        if let Some(mut log) = self.logs.remove(&session) {
            let tail = log.redactor.flush();
            if let Ok(offset) = log.append_redacted_output(&tail)
                && let Some(pipe) = &mut self.persist
            {
                pipe.session_log_offset(session, offset);
            }
            let _ = log.writer.append_event(event, detail);
            log.writer.flush();
        }
    }

    /// 새 tab에 pane 하나를 만들어 세션을 attach하고 포커스한다.
    /// needsInput hook 세션 키 — `{workspace_id}:{session_id}`. SessionId는 워커마다 1부터라
    /// 전역 유일하지 않으므로 workspace_id로 스코프해 워크스페이스 간 오표시를 막는다(codex High).
    fn session_key(&self, id: SessionId) -> String {
        format!("{}:{}", self.workspace_id, id.0)
    }

    /// DEPPY_SESSION_ID를 주입한 셸 spec — 이 셸에서 실행된 claude/codex의 hook이 세션을
    /// 식별해 needsInput을 보고한다(옵션2 hook 배선). spawn 시점에 이미 안다(pane 순서 무관).
    /// spawn spec의 cwd를 persist 기록용 문자열로 — None이면 앱 프로세스 cwd.
    fn spawn_cwd_string(cwd: &Option<std::path::PathBuf>) -> String {
        cwd.as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            })
    }

    fn shell_with_session(&self, id: SessionId) -> CommandSpec {
        let mut spec = self.shell.clone();
        spec.env
            .push(("DEPPY_SESSION_ID".to_owned(), self.session_key(id)));
        // 워크스페이스 기본 env(.env 자동 주입). secret은 여기(spawn 직전)에서만 resolve.
        // 셸은 에이전트와 달리 spawn 실패보다 부분 주입이 낫다 — 실패 키는 건너뛰고 경고.
        spec.env.extend(self.default_env_plain.iter().cloned());
        for (key, credential_id) in &self.default_env_secrets {
            match self.secret_store.get_secret(credential_id) {
                Ok(value) => {
                    self.redaction.register(&value);
                    spec.env.push((key.clone(), value.expose().to_owned()));
                }
                Err(e) => {
                    // credential id만 로그 — secret 값/키 이름은 남기지 않는다(6.3 관례).
                    tracing::warn!(
                        credential_id,
                        "기본 env secret resolve 실패 — 건너뜀: {e:#}"
                    );
                }
            }
        }
        spec
    }

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

    /// 복원(PR-14)이 spawn하는 fresh 셸의 scrollback 기본값 — 실제 config 값은
    /// 복원 경로에 없어 app::config::TerminalConfig 기본값(10_000)과 맞춘 상수를 쓴다.
    const RESTORE_SCROLLBACK_LINES: usize = 10_000;

    /// 이전 실행이 저장한 mux layout을 복원한다 (설계문서 §11.1~11.5, §14, PR-14).
    /// `RestoreWorkspace` 명령 핸들러가 빈 상태(세션 0)를 확인한 뒤 호출한다.
    /// 저장된 tab이 없으면 아무 것도 하지 않는다(기존 빈 시작 동작 유지).
    ///
    /// agent 세션은 재실행하지 않는다 — 저장된 pane의 session_kind와 무관하게
    /// 항상 fresh 셸만 spawn한다(agent 명령 재실행은 파괴적일 수 있다).
    fn restore_saved_layout(&mut self) {
        let (tabs, active_tab) = match &mut self.persist {
            Some(pipe) => pipe.take_saved_layout(),
            None => return,
        };
        if tabs.is_empty() {
            return;
        }
        // 복원 tab/pane 제목의 "셸 N"/"에이전트 N" 최대 N 이상으로 counter를 전진 —
        // tabs.len()만으로는 중간 tab을 닫았던 경우(셸 1·3만 남음) 다음 spawn이
        // 기존 "셸 3"과 충돌한다 (codex 리뷰). id는 유일하지만 제목 정합을 위해.
        let max_suffix = tabs
            .iter()
            .flat_map(|tab| {
                std::iter::once(tab.title.as_str())
                    .chain(tab.panes.iter().map(|p| p.title.as_str()))
            })
            .filter_map(title_suffix)
            .max()
            .unwrap_or(0);
        for tab in tabs {
            self.restore_tab(tab);
        }
        self.tab_counter = self.tab_counter.max(max_suffix);
        if active_tab.is_some() {
            self.mux.window.active_tab = active_tab;
        }
        self.mux.fix_focus();
        self.emit_mux_and_watched();
        // PR-A2: 열람 전용으로 복원된 exited 세션의 상태 배지 정합 — 상태 뷰만
        // 재공표한다. SessionExited는 다시 emit하지 않는다(재시작마다 완료 알림이
        // 재발화하는 것 방지 — 복원 시점에 exited인 세션은 전부 archived 복원분).
        let restored_exited: Vec<(SessionId, Option<u32>)> = self
            .sessions
            .iter()
            .filter_map(|(id, live)| match live.lifecycle() {
                session::SessionLifecycle::Exited { exit_code } => Some((*id, exit_code)),
                session::SessionLifecycle::Running => None,
            })
            .collect();
        for (session, exit_code) in restored_exited {
            let status = if exit_code == Some(0) {
                session::SessionStatus::Done
            } else {
                session::SessionStatus::Error
            };
            self.emit(RuntimeEvent::SessionStatusViewChanged {
                session,
                view: session::SessionStatusView::process_exit(status),
            });
        }
    }

    /// 저장된 tab 하나를 재구성한다 — tab/pane id, layout 구조, active_pane은
    /// 저장된 그대로 재사용한다(내부 일관성 + 재저장 시 같은 행을 갱신하기 위함).
    fn restore_tab(&mut self, tab: persist::TabState) {
        for pane in &tab.panes {
            self.restore_pane(pane);
        }
        let restored = MuxTab {
            id: tab.id,
            title: tab.title,
            layout: tab.layout,
            active_pane: tab.active_pane,
        };
        self.mux.window.add_tab(restored.id.clone());
        self.mux.tabs.insert(restored.id.clone(), restored);
    }

    /// 저장된 pane 하나에 fresh 셸을 spawn해 attach한다. spawn 실패 시에도 pane
    /// 자체는 만든다(session_id 없이) — 기존 "세션을 잃은 pane" 모델과 동일하게
    /// layout/tab 구조는 살아있게 한다.
    /// agent였던 pane은 respawn 대신 열람 전용 복원(PR-A2) — agent 재실행 금지는
    /// persistence 헤더의 안전 요구사항이고, 결과 화면 보존이 목적이다.
    fn restore_pane(&mut self, pane_state: &persist::PaneState) {
        if let Some(persistent_id) = pane_state.session_id.as_deref() {
            let was_agent = self
                .persist
                .as_ref()
                .and_then(|pipe| pipe.restored_session_kind(persistent_id))
                .is_some_and(|kind| kind == "agent");
            if was_agent && self.restore_archived_pane(pane_state, persistent_id) {
                return;
            }
        }
        let mut pane = MuxPane::new(pane_state.id.clone(), pane_state.title.clone());
        let id = SessionId(self.next_id);
        self.next_id += 1;
        // A안(2026-07-08): pane별 마지막 작업 폴더로 복원 — 저장 cwd가 유효 디렉터리면
        // 그 폴더에서 셸을 띄우고, 그 폴더의 .env도 이 pane에만 주입한다(pane별 프로젝트).
        // 무효/미기록이면 워크스페이스 cwd(기존 동작)로 폴백.
        let mut spec = self.shell_with_session(id);
        let restored_cwd = pane_state
            .cwd
            .as_deref()
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_dir());
        if let Some(dir) = &restored_cwd {
            spec.cwd = Some(dir.clone());
            let dotenv = dir.join(".env");
            if let Ok(content) = std::fs::read_to_string(&dotenv) {
                for (key, value) in crate::dotenv::parse_dotenv(&content) {
                    // 워크스페이스 기본 env보다 뒤에 붙어 pane 폴더 값이 이긴다
                    // (PTY env 적용은 순차라 마지막 값 승리). secret 값은 로그에
                    // 새지 않게 redaction에 등록.
                    if crate::dotenv::is_secret_key(&key) {
                        self.redaction
                            .register(&secret::SecretString::new(value.clone()));
                    }
                    spec.env.push((key, value));
                }
            }
        }
        let spawn_cwd = Self::spawn_cwd_string(&spec.cwd);
        let (restore_cols, restore_rows) = pane_state
            .session_id
            .as_deref()
            .map(|persistent_id| Self::restored_terminal_size(&self.logs_root, persistent_id))
            .unwrap_or((DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS));
        match Self::spawn_session(
            id,
            session::SessionKind::Shell,
            &spec,
            restore_cols,
            restore_rows,
            Self::RESTORE_SCROLLBACK_LINES,
        ) {
            Ok(mut new_session) => {
                if let Some(persistent_id) = pane_state.session_id.as_deref() {
                    Self::replay_saved_ansi(&self.logs_root, persistent_id, &mut new_session);
                }
                self.sessions.insert(id, new_session);
                // 복원된 셸도 status detector 설치 — 없으면 상태 감지가 아예 안 됐다
                // (셸 135가 복원 셸이라 built-in 프롬프트 감지도 무동작, #92/#93).
                self.detectors.insert(
                    id,
                    StatusDetector::new(StatusPatterns::compile(None, None, None, None)),
                );
                pane.session_id = Some(id);
                if let Some(pipe) = &mut self.persist {
                    let args: Vec<String> = self.shell.args.clone();
                    if let Some(persistent_id) = pane_state.session_id.as_deref() {
                        pipe.session_restored(
                            id,
                            persistent_id,
                            "shell",
                            None,
                            &pane_state.title,
                            &self.shell.program,
                            &args,
                            &spawn_cwd,
                        );
                    } else {
                        pipe.session_spawned(
                            id,
                            "shell",
                            None,
                            &pane_state.title,
                            &self.shell.program,
                            &args,
                            &spawn_cwd,
                        );
                    }
                }
                self.open_session_log(id);
            }
            Err(e) => {
                tracing::warn!(pane_id = %pane_state.id.0, "복원 중 셸 spawn 실패: {e:#}");
            }
        }
        self.mux.panes.insert(pane_state.id.clone(), pane);
    }

    /// agent pane의 열람 전용 복원 (PR-A2): 디스크 아카이브 1차, 로그 tail 폴백.
    /// 성공 시 세션·pane 등록까지 마치고 true — 재결속 실패면 false로
    /// 셸 respawn 경로에 맡긴다 (행이 이미 소비된 예외 상황).
    fn restore_archived_pane(
        &mut self,
        pane_state: &persist::PaneState,
        persistent_id: &str,
    ) -> bool {
        let id = SessionId(self.next_id);
        self.next_id += 1;
        // 재결속 먼저 — save_layout이 rows에서 UUID를 찾으므로 이게 빠지면
        // 다음 저장에서 pane↔세션 연결이 영구 유실된다 (계획 문서 §1 함정)
        if let Some(pipe) = &mut self.persist
            && !pipe.session_rebound_archived(id, persistent_id)
        {
            return false;
        }
        let restored = match storage::scrollback_archive::read(&self.logs_root, persistent_id) {
            Ok(Some((meta, dump))) => {
                self.archived_on_disk.insert(id);
                Session::restore_archived(
                    id,
                    archive_kind_from_u8(meta.kind),
                    meta.cols,
                    meta.rows,
                    meta.scrollback_lines as usize,
                    meta.exit_code,
                    &dump,
                )
            }
            _ => {
                // 폴백: 아카이브 부재(레거시/GC/손상) — redacted.ansi.log tail을
                // 열람 전용 세션에 재생 (VS Code revive/reconnection 2계층 차용)
                let (cols, rows) = Self::restored_terminal_size(&self.logs_root, persistent_id);
                let mut session = Session::restore_archived(
                    id,
                    session::SessionKind::Agent,
                    cols,
                    rows,
                    Self::RESTORE_SCROLLBACK_LINES,
                    None,
                    &[],
                );
                Self::replay_saved_ansi(&self.logs_root, persistent_id, &mut session);
                session
            }
        };
        self.sessions.insert(id, restored);
        self.exited_order.push_back(id);
        let mut pane = MuxPane::new(pane_state.id.clone(), pane_state.title.clone());
        pane.session_id = Some(id);
        self.mux.panes.insert(pane_state.id.clone(), pane);
        tracing::info!(persistent_id, session = id.0, "agent pane 열람 전용 복원");
        true
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
                message: MessagePayload::new("runtime.split.target_missing"),
            });
            return;
        };
        let id = SessionId(self.next_id);
        self.next_id += 1;
        match Self::spawn_session(
            id,
            session::SessionKind::Shell,
            &self.shell_with_session(id),
            80,
            24,
            scrollback_lines,
        ) {
            Ok(new_session) => {
                self.sessions.insert(id, new_session);
                // 분할로 만든 셸도 status detector 설치 (감지 누락 방지, #92/#93).
                self.detectors.insert(
                    id,
                    StatusDetector::new(StatusPatterns::compile(None, None, None, None)),
                );
                if let Some(pipe) = &mut self.persist {
                    let args: Vec<String> = self.shell.args.clone();
                    pipe.session_spawned(
                        id,
                        "shell",
                        None,
                        SHELL_TITLE_ID,
                        &self.shell.program,
                        &args,
                        &Self::spawn_cwd_string(&self.shell.cwd),
                    );
                }
                self.open_session_log(id);
                self.tab_counter += 1;
                let pane_id = MuxPaneId::new();
                let mut pane = MuxPane::new(
                    pane_id.clone(),
                    format!("{SHELL_TITLE_ID} {}", self.tab_counter),
                );
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
                    self.status_overrides.remove(&id);
                    self.close_session_log(id, "killed", None);
                    self.emit(RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Shell,
                        message: MessagePayload::new("runtime.split.target_lost"),
                    });
                    return;
                }
                self.mux.focus.focus(pane_id);
                self.emit_mux_snapshot();
                self.emit(RuntimeEvent::ShellSpawned { session: id });
                self.push_watched_viewports();
            }
            Err(e) => self.emit(RuntimeEvent::SpawnFailed {
                kind: SpawnKind::Shell,
                message: MessagePayload::new("runtime.spawn_failed.shell")
                    .arg("error", format!("{e:#}"))
                    .diagnostic(format!("{e:#}")),
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
            self.final_drain(session);
            self.sessions.remove(&session);
            self.exited_order.retain(|s| *s != session);
            self.hidden_scrollback.remove(&session);
            self.status_overrides.remove(&session);
            self.detectors.remove(&session);
            self.close_session_log(session, "killed", None);
            if let Some(pipe) = &mut self.persist {
                pipe.session_exited(session);
            }
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
                self.final_drain(session);
                self.sessions.remove(&session);
                self.exited_order.retain(|s| *s != session);
                self.hidden_scrollback.remove(&session);
                self.detectors.remove(&session);
                self.status_overrides.remove(&session);
                self.close_session_log(session, "killed", None);
                if let Some(pipe) = &mut self.persist {
                    pipe.session_exited(session);
                }
            }
        }
        self.mux.window.close_tab(&tab_id);
        self.mux.fix_focus();
        self.emit_mux_and_watched();
    }

    /// 세션을 제거하기 전에 reader 채널에 남은 출력을 마지막으로 로그에 기록한다
    /// (§14.5 log writer 보존 — kill/close 직전 출력 유실 방지, codex 리뷰).
    /// redaction 경로는 pump_sessions의 콜백과 동일하다.
    fn final_drain(&mut self, session: SessionId) {
        let Some(active) = self.sessions.get_mut(&session) else {
            return;
        };
        let mut log = self.logs.get_mut(&session);
        let mut latest_offset = None;
        let mut drained = 0usize;
        for _ in 0..FINAL_DRAIN_MAX_PUMPS {
            let result = active.pump(|chunk| {
                if let Some(log) = log.as_mut() {
                    let redacted = log.redactor.redact_chunk(chunk);
                    match log.append_redacted_output(&redacted) {
                        Ok(offset) => latest_offset = Some(offset),
                        Err(e) => tracing::warn!("세션 로그 최종 기록 실패: {e:#}"),
                    }
                }
            });
            drained = drained.saturating_add(result.output_bytes);
            if !result.produced_output || drained >= FINAL_DRAIN_MAX_BYTES {
                break;
            }
        }
        if let Some(offset) = latest_offset
            && let Some(pipe) = &mut self.persist
        {
            pipe.session_log_offset(session, offset);
        }
    }

    /// mux 스냅샷을 push하고, visible(active tab) 세션들의 화면도 즉시 push한다
    /// (tab/포커스 전환 직후 stale 화면 방지).
    fn emit_mux_and_watched(&mut self) {
        self.emit_mux_snapshot();
        self.push_watched_viewports();
    }

    /// mux 스냅샷만 emit (+ 영속 저장). spawn 경로는 이걸 먼저 부르고
    /// Spawned 이벤트를 보낸 뒤 [`Self::push_watched_viewports`]를 불러야
    /// "slot에 Viewport가 있으면 그 세션의 Spawned가 같은 drain에 포함"이라는
    /// RuntimeEventReceiver::drain의 happens-before 계약이 유지된다 (codex 리뷰).
    fn emit_mux_snapshot(&mut self) {
        // mux 구조가 바뀐 지점 — 가시성 전이에 맞춰 scrollback cap 조정 (§14.3)
        self.reconcile_visibility();
        // 영속 layout도 같은 시점에 저장 (§11.2~11.5)
        if let Some(pipe) = &mut self.persist {
            pipe.save_layout(&self.mux.window, &self.mux.tabs, &self.mux.panes);
        }
        self.emit(RuntimeEvent::MuxUpdated {
            snapshot: Arc::new(self.mux.snapshot()),
        });
    }

    fn push_watched_viewports(&mut self) {
        if !self.render_active {
            return; // Warm 등 — snapshot 생성 금지 (§14.1). 세션 pump는 계속된다.
        }
        // 아카이브된 세션의 pane이 보이면 먼저 복원한다 — 복원 직후 dirty라
        // 아래 루프가 같은 tick에 Viewport를 push한다 ("연결 중…" 공백 없음).
        for session in self.mux.watched_sessions() {
            if !self.sessions.contains_key(&session)
                && (self.archived.contains_key(&session)
                    || self.archived_on_disk.contains(&session))
            {
                self.inflate_archived(session);
            }
        }
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
    fn pump_sessions(&mut self, allow_viewport: bool) -> PumpActivity {
        // 이전 tick까지 쌓인 exited 세션 중 cap 초과분을 먼저 archive한다.
        // 이번 tick에 새로 종료되는 세션은 exited_order에 이번 tick 끝에 추가되므로
        // 다음 tick에야 archive 대상이 된다 — SessionExited emit과 detach MuxUpdated가
        // 서로 다른 tick(≈다른 UI drain)에 나뉘어, 알림/상태가 유실되지 않는다 (codex 리뷰).
        self.archive_over_cap();
        let watched = self.mux.watched_sessions();
        let mut events = Vec::new();
        let mut log_offsets = Vec::new();
        let mut status_updates = Vec::new();
        let mut activity = PumpActivity::default();
        for active in self.sessions.values_mut() {
            let active_id = active.id();
            let mut log = self.logs.get_mut(&active_id);
            let mut detector = self.detectors.get_mut(&active_id);
            let result = active.pump(|chunk| {
                if let Some(log) = log.as_mut() {
                    // redaction 후에만 디스크에 닿는다 (7장 — raw 평문 저장 금지)
                    let redacted = log.redactor.redact_chunk(chunk);
                    match log.append_redacted_output(&redacted) {
                        Ok(offset) => log_offsets.push((active_id, offset)),
                        Err(e) => tracing::warn!("세션 로그 기록 실패: {e:#}"),
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
                let screen = detector
                    .should_scan_screen(result.produced_output)
                    .then(|| active.screen_text());
                if let Some(status) = detector.evaluate(screen.as_deref()) {
                    status_updates.push((active.id(), status));
                    let view =
                        detector.status_view(self.status_overrides.get(&active.id()).copied());
                    events.push(RuntimeEvent::SessionStatusChanged {
                        session: active.id(),
                        status,
                    });
                    events.push(RuntimeEvent::SessionStatusViewChanged {
                        session: active.id(),
                        view,
                    });
                }
            }
            if self.render_active && result.dirty && watched.contains(&active.id()) {
                if allow_viewport {
                    if let Some(snapshot) = active.take_snapshot() {
                        events.push(RuntimeEvent::Viewport {
                            session: active.id(),
                            snapshot: Arc::new(snapshot),
                            bracketed_paste: active.bracketed_paste(),
                        });
                        activity.viewport_emitted = true;
                    } else {
                        // backend가 일시적으로 snapshot을 못 만들면 dirty를 유지하고
                        // 다음 paced tick에서 재시도한다.
                        activity.pending_viewport = true;
                    }
                } else {
                    // PTY/parser/log는 즉시 처리하되 snapshot만 다음 display cadence로 합친다.
                    activity.pending_viewport = true;
                }
            }
            if result.just_exited {
                let class = if watched.contains(&active.id()) {
                    TerminalCacheClass::Visible
                } else {
                    TerminalCacheClass::Exited
                };
                if active.cache_class() != class
                    && let Some(event) = active.set_cache_class(class)
                {
                    trace_terminal_cache_event(active.id(), event);
                }
                if class != TerminalCacheClass::Hidden {
                    self.hidden_scrollback.remove(&active.id());
                }
            }
            if result.just_exited
                && let session::SessionLifecycle::Exited { exit_code } = active.lifecycle()
            {
                let status = if exit_code == Some(0) {
                    session::SessionStatus::Done
                } else {
                    session::SessionStatus::Error
                };
                self.status_overrides.remove(&active.id());
                events.push(RuntimeEvent::SessionStatusViewChanged {
                    session: active.id(),
                    view: session::SessionStatusView::process_exit(status),
                });
                events.push(RuntimeEvent::SessionExited {
                    session: active.id(),
                    exit_code,
                });
            }
        }
        if let Some(pipe) = &mut self.persist {
            for (session, offset) in log_offsets {
                pipe.session_log_offset(session, offset);
            }
            for (session, status) in status_updates {
                pipe.session_status(session, status);
            }
        }
        // 종료 세션의 로그 마감 (carry flush + exited 이벤트)
        let mut exited: Vec<(SessionId, Option<u32>)> = events
            .iter()
            .filter_map(|e| match e {
                RuntimeEvent::SessionExited { session, exit_code } => Some((*session, *exit_code)),
                _ => None,
            })
            .collect();
        // 같은 tick에 여러 개 종료되면 HashMap 순회 순서라 비결정적 — SessionId(단조
        // 증가 = spawn 순서)로 정렬해 결정적 LRU를 만든다 (오래된 것이 앞. codex 리뷰)
        exited.sort_by_key(|(s, _)| s.0);
        for (session, exit_code) in exited {
            let detail = exit_code.map(|c| format!("exit code {c}"));
            self.close_session_log(session, "exited", detail.as_deref());
            self.detectors.remove(&session);
            self.status_overrides.remove(&session);
            if let Some(pipe) = &mut self.persist {
                pipe.session_exited(session);
            }
            // scrollback 열람용으로 backend를 유지하되 개수를 유계로 (§14.3)
            self.exited_order.push_back(session);
            // 최종 grid를 디스크 아카이브로 기록 (PR-A1) — exited grid는 불변이라
            // 이 시점 1회 기록으로 suspend/재시작 생존이 보장된다.
            self.write_scrollback_archive(session);
        }
        // 이번 tick의 이벤트(SessionExited/Viewport 등)를 먼저 emit한다.
        // archival의 detach MuxUpdated가 이보다 먼저 가면, 같은 tick에 cap 초과로
        // 다수 종료 시 그 세션들이 mux에서 사라진 뒤 SessionExited가 도착해
        // UI가 exit 상태/알림을 무시한다 (codex 리뷰) — 그래서 exit을 먼저 내보낸다.
        let exited_sessions: Vec<SessionId> = events
            .iter()
            .filter_map(|e| match e {
                RuntimeEvent::SessionExited { session, .. } => Some(*session),
                _ => None,
            })
            .collect();
        for event in events {
            self.emit(event);
        }
        // 셸 세션 종료 → pane 자동 닫힘 (tmux 관례 — exit하면 pane이 접히고 이웃이
        // 공간을 차지, 2026-07-05 사용자 요청). agent pane은 결과 상태(✅/❌)와
        // scrollback을 봐야 하므로 유지한다. SessionExited emit **후**라 UI는 같은
        // drain에서 exit 알림을 먼저 받고 MuxUpdated로 pane 제거를 본다 (채널 FIFO).
        // close_pane의 세션 정리는 위 exited 처리와 겹쳐도 멱등(no-op)이다.
        for session in exited_sessions {
            let is_shell = self
                .sessions
                .get(&session)
                .is_some_and(|s| s.kind() == session::SessionKind::Shell);
            if !is_shell {
                continue;
            }
            let pane = self
                .mux
                .panes
                .values()
                .find(|p| p.session_id == Some(session))
                .map(|p| p.id.clone());
            if let Some(pane) = pane {
                self.close_pane(pane);
            }
        }
        activity
    }

    fn session_status_view(&self, session: SessionId) -> session::SessionStatusView {
        let user_override = self.status_overrides.get(&session).copied();
        if let Some(detector) = self.detectors.get(&session) {
            return detector.status_view(user_override);
        }
        session::SessionStatusView::detected(
            session::SessionStatus::Running,
            session::StatusSource::IdleHeuristic,
            Some(session::StatusConfidence {
                score: 0.0,
                reason: "no_detector".into(),
            }),
            user_override,
        )
    }

    /// 가시성/lifecycle 전이에 맞춰 terminal cache budget을 적용한다 (§14.3):
    /// visible(active tab)은 visible budget, hidden running은 hidden budget,
    /// non-visible exited는 exited-retained budget. visible이 항상 우선한다.
    fn reconcile_visibility(&mut self) {
        let visible: std::collections::HashSet<SessionId> =
            self.mux.watched_sessions().into_iter().collect();
        let sessions: Vec<SessionId> = self.sessions.keys().copied().collect();
        for id in sessions {
            let Some(session) = self.sessions.get_mut(&id) else {
                continue;
            };
            let class = if visible.contains(&id) {
                TerminalCacheClass::Visible
            } else if session.lifecycle().is_running() {
                TerminalCacheClass::Hidden
            } else {
                TerminalCacheClass::Exited
            };
            if class == TerminalCacheClass::Hidden {
                self.hidden_scrollback.insert(id);
            } else {
                self.hidden_scrollback.remove(&id);
            }
            if session.cache_class() != class
                && let Some(event) = session.set_cache_class(class)
            {
                trace_terminal_cache_event(id, event);
            }
        }
    }

    /// exited backend 개수 cap 초과분을 archive한다 (§14.3). pump 시작 시 호출 —
    /// 이번 tick의 신규 exit보다 최소 한 tick 뒤에 archive되도록.
    /// backend는 압축 아카이브(zlib ANSI)로 내려 pane을 유지하고, 다시 보이면
    /// 복원한다. 직렬화 미지원 백엔드만 기존대로 drop + pane detach
    /// ("연결 중…" 갇힘 방지 — codex 리뷰의 detach 사유는 복원 훅이 대신한다).
    fn archive_over_cap(&mut self) {
        // 현재 보이는(active tab의) pane 세션은 archive하지 않는다 — split이면
        // 비포커스 pane도 화면에 있어 사용자가 그 scrollback을 보는 중일 수 있다
        // (codex 리뷰: focused 하나만 제외하면 부족). watched = visible.
        let visible = self.mux.watched_sessions();
        let mut to_archive =
            exited_to_archive(&self.exited_order, self.max_exited_backends, &visible);
        let cache_bytes = self.terminal_cache_bytes();
        if cache_bytes > self.cache_budget_bytes {
            let bytes_by_session: std::collections::HashMap<SessionId, usize> = self
                .sessions
                .iter()
                .map(|(id, session)| (*id, session.cache_footprint().estimated_bytes))
                .collect();
            for session in exited_to_archive_for_budget(
                &self.exited_order,
                &visible,
                &bytes_by_session,
                cache_bytes,
                self.cache_budget_bytes,
            ) {
                if !to_archive.contains(&session) {
                    to_archive.push(session);
                }
            }
        }
        let mut detached_any = false;
        for session in to_archive {
            let Some(live) = self.sessions.get(&session) else {
                continue;
            };
            let estimated_bytes = live.cache_footprint().estimated_bytes;
            // 압축 아카이브 시도 — 성공하면 pane을 유지하고 다시 보일 때 복원한다
            let entry = self.make_archive_entry(live);
            let restorable = entry.is_some();
            if let Some(entry) = entry {
                self.archived_order.push_back(session);
                self.archived.insert(session, entry);
                self.trim_archived_budget();
            }
            self.sessions.remove(&session);
            self.exited_order.retain(|s| *s != session);
            self.hidden_scrollback.remove(&session);
            if !restorable {
                // 복원 불가(직렬화 미지원) — 기존 동작: pane detach로
                // "연결 중…" 갇힘을 방지한다 (codex 리뷰)
                for pane in self.mux.panes.values_mut() {
                    if pane.session_id == Some(session) {
                        pane.session_id = None;
                        detached_any = true;
                    }
                }
            }
            tracing::info!(
                session = session.0,
                estimated_bytes,
                restorable,
                cache_class = ?TerminalCacheClass::Exited,
                "terminal cache archived exited backend"
            );
        }
        if detached_any {
            self.emit_mux_snapshot();
        }
    }

    /// exited 세션의 scrollback을 zlib 압축 아카이브 항목으로 만든다.
    /// 직렬화 미지원 백엔드(예: experimental ghostty)는 None.
    fn make_archive_entry(&self, live: &Session) -> Option<ArchivedScrollback> {
        let dump = live.serialize_scrollback()?;
        let footprint = live.cache_footprint();
        let exit_code = match live.lifecycle() {
            session::SessionLifecycle::Exited { exit_code } => exit_code,
            session::SessionLifecycle::Running => None,
        };
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &dump).ok()?;
        let compressed = encoder.finish().ok()?;
        Some(ArchivedScrollback {
            kind: live.kind(),
            cols: footprint.columns as u16,
            rows: footprint.screen_lines as u16,
            scrollback_lines: footprint.scrollback_limit_lines,
            exit_code,
            compressed,
        })
    }

    /// 아카이브 총 바이트가 예산을 넘으면 오래된 것부터 제거한다 (LRU).
    fn trim_archived_budget(&mut self) {
        let mut total: usize = self.archived.values().map(|a| a.compressed.len()).sum();
        while total > ARCHIVED_SCROLLBACK_BUDGET_BYTES {
            let Some(oldest) = self.archived_order.pop_front() else {
                break;
            };
            if let Some(dropped) = self.archived.remove(&oldest) {
                total -= dropped.compressed.len();
                tracing::info!(session = oldest.0, "archived scrollback 예산 초과 — 제거");
            }
        }
    }

    /// exited 세션의 최종 grid를 디스크 아카이브로 기록한다 (PR-A1).
    /// persist UUID 세션만 대상, 파일이 있으면 skip(exited grid 불변), 빈 grid 생략.
    /// 아카이브는 grid 원문이므로 디스크에 닿기 전 redaction 필수 (§7 — 로그와 달리
    /// 이 덤프는 StreamRedactor를 거치지 않은 상태다).
    fn write_scrollback_archive(&mut self, session: SessionId) {
        let Some(key) = self
            .persist
            .as_ref()
            .and_then(|pipe| pipe.session_log_key(session))
            .map(str::to_owned)
        else {
            return; // 비영속 세션 — 메모리 아카이브만
        };
        if storage::scrollback_archive::exists(&self.logs_root, &key) {
            self.archived_on_disk.insert(session);
            return;
        }
        let Some(live) = self.sessions.get(&session) else {
            return;
        };
        // 빈 grid는 기록 생략 (VS Code v1.69 노이즈 억제 차용)
        let footprint = live.cache_footprint();
        if footprint.history_lines == 0 && live.screen_text().trim().is_empty() {
            return;
        }
        let Some(dump) = live.serialize_scrollback() else {
            return; // 직렬화 미지원 백엔드 (experimental ghostty)
        };
        let mut redactor = self.redaction.stream_redactor();
        let mut redacted = redactor.redact_chunk(&dump);
        redacted.extend(redactor.flush());
        let meta = storage::scrollback_archive::ArchiveMeta {
            kind: archive_kind_to_u8(live.kind()),
            cols: footprint.columns.min(u16::MAX as usize) as u16,
            rows: footprint.screen_lines.min(u16::MAX as usize) as u16,
            scrollback_lines: footprint.scrollback_limit_lines.min(u32::MAX as usize) as u32,
            exit_code: match live.lifecycle() {
                session::SessionLifecycle::Exited { exit_code } => exit_code,
                session::SessionLifecycle::Running => None,
            },
        };
        match storage::scrollback_archive::write(&self.logs_root, &key, &meta, &redacted) {
            Ok(()) => {
                self.archived_on_disk.insert(session);
                if let Err(e) = storage::scrollback_archive::gc(
                    &self.logs_root,
                    storage::scrollback_archive::ARCHIVE_DISK_BUDGET_BYTES,
                ) {
                    tracing::warn!("scrollback 아카이브 GC 실패: {e:#}");
                }
            }
            Err(e) => tracing::warn!(session = session.0, "scrollback 아카이브 기록 실패: {e:#}"),
        }
    }

    /// 아카이브된 세션의 pane이 다시 보이면 백엔드를 복원한다 (열람 시 inflate).
    /// 메모리 아카이브 우선, 예산 축출로 내려갔으면 디스크 아카이브 폴백 (PR-A1).
    /// 복원된 세션은 다시 exited LRU의 최신 자리로 들어간다.
    fn inflate_archived(&mut self, session: SessionId) {
        if let Some(entry) = self.archived.remove(&session) {
            self.archived_order.retain(|s| *s != session);
            let mut dump = Vec::new();
            let mut decoder = flate2::read::ZlibDecoder::new(entry.compressed.as_slice());
            if std::io::Read::read_to_end(&mut decoder, &mut dump).is_ok() {
                self.insert_restored_session(
                    session,
                    entry.kind,
                    entry.cols,
                    entry.rows,
                    entry.scrollback_lines,
                    entry.exit_code,
                    &dump,
                );
                tracing::info!(session = session.0, "archived scrollback 복원 (메모리)");
                return;
            }
            tracing::warn!(
                session = session.0,
                "메모리 아카이브 해제 실패 — 디스크 폴백"
            );
        }
        // 디스크 폴백: exit 시 기록해 둔 scrollback.zlib (persist UUID 세션만)
        let Some(key) = self
            .persist
            .as_ref()
            .and_then(|pipe| pipe.session_log_key(session))
            .map(str::to_owned)
        else {
            return;
        };
        match storage::scrollback_archive::read(&self.logs_root, &key) {
            Ok(Some((meta, dump))) => {
                self.insert_restored_session(
                    session,
                    archive_kind_from_u8(meta.kind),
                    meta.cols,
                    meta.rows,
                    meta.scrollback_lines as usize,
                    meta.exit_code,
                    &dump,
                );
                tracing::info!(session = session.0, "archived scrollback 복원 (디스크)");
            }
            Ok(None) => {
                self.archived_on_disk.remove(&session);
            }
            Err(e) => tracing::warn!(session = session.0, "디스크 아카이브 읽기 실패: {e:#}"),
        }
    }

    /// 아카이브 덤프로 열람 전용 세션을 만들어 편입한다 (inflate 공통 경로).
    #[expect(clippy::too_many_arguments, reason = "아카이브 메타 필드 그대로")]
    fn insert_restored_session(
        &mut self,
        session: SessionId,
        kind: session::SessionKind,
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
        exit_code: Option<u32>,
        dump: &[u8],
    ) {
        let restored =
            Session::restore_archived(session, kind, cols, rows, scrollback_lines, exit_code, dump);
        self.sessions.insert(session, restored);
        self.exited_order.push_back(session);
    }

    fn terminal_cache_bytes(&self) -> usize {
        self.sessions
            .values()
            .map(|session| session.cache_footprint().estimated_bytes)
            .sum()
    }
}

fn trace_terminal_cache_event(session: SessionId, event: TerminalCacheEvent) {
    tracing::info!(
        session = session.0,
        cache_class = ?event.class,
        kind = ?event.kind,
        budget_bytes = event.budget.max_bytes,
        budget_lines = event.budget.max_scrollback_lines,
        before_bytes = event.before.estimated_bytes,
        after_bytes = event.after.estimated_bytes,
        before_history_lines = event.before.history_lines,
        after_history_lines = event.after.history_lines,
        dropped_history_lines = event.dropped_history_lines(),
        freed_estimated_bytes = event.freed_estimated_bytes(),
        "terminal cache budget applied"
    );
}

/// "workspace.spawn.shell 3" / legacy "셸 3" 같은 제목에서 뒤의 숫자를 뽑는다
/// (복원 시 counter 전진용).
fn title_suffix(title: &str) -> Option<u64> {
    title.rsplit(' ').next()?.parse().ok()
}

/// zsh ZLE의 기본 PROMPT_EOL_MARK redraw는 `%`를 역상으로 그린 뒤 정확히
/// `COLUMNS - 1`개의 공백을 출력하고 CR로 되돌아온다. 이 raw ANSI 패턴의 가장
/// 마지막 항목으로 `terminal.size` 도입 전 로그의 최종 열 수를 복구한다.
fn infer_zsh_terminal_cols(path: &std::path::Path, max_bytes: u64) -> std::io::Result<Option<u16>> {
    use std::io::{Read as _, Seek as _};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(std::io::SeekFrom::Start(start))?;
    let mut tail = Vec::with_capacity(usize::try_from(len - start).unwrap_or(0));
    file.read_to_end(&mut tail)?;
    Ok(infer_zsh_terminal_cols_from_bytes(&tail))
}

fn infer_zsh_terminal_cols_from_bytes(bytes: &[u8]) -> Option<u16> {
    const PREFIX: &[u8] = b"\x1b[1m\x1b[7m%\x1b[27m\x1b[1m\x1b[0m";
    const SUFFIX: &[u8] = b"\r \r\r";

    let mut latest = None;
    let mut index = 0usize;
    while index + PREFIX.len() <= bytes.len() {
        if &bytes[index..index + PREFIX.len()] != PREFIX {
            index += 1;
            continue;
        }
        let mut cursor = index + PREFIX.len();
        let spaces_start = cursor;
        while cursor < bytes.len() && bytes[cursor] == b' ' && cursor - spaces_start <= 500 {
            cursor += 1;
        }
        let spaces = cursor - spaces_start;
        if spaces > 0
            && cursor + SUFFIX.len() <= bytes.len()
            && &bytes[cursor..cursor + SUFFIX.len()] == SUFFIX
            && let Ok(cols) = u16::try_from(spaces + 1)
            && (10..=500).contains(&cols)
        {
            latest = Some(cols);
        }
        index += PREFIX.len();
    }
    latest
}

/// ANSI tail cutoff가 escape/UTF-8 sequence 한가운데 놓이지 않게 다음 newline 뒤로
/// 정렬한다. newline 없는 병적 giant line은 복원하지 않는다. 어느 경우든 cutoff 이후
/// 최대 `max_bytes`만 읽으므로 시작 지연이 로그 전체 크기에 비례하지 않는다.
fn seek_ansi_replay_tail(file: &mut std::fs::File, max_bytes: u64) -> std::io::Result<u64> {
    use std::io::{Read as _, Seek as _};

    let len = file.metadata()?.len();
    if len <= max_bytes {
        file.seek(std::io::SeekFrom::Start(0))?;
        return Ok(0);
    }
    let cutoff = len.saturating_sub(max_bytes);
    file.seek(std::io::SeekFrom::Start(cutoff))?;
    let mut position = cutoff;
    let mut buffer = [0u8; 8192];
    while position < len {
        let remaining = usize::try_from((len - position).min(buffer.len() as u64)).unwrap_or(0);
        let read = file.read(&mut buffer[..remaining])?;
        if read == 0 {
            break;
        }
        if let Some(index) = buffer[..read].iter().position(|byte| *byte == b'\n') {
            let start = position + index as u64 + 1;
            file.seek(std::io::SeekFrom::Start(start))?;
            return Ok(start);
        }
        position += read as u64;
    }
    file.seek(std::io::SeekFrom::Start(len))?;
    Ok(len)
}

/// exit 순서(오래된 것이 앞)에서 cap 초과분을 archive(backend drop) 대상으로 돌려준다
/// (§14.3 exited backend 개수 유계). visible(active tab) 세션은 건너뛰되, 초과분을
/// 채우기 위해 그다음 오래된 비-visible 세션을 계속 고른다 — visible을 나중에 필터링하면
/// 부족분을 못 채워 cap을 넘긴다 (codex 리뷰). 순수 함수 — 단위 테스트 대상.
fn exited_to_archive(
    exited_order: &std::collections::VecDeque<SessionId>,
    cap: usize,
    visible: &[SessionId],
) -> Vec<SessionId> {
    let excess = exited_order.len().saturating_sub(cap);
    if excess == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for session in exited_order {
        if out.len() >= excess {
            break;
        }
        if !visible.contains(session) {
            out.push(*session);
        }
    }
    out
}

fn exited_to_archive_for_budget(
    exited_order: &std::collections::VecDeque<SessionId>,
    visible: &[SessionId],
    bytes_by_session: &std::collections::HashMap<SessionId, usize>,
    current_bytes: usize,
    budget_bytes: usize,
) -> Vec<SessionId> {
    let mut remaining = current_bytes.saturating_sub(budget_bytes);
    if remaining == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for session in exited_order {
        if visible.contains(session) {
            continue;
        }
        out.push(*session);
        remaining = remaining.saturating_sub(*bytes_by_session.get(session).unwrap_or(&0));
        if remaining == 0 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{SplitDirection, WorkspaceRuntimeState};
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
            cwd: None,
        }
    }

    #[test]
    fn durable_event_queue_포화는_구독자를_degraded로_표시한다() {
        let (tx, _rx) = sync_channel(1);
        let overflowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let subscriber = Subscriber {
            events: tx,
            overflowed: Arc::clone(&overflowed),
            viewports: Arc::default(),
            input_pressures: Arc::default(),
            resource_usage: Arc::default(),
            wake: None,
        };
        let mut wakes = Vec::new();
        assert!(enqueue_durable_event(
            &subscriber,
            RuntimeEvent::ShellSpawned {
                session: SessionId(1)
            },
            &mut wakes,
        ));
        assert!(!enqueue_durable_event(
            &subscriber,
            RuntimeEvent::ShellSpawned {
                session: SessionId(2)
            },
            &mut wakes,
        ));
        assert!(overflowed.load(std::sync::atomic::Ordering::Acquire));
    }

    /// 안정성 감사 High #1: 주기 ResourceUsage가 느린(드레인 안 하는) 구독자 채널에
    /// 누적되지 않고 latest-value slot 하나로 코얼레싱된다.
    #[test]
    fn resource_usage는_드레인_없이도_slot_하나로_코얼레싱된다() {
        init_mock_store();
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("resource-slot"),
            RedactionService::new(),
            spec("/bin/sh", &["-c", "sleep 30"]),
            None,
        );
        let rx = client.subscribe();
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        // 샘플 주기(2s) 두 번 이상 경과 — 드레인하지 않고 방치.
        std::thread::sleep(Duration::from_millis(5200));
        let events = rx.drain();
        let resource_count = events
            .iter()
            .filter(|e| matches!(e, RuntimeEvent::ResourceUsage { .. }))
            .count();
        assert!(
            resource_count <= 1,
            "ResourceUsage가 채널에 누적됨 (count={resource_count}) — slot 코얼레싱 회귀"
        );
        assert_eq!(
            resource_count, 1,
            "5초간 샘플이 최소 1회는 slot에 있어야 함 (모니터 미동작?)"
        );
    }

    #[test]
    fn in_process_command_queue_full은_err로_surface된다() {
        let (tx, _rx) = sync_channel(1);
        let client = InProcessRuntimeClient {
            command_tx: Some(tx),
            subscribers: Arc::default(),
            worker: None,
            worker_thread: None,
        };
        client
            .send_command(RuntimeCommand::SetWorkspaceState(
                WorkspaceRuntimeState::Active,
            ))
            .unwrap();
        let err = client
            .send_command(RuntimeCommand::SetWorkspaceState(
                WorkspaceRuntimeState::Warm,
            ))
            .unwrap_err();
        assert!(err.to_string().contains("명령 큐 가득참"));
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
    fn pty_output_wake는_긴_fallback_batch보다_먼저_viewport를_보낸다() {
        // wake 배선이 없으면 marker는 2초 batch timeout 뒤에야 보인다. reader의
        // unpark가 연결되어 있으면 child의 150ms sleep 직후 도착해야 한다.
        let client = InProcessRuntimeClient::with_shell(
            2_000,
            test_store(),
            test_logs_root("output-wake-latency"),
            RedactionService::new(),
            spec(
                "/bin/sh",
                &["-c", "sleep 0.15; printf 'wake-latency\\n'; sleep 1"],
            ),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(5), |event| match event {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        let started = Instant::now();
        probe.wait_for(Duration::from_secs(5), |event| match event {
            RuntimeEvent::Viewport { snapshot, .. }
                if snapshot_text(snapshot, 0).contains("wake-latency") =>
            {
                Some(())
            }
            _ => None,
        });
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "PTY output wake가 동작하지 않아 2초 fallback batch를 기다림"
        );
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
            None,
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
            None,
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
        // agent 세션으로 검증 — 셸은 exit 시 pane이 자동으로 닫힌다(2026-07-05).
        // §14.3 "종료 후 scrollback 열람" 계약은 pane이 유지되는 agent에 적용된다.
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("t"),
            RedactionService::new(),
            spec("/bin/echo", &["unused"]),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(spawn_agent_cmd("echo done", None, None))
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::AgentSpawned { session } => Some(*session),
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
    fn suspended_이후_spawn은_거부된다() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("t"),
            RedactionService::new(),
            spec("/bin/echo", &["hi"]),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SetWorkspaceState(
                crate::command::WorkspaceRuntimeState::Suspended,
            ))
            .unwrap();
        // suspend 후 큐에 들어온 spawn — 세션 생성 없이 SpawnFailed로 거부돼야 한다
        // (suspend 직전 UI가 못 본 spawn이 shutdown 경로에서 PTY를 만들었다 즉시
        // 죽이는 race 차단).
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let message = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SpawnFailed { kind, message } if *kind == SpawnKind::Shell => {
                Some(message.message_id.clone())
            }
            _ => None,
        });
        assert_eq!(message, "runtime.spawn_failed.suspended");
        assert!(
            !probe
                .seen
                .iter()
                .any(|e| matches!(e, RuntimeEvent::ShellSpawned { .. })),
            "suspended 상태에서 세션이 생성되면 안 된다"
        );
    }

    #[test]
    #[cfg(unix)]
    fn suspended_이후_split은_거부된다() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("suspended-split"),
            RedactionService::new(),
            spec("/bin/sleep", &["5"]),
            None,
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
            RuntimeEvent::ShellSpawned { .. } => Some(()),
            _ => None,
        });
        let pane = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot } => snapshot
                .tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .find(|pane| pane.session_id.is_some())
                .map(|pane| pane.id.clone()),
            _ => None,
        });
        probe.seen.clear();
        client
            .send_command(RuntimeCommand::SetWorkspaceState(
                crate::command::WorkspaceRuntimeState::Suspended,
            ))
            .unwrap();
        client
            .send_command(RuntimeCommand::SplitPane {
                pane,
                direction: mux::SplitDirection::Horizontal,
                scrollback_lines: 100,
            })
            .unwrap();
        let message = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SpawnFailed { kind, message } if *kind == SpawnKind::Shell => {
                Some(message.message_id.clone())
            }
            _ => None,
        });
        assert_eq!(message, "runtime.spawn_failed.suspended");
        assert!(
            !probe
                .seen
                .iter()
                .any(|e| matches!(e, RuntimeEvent::ShellSpawned { .. })),
            "suspended split must not spawn a new shell"
        );
    }

    #[test]
    #[cfg(unix)]
    fn 셸_exit시_pane_자동_닫힘_agent는_유지() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("t"),
            RedactionService::new(),
            spec("/bin/echo", &["bye"]),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        // agent(pane 유지 기대) + 셸(즉시 종료 — pane 자동 닫힘 기대)
        client
            .send_command(spawn_agent_cmd("sleep 5", None, None))
            .unwrap();
        let agent = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::AgentSpawned { session } => Some(*session),
            _ => None,
        });
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let shell = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SessionExited { session, .. } if *session == shell => Some(()),
            _ => None,
        });
        // exit 직후의 MuxUpdated에서 셸 pane은 사라지고 agent pane은 남는다
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot } => {
                let sessions: Vec<_> = snapshot
                    .tabs
                    .iter()
                    .flat_map(|t| &t.panes)
                    .filter_map(|p| p.session_id)
                    .collect();
                (!sessions.contains(&shell) && sessions.contains(&agent)).then_some(())
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        // sh가 env를 출력 — plain + secret(spawn 직전 resolve) 주입 검증
        client
            .send_command(RuntimeCommand::SpawnAgent {
                agent_config_id: None,
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
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnAgent {
                agent_config_id: None,
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
        // resolve 실패 → SpawnFailed, payload에 secret 값 없음 (credential id만)
        let message = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SpawnFailed { message, .. } => Some(message.clone()),
            _ => None,
        });
        assert_eq!(message.message_id, "runtime.spawn_failed.agent_secret");
        assert_eq!(message.arg_value("credential_id"), Some("cred-없음"));
        assert!(
            !format!("{message:?}").contains("누출되면 안 됨"),
            "failed spawn payload must not include command args"
        );
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
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        // secret env를 stdout으로 두 번 출력 (chunk 분할 가능성 포함)
        client
            .send_command(RuntimeCommand::SpawnAgent {
                agent_config_id: None,
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
            None,
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
            agent_config_id: None,
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
            None,
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
            None,
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
    fn resource_usage는_session_child_usage를_포함한다() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("resource-child"),
            RedactionService::new(),
            spec("/bin/sleep", &["5"]),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        let usage = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::ResourceUsage { session_usage, .. } => session_usage
                .iter()
                .find(|usage| usage.session == session && usage.pid.is_some())
                .cloned(),
            _ => None,
        });
        assert!(usage.process_count >= 1);
        assert!(usage.rss_bytes > 0);
    }

    #[test]
    #[cfg(unix)]
    fn oversized_input은_pressure_event로_surface된다() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("input-pressure"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        client
            .send_command(RuntimeCommand::WriteInput {
                session,
                bytes: vec![b'x'; pty::PtyInputQueuePolicy::default().max_bytes + 1],
            })
            .unwrap();
        let pressure = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::PtyInputPressure {
                session: pressure_session,
                pressure,
            } if *pressure_session == session => Some(pressure.clone()),
            _ => None,
        });
        assert_eq!(pressure.reason, pty::PtyInputRejectReason::PayloadTooLarge);
    }

    #[test]
    #[cfg(unix)]
    fn input_pressure_events_are_coalesced_per_session() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("input-pressure-coalesce"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            None,
        );
        let rx = client.subscribe();
        let mut probe = Probe::new(rx);
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        probe.seen.clear();
        let oversized = vec![b'x'; pty::PtyInputQueuePolicy::default().max_bytes + 1];
        for _ in 0..8 {
            client
                .send_command(RuntimeCommand::WriteInput {
                    session,
                    bytes: oversized.clone(),
                })
                .unwrap();
        }
        std::thread::sleep(Duration::from_millis(100));
        let events = probe.rx.drain();
        let pressure_count = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    RuntimeEvent::PtyInputPressure {
                        session: pressure_session,
                        ..
                    } if *pressure_session == session
                )
            })
            .count();
        assert_eq!(pressure_count, 1);
    }

    #[test]
    #[cfg(unix)]
    fn user_status_override는_status_view_event로_surface된다() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("status-override"),
            RedactionService::new(),
            pty::default_shell(),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(spawn_agent_cmd("cat", None, None))
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::AgentSpawned { session } => Some(*session),
            _ => None,
        });
        probe.seen.clear();
        client
            .send_command(RuntimeCommand::SetUserStatusOverride {
                session,
                override_: session::UserStatusOverride::Mark(session::SessionStatus::Done),
            })
            .unwrap();
        let view = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::SessionStatusViewChanged {
                session: view_session,
                view,
            } if *view_session == session => Some(view.clone()),
            _ => None,
        });
        assert_eq!(view.status, session::SessionStatus::Done);
        assert_eq!(view.detected_status, session::SessionStatus::Running);
        assert_eq!(view.source, session::StatusSource::UserOverride);
        assert_eq!(view.user_override, Some(session::SessionStatus::Done));
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
            None,
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
            None,
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

    /// 세션 영속 파이프라인 (runtime↔persist 배선): spawn→running 행,
    /// exit→exited 행, mux layout 저장. 재시작 시 reconcile(PR-14)과 맞물린다.
    #[cfg(unix)]
    #[test]
    fn 세션과_layout이_영속된다() {
        init_mock_store();
        let dir = std::env::temp_dir().join(format!("deppy-rtpersist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("metadata.sqlite3");
        // 스키마 준비 (앱 마이그레이션 대행: workspaces + persist DDL)
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE workspaces (id TEXT PRIMARY KEY, name TEXT, path TEXT,
                     created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
                 CREATE TABLE agent_configs (id TEXT PRIMARY KEY);
                 INSERT INTO workspaces (id) VALUES ('ws-rt');",
            )
            .unwrap();
            conn.execute_batch(persist::MIGRATION_SQL).unwrap();
        }

        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("persist"),
            RedactionService::new(),
            spec("/bin/echo", &["persist-ok"]),
            Some(crate::persistence::PersistConfig {
                db_path: db_path.clone(),
                workspace_id: "ws-rt".into(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());
        // agent 세션으로 검증 — 셸은 exit 시 pane이 자동으로 닫혀 layout에서 사라진다
        // (2026-07-05). 영속 파이프라인(세션 행 + layout의 pane→세션 참조)은 pane이
        // 유지되는 agent로 확인한다.
        client
            .send_command(spawn_agent_cmd("echo persist-ok", None, None))
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SessionExited { .. } => Some(()),
            _ => None,
        });
        // exit 기록이 pump에서 일어난 뒤 확인 — 약간의 여유
        std::thread::sleep(Duration::from_millis(100));

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let (kind, command, status): (String, String, String) = conn
            .query_row(
                "SELECT session_kind, command, status FROM sessions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        // config id 없는 SpawnAgent는 스키마 CHECK(agent kind ⇒ agent_id 필수) 때문에
        // "shell" kind로 기록된다 — 런타임 SessionKind는 Agent라 pane은 유지된다.
        assert_eq!(kind, "shell");
        assert_eq!(command, "/bin/sh");
        assert_eq!(status, "exited");
        // mux layout: window/tab/pane가 저장되고 pane이 영속 session id를 참조
        let windows = persist::load_window_layouts(&conn, "ws-rt").unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].tabs.len(), 1);
        let pane_session = windows[0].tabs[0].panes[0].session_id.clone().unwrap();
        let persisted_id: String = conn
            .query_row("SELECT id FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pane_session, persisted_id);
        // 재시작 crash recovery와의 연동: exited라 reconcile 대상 아님 (멱등)
        assert_eq!(persist::reconcile_orphan_sessions(&conn).unwrap(), 0);
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// PR-A1: 세션 exit 시 최종 grid가 디스크 아카이브(scrollback.zlib)로 기록되고,
    /// 메타·내용이 라운드트립된다 (suspend/재시작 생존의 원천).
    #[cfg(unix)]
    #[test]
    fn exit시_scrollback_아카이브가_디스크에_기록된다() {
        init_mock_store();
        let dir = std::env::temp_dir().join(format!("deppy-rtarchive-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("metadata.sqlite3");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE workspaces (id TEXT PRIMARY KEY, name TEXT, path TEXT,
                     created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
                 CREATE TABLE agent_configs (id TEXT PRIMARY KEY);
                 INSERT INTO workspaces (id) VALUES ('ws-arch');",
            )
            .unwrap();
            conn.execute_batch(persist::MIGRATION_SQL).unwrap();
        }
        let logs_root = test_logs_root("archive");
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            logs_root.clone(),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path: db_path.clone(),
                workspace_id: "ws-arch".into(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(spawn_agent_cmd("echo archive-roundtrip-marker", None, None))
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SessionExited { .. } => Some(()),
            _ => None,
        });
        std::thread::sleep(Duration::from_millis(200));

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let uuid: String = conn
            .query_row("SELECT id FROM sessions", [], |r| r.get(0))
            .unwrap();
        drop(conn);
        let (meta, dump) = storage::scrollback_archive::read(&logs_root, &uuid)
            .unwrap()
            .expect("exit 시점에 아카이브 파일이 기록돼야 함");
        assert_eq!(meta.kind, 1, "SpawnAgent 세션은 agent kind");
        assert_eq!(meta.exit_code, Some(0));
        let text = String::from_utf8_lossy(&dump);
        assert!(text.contains("archive-roundtrip-marker"), "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// PR-A2: agent pane은 재시작 후 respawn 대신 열람 전용 복원된다 —
    /// 아카이브 1차 → (파일 삭제 시) 로그 tail 폴백, 재결속으로 2회 왕복에도
    /// pane↔UUID 연결이 유지된다.
    #[cfg(unix)]
    #[test]
    fn 재시작시_agent_pane은_열람전용으로_복원된다() {
        init_mock_store();
        let dir = std::env::temp_dir().join(format!("deppy-rta2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("metadata.sqlite3");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE workspaces (id TEXT PRIMARY KEY, name TEXT, path TEXT,
                     created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
                 CREATE TABLE agent_configs (id TEXT PRIMARY KEY);
                 INSERT INTO workspaces (id) VALUES ('ws-a2');
                 INSERT INTO agent_configs (id) VALUES ('cfg-1');",
            )
            .unwrap();
            conn.execute_batch(persist::MIGRATION_SQL).unwrap();
        }
        let persist_config = || crate::persistence::PersistConfig {
            db_path: db_path.clone(),
            workspace_id: "ws-a2".into(),
        };
        let logs_root = test_logs_root("a2-archive");
        let make_client = || {
            InProcessRuntimeClient::with_shell(
                5,
                test_store(),
                logs_root.clone(),
                RedactionService::new(),
                spec("/bin/cat", &[]),
                Some(persist_config()),
            )
        };
        let viewport_text = |snapshot: &terminal::TerminalViewportSnapshot| -> String {
            snapshot
                .visible_cells
                .iter()
                .filter(|c| !c.wide_spacer)
                .map(|c| c.c)
                .collect()
        };

        // 1) agent(config id 있음 — DB kind 'agent') 실행 → 종료 → 워커 종료
        {
            let client = make_client();
            let mut probe = Probe::new(client.subscribe());
            let mut cmd = spawn_agent_cmd("echo a2-restore-marker", None, None);
            if let RuntimeCommand::SpawnAgent {
                agent_config_id, ..
            } = &mut cmd
            {
                *agent_config_id = Some("cfg-1".into());
            }
            client.send_command(cmd).unwrap();
            probe.wait_for(Duration::from_secs(15), |e| match e {
                RuntimeEvent::SessionExited { .. } => Some(()),
                _ => None,
            });
            std::thread::sleep(Duration::from_millis(200));
        }

        // 2) 재시작 1: 아카이브로 열람 전용 복원 — respawn 없이 내용이 보인다
        let restore_and_check = |round: &str| {
            let client = make_client();
            let mut probe = Probe::new(client.subscribe());
            client
                .send_command(RuntimeCommand::RestoreWorkspace)
                .unwrap();
            let text = probe.wait_for(Duration::from_secs(15), |e| match e {
                RuntimeEvent::Viewport { snapshot, .. } => {
                    let text = viewport_text(snapshot);
                    text.contains("a2-restore-marker").then_some(text)
                }
                _ => None,
            });
            assert!(text.contains("a2-restore-marker"), "{round}: {text}");
            // 워커 종료 후 DB 확인: respawn이었다면 kind가 'shell'로 덮였을 것
            drop(probe);
            drop(client);
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let (count, kind): (i64, String) = conn
                .query_row(
                    "SELECT COUNT(*), MAX(session_kind) FROM sessions",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(count, 1, "{round}: 세션 행이 늘면 재결속 실패");
            assert_eq!(kind, "agent", "{round}: respawn이면 shell로 덮인다");
        };
        restore_and_check("재시작1");
        // 3) 재시작 2 (2회 왕복 — 재결속이 layout 저장을 통과했는지)
        restore_and_check("재시작2");

        // 4) 아카이브 삭제 → 로그 tail 폴백으로도 열람 전용 복원
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let uuid: String = conn
            .query_row("SELECT id FROM sessions", [], |r| r.get(0))
            .unwrap();
        drop(conn);
        let archive = storage::scrollback_archive::archive_path(&logs_root, &uuid).unwrap();
        std::fs::remove_file(&archive).unwrap();
        restore_and_check("로그폴백");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 복원 UX (PR-14, 설계문서 §11.1~11.5·§14): 첫 worker가 만든 셸 2개 + split
    /// 1개(tab 2개/pane 3개) 구조가 종료 후 새 worker 시작 시 fresh 셸로 복원되는지
    /// 확인한다. 임시 파일 DB(WAL) — worker 자체 연결의 다중 프로세스 재시작 시나리오를
    /// in-memory보다 정확히 재현한다.
    #[cfg(unix)]
    #[test]
    fn 재시작시_저장된_layout이_복원된다() {
        init_mock_store();
        let dir = std::env::temp_dir().join(format!("deppy-rtrestore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("metadata.sqlite3");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE workspaces (id TEXT PRIMARY KEY, name TEXT, path TEXT,
                     created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
                 CREATE TABLE agent_configs (id TEXT PRIMARY KEY);
                 INSERT INTO workspaces (id) VALUES ('ws-restore');",
            )
            .unwrap();
            conn.execute_batch(persist::MIGRATION_SQL).unwrap();
        }
        let persist_config = || crate::persistence::PersistConfig {
            db_path: db_path.clone(),
            workspace_id: "ws-restore".into(),
        };

        // 첫 worker: 셸 2개 spawn 후 하나를 분할 → tab 2개(pane 2개 + pane 1개).
        {
            let client = InProcessRuntimeClient::with_shell(
                5,
                test_store(),
                test_logs_root("restore-1"),
                RedactionService::new(),
                spec("/bin/cat", &[]),
                Some(persist_config()),
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
            let tab_a = mux.tabs[0].id.clone();
            let pane_a = mux.tabs[0].panes[0].id.clone();
            // 분할 대상 tab을 먼저 활성화한다 — 복원 후 active_tab이 이 tab을
            // 가리키는지 결정적으로 검증하기 위함.
            client
                .send_command(RuntimeCommand::SelectTab { tab: tab_a.clone() })
                .unwrap();
            client
                .send_command(RuntimeCommand::SplitPane {
                    pane: pane_a,
                    direction: SplitDirection::Horizontal,
                    scrollback_lines: 100,
                })
                .unwrap();
            let tab_a_wait = tab_a.clone();
            probe.wait_for(Duration::from_secs(15), move |e| match e {
                RuntimeEvent::MuxUpdated { snapshot }
                    if snapshot.active_tab == Some(tab_a_wait.clone())
                        && snapshot
                            .tabs
                            .iter()
                            .find(|t| t.id == tab_a_wait)
                            .is_some_and(|t| t.panes.len() == 2) =>
                {
                    Some(())
                }
                _ => None,
            });
            // client가 스코프를 벗어나며 Drop → shutdown()이 worker join까지 동기 대기
            // (그 전에 이미 마지막 emit_mux_snapshot이 DB에 커밋된 뒤였다).
        }

        // 새 worker: 같은 DB로 시작 → subscribe 후 RestoreWorkspace를 보내야 복원된다.
        let client2 = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("restore-2"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(persist_config()),
        );
        let mut probe2 = Probe::new(client2.subscribe());
        client2
            .send_command(RuntimeCommand::RestoreWorkspace)
            .unwrap();
        let mux = probe2.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.len() == 2 => {
                Some(snapshot.clone())
            }
            _ => None,
        });
        // tab 구성: 분할된 tab(pane 2개) + 단일 tab(pane 1개) — 순서 무관하게 확인
        let mut pane_counts: Vec<usize> = mux.tabs.iter().map(|t| t.panes.len()).collect();
        pane_counts.sort_unstable();
        assert_eq!(
            pane_counts,
            vec![1, 2],
            "복원된 tab/pane 구성이 저장 시와 다름"
        );

        // 분할했던 tab이 active로 복원됨
        let active = mux.active_tab.clone().expect("active_tab이 복원돼야 함");
        let active_tab = mux.tabs.iter().find(|t| t.id == active).unwrap();
        assert_eq!(active_tab.panes.len(), 2);

        // 복원된 pane마다 fresh 셸 세션이 attach — active tab의 두 pane 모두
        // session_id를 갖고, 그 세션들의 Viewport가 (watched pane라) 도착해야 한다.
        let restored_sessions: Vec<SessionId> = active_tab
            .panes
            .iter()
            .map(|p| {
                p.session_id
                    .expect("복원된 pane에 fresh 세션이 attach돼야 함")
            })
            .collect();
        assert_eq!(restored_sessions.len(), 2);
        for session in restored_sessions {
            probe2.wait_for(Duration::from_secs(15), move |e| match e {
                RuntimeEvent::Viewport { session: s, .. } if *s == session => Some(()),
                _ => None,
            });
        }

        drop(client2);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 앱 release 재빌드/재실행 시 fresh 셸을 붙이더라도 이전 agent 출력과 ANSI
    /// truecolor가 영속 세션 로그에서 복원돼야 한다.
    #[cfg(unix)]
    #[test]
    fn 재시작시_ansi_scrollback과_color가_복원된다() {
        init_mock_store();
        let dir = std::env::temp_dir().join(format!(
            "deppy-rt-ansi-restore-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("metadata.sqlite3");
        let logs_root = dir.join("logs");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE workspaces (id TEXT PRIMARY KEY, name TEXT, path TEXT,
                     created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
                 CREATE TABLE agent_configs (id TEXT PRIMARY KEY);
                 INSERT INTO workspaces (id) VALUES ('ws-ansi');",
            )
            .unwrap();
            conn.execute_batch(persist::MIGRATION_SQL).unwrap();
        }
        let persist_config = || crate::persistence::PersistConfig {
            db_path: db_path.clone(),
            workspace_id: "ws-ansi".into(),
        };

        {
            let client = InProcessRuntimeClient::with_shell(
                5,
                test_store(),
                logs_root.clone(),
                RedactionService::new(),
                spec(
                    "/bin/sh",
                    &[
                        "-c",
                        r"printf '\033[38;2;12;34;56m\033[48;2;78;90;123mPERSIST-COLOR\033[0m\r\n'; exec /bin/cat",
                    ],
                ),
                Some(persist_config()),
            );
            let mut probe = Probe::new(client.subscribe());
            client
                .send_command(RuntimeCommand::SpawnShell {
                    cols: 80,
                    rows: 24,
                    scrollback_lines: 100,
                })
                .unwrap();
            let runtime_session = probe.wait_for(Duration::from_secs(15), |event| match event {
                RuntimeEvent::Viewport {
                    session, snapshot, ..
                } if snapshot.visible_cells.iter().any(|cell| {
                    cell.c == 'P' && cell.fg == [12, 34, 56] && cell.bg == [78, 90, 123]
                }) =>
                {
                    Some(*session)
                }
                _ => None,
            });
            client
                .send_command(RuntimeCommand::Resize {
                    session: runtime_session,
                    cols: 121,
                    rows: 47,
                })
                .unwrap();
            probe.wait_for(Duration::from_secs(15), |event| match event {
                RuntimeEvent::Viewport { snapshot, .. }
                    if (snapshot.cols, snapshot.rows) == (121, 47) =>
                {
                    Some(())
                }
                _ => None,
            });
        }

        let persistent_id: String = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row("SELECT id FROM sessions", [], |row| row.get(0))
            .unwrap();
        let ansi_path = SessionLogWriter::ansi_path(&logs_root, &persistent_id).unwrap();
        assert_eq!(
            SessionLogWriter::load_terminal_size(&logs_root, &persistent_id).unwrap(),
            Some((121, 47))
        );
        assert!(
            std::fs::read(&ansi_path)
                .unwrap()
                .windows(b"\x1b[38;2;12;34;56m".len())
                .any(|bytes| bytes == b"\x1b[38;2;12;34;56m"),
            "영속 ANSI 로그에 truecolor escape가 보존돼야 함"
        );

        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            logs_root.clone(),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(persist_config()),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::RestoreWorkspace)
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::Viewport { snapshot, .. }
                if (snapshot.cols, snapshot.rows) == (121, 47)
                    && snapshot.visible_cells.iter().any(|cell| {
                        cell.c == 'P' && cell.fg == [12, 34, 56] && cell.bg == [78, 90, 123]
                    }) =>
            {
                Some(())
            }
            _ => None,
        });

        // restore가 새 UUID를 매번 만들면 다음 재시작에서 로그 연결이 다시 끊긴다.
        let row_count: i64 = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(row_count, 1);
        drop(client);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 회귀 (codex 리뷰 P2): RestoreWorkspace가 "빈 상태"에서만 복원한다.
    /// SpawnShell이 먼저 처리돼 세션이 생긴 뒤 온 RestoreWorkspace는 skip돼야
    /// 저장 layout이 새 세션 위에 덧붙는 hybrid 상태를 만들지 않는다.
    /// §14.1 wake: 상태 이벤트가 채널에 들어갈 때 subscribe_with_wake의 콜백이
    /// 호출된다 — UI가 숨겨져도 worker가 깨워 알림을 처리하게 하는 핵심.
    #[cfg(unix)]
    #[test]
    fn subscribe_with_wake는_상태이벤트에_깨운다() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        init_mock_store();
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("wake"),
            RedactionService::new(),
            spec("/bin/sh", &["-c", "sleep 30"]),
            None,
        );
        let woke = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&woke);
        let rx = client.subscribe_with_wake(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        let mut probe = Probe::new(rx);
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| {
            matches!(e, RuntimeEvent::ShellSpawned { .. }).then_some(())
        });
        assert!(
            woke.load(Ordering::SeqCst) > 0,
            "상태 이벤트가 wake 콜백을 호출해야 함"
        );
    }

    /// §14.1 Warm: 앱이 안 보일 때 snapshot 생성을 멈추되(Viewport 없음) 세션은 살아
    /// PTY/로그가 계속된다. Active 복귀 시 쌓인 화면이 즉시 다시 push된다.
    #[cfg(unix)]
    #[test]
    fn warm에서_viewport_중단_active복귀시_재개() {
        init_mock_store();
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("warm"),
            RedactionService::new(),
            spec("/bin/sh", &["-c", "printf WARMTEST; sleep 30"]),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        // Warm으로 전환 후 spawn — Viewport가 생성되면 안 된다
        client
            .send_command(RuntimeCommand::SetWorkspaceState(
                WorkspaceRuntimeState::Warm,
            ))
            .unwrap();
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
        // 세션이 WARMTEST를 출력할 시간을 준 뒤에도 Viewport는 없어야 한다
        let until = Instant::now() + Duration::from_millis(800);
        while Instant::now() < until {
            probe.seen.extend(probe.rx.drain());
            std::thread::sleep(Duration::from_millis(20));
        }
        let warm_viewport = probe.seen.iter().any(|e| {
            matches!(e, RuntimeEvent::Viewport { snapshot, .. }
                if snapshot_text(snapshot, 0).contains("WARMTEST"))
        });
        assert!(!warm_viewport, "Warm 상태에서 Viewport가 생성됨");

        // Active 복귀 → 쌓인 WARMTEST 화면이 Viewport로 도착
        client
            .send_command(RuntimeCommand::SetWorkspaceState(
                WorkspaceRuntimeState::Active,
            ))
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport { snapshot, .. }
                if snapshot_text(snapshot, 0).contains("WARMTEST") =>
            {
                Some(())
            }
            _ => None,
        });
    }

    /// §14.3 가시성 전이: 세션을 hidden(다른 tab)으로 보냈다가 다시 visible로
    /// 되돌려도 세션이 살아 화면을 정상 렌더한다 (set_visible cap/uncap이 backend를
    /// 깨지 않음). scrollback 크기 자체는 이벤트로 관측 불가 — 렌더 정상으로 검증.
    #[cfg(unix)]
    #[test]
    fn 가시성_전이_후_세션_렌더_유지() {
        init_mock_store();
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("vis"),
            RedactionService::new(),
            spec("/bin/sh", &["-c", "printf VISMARKER; sleep 30"]),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        // A spawn (tab 1, visible)
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 5000,
            })
            .unwrap();
        let mux_a = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.len() == 1 => {
                Some(snapshot.clone())
            }
            _ => None,
        });
        let tab_a = mux_a.tabs[0].id.clone();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport { snapshot, .. }
                if snapshot_text(snapshot, 0).contains("VISMARKER") =>
            {
                Some(())
            }
            _ => None,
        });
        // B spawn (tab 2 활성 → A hidden, reconcile이 A를 cap)
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 5000,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.len() == 2 => Some(()),
            _ => None,
        });
        // A tab으로 복귀 (A visible → reconcile uncap) → A의 Viewport에 VISMARKER 재도착
        client
            .send_command(RuntimeCommand::SelectTab { tab: tab_a.clone() })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport { snapshot, .. }
                if snapshot_text(snapshot, 0).contains("VISMARKER") =>
            {
                Some(())
            }
            _ => None,
        });
    }

    #[test]
    fn exited_archive_cap() {
        use std::collections::VecDeque;
        let ids: VecDeque<SessionId> = (1..=5).map(SessionId).collect();
        let none: &[SessionId] = &[];
        // cap 이하면 archive 없음
        assert!(super::exited_to_archive(&ids, 5, none).is_empty());
        assert!(super::exited_to_archive(&ids, 10, none).is_empty());
        // cap 초과: 가장 오래된 것부터 (앞쪽) 초과분만
        assert_eq!(
            super::exited_to_archive(&ids, 3, none),
            vec![SessionId(1), SessionId(2)]
        );
        assert_eq!(
            super::exited_to_archive(&ids, 0, none),
            (1..=5).map(SessionId).collect::<Vec<_>>()
        );
        assert!(super::exited_to_archive(&VecDeque::new(), 3, none).is_empty());
        // visible은 건너뛰고 초과분을 다음 비-visible로 채운다 (cap 유지):
        // 5개, cap 3, 초과 2. 가장 오래된 1이 visible → 2·3을 archive
        assert_eq!(
            super::exited_to_archive(&ids, 3, &[SessionId(1)]),
            vec![SessionId(2), SessionId(3)]
        );
        // visible이 너무 많아 채울 수 없으면 있는 만큼만 (cap 초과 감수)
        assert_eq!(
            super::exited_to_archive(
                &ids,
                3,
                &[SessionId(2), SessionId(3), SessionId(4), SessionId(5)]
            ),
            vec![SessionId(1)]
        );
    }

    #[test]
    fn exited_archive_global_budget는_visible을_보존한다() {
        use std::collections::{HashMap, VecDeque};
        let ids: VecDeque<SessionId> = (1..=5).map(SessionId).collect();
        let bytes = HashMap::from([
            (SessionId(1), 40),
            (SessionId(2), 30),
            (SessionId(3), 25),
            (SessionId(4), 20),
            (SessionId(5), 10),
        ]);

        assert!(super::exited_to_archive_for_budget(&ids, &[], &bytes, 90, 90).is_empty());
        assert_eq!(
            super::exited_to_archive_for_budget(&ids, &[], &bytes, 125, 80),
            vec![SessionId(1), SessionId(2)]
        );
        assert_eq!(
            super::exited_to_archive_for_budget(&ids, &[SessionId(1)], &bytes, 125, 80),
            vec![SessionId(2), SessionId(3)]
        );
        assert_eq!(
            super::exited_to_archive_for_budget(
                &ids,
                &[SessionId(1), SessionId(2), SessionId(3), SessionId(4)],
                &bytes,
                125,
                80,
            ),
            vec![SessionId(5)]
        );
    }

    #[test]
    fn ansi_replay_tail은_상한과_ansi_경계를_지킨다() {
        use std::io::Read as _;

        let path = std::env::temp_dir().join(format!(
            "deppy-ansi-tail-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let recent = b"\x1b[38;2;12;34;56mRECENT\x1b[0m\n";
        let mut data = b"old-prefix-possibly-mid-escape-\x1b[31mOLD\n".to_vec();
        data.extend_from_slice(recent);
        std::fs::write(&path, &data).unwrap();

        let mut file = std::fs::File::open(&path).unwrap();
        let start = super::seek_ansi_replay_tail(&mut file, recent.len() as u64 + 8).unwrap();
        assert!(start > 0);
        let mut restored = Vec::new();
        file.read_to_end(&mut restored).unwrap();
        assert_eq!(restored, recent);
        assert!(restored.len() as u64 <= recent.len() as u64 + 8);

        // cutoff 뒤에도 newline이 없는 giant line은 중간 escape/text를 그리지 않고 생략.
        std::fs::write(&path, vec![b'x'; 128]).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        assert_eq!(super::seek_ansi_replay_tail(&mut file, 16).unwrap(), 128);
        let mut restored = Vec::new();
        file.read_to_end(&mut restored).unwrap();
        assert!(restored.is_empty());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn 구버전_zsh_ansi에서_마지막_terminal_너비를_복구한다() {
        fn marker(cols: usize) -> Vec<u8> {
            let mut bytes = b"\x1b[1m\x1b[7m%\x1b[27m\x1b[1m\x1b[0m".to_vec();
            bytes.extend(std::iter::repeat_n(b' ', cols - 1));
            bytes.extend_from_slice(b"\r \r\r");
            bytes
        }

        let mut log = b"old output\r\n".to_vec();
        log.extend(marker(121));
        log.extend_from_slice(b"prompt\r\n");
        log.extend(marker(84));
        assert_eq!(super::infer_zsh_terminal_cols_from_bytes(&log), Some(84));

        // tail에 잘린 최신 marker가 있어도 마지막 완전한 항목을 사용한다.
        log.extend_from_slice(b"\x1b[1m\x1b[7m%\x1b[27m\x1b[1m\x1b[0m   ");
        assert_eq!(super::infer_zsh_terminal_cols_from_bytes(&log), Some(84));
        assert_eq!(
            super::infer_zsh_terminal_cols_from_bytes(b"plain shell output"),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn title_suffix_파싱() {
        assert_eq!(super::title_suffix("셸 3"), Some(3));
        assert_eq!(super::title_suffix("에이전트 12"), Some(12));
        assert_eq!(super::title_suffix("workspace.spawn.shell 4"), Some(4));
        assert_eq!(super::title_suffix("workspace.spawn.agent 5"), Some(5));
        assert_eq!(super::title_suffix("셸"), None);
        assert_eq!(super::title_suffix("이름 없음"), None);
        // 복원 counter 전진: "셸 1"·"셸 3"만 남아도(중간 닫힘) max는 3
        let titles = ["셸 1", "셸 3"];
        let max = titles.iter().filter_map(|t| super::title_suffix(t)).max();
        assert_eq!(max, Some(3));
    }

    #[test]
    #[cfg(unix)]
    fn restore는_세션이_있으면_skip한다() {
        init_mock_store();
        let dir = std::env::temp_dir().join(format!("deppy-rtskip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("metadata.sqlite3");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE workspaces (id TEXT PRIMARY KEY, name TEXT, path TEXT,
                     created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
                 CREATE TABLE agent_configs (id TEXT PRIMARY KEY);
                 INSERT INTO workspaces (id) VALUES ('ws-skip');",
            )
            .unwrap();
            conn.execute_batch(persist::MIGRATION_SQL).unwrap();
        }
        let persist_config = || crate::persistence::PersistConfig {
            db_path: db_path.clone(),
            workspace_id: "ws-skip".into(),
        };

        // 첫 worker: tab 2개 저장 (셸 2개).
        {
            let client = InProcessRuntimeClient::with_shell(
                5,
                test_store(),
                test_logs_root("skip-1"),
                RedactionService::new(),
                spec("/bin/cat", &[]),
                Some(persist_config()),
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
            probe.wait_for(Duration::from_secs(15), |e| match e {
                RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.len() == 2 => Some(()),
                _ => None,
            });
        }

        // 새 worker: SpawnShell을 먼저 보내고 그 다음 RestoreWorkspace를 보낸다.
        // 세션이 이미 있으므로 복원은 skip → tab은 방금 만든 1개만 남아야 한다.
        let client2 = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("skip-2"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(persist_config()),
        );
        let mut probe2 = Probe::new(client2.subscribe());
        client2
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        // 새 셸 tab 1개 관측
        probe2.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.len() == 1 => Some(()),
            _ => None,
        });
        client2
            .send_command(RuntimeCommand::RestoreWorkspace)
            .unwrap();
        // restore가 skip되므로 tab 수가 2 이상으로 늘지 않는다. 명령 처리가
        // 확실히 끝나도록 뒤따르는 무해한 명령(SelectTab 없이 재확인)으로 배출을 유도.
        std::thread::sleep(Duration::from_millis(200));
        probe2.seen.extend(probe2.rx.drain());
        let max_tabs = probe2
            .seen
            .iter()
            .filter_map(|e| match e {
                RuntimeEvent::MuxUpdated { snapshot } => Some(snapshot.tabs.len()),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        assert_eq!(max_tabs, 1, "세션이 있는데 복원이 실행돼 tab이 덧붙었다");

        drop(client2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
