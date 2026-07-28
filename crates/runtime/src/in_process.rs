//! v0 구현체 (설계문서 2.3). worker thread가 세션들을 소유한다.
//! 세션 로직(PTY+terminal+lifecycle)은 session crate 소관 (PR-08).

use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use deppy_core::{MuxPaneId, MuxTabId};
use mux::{FocusManager, MuxPane, MuxSnapshot, MuxTab, MuxWindow, PaneSnapshot, TabSnapshot};
use std::path::PathBuf;

use pty::CommandSpec;
#[cfg(test)]
use secret::SecretStore;
use secret::{RedactionLease, RedactionService, StreamRedactor};
use session::{Session, StatusDetector, StatusPatterns};
use storage::SessionLogWriter;
use terminal::{TERMINAL_GLOBAL_CACHE_BUDGET_BYTES, TerminalCacheClass, TerminalCacheEvent};

use crate::client::{
    LOCAL_EVENT_QUEUE_CAP, RuntimeClient, RuntimeCommandSink, RuntimeEventReceiver,
    RuntimeEventStream,
};
use crate::command::{
    RUNTIME_COMMAND_QUEUE_BYTES_MAX, RUNTIME_SESSION_CAP, RuntimeCommand, SessionId,
};
use crate::event::{AgentConfigCorrelationId, MessagePayload, RuntimeEvent, SpawnKind};
use crate::host::{
    RuntimeCommandDispatcher, RuntimeHost, RuntimeSecret, RuntimeSecretResolver, RuntimeWake,
};
use crate::resource_monitor::{
    ProcessResourceMonitor, ProcessResourceMonitorConfig, SessionResourceTarget,
};

/// 재시작 시 한 세션에서 terminal parser로 다시 읽는 ANSI tail 상한. storage의
/// 파일별 보존 상한도 이 값과 같아 시작 I/O/CPU와 디스크를 함께 유계로 유지한다.
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
    /// wake가 화면 렌더에 묶여 있는가(GUI = request_repaint). true면 GUI가 렌더하지
    /// 않는 원격 전용 Viewport(원격 시청 lease가 hidden/Warm에서 만든 스냅샷)에는
    /// 깨우지 않는다 — 시청 중 폰 출력이 데스크톱 repaint를 유발하는 것을 차단
    /// (P5 리뷰 P1, §14.1 "웹 계층은 egui repaint를 유발하지 않는다"). slot 기록은
    /// 그대로라 탭 전환 시 따라잡기는 보존된다.
    render_bound: bool,
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
    command_tx: Option<SyncSender<QueuedRuntimeCommand>>,
    command_budget: Arc<RuntimeCommandQueueBudget>,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// command 송신과 PTY 출력 도착이 timeout을 기다리지 않고 worker를 깨우는 핸들.
    /// `Thread::unpark` 토큰은 1개로 coalesce되어 wake 폭주가 누적되지 않는다.
    worker_thread: Option<std::thread::Thread>,
    /// shutdown 명시 신호 — command_sink()가 SyncSender 클론을 배포한 뒤로는 채널
    /// Disconnected에만 의존하면 join이 영원히 안 끝난다(웹 브리지가 sink를 쥔 채
    /// 앱 종료 → 데드락, P5 리뷰 P1). worker는 이 플래그로도 종료한다.
    shutdown_flag: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(test)]
struct SecretStoreResolver(Arc<dyn SecretStore>);

#[cfg(test)]
impl RuntimeSecretResolver for SecretStoreResolver {
    fn resolve(&self, logical_credential_id: &str) -> anyhow::Result<RuntimeSecret> {
        self.0
            .get_secret(logical_credential_id)
            .map(RuntimeSecret::from_secret_string)
    }
}

impl InProcessRuntimeClient {
    /// `output_batch_ms`: 출력/명령이 없을 때 worker fallback poll 주기
    /// (설계문서 10.1, config.performance 소비). 실제 출력은 PTY reader wake로 즉시
    /// pump하고 연속 viewport만 8ms로 합친다. 시작 시점에 고정 — 변경은 앱 재시작 필요.
    /// `secret_store`: SpawnAgent의 secret env를 spawn 직전에 resolve할 때만 사용 (6.3).
    /// `logs_root`: 세션별 redacted 로그 디렉터리 (7장). `redaction`: 공유 레지스트리 —
    /// UI(credential 저장)와 worker(spawn 주입)가 같은 인스턴스에 등록한다.
    #[cfg(test)]
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
    #[cfg(test)]
    pub fn with_shell(
        output_batch_ms: u64,
        secret_store: Arc<dyn SecretStore>,
        logs_root: PathBuf,
        redaction: RedactionService,
        shell: CommandSpec,
        persist: Option<crate::persistence::PersistConfig>,
    ) -> Self {
        Self::try_with_shell_and_resolver(
            output_batch_ms,
            Arc::new(SecretStoreResolver(secret_store)),
            logs_root,
            redaction,
            shell,
            persist,
        )
        .expect("runtime worker thread 생성")
    }

    /// App composition adapter entry point. The resolver receives only logical
    /// credential ids and can atomically map them to published physical slots.
    pub fn try_new_with_resolver(
        output_batch_ms: u64,
        resolver: Arc<dyn RuntimeSecretResolver>,
        logs_root: PathBuf,
        redaction: RedactionService,
        persist: Option<crate::persistence::PersistConfig>,
        cwd: Option<PathBuf>,
        extra_env: Vec<(String, String)>,
    ) -> anyhow::Result<Self> {
        crate::host::validate_runtime_worker_config(
            output_batch_ms,
            &logs_root,
            persist.as_ref(),
            cwd.as_deref(),
            &extra_env,
        )?;
        let mut shell = pty::default_shell();
        shell.cwd = cwd;
        shell.env.extend(extra_env);
        Self::try_with_shell_and_resolver(
            output_batch_ms,
            resolver,
            logs_root,
            redaction,
            shell,
            persist,
        )
    }

    fn try_with_shell_and_resolver(
        output_batch_ms: u64,
        resolver: Arc<dyn RuntimeSecretResolver>,
        logs_root: PathBuf,
        redaction: RedactionService,
        shell: CommandSpec,
        persist: Option<crate::persistence::PersistConfig>,
    ) -> anyhow::Result<Self> {
        crate::host::validate_runtime_worker_config(
            output_batch_ms,
            &logs_root,
            persist.as_ref(),
            shell.cwd.as_deref(),
            &shell.env,
        )?;
        crate::command::validate_launch_spec(
            &shell.program,
            &shell.args,
            &shell.env,
            shell.cwd.as_deref(),
        )?;
        // 세션 id(u64)는 실행마다 1부터 다시 시작한다 — 이전 실행 로그에
        // append되지 않도록 실행(run) 단위 하위 디렉터리로 격리한다.
        // (영속 세션 id 도입은 PR-14)
        let run_ms = deppy_core::time::unix_ms();
        // 같은 ms의 다중 인스턴스/테스트 충돌 방지: pid + 프로세스 내 카운터
        static RUN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = RUN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let run_logs_root = logs_root.join(format!("run-{run_ms}-{}-{seq}", std::process::id()));
        let (command_tx, command_rx) = sync_channel(IN_PROCESS_CMD_QUEUE_CAP);
        let command_budget = Arc::new(RuntimeCommandQueueBudget::default());
        let subscribers: Arc<Mutex<Vec<Subscriber>>> = Arc::default();
        let worker_subscribers = Arc::clone(&subscribers);
        let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown_flag);
        let worker = std::thread::Builder::new()
            .name("runtime-worker".into())
            .spawn(move || {
                let persist_pipe =
                    persist.as_ref().and_then(
                        |config| match crate::persistence::PersistPipe::open(config) {
                            Ok(pipe) => Some(pipe),
                            Err(error) => {
                                trace_runtime_failure("persist_open", "persist_open_failed", error);
                                None
                            }
                        },
                    );
                // 증분 예산 캐시 시드 — 이전 실행/재시작이 남긴 디스크 아카이브 총량을
                // 워커당 1회 읽기 전용 스캔으로 복원한다 (A1 리뷰 P2). 이후 exit는
                // 이 캐시에 증분만 더하고 예산 초과 시에만 전체 스캔(gc)한다.
                let archive_disk_bytes = storage::scrollback_archive::scan_total(&logs_root);
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
                    session_redaction_leases: std::collections::HashMap::new(),
                    seed_redaction_lease: None,
                    logs: std::collections::HashMap::new(),
                    detectors: std::collections::HashMap::new(),
                    status_overrides: std::collections::HashMap::new(),
                    logs_root,
                    run_logs_root,
                    redaction,
                    secret_resolver: resolver,
                    mux: MuxState::new(),
                    tab_counter: 0,
                    persist: persist_pipe,
                    exited_order: std::collections::VecDeque::new(),
                    max_exited_backends: DEFAULT_MAX_EXITED_BACKENDS,
                    cache_budget_bytes: TERMINAL_GLOBAL_CACHE_BUDGET_BYTES,
                    archived: std::collections::HashMap::new(),
                    archived_order: std::collections::VecDeque::new(),
                    archived_on_disk: std::collections::HashSet::new(),
                    archive_disk_bytes,
                    hidden_scrollback: std::collections::HashSet::new(),
                    render_active: true,
                    suspended: false,
                    resource_monitor: ProcessResourceMonitor::new(
                        ProcessResourceMonitorConfig::default(),
                    ),
                    pressured_sessions: std::collections::HashSet::new(),
                    remote_viewing: std::collections::HashMap::new(),
                    shutdown_requested: worker_shutdown,
                }
                .run();
            })
            .map_err(|_| anyhow::anyhow!("runtime_worker_thread_spawn_failed"))?;
        let worker_thread = Some(worker.thread().clone());
        Ok(Self {
            command_tx: Some(command_tx),
            command_budget,
            subscribers,
            worker: Some(worker),
            worker_thread,
            shutdown_flag,
        })
    }

    /// worker로 명령을 보내는 복제 가능한 싱크 (P5b — 웹 계층 등 다른 스레드용).
    /// SyncSender 복제 + unpark 핸들만 캡처해 client 수명과 분리된다. worker가 종료되면
    /// try_send가 실패하고 경고 로그만 남는다 — 호출측은 fire-and-forget.
    pub fn command_sink(&self) -> Option<Arc<dyn Fn(RuntimeCommand) + Send + Sync>> {
        let tx = self.command_tx.as_ref()?.clone();
        let command_budget = Arc::clone(&self.command_budget);
        let worker_thread = self.worker_thread.clone();
        Some(Arc::new(move |command| {
            let queued = match prepare_queued_command(command, &command_budget) {
                Ok(queued) => queued,
                Err(_) => {
                    tracing::warn!(
                        error_code = "runtime_command_invalid",
                        "web→runtime 명령 거부"
                    );
                    return;
                }
            };
            match tx.try_send(queued) {
                Ok(()) => {
                    if let Some(worker_thread) = &worker_thread {
                        worker_thread.unpark();
                    }
                }
                Err(_) => tracing::warn!(
                    error_code = "runtime_command_enqueue_failed",
                    "web→runtime 명령 전송 실패"
                ),
            }
        }))
    }

    /// worker를 종료시키고 세션 정리(PtySession Drop)까지 동기적으로 기다린다.
    /// 앱 종료 경로(on_exit)에서 호출 — main 리턴과 worker 정리 사이의
    /// 스케줄링 경합으로 자식 프로세스가 reap되지 않는 문제 방지.
    pub fn shutdown(&mut self) {
        // 명시 플래그 + 채널 drop 이중화 — command_sink 클론이 밖에 살아 있어도
        // (웹 브리지 등) worker가 반드시 종료한다 (P5 리뷰 P1: 앱 종료 데드락).
        self.shutdown_flag
            .store(true, std::sync::atomic::Ordering::Release);
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
        let queued = prepare_queued_command(command, &self.command_budget)?;
        let Some(tx) = self.command_tx.as_ref() else {
            anyhow::bail!("runtime worker가 종료됨");
        };
        match tx.try_send(queued) {
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
                render_bound: false,
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
    /// wake=repaint인 GUI용 — 원격 전용 Viewport에는 깨우지 않는다 (P5 리뷰 P1).
    pub fn subscribe_with_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) -> RuntimeEventReceiver {
        self.subscribe_waked(wake, true)
    }

    /// 렌더와 무관한 백그라운드 소비자(웹 브리지 등)용 구독 — 원격 전용 Viewport에도
    /// 깨운다 (시청 프레임 라우팅에 필요). GUI는 [`Self::subscribe_with_wake`]를 쓸 것.
    pub fn subscribe_with_wake_background(
        &self,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> RuntimeEventReceiver {
        self.subscribe_waked(wake, false)
    }

    fn subscribe_waked(
        &self,
        wake: Arc<dyn Fn() + Send + Sync>,
        render_bound: bool,
    ) -> RuntimeEventReceiver {
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
                render_bound,
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

impl RuntimeHost for InProcessRuntimeClient {
    fn submit(&self, command: RuntimeCommand) -> anyhow::Result<()> {
        RuntimeCommandSink::send_command(self, command)
    }

    fn command_dispatcher(&self) -> Option<RuntimeCommandDispatcher> {
        let tx = self.command_tx.as_ref()?.clone();
        let command_budget = Arc::clone(&self.command_budget);
        let worker_thread = self.worker_thread.clone();
        Some(Arc::new(move |command| {
            let queued = prepare_queued_command(command, &command_budget)?;
            match tx.try_send(queued) {
                Ok(()) => {
                    if let Some(worker_thread) = &worker_thread {
                        worker_thread.unpark();
                    }
                    Ok(())
                }
                Err(TrySendError::Full(_)) => {
                    anyhow::bail!("runtime 명령 큐 가득참 — local runtime backpressure")
                }
                Err(TrySendError::Disconnected(_)) => {
                    anyhow::bail!("runtime worker가 종료됨")
                }
            }
        }))
    }

    fn subscribe_with_wake(&self, wake: RuntimeWake) -> RuntimeEventReceiver {
        InProcessRuntimeClient::subscribe_with_wake(self, wake)
    }

    fn subscribe_with_wake_background(&self, wake: RuntimeWake) -> RuntimeEventReceiver {
        InProcessRuntimeClient::subscribe_with_wake_background(self, wake)
    }

    fn shutdown(&mut self) {
        InProcessRuntimeClient::shutdown(self);
    }
}

/// 터미널 검색 매치 수 하드캡 (T3) — 기형 클라이언트가 과대한 상한을 보내도 방어한다.
const SEARCH_MAX_MATCHES_HARD_CAP: usize = 5_000;

/// exited 세션의 terminal backend(scrollback)를 유지하는 최대 개수 기본값 (§14.2/14.3).
/// 초과분은 가장 오래 전에 종료된 것부터 압축 아카이브로 내려 메모리를 유계로 만든다
/// (설정에서 변경 — SetTerminalCachePolicy).
/// 원격 시청 lease TTL 상한 — 브리지가 보낸 ttl_ms를 이 값으로 캡한다 (P5a).
/// 갱신이 끊긴 lease가 최대 이 시간 안에는 반드시 원복되게 하는 백스톱.
const REMOTE_VIEWING_TTL_CAP: Duration = Duration::from_secs(300);

const DEFAULT_MAX_EXITED_BACKENDS: usize = 64;
const MIN_RUNTIME_CACHE_BUDGET_BYTES: usize = 1024 * 1024;
const MAX_RUNTIME_CACHE_BUDGET_BYTES: usize = 2048 * 1024 * 1024;
/// 전역 예산 초과 시 live 세션을 트림해도 세션마다 최소 유지하는 스크롤백 줄 수.
/// 이 밑으로는 안 줄인다(사용성 보호) — 모든 세션이 여기 도달하면 트림을 멈춘다.
const LIVE_TRIM_FLOOR_LINES: usize = 200;

fn clamp_runtime_cache_budget_bytes(bytes: usize) -> usize {
    bytes.clamp(
        MIN_RUNTIME_CACHE_BUDGET_BYTES,
        MAX_RUNTIME_CACHE_BUDGET_BYTES,
    )
}

/// 압축 아카이브 총 바이트 예산 — 초과 시 오래된 아카이브부터 제거 (LRU).
/// 개당 압축 ANSI ~수십 KB라 넉넉한 개수를 담는다.
const ARCHIVED_SCROLLBACK_BUDGET_BYTES: usize = 16 * 1024 * 1024;
/// Disk archive format's existing uncompressed ceiling is 32MiB. Memory archives use the same
/// ceiling so an entry that is valid on disk cannot become unrestorable after LRU promotion.
const MAX_ARCHIVED_SCROLLBACK_RESTORE_BYTES: usize = 32 * 1024 * 1024;
/// Local runtime command queue cap. Commands are ordered and cannot be coalesced
/// safely in the transport boundary, so overflow is surfaced to the caller.
const IN_PROCESS_CMD_QUEUE_CAP: usize = 1024;
const FINAL_DRAIN_MAX_BYTES: usize = 2 * 1024 * 1024;
const FINAL_DRAIN_MAX_PUMPS: usize = 32;
const SHELL_TITLE_ID: &str = "workspace.spawn.shell";
const AGENT_TITLE_ID: &str = "workspace.spawn.agent";
type PreparedSecretEnv = (Vec<(String, String)>, Vec<RedactionLease>);

fn trace_runtime_failure<E>(phase: &'static str, error_code: &'static str, _source: E) {
    tracing::warn!(
        kind = "runtime",
        phase,
        error_code,
        "runtime operation failed"
    );
}

fn sanitized_spawn_failure(message_id: &'static str, error_code: &'static str) -> MessagePayload {
    MessagePayload::new(message_id)
        .arg("error_code", error_code)
        .diagnostic(error_code)
}

#[derive(Default)]
struct RuntimeCommandQueueBudget {
    retained_bytes: std::sync::atomic::AtomicUsize,
}

impl RuntimeCommandQueueBudget {
    fn reserve(
        self: &Arc<Self>,
        retained_bytes: usize,
    ) -> anyhow::Result<RuntimeCommandQueueReservation> {
        let reserved = self.retained_bytes.fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |current| {
                current
                    .checked_add(retained_bytes)
                    .filter(|next| *next <= RUNTIME_COMMAND_QUEUE_BYTES_MAX)
            },
        );
        if reserved.is_err() {
            anyhow::bail!("runtime_command_queue_bytes_exceeded");
        }
        Ok(RuntimeCommandQueueReservation {
            budget: Arc::clone(self),
            retained_bytes,
        })
    }

    #[cfg(test)]
    fn retained_bytes(&self) -> usize {
        self.retained_bytes
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

struct RuntimeCommandQueueReservation {
    budget: Arc<RuntimeCommandQueueBudget>,
    retained_bytes: usize,
}

impl Drop for RuntimeCommandQueueReservation {
    fn drop(&mut self) {
        let previous = self
            .budget
            .retained_bytes
            .fetch_sub(self.retained_bytes, std::sync::atomic::Ordering::AcqRel);
        debug_assert!(previous >= self.retained_bytes);
    }
}

struct QueuedRuntimeCommand {
    command: RuntimeCommand,
    reservation: RuntimeCommandQueueReservation,
}

impl QueuedRuntimeCommand {
    fn into_command(self) -> RuntimeCommand {
        let Self {
            command,
            reservation,
        } = self;
        drop(reservation);
        command
    }
}

fn prepare_queued_command(
    mut command: RuntimeCommand,
    budget: &Arc<RuntimeCommandQueueBudget>,
) -> anyhow::Result<QueuedRuntimeCommand> {
    let retention = crate::command::prepare_runtime_command_for_retention_internal(&mut command)?;
    let reservation = budget.reserve(retention.retained_bytes())?;
    Ok(QueuedRuntimeCommand {
        command,
        reservation,
    })
}

struct Worker {
    command_rx: Receiver<QueuedRuntimeCommand>,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
    batch: Duration,
    shell: CommandSpec,
    /// 워크스페이스 기본 env(.env 자동 주입). 이후 SpawnShell/SpawnAgent에 적용된다.
    /// secret은 (key, credential_id)로 들고 spawn 직전에만 resolve한다(6.3).
    default_env_plain: Vec<(String, String)>,
    default_env_secrets: Vec<(String, String)>,
    /// 이 워커의 workspace id — needsInput hook 키(`{workspace_id}:{session_id}`)에 쓴다.
    workspace_id: String,
    next_id: u64,
    /// 다중 세션 (PR-08 Session Runtime). 세션 로직은 session crate 소관.
    sessions: std::collections::HashMap<SessionId, Session>,
    /// Checked redaction leases are retained for exactly as long as their live/readable session.
    /// A session may need more than one lease when restored dotenv values supplement the resolved
    /// default credential set. Removal/archive drops the complete set and starts grace expiry.
    session_redaction_leases: std::collections::HashMap<SessionId, Vec<RedactionLease>>,
    /// Wire compatibility for `SeedRedaction`: one latest-only checked lease replaces the legacy
    /// permanent corpus registration. Production composition no longer sends this command.
    seed_redaction_lease: Option<RedactionLease>,
    /// spawn 직전 secret resolve 전용 (6.3). worker 단일 스레드 접근 (1.4).
    secret_resolver: Arc<dyn RuntimeSecretResolver>,
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
    /// 이 runtime에 배정된 프로세스 전역 터미널 캐시 바이트 예산의 share.
    cache_budget_bytes: usize,
    /// 압축 아카이브 — 백엔드를 내린 exited 세션의 zlib(ANSI) 덤프. pane이 다시
    /// 보이면 복원(inflate)한다 (§14.3 확장, 2026-07-11).
    archived: std::collections::HashMap<SessionId, ArchivedScrollback>,
    /// 아카이브 삽입 순서 (오래된 것이 앞 — 총 바이트 예산 초과 시 제거 순서)
    archived_order: std::collections::VecDeque<SessionId>,
    /// 디스크 아카이브(scrollback.zlib)가 있는 세션들 (PR-A1) — 메모리 아카이브가
    /// 예산 축출돼도 디스크에서 복원 가능함을 fs stat 없이 판정한다.
    archived_on_disk: std::collections::HashSet<SessionId>,
    /// 디스크 아카이브 총 바이트의 증분 캐시 (A1 리뷰 P2). 워커 시작 시 1회 스캔으로
    /// 시드하고, 기록 성공마다 그 파일 크기만 더한다. 예산 초과가 확정될 때만 gc를
    /// 호출(그때만 전체 디렉터리 스캔+제거)해 매 exit 전체 스캔 비용을 없앤다.
    archive_disk_bytes: u64,
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
    /// 원격 시청 lease — 세션별 만료 시각 (v3.3 P5a). 시청 중에는 hidden tab/Warm에서도
    /// 스냅샷을 생성한다("visible 등가" 승격). 비어 있으면 어떤 경로에도 추가 비용 없음.
    remote_viewing: std::collections::HashMap<SessionId, std::time::Instant>,
    /// shutdown 명시 신호 (P5 리뷰 P1) — command_sink 클론이 채널을 살려둬도 종료.
    shutdown_requested: Arc<std::sync::atomic::AtomicBool>,
}

fn collect_resource_targets_if_due(
    monitor: &ProcessResourceMonitor,
    now: Instant,
    collect: impl FnOnce() -> Vec<SessionResourceTarget>,
) -> Option<Vec<SessionResourceTarget>> {
    monitor.is_due(now).then(collect)
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

/// 증분 예산 캐시만으로 예산 초과를 판정한다 (A1 리뷰 P2) — true면 gc(전체 스캔)가
/// 필요하다. 전체 디렉터리 스캔 없이 캐시+이번 기록 크기로만 결정하는 순수 함수라
/// 단위 테스트로 "스캔 없이 정확히 감지"를 검증한다.
fn archive_cache_needs_gc(cached_bytes: u64, written_len: u64, budget: u64) -> bool {
    cached_bytes.saturating_add(written_len) > budget
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveInflateError {
    OutputLimit,
    InvalidStream,
}

fn inflate_archived_bounded(
    compressed: &[u8],
    max_bytes: usize,
) -> Result<Vec<u8>, ArchiveInflateError> {
    use std::io::Read as _;

    if max_bytes > MAX_ARCHIVED_SCROLLBACK_RESTORE_BYTES {
        return Err(ArchiveInflateError::OutputLimit);
    }
    let mut decoder = flate2::read::ZlibDecoder::new(compressed);
    let mut dump = Vec::with_capacity(compressed.len().min(max_bytes));
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let remaining = max_bytes.saturating_sub(dump.len());
        if remaining == 0 {
            let mut overflow = [0_u8; 1];
            match decoder.read(&mut overflow) {
                Ok(0) => return Ok(dump),
                Ok(_) => {
                    dump.clear();
                    dump.shrink_to_fit();
                    return Err(ArchiveInflateError::OutputLimit);
                }
                Err(_) => {
                    dump.clear();
                    dump.shrink_to_fit();
                    return Err(ArchiveInflateError::InvalidStream);
                }
            }
        }
        let read_cap = remaining.min(chunk.len());
        match decoder.read(&mut chunk[..read_cap]) {
            Ok(0) => return Ok(dump),
            Ok(read) => dump.extend_from_slice(&chunk[..read]),
            Err(_) => {
                dump.clear();
                dump.shrink_to_fit();
                return Err(ArchiveInflateError::InvalidStream);
            }
        }
    }
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

    /// `persistent_id`: SessionId → 영속 UUID 해석기(워커의 PersistPipe). 경계를 넘는
    /// 식별자를 UUID로 통일하기 위해 스냅샷에 함께 싣는다 (v3.7 I1).
    fn snapshot(&self, persistent_id: impl Fn(SessionId) -> Option<String>) -> MuxSnapshot {
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
                        persistent_session_id: pane.session_id.and_then(&persistent_id),
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
    fn session_capacity_available(&self) -> bool {
        self.sessions.len() < RUNTIME_SESSION_CAP
    }

    fn reject_invalid_command(&self, command: &RuntimeCommand) {
        match command {
            RuntimeCommand::SpawnAgent {
                agent_config_id, ..
            } => {
                let correlation_id = agent_config_id
                    .as_ref()
                    .filter(|id| crate::command::agent_config_id_is_valid(id))
                    .cloned()
                    .map(AgentConfigCorrelationId::from_validated);
                self.emit(RuntimeEvent::SpawnFailed {
                    kind: SpawnKind::Agent,
                    message: MessagePayload::new("runtime.spawn_failed.invalid_command"),
                });
                self.emit_agent_spawn_resolved(correlation_id, None);
            }
            RuntimeCommand::SpawnShell { .. } | RuntimeCommand::SplitPane { .. } => {
                self.emit(RuntimeEvent::SpawnFailed {
                    kind: SpawnKind::Shell,
                    message: MessagePayload::new("runtime.spawn_failed.invalid_command"),
                });
            }
            _ => tracing::warn!(
                error_code = "runtime_command_invalid",
                "runtime worker 명령 거부"
            ),
        }
    }

    fn reject_session_capacity(
        &self,
        kind: SpawnKind,
        correlation_id: Option<AgentConfigCorrelationId>,
    ) {
        self.emit(RuntimeEvent::SpawnFailed {
            kind,
            message: MessagePayload::new("runtime.spawn_failed.session_limit"),
        });
        if matches!(kind, SpawnKind::Agent) {
            self.emit_agent_spawn_resolved(correlation_id, None);
        }
    }

    /// Resolve one launch's complete logical-id set before any plaintext is moved into a process
    /// environment. Redaction registration is one checked atomic lease: a missing secret, unsafe
    /// legacy corpus, short value, or capacity failure leaves no partial lease and must block the
    /// launch.
    fn resolve_secret_set(
        &self,
        logical_ids: Vec<String>,
    ) -> anyhow::Result<(Vec<RuntimeSecret>, Option<RedactionLease>)> {
        let mut resolved = Vec::with_capacity(logical_ids.len());
        for logical_id in logical_ids {
            resolved.push(self.secret_resolver.resolve(&logical_id)?);
        }
        if resolved.is_empty() {
            return Ok((resolved, None));
        }
        self.redaction.ensure_safe()?;
        let secret_refs = resolved
            .iter()
            .map(RuntimeSecret::as_secret_string)
            .collect::<Vec<_>>();
        let lease = self.redaction.acquire_execution_lease(&secret_refs)?;
        Ok((resolved, Some(lease)))
    }

    fn resolve_secret_env(
        &self,
        entries: Vec<(String, String)>,
    ) -> anyhow::Result<PreparedSecretEnv> {
        let mut keys = Vec::with_capacity(entries.len());
        let mut logical_ids = Vec::with_capacity(entries.len());
        for (key, logical_id) in entries {
            keys.push(key);
            logical_ids.push(logical_id);
        }
        let (resolved, lease) = self.resolve_secret_set(logical_ids)?;
        let env = keys
            .into_iter()
            .zip(resolved)
            .map(|(key, value)| (key, value.into_string()))
            .collect();
        Ok((env, lease.into_iter().collect()))
    }

    fn prepare_agent_env(
        &self,
        launch_plain: Vec<(String, String)>,
        launch_secrets: Vec<(String, String)>,
    ) -> anyhow::Result<PreparedSecretEnv> {
        let default_secret_count = self.default_env_secrets.len();
        let mut all_secrets = self.default_env_secrets.clone();
        all_secrets.extend(launch_secrets);
        crate::command::validate_env_entries_with_base(
            &self.default_env_plain,
            &launch_plain,
            &all_secrets,
        )?;
        let (mut resolved_secrets, leases) = self.resolve_secret_env(all_secrets)?;
        let launch_secret_env = resolved_secrets.split_off(default_secret_count);
        let mut env = self.default_env_plain.clone();
        env.extend(resolved_secrets);
        env.extend(launch_plain);
        env.extend(launch_secret_env);
        Ok((env, leases))
    }

    /// Restored dotenv values are already plaintext, but secret-like keys still participate in a
    /// checked lease before spawn. The temporary wrappers zeroize their duplicate buffers after
    /// the corpus has accepted the complete set.
    fn acquire_dotenv_redaction_lease(
        &self,
        dotenv: &[(String, String)],
    ) -> anyhow::Result<Option<RedactionLease>> {
        let secrets = dotenv
            .iter()
            .filter(|(key, _)| crate::dotenv::is_secret_key(key))
            .map(|(_, value)| {
                RuntimeSecret::from_secret_string(secret::SecretString::new(value.clone()))
            })
            .collect::<Vec<_>>();
        if secrets.is_empty() {
            return Ok(None);
        }
        self.redaction.ensure_safe()?;
        let refs = secrets
            .iter()
            .map(RuntimeSecret::as_secret_string)
            .collect::<Vec<_>>();
        self.redaction
            .acquire_execution_lease(&refs)
            .map(Some)
            .map_err(Into::into)
    }

    fn retain_session_redaction_leases(&mut self, session: SessionId, leases: Vec<RedactionLease>) {
        if !leases.is_empty() {
            let replaced = self.session_redaction_leases.insert(session, leases);
            debug_assert!(
                replaced.is_none(),
                "session ids are monotonic within one worker"
            );
        }
    }

    fn remove_session(&mut self, session: SessionId) -> Option<Session> {
        self.session_redaction_leases.remove(&session);
        self.sessions.remove(&session)
    }

    fn spawn_session(
        id: SessionId,
        kind: session::SessionKind,
        spec: &CommandSpec,
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
    ) -> anyhow::Result<Session> {
        crate::command::validate_host_command(&RuntimeCommand::SpawnShell {
            cols,
            rows,
            scrollback_lines,
        })?;
        crate::command::validate_launch_spec_with_internal_env(
            &spec.program,
            &spec.args,
            &spec.env,
            spec.cwd.as_deref(),
            "DEPPY_SESSION_ID",
        )?;
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
                        self.handle_command(command.into_command());
                        handled += 1;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected
                || self
                    .shutdown_requested
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                // 종료 직전 잔여 명령을 **전량** 소화한다 — burst cap(128)만 처리하고
                // 끊으면 대량 ClosePane 꼬리(예: 워크스페이스 종료가 pane 수만큼 보낸
                // 닫기)가 유실돼 빈 레이아웃이 저장되지 않고 재활성 시 pane이 부활한다
                // (codex P2). cap은 평시 PTY pump 공정성용이고, 종료 시엔 더 이상
                // 새 명령이 들어오지 않아 큐가 유한하므로 전량 드레인이 안전하다.
                while let Ok(command) = self.command_rx.try_recv() {
                    self.handle_command(command.into_command());
                }
                // 기존 recv_timeout 루프처럼 마지막 command batch 뒤 한 번은 pump해
                // command 직후 도착한 PTY tail과 status/persistence를 반영하고 종료한다.
                // shutdown 플래그 경로: command_sink 클론(웹 브리지)이 채널을 살려둬도
                // 여기서 종료한다 (P5 리뷰 P1 — 앱 종료 데드락 방지).
                let _ = self.pump_sessions(true);
                self.pump_resource_monitor();
                self.pump_input_pressure_resolution();
                break; // client drop 또는 명시 shutdown → 종료
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
            if let Err(error) = pipe.flush_async_writes() {
                trace_runtime_failure("persist_shutdown_flush", "persist_flush_failed", error);
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
        self.emit_gated(event, true)
    }

    fn emit_agent_spawn_resolved(
        &self,
        agent_config_id: Option<AgentConfigCorrelationId>,
        session: Option<SessionId>,
    ) {
        if let Some(agent_config_id) = agent_config_id {
            self.emit(RuntimeEvent::AgentSpawnResolved {
                agent_config_id,
                session,
            });
        }
    }

    /// `gui_viewport`: 이 Viewport가 GUI 렌더 대상(visible pane + Active)인가.
    /// false(원격 시청 전용 스냅샷)면 render_bound 구독자(GUI)는 깨우지 않는다 —
    /// slot 기록은 유지해 탭 전환/Active 복귀 시 따라잡는다 (P5 리뷰 P1).
    /// Viewport 외 이벤트에는 무의미(항상 true로 호출).
    fn emit_gated(&self, event: RuntimeEvent, gui_viewport: bool) {
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
                    {
                        let mut slot = subscriber.viewports.lock().expect("viewport slot lock");
                        let prev = slot.insert(*session, event.clone());
                        // 미소비 이전 스냅샷의 dirty 델타를 합친다 — 안 그러면 그 행들이
                        // renderer 재shaping에서 빠져 stale로 남는다 (event.rs 헬퍼 주석).
                        if let Some(prev) = prev
                            && let Some(current) = slot.get_mut(session)
                        {
                            crate::event::merge_unconsumed_viewport_dirty(&prev, current);
                        }
                    }
                    // Viewport(출력)도 wake — push는 dirty(이번 tick 새 출력) 게이트라
                    // idle엔 발생하지 않고, 출력 도착 시에만 UI를 깨운다. 이로써 UI측
                    // 50ms 상시 폴링(가시+running 시 20fps 리페인트 = idle CPU ~10%)을
                    // 제거할 수 있다 (가시 상태 상시 리페인트 원인 조사, 2026-07-04).
                    // 단 원격 전용 스냅샷(gui_viewport=false)은 GUI(render_bound)를
                    // 깨우지 않는다 — Warm/hidden 시청이 repaint를 유발하지 않게 (P5 리뷰).
                    if let Some(wake) = &subscriber.wake
                        && (gui_viewport || !subscriber.render_bound)
                    {
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
        let now = Instant::now();
        let Some(targets) = collect_resource_targets_if_due(&self.resource_monitor, now, || {
            self.sessions
                .values()
                .map(|session| SessionResourceTarget {
                    session: session.id(),
                    identity: session.process_identity(),
                })
                .collect()
        }) else {
            return;
        };
        if let Some((snapshot, session_usage)) = self
            .resource_monitor
            .sample_if_due_with_sessions_at(now, &targets)
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

    fn handle_command(&mut self, mut command: RuntimeCommand) {
        // All production senders already use this primitive before queue retention. Reapplying it
        // here is an idempotent defense for direct/internal producers and preserves fail-closed
        // worker semantics without duplicating validation or canonicalization rules.
        if crate::command::prepare_runtime_command_for_retention_internal(&mut command).is_err() {
            self.reject_invalid_command(&command);
            return;
        }
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
                if !self.session_capacity_available() {
                    self.reject_session_capacity(SpawnKind::Shell, None);
                    return;
                }
                let id = SessionId(self.next_id);
                self.next_id += 1;
                let (spec, leases) = match self.shell_with_session(id) {
                    Ok(prepared) => prepared,
                    Err(_) => {
                        self.emit(RuntimeEvent::SpawnFailed {
                            kind: SpawnKind::Shell,
                            message: MessagePayload::new("runtime.spawn_failed.shell_secret"),
                        });
                        return;
                    }
                };
                match Self::spawn_session(
                    id,
                    session::SessionKind::Shell,
                    &spec, // 테스트 주입 가능해야 하므로 default_shell 헬퍼 대신 spec 직접
                    cols,
                    rows,
                    scrollback_lines,
                ) {
                    Ok(new_session) => {
                        self.sessions.insert(id, new_session);
                        self.retain_session_redaction_leases(id, leases);
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
                    Err(error) => {
                        trace_runtime_failure("spawn_shell", "pty_spawn_failed", error);
                        self.emit(RuntimeEvent::SpawnFailed {
                            kind: SpawnKind::Shell,
                            message: sanitized_spawn_failure(
                                "runtime.spawn_failed.shell",
                                "pty_spawn_failed",
                            ),
                        });
                    }
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
                let correlation_id = agent_config_id
                    .as_ref()
                    .map(|id| AgentConfigCorrelationId::from_validated(id.clone()));
                if self.suspended {
                    self.emit(RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Agent,
                        message: MessagePayload::new("runtime.spawn_failed.suspended"),
                    });
                    self.emit_agent_spawn_resolved(correlation_id, None);
                    return;
                }
                if !self.session_capacity_available() {
                    self.reject_session_capacity(SpawnKind::Agent, correlation_id);
                    return;
                }
                // Resolve the complete set and acquire one checked redaction lease before any
                // plaintext reaches the process environment. Workspace defaults are lower
                // precedence than per-launch values, so launcher PATH/YOLO settings remain exact.
                let (mut env, redaction_leases) =
                    match self.prepare_agent_env(env_plain, env_secrets) {
                        Ok(prepared) => prepared,
                        Err(_) => {
                            self.emit(RuntimeEvent::SpawnFailed {
                                kind: SpawnKind::Agent,
                                message: MessagePayload::new("runtime.spawn_failed.agent_secret"),
                            });
                            self.emit_agent_spawn_resolved(correlation_id, None);
                            return;
                        }
                    };
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
                        self.retain_session_redaction_leases(id, redaction_leases);
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
                        self.emit_agent_spawn_resolved(correlation_id, Some(id));
                        self.push_watched_viewports();
                    }
                    Err(error) => {
                        trace_runtime_failure("spawn_agent", "pty_spawn_failed", error);
                        self.emit(RuntimeEvent::SpawnFailed {
                            kind: SpawnKind::Agent,
                            message: sanitized_spawn_failure(
                                "runtime.spawn_failed.agent",
                                "pty_spawn_failed",
                            ),
                        });
                        self.emit_agent_spawn_resolved(correlation_id, None);
                    }
                }
            }
            RuntimeCommand::SetSessionDefaultEnv {
                env_plain,
                env_secrets,
            } => {
                // 이후 SpawnShell/SpawnAgent부터 적용 — 기존 세션은 건드리지 않는다.
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
                // 앱의 전역 최소 설정은 32MiB지만 resident runtime 사이에 나눈 share는
                // 그보다 작을 수 있다. 워커 경계에서는 비정상 0만 1MiB로 방어한다.
                self.cache_budget_bytes = clamp_runtime_cache_budget_bytes(cache_budget_bytes);
            }
            RuntimeCommand::SetRemoteViewing {
                session,
                viewing,
                ttl_ms,
            } => {
                if viewing {
                    // 살아있거나 아카이브에서 복원 가능한 세션만 — 이미 kill/close된
                    // id는 무시한다 (stale 커맨드가 유령 lease를 만들지 않게).
                    let known = self.sessions.contains_key(&session)
                        || self.archived.contains_key(&session)
                        || self.archived_on_disk.contains(&session);
                    if known {
                        let ttl =
                            Duration::from_millis(u64::from(ttl_ms)).min(REMOTE_VIEWING_TTL_CAP);
                        let is_new = self
                            .remote_viewing
                            .insert(session, std::time::Instant::now() + ttl)
                            .is_none();
                        // 신규 lease만: visible 등가 승격(hidden cap 해제) 후 현재 화면을
                        // 즉시 push — 시청자가 다음 출력까지 빈 화면을 보지 않는다.
                        // 갱신(15s 주기 재전송)은 만료 연장만 — 매번 전 대상 풀 스냅샷을
                        // 다시 만들지 않는다 (P5 리뷰 P3).
                        if is_new {
                            self.reconcile_visibility();
                            self.push_watched_viewports();
                        }
                    }
                } else if self.remote_viewing.remove(&session).is_some() {
                    // 해제 즉시 hidden cap 재적용 (tombstone 관례 — trailing 승격 차단).
                    self.reconcile_visibility();
                }
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
                    // hidden/exited 세션 resize는 cache class 재적용(compress_history(0))으로
                    // reflow가 만든 raw 셀을 해제한다 — 그 페이지를 OS로 반환하도록 신호.
                    let freed_scrollback = matches!(
                        active.cache_class(),
                        TerminalCacheClass::Hidden | TerminalCacheClass::Exited
                    );
                    self.save_terminal_size(session, cols, rows);
                    if freed_scrollback {
                        crate::signal_memory_released();
                    }
                }
            }
            RuntimeCommand::Scroll { session, delta } => {
                if let Some(active) = self.sessions.get_mut(&session) {
                    active.scroll(delta);
                }
            }
            RuntimeCommand::ScrollToBottom { session } => {
                if let Some(active) = self.sessions.get_mut(&session) {
                    active.scroll_to_bottom();
                }
            }
            RuntimeCommand::ScrollToPrompt { session, direction } => {
                // 마크 조회·델타 계산은 세션 소유 — 이동은 기존 Scroll과 같은 경로
                // (scroll → mark_full_dirty → 다음 pump이 Viewport emit).
                if let Some(active) = self.sessions.get_mut(&session) {
                    active.scroll_to_prompt(direction);
                }
            }
            RuntimeCommand::ExtractLastOutput { session } => {
                // 추출(마크 좌표 ↔ backend 텍스트)은 세션 소유. 마크가 없으면 빈 text로
                // 회신한다 — 복원 세션/훅 없는 셸의 판정·알림은 UI 몫 (조용한 실패 금지).
                if let Some(active) = self.sessions.get(&session) {
                    let (text, truncated) = active.extract_last_output();
                    self.emit(RuntimeEvent::LastOutputExtracted {
                        session,
                        text,
                        truncated,
                    });
                }
            }
            RuntimeCommand::EmergencyPersistFlush => {
                // 메모리 압박 사전 안전망 (로드맵 C2) — OOM-kill은 Drop을 실행하지
                // 않으므로 pending debounce 배치를 지금 커밋한다. 실패해도 앱을 막지
                // 않는 best-effort (압박 상황에서 부하를 더하지 않는다).
                if let Some(pipe) = &mut self.persist
                    && let Err(error) = pipe.flush_async_writes()
                {
                    trace_runtime_failure("emergency_persist_flush", "persist_flush_failed", error);
                }
            }
            RuntimeCommand::FreezeSession { session } => {
                // 폭주 세션 동결 (로드맵 B3) — 결과를 명시 회신해 낙관적 상태 대신
                // 실제 프로세스 상태를 UI가 반영한다. 종료/부재 세션은 무해히 무시.
                if let Some(active) = self.sessions.get(&session) {
                    let frozen = active.freeze();
                    self.emit(RuntimeEvent::SessionFreezeChanged { session, frozen });
                }
            }
            RuntimeCommand::ResumeSession { session } => {
                if let Some(active) = self.sessions.get(&session) {
                    let resumed = active.resume();
                    self.emit(RuntimeEvent::SessionFreezeChanged {
                        session,
                        frozen: !resumed,
                    });
                }
            }
            RuntimeCommand::NoteTurnStart { session } => {
                // hook 턴 경계 = 입력과 동등한 리셋 신호 (WriteInput의 on_input과 같은 처리).
                // 변화는 다음 tick의 evaluate()가 SessionStatusChanged로 알린다.
                if let Some(detector) = self.detectors.get_mut(&session) {
                    detector.on_turn_start();
                }
            }
            RuntimeCommand::SearchScrollback {
                session,
                query,
                max_matches,
            } => {
                // 상한을 하드캡으로 한 번 더 조인다(기형 클라이언트 방어 — remote 경로).
                let cap = (max_matches as usize).clamp(1, SEARCH_MAX_MATCHES_HARD_CAP);
                if let Some(active) = self.sessions.get(&session) {
                    let result = active.search_scrollback(&query, cap);
                    self.emit(RuntimeEvent::ScrollbackSearchResult {
                        session,
                        query,
                        result,
                    });
                }
            }
            RuntimeCommand::SeedRedaction { credential_ids } => {
                // Wire compatibility only. Replace one latest-only checked lease instead of
                // permanently growing the corpus; production composition no longer sends seeds.
                match self.resolve_secret_set(credential_ids) {
                    Ok((_resolved, lease)) => self.seed_redaction_lease = lease,
                    Err(_) => {
                        tracing::warn!("redaction seed rejected");
                    }
                }
            }
            RuntimeCommand::KillSession { session } => {
                self.final_drain(session);
                // Session drop → PtySession Drop이 process group 정리를 보장한다
                self.remove_session(session);
                self.exited_order.retain(|s| *s != session);
                self.hidden_scrollback.remove(&session);
                self.remote_viewing.remove(&session);
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
            Err(error) => {
                trace_runtime_failure("session_log_open", "session_log_open_failed", error);
            }
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
                trace_runtime_failure("terminal_size_load", "terminal_size_load_failed", error)
            }
        }

        let path = match SessionLogWriter::ansi_path(logs_root, persistent_id) {
            Ok(path) => path,
            Err(error) => {
                trace_runtime_failure("ansi_path", "ansi_path_rejected", error);
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
                tracing::warn!(
                    persistent_id,
                    error_code = %io_error_code(&error),
                    "복원 터미널 너비 탐색 실패"
                );
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
            trace_runtime_failure("terminal_size_save", "terminal_size_save_failed", error);
        }
    }

    /// pane이 가리키는 이전 영속 세션의 redacted ANSI를 새 terminal backend에
    /// 스트리밍 재생한다. 파일이 없는 최초/legacy 세션은 정상적인 빈 복원이다.
    fn replay_saved_ansi(logs_root: &std::path::Path, persistent_id: &str, session: &mut Session) {
        Self::replay_saved_ansi_ext(logs_root, persistent_id, session, true);
    }

    /// `finish_boundary`: 재생 후 모드 경계(alt-screen 종료 등)를 리셋할지.
    /// 셸 respawn 복원은 새 PTY가 붙기 전 정리가 필요해 true. **agent 열람 전용
    /// 복원(PR-A2 폴백)은 false** — PTY가 안 붙으므로, alt-screen을 강제 종료하면
    /// "종료 순간 화면 보존" 정책(PR-A1 §8)을 어기고 primary 빈 버퍼가 보인다.
    fn replay_saved_ansi_ext(
        logs_root: &std::path::Path,
        persistent_id: &str,
        session: &mut Session,
        finish_boundary: bool,
    ) {
        let path = match SessionLogWriter::ansi_path(logs_root, persistent_id) {
            Ok(path) => path,
            Err(_) => {
                tracing::warn!(
                    persistent_id,
                    error_code = "ansi_replay_path_rejected",
                    "복원 ANSI 로그 경로 거부"
                );
                return;
            }
        };
        let (mut file, snapshot_len) = match open_regular_snapshot(&path) {
            Ok(opened) => opened,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!(persistent_id, error_code = %io_error_code(&e), "복원 ANSI 로그 열기 실패");
                return;
            }
        };
        // Storage compacts this file to the same 16MiB ceiling. Reject a hostile replacement
        // instead of making the ANSI boundary scanner traverse attacker-controlled excess bytes.
        if snapshot_len > MAX_ANSI_REPLAY_BYTES {
            tracing::warn!(
                persistent_id,
                error_code = "ansi_replay_file_too_large",
                "복원 ANSI 로그 거부"
            );
            return;
        }
        let replay_start = match seek_ansi_replay_tail(
            &mut file,
            snapshot_len,
            MAX_ANSI_REPLAY_BYTES,
        ) {
            Ok(start) => start,
            Err(e) => {
                tracing::warn!(persistent_id, error_code = %io_error_code(&e), "복원 ANSI tail 탐색 실패");
                return;
            }
        };
        let expected_bytes = snapshot_len.saturating_sub(replay_start);
        let mut snapshot = std::io::Read::take(&mut file, expected_bytes);
        match session.replay_ansi(&mut snapshot) {
            Ok(bytes) if snapshot.limit() == 0 => {
                if bytes > 0
                    && finish_boundary
                    && let Err(error) = session.finish_ansi_replay()
                {
                    trace_runtime_failure("ansi_replay_finish", "ansi_replay_finish_failed", error);
                }
                tracing::info!(
                    persistent_id,
                    bytes,
                    replay_start,
                    "이전 ANSI scrollback 복원"
                );
            }
            Ok(_) => tracing::warn!(
                persistent_id,
                error_code = "ansi_replay_short_read",
                "이전 ANSI scrollback 복원 실패"
            ),
            Err(_) => tracing::warn!(
                persistent_id,
                error_code = "ansi_replay_read_failed",
                "이전 ANSI scrollback 복원 실패"
            ),
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

    fn shell_with_session(
        &self,
        id: SessionId,
    ) -> anyhow::Result<(CommandSpec, Vec<RedactionLease>)> {
        crate::command::validate_env_entries_with_base(
            &self.shell.env,
            &self.default_env_plain,
            &self.default_env_secrets,
        )?;
        let mut spec = self.shell.clone();
        spec.env
            .push(("DEPPY_SESSION_ID".to_owned(), self.session_key(id)));
        // 워크스페이스 기본 env(.env 자동 주입). The complete secret set is resolved and
        // protected before spawn; partial injection would silently change command semantics.
        spec.env.extend(self.default_env_plain.iter().cloned());
        let (secret_env, leases) = self.resolve_secret_env(self.default_env_secrets.clone())?;
        spec.env.extend(secret_env);
        Ok((spec, leases))
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
        // PR-A2: 열람 전용으로 복원된 exited 세션을 UI에 알린다. SessionRestored는
        // 완료 알림을 재발화하지 않으면서(재시작마다 done 알림 중복 방지) UI의 생존
        // 추적(LiveSessionTracker)·exit_code 부기·상태 배지를 갱신한다. SessionExited를
        // 그대로 쓰면 알림이 중복되고, 안 쓰면 세션이 "영원히 살아있는 것"으로 취급돼
        // auto-suspend/warm 축출이 무력화된다 (codex 리뷰 P1).
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
            self.emit(RuntimeEvent::SessionRestored { session, exit_code });
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

    /// Bounded dotenv projection for restored panes. Any missing/invalid/over-limit input is an
    /// empty fail-closed projection; restoration never applies a partial first-file result.
    fn restored_dotenv(dir: &std::path::Path) -> Vec<(String, String)> {
        match crate::dotenv::read_dotenv_merged_bounded(dir) {
            Ok(Some(dotenv)) => dotenv,
            Ok(None) | Err(_) => Vec::new(),
        }
    }

    /// 저장된 pane 하나에 fresh 셸을 spawn해 attach한다. spawn 실패 시에도 pane
    /// 자체는 만든다(session_id 없이) — 기존 "세션을 잃은 pane" 모델과 동일하게
    /// layout/tab 구조는 살아있게 한다.
    /// agent였던 pane은 respawn 대신 열람 전용 복원(PR-A2) — agent 재실행 금지는
    /// persistence 헤더의 안전 요구사항이고, 결과 화면 보존이 목적이다.
    fn restore_pane(&mut self, pane_state: &persist::PaneState) {
        if !self.session_capacity_available() {
            let pane = MuxPane::new(pane_state.id.clone(), pane_state.title.clone());
            self.mux.panes.insert(pane_state.id.clone(), pane);
            tracing::warn!(error_code = "runtime_session_limit", "복원 세션 상한 도달");
            return;
        }
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
        let (mut spec, mut redaction_leases) = match self.shell_with_session(id) {
            Ok(prepared) => prepared,
            Err(_) => {
                tracing::warn!(pane_id = %pane_state.id.0, "복원 중 셸 secret 준비 실패");
                self.mux.panes.insert(pane_state.id.clone(), pane);
                return;
            }
        };
        let restored_cwd = pane_state
            .cwd
            .as_deref()
            .filter(|cwd| crate::command::validate_runtime_path(std::path::Path::new(cwd)).is_ok())
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_dir());
        if let Some(dir) = &restored_cwd {
            spec.cwd = Some(dir.clone());
            let dotenv = Self::restored_dotenv(dir);
            match self.acquire_dotenv_redaction_lease(&dotenv) {
                Ok(Some(lease)) => redaction_leases.push(lease),
                Ok(None) => {}
                Err(_) => {
                    tracing::warn!(pane_id = %pane_state.id.0, "복원 중 dotenv redaction 준비 실패");
                    self.mux.panes.insert(pane_state.id.clone(), pane);
                    return;
                }
            }
            // 워크스페이스 기본 env보다 뒤에 붙어 pane 폴더 값이 이긴다.
            spec.env.extend(dotenv);
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
                self.retain_session_redaction_leases(id, redaction_leases);
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
            Err(error) => {
                trace_runtime_failure("restore_shell_spawn", "pty_spawn_failed", error);
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
        // 아카이브는 스트리밍으로 backend에 직접 feed — dump(≤32MB)를 통째로 올리면
        // 시작 복원이 pane 수만큼 순간 메모리 스파이크를 만든다 (2026-07-16).
        let archived = match storage::scrollback_archive::open(&self.logs_root, persistent_id) {
            Ok(Some(mut stream)) => {
                let meta = stream.meta;
                if crate::command::validate_host_command(&RuntimeCommand::SpawnShell {
                    cols: meta.cols,
                    rows: meta.rows,
                    scrollback_lines: meta.scrollback_lines as usize,
                })
                .is_err()
                {
                    return false;
                }
                let session = Session::restore_archived(
                    id,
                    archive_kind_from_u8(meta.kind),
                    meta.cols,
                    meta.rows,
                    meta.scrollback_lines as usize,
                    meta.exit_code,
                    &mut stream,
                );
                // 손상(절단/초과)이면 부분 feed된 세션을 버리고 폴백 — read() 손상 규약과 동일
                stream.finish().then_some(session)
            }
            _ => None,
        };
        let (restored, restored_from_disk) = match archived {
            Some(session) => (session, true),
            None => {
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
                    &mut std::io::empty(),
                );
                // 열람 전용 — 모드 경계 리셋 생략(alt-screen 화면 보존, codex 리뷰 P2)
                Self::replay_saved_ansi_ext(&self.logs_root, persistent_id, &mut session, false);
                (session, false)
            }
        };
        if let Some(pipe) = &mut self.persist
            && !pipe.session_rebound_archived(id, persistent_id)
        {
            return false;
        }
        self.sessions.insert(id, restored);
        if restored_from_disk {
            self.archived_on_disk.insert(id);
        }
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
        if !self.session_capacity_available() {
            self.reject_session_capacity(SpawnKind::Shell, None);
            return;
        }
        let id = SessionId(self.next_id);
        self.next_id += 1;
        let (spec, redaction_leases) = match self.shell_with_session(id) {
            Ok(prepared) => prepared,
            Err(_) => {
                self.emit(RuntimeEvent::SpawnFailed {
                    kind: SpawnKind::Shell,
                    message: MessagePayload::new("runtime.spawn_failed.shell_secret"),
                });
                return;
            }
        };
        match Self::spawn_session(
            id,
            session::SessionKind::Shell,
            &spec,
            80,
            24,
            scrollback_lines,
        ) {
            Ok(new_session) => {
                self.sessions.insert(id, new_session);
                self.retain_session_redaction_leases(id, redaction_leases);
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
                    self.remove_session(id);
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
            Err(error) => {
                trace_runtime_failure("split_shell_spawn", "pty_spawn_failed", error);
                self.emit(RuntimeEvent::SpawnFailed {
                    kind: SpawnKind::Shell,
                    message: sanitized_spawn_failure(
                        "runtime.spawn_failed.shell",
                        "pty_spawn_failed",
                    ),
                });
            }
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
            self.remove_session(session);
            self.exited_order.retain(|s| *s != session);
            self.hidden_scrollback.remove(&session);
            self.remote_viewing.remove(&session);
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
                self.remove_session(session);
                self.exited_order.retain(|s| *s != session);
                self.hidden_scrollback.remove(&session);
                self.remote_viewing.remove(&session);
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
                        Err(error) => trace_runtime_failure(
                            "session_log_final_append",
                            "session_log_append_failed",
                            error,
                        ),
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
            snapshot: Arc::new(self.mux.snapshot(|session| {
                // 영속 UUID(sessions.id) — 경계를 넘는 식별자. persist가 없으면 None이고,
                // 그 세션은 원격에서 표시 전용으로 강등된다 (v3.7 I1).
                self.persist
                    .as_ref()
                    .and_then(|pipe| pipe.session_log_key(session))
                    .map(str::to_owned)
            })),
        });
    }

    fn push_watched_viewports(&mut self) {
        // Warm이면 GUI 몫(watched)은 멈추지만(§14.1 — snapshot 생성 금지, 세션 pump는
        // 계속), 원격 시청 lease 세션은 계속 스냅샷을 생성한다 — 시청 승격은 스냅샷
        // 생성만 허용하고 egui repaint는 유발하지 않는다 (P5a). lease 0이면 기존과 동일.
        let gui_targets = if self.render_active {
            self.mux.watched_sessions()
        } else {
            Vec::new()
        };
        let mut targets = gui_targets.clone();
        for session in self.remote_viewed_sessions() {
            if !targets.contains(&session) {
                targets.push(session);
            }
        }
        // 아카이브된 세션의 pane이 보이면(또는 원격 시청이면) 먼저 복원한다 — 복원 직후
        // dirty라 아래 루프가 같은 tick에 Viewport를 push한다 ("연결 중…" 공백 없음).
        for session in &targets {
            if !self.sessions.contains_key(session)
                && (self.archived.contains_key(session) || self.archived_on_disk.contains(session))
            {
                self.inflate_archived(*session);
            }
        }
        let mut events = Vec::new();
        for session in targets {
            if let Some(active) = self.sessions.get_mut(&session)
                && let Some(snapshot) = active.take_snapshot()
            {
                events.push((
                    RuntimeEvent::Viewport {
                        session,
                        snapshot: Arc::new(snapshot),
                        bracketed_paste: active.bracketed_paste(),
                    },
                    // 원격 전용(GUI 비대상) 스냅샷은 render_bound 구독자를 깨우지 않는다.
                    gui_targets.contains(&session),
                ));
            }
        }
        for (event, gui_viewport) in events {
            self.emit_gated(event, gui_viewport);
        }
    }

    /// lease 만료분을 걷어내고 원격 시청 중인 세션 목록을 돌려준다 (P5a). 만료가
    /// 있었으면 hidden cap 재적용을 위해 reconcile_visibility를 즉시 호출한다 —
    /// mux 전이가 없는 유휴 상태에서도 visible 등가 승격이 lease보다 오래 남지 않는다.
    fn remote_viewed_sessions(&mut self) -> Vec<SessionId> {
        if self.remote_viewing.is_empty() {
            return Vec::new();
        }
        let now = std::time::Instant::now();
        let before = self.remote_viewing.len();
        self.remote_viewing.retain(|_, expiry| *expiry > now);
        if self.remote_viewing.len() != before {
            self.reconcile_visibility();
        }
        self.remote_viewing.keys().copied().collect()
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
        // 원격 시청 lease 세션 — GUI 가시성과 무관하게 Viewport 대상 (P5a).
        let remote_viewed = self.remote_viewed_sessions();
        // (이벤트, gui_viewport) — Viewport만 원격 전용 여부를 구분한다 (P5 리뷰 P1).
        let mut events: Vec<(RuntimeEvent, bool)> = Vec::new();
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
                        Err(error) => trace_runtime_failure(
                            "session_log_append",
                            "session_log_append_failed",
                            error,
                        ),
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
                    events.push((
                        RuntimeEvent::SessionStatusChanged {
                            session: active.id(),
                            status,
                        },
                        true,
                    ));
                    events.push((
                        RuntimeEvent::SessionStatusViewChanged {
                            session: active.id(),
                            view,
                        },
                        true,
                    ));
                }
            }
            let viewport_wanted = (self.render_active && watched.contains(&active.id()))
                || remote_viewed.contains(&active.id());
            if viewport_wanted && result.dirty {
                if allow_viewport {
                    if let Some(snapshot) = active.take_snapshot() {
                        // 원격 전용(GUI 비대상) 스냅샷은 render_bound 구독자를 깨우지 않는다.
                        let gui_viewport = self.render_active && watched.contains(&active.id());
                        events.push((
                            RuntimeEvent::Viewport {
                                session: active.id(),
                                snapshot: Arc::new(snapshot),
                                bracketed_paste: active.bracketed_paste(),
                            },
                            gui_viewport,
                        ));
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
                // 원격 시청 중 세션도 visible 등가 — 시청자가 마지막 화면을 본다 (P5a).
                let class =
                    if watched.contains(&active.id()) || remote_viewed.contains(&active.id()) {
                        TerminalCacheClass::Visible
                    } else {
                        TerminalCacheClass::Exited
                    };
                if active.cache_class() != class {
                    if let Some(event) = active.set_cache_class(class) {
                        trace_terminal_cache_event(active.id(), event);
                    }
                    // Exited 전환은 스크롤백을 전체 압축·트림해 셀 배열을 해제한다 —
                    // 트림 이벤트 유무와 무관하게 해제 페이지를 OS로 돌려주도록 신호한다.
                    if class == TerminalCacheClass::Exited {
                        crate::signal_memory_released();
                    }
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
                events.push((
                    RuntimeEvent::SessionStatusViewChanged {
                        session: active.id(),
                        view: session::SessionStatusView::process_exit(status),
                    },
                    true,
                ));
                events.push((
                    RuntimeEvent::SessionExited {
                        session: active.id(),
                        exit_code,
                    },
                    true,
                ));
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
            .filter_map(|(e, _)| match e {
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
            .filter_map(|(e, _)| match e {
                RuntimeEvent::SessionExited { session, .. } => Some(*session),
                _ => None,
            })
            .collect();
        for (event, gui_viewport) in events {
            self.emit_gated(event, gui_viewport);
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
        let mut visible: std::collections::HashSet<SessionId> =
            self.mux.watched_sessions().into_iter().collect();
        // 원격 시청 lease 세션은 visible 등가 — hidden cap 미적용 (P5a). 만료 정리는
        // remote_viewed_sessions가 하고, 여기서는 현재 map을 그대로 신뢰한다.
        visible.extend(self.remote_viewing.keys().copied());
        let sessions: Vec<SessionId> = self.sessions.keys().copied().collect();
        // hidden/exited 전환이 스크롤백을 해제하면(트림/압축) 그 페이지를 OS로 돌려주도록
        // purge 훅을 부른다. 여러 세션이 한 번에 숨겨져도 pass당 1회로 합친다.
        let mut freed_memory = false;
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
            if session.cache_class() != class {
                if let Some(event) = session.set_cache_class(class) {
                    trace_terminal_cache_event(id, event);
                }
                match class {
                    // Hidden/Exited 전환은 compress_history(0)로 HOT 창까지 셀 배열을
                    // 해제한다 — 트림 이벤트 유무와 무관하게(작은 스크롤백은 이벤트
                    // 없이도 해제됨) purge를 신호한다.
                    TerminalCacheClass::Hidden | TerminalCacheClass::Exited => {
                        freed_memory = true;
                    }
                    // 사용자가 세션을 열었다 — 압박 트림됐던 스크롤백 상한을 회복한다
                    // (가시성 기반 회복, 리뷰 A-H1). 상한만 올려 즉시 메모리는 안 늘고
                    // 이후 feed로 채워진다. 여전히 예산 초과면 다음 tick이 hidden부터
                    // 다시 트림하므로 이 세션은 보호된다.
                    TerminalCacheClass::Visible => {
                        session.clear_pressure_trim();
                    }
                }
            }
        }
        if freed_memory {
            crate::signal_memory_released();
        }
    }

    /// exited backend 개수 cap 초과분을 archive한다 (§14.3). pump 시작 시 호출 —
    /// 이번 tick의 신규 exit보다 최소 한 tick 뒤에 archive되도록.
    /// backend는 압축 아카이브(zlib ANSI)로 내려 pane을 유지하고, 다시 보이면
    /// 복원한다. 직렬화 미지원 백엔드만 기존대로 drop + pane detach
    /// ("연결 중…" 갇힘 방지 — codex 리뷰의 detach 사유는 복원 훅이 대신한다).
    fn archive_over_cap(&mut self) {
        // 아카이브는 백엔드를 통째로 드롭해(그리드+히스토리 전체) 압축 경로보다 훨씬
        // 많은 셀 배열을 해제한다. mimalloc은 명시적 purge 없이는 그 페이지를 OS로
        // 반환하지 않으므로(실측), 세션이 하나라도 제거됐으면 끝에서 purge를 신호한다.
        // archive_over_cap은 세션을 제거만 하고 추가하지 않으므로 len 감소 = 아카이브.
        let sessions_before = self.sessions.len();
        // 현재 보이는(active tab의) pane 세션은 archive하지 않는다 — split이면
        // 비포커스 pane도 화면에 있어 사용자가 그 scrollback을 보는 중일 수 있다
        // (codex 리뷰: focused 하나만 제외하면 부족). watched = visible.
        let mut visible = self.mux.watched_sessions();
        // 원격 시청 중 세션도 archive 금지 — 시청 중 backend가 내려가 화면이 얼거나
        // inflate↔archive 플립플롭이 생기는 것을 막는다 (P5a).
        visible.extend(self.remote_viewing.keys().copied());
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
            self.remove_session(session);
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
        // 세션이 하나라도 아카이브(제거)됐으면 해제된 백엔드 페이지를 OS로 반환한다.
        if self.sessions.len() < sessions_before {
            crate::signal_memory_released();
        }
        // exited 아카이브만으로 예산을 못 맞추면 live 세션 스크롤백을 트림한다 — 전역
        // 강제가 exited 세션만 회수하던 구멍(리뷰 B-M1)을 메운다. hidden 먼저, visible은
        // 최후수단. 트림이 셀 배열을 해제하므로 성공 시 해제 페이지를 OS로 반환.
        if self.terminal_cache_bytes() > self.cache_budget_bytes
            && self.trim_live_over_budget(&visible)
        {
            crate::signal_memory_released();
        }
        // 트림 회복은 총량 기반이 아니라 **가시성 기반**이다(reconcile_visibility). 총량
        // 기반은 트림된 세션이 스스로 잠잠해 보이게 만들어(floor가 신호를 억눌러) 균형
        // 워크로드에선 영영 회복 안 되거나(M1) 지속 runaway에선 진동(M2)했다. 대신
        // 사용자가 세션을 열(Visible 전이) 때 그 세션 floor만 해제한다 — hidden 세션은
        // 안 보이므로 트림 유지, 열면 회복(리뷰 A-H1 재설계).
    }

    /// exited 아카이브 후에도 전역 예산을 초과하면 live 세션의 스크롤백을 트림해 예산
    /// 안으로 넣는다. 안 보이는(hidden/원격시청 아님) 세션을 무거운 순으로 먼저 줄이고,
    /// 그래도 초과하면 최후수단으로 보이는 세션을 줄인다. 각 세션은 히스토리를 절반씩
    /// (FLOOR까지) 줄여 수렴시키고, 모두 FLOOR면 멈춘다(무한루프 없음). 하나라도 줄였으면
    /// true. 트림은 사용자가 보던 스크롤백을 줄일 수 있어 warn 로그를 남긴다.
    fn trim_live_over_budget(&mut self, visible: &[SessionId]) -> bool {
        let budget = self.cache_budget_bytes;
        let mut trimmed_any = false;
        // 트림을 못 하는(불변식이 깨진) 세션은 제외하고 다른 세션 계속 — 한 세션 때문에
        // 전체를 포기하지 않는다(리뷰 A-L1). 실무상 도달 불가하나 방어적.
        let mut cannot_trim: std::collections::HashSet<SessionId> =
            std::collections::HashSet::new();
        loop {
            // 현재 세션들의 (id, 추정바이트, 히스토리, 가시성) 스냅샷. cache_bytes는 전
            // 세션 합(제외 세션 포함)이어야 예산 판정이 정확하다 — 후보만 제외한다.
            let footprints: Vec<LiveTrimEntry> = self
                .sessions
                .iter()
                .map(|(id, session)| {
                    let fp = session.cache_footprint();
                    LiveTrimEntry {
                        id: *id,
                        estimated_bytes: fp.estimated_bytes,
                        history_lines: fp.history_lines,
                        visible: visible.contains(id),
                    }
                })
                .collect();
            let cache_bytes: usize = footprints.iter().map(|e| e.estimated_bytes).sum();
            let candidates: Vec<LiveTrimEntry> = footprints
                .into_iter()
                .filter(|e| !cannot_trim.contains(&e.id))
                .collect();
            let Some((id, target)) = select_next_live_trim(&candidates, cache_bytes, budget) else {
                if cache_bytes > budget {
                    tracing::warn!(
                        budget,
                        actual = cache_bytes,
                        "모든 세션을 최소 스크롤백까지 줄였으나 전역 예산 초과 지속"
                    );
                }
                return trimmed_any;
            };
            let over_bytes = cache_bytes.saturating_sub(budget);
            let was_visible = visible.contains(&id);
            let Some(session) = self.sessions.get_mut(&id) else {
                cannot_trim.insert(id);
                continue;
            };
            if session.trim_scrollback(target).is_none() {
                cannot_trim.insert(id); // 이 세션은 못 줄임 — 제외하고 다른 세션 계속
                continue;
            }
            trimmed_any = true;
            tracing::warn!(
                session = id.0,
                target,
                over_bytes,
                visible = was_visible,
                "전역 터미널 캐시 예산 초과 — live 세션 스크롤백 트림"
            );
        }
    }

    /// exited 세션의 scrollback을 zlib 압축 아카이브 항목으로 만든다.
    /// 직렬화 미지원 백엔드(예: experimental ghostty)는 None.
    fn make_archive_entry(&self, live: &Session) -> Option<ArchivedScrollback> {
        let dump = live.serialize_scrollback()?;
        if dump.len() > MAX_ARCHIVED_SCROLLBACK_RESTORE_BYTES {
            return None;
        }
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
        if self.archive_disk_bytes == storage::scrollback_archive::ARCHIVE_DISK_USAGE_UNKNOWN {
            match storage::scrollback_archive::gc(
                &self.logs_root,
                storage::scrollback_archive::ARCHIVE_DISK_BUDGET_BYTES,
            ) {
                Ok(total) => self.archive_disk_bytes = total,
                Err(error) => {
                    trace_runtime_failure(
                        "scrollback_archive_gc",
                        "scrollback_archive_gc_failed",
                        error,
                    );
                    return;
                }
            }
        }
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
            Ok(written_len) => self.finish_archive_write(session, &key, written_len),
            Err(error) => trace_runtime_failure(
                "scrollback_archive_write",
                "scrollback_archive_write_failed",
                error,
            ),
        }
    }

    fn finish_archive_write(&mut self, session: SessionId, key: &str, written_len: u64) {
        let accounted = self.account_archive_write(written_len);
        if accounted && storage::scrollback_archive::exists(&self.logs_root, key) {
            self.archived_on_disk.insert(session);
        } else if !accounted
            && let Ok(path) = storage::scrollback_archive::archive_path(&self.logs_root, key)
            && let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            trace_runtime_failure(
                "scrollback_archive_rollback",
                "scrollback_archive_rollback_failed",
                error,
            );
        }
    }

    /// 디스크 아카이브 기록 후 증분 예산 캐시를 갱신한다 (A1 리뷰 P2). 예산 내면
    /// 전체 스캔 없이 크기만 더하고, 초과가 확정될 때만 gc(전체 스캔+오래된 것부터
    /// 제거)를 호출해 캐시를 실제 총량으로 재동기화한다. 이로써 매 exit의 GC 비용이
    /// "지금까지 존재한 세션 수"에 비례하는 문제를 없앤다.
    fn account_archive_write(&mut self, written_len: u64) -> bool {
        let budget = storage::scrollback_archive::ARCHIVE_DISK_BUDGET_BYTES;
        if archive_cache_needs_gc(self.archive_disk_bytes, written_len, budget) {
            match storage::scrollback_archive::gc(&self.logs_root, budget) {
                Ok(total) => {
                    self.archive_disk_bytes = total;
                    true
                }
                Err(error) => {
                    trace_runtime_failure(
                        "scrollback_archive_gc",
                        "scrollback_archive_gc_failed",
                        error,
                    );
                    self.archive_disk_bytes =
                        storage::scrollback_archive::ARCHIVE_DISK_USAGE_UNKNOWN;
                    false
                }
            }
        } else {
            self.archive_disk_bytes = self.archive_disk_bytes.saturating_add(written_len);
            true
        }
    }

    /// 아카이브된 세션의 pane이 다시 보이면 백엔드를 복원한다 (열람 시 inflate).
    /// 메모리 아카이브 우선, 예산 축출로 내려갔으면 디스크 아카이브 폴백 (PR-A1).
    /// 복원된 세션은 다시 exited LRU의 최신 자리로 들어간다.
    fn inflate_archived(&mut self, session: SessionId) {
        if !self.session_capacity_available() {
            tracing::warn!(
                error_code = "runtime_session_limit",
                "아카이브 세션 복원 거부"
            );
            return;
        }
        if let Some(entry) = self.archived.remove(&session) {
            self.archived_order.retain(|s| *s != session);
            if let Ok(dump) =
                inflate_archived_bounded(&entry.compressed, MAX_ARCHIVED_SCROLLBACK_RESTORE_BYTES)
            {
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
        // 디스크 dump도 스트리밍 feed — 열람 복원이 순간 메모리 스파이크를 만들지 않게.
        match storage::scrollback_archive::open(&self.logs_root, &key) {
            Ok(Some(mut stream)) => {
                let meta = stream.meta;
                if crate::command::validate_host_command(&RuntimeCommand::SpawnShell {
                    cols: meta.cols,
                    rows: meta.rows,
                    scrollback_lines: meta.scrollback_lines as usize,
                })
                .is_err()
                {
                    tracing::warn!(
                        error_code = "runtime_archive_meta_invalid",
                        "디스크 아카이브 복원 거부"
                    );
                    return;
                }
                let restored = Session::restore_archived(
                    session,
                    archive_kind_from_u8(meta.kind),
                    meta.cols,
                    meta.rows,
                    meta.scrollback_lines as usize,
                    meta.exit_code,
                    &mut stream,
                );
                if stream.finish() {
                    self.adopt_restored_session(session, restored);
                    tracing::info!(session = session.0, "archived scrollback 복원 (디스크)");
                } else {
                    // 손상 — 부분 feed된 세션은 버린다 (아카이브는 finish가 삭제)
                    self.archived_on_disk.remove(&session);
                }
            }
            Ok(None) => {
                self.archived_on_disk.remove(&session);
            }
            Err(error) => trace_runtime_failure(
                "scrollback_archive_open",
                "scrollback_archive_open_failed",
                error,
            ),
        }
    }

    /// 아카이브 덤프로 열람 전용 세션을 만들어 편입한다 (인메모리 아카이브 inflate 경로).
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
        if !self.session_capacity_available()
            || crate::command::validate_host_command(&RuntimeCommand::SpawnShell {
                cols,
                rows,
                scrollback_lines,
            })
            .is_err()
        {
            return;
        }
        let mut reader = dump;
        let restored = Session::restore_archived(
            session,
            kind,
            cols,
            rows,
            scrollback_lines,
            exit_code,
            &mut reader,
        );
        self.adopt_restored_session(session, restored);
    }

    /// 복원된 열람 전용 세션을 세션 테이블 + exited LRU에 편입한다.
    fn adopt_restored_session(&mut self, session: SessionId, restored: Session) {
        if !self.session_capacity_available() {
            return;
        }
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
fn io_error_code(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;

    match error.kind() {
        ErrorKind::NotFound => "not_found",
        ErrorKind::PermissionDenied => "permission_denied",
        ErrorKind::InvalidData | ErrorKind::InvalidInput => "invalid_input",
        ErrorKind::UnexpectedEof => "short_read",
        _ => "io_failed",
    }
}

fn bounded_input_error(code: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, code)
}

fn open_regular_snapshot(path: &std::path::Path) -> std::io::Result<(std::fs::File, u64)> {
    let path_metadata = std::fs::symlink_metadata(path)?;
    if !path_metadata.file_type().is_file() {
        return Err(bounded_input_error("terminal_restore_not_regular"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let file_metadata = file.metadata()?;
    if !file_metadata.file_type().is_file() {
        return Err(bounded_input_error("terminal_restore_not_regular"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(bounded_input_error("terminal_restore_replaced"));
        }
    }
    Ok((file, file_metadata.len()))
}

fn read_tail_snapshot<R: std::io::Read + std::io::Seek>(
    reader: &mut R,
    snapshot_len: u64,
    max_bytes: u64,
) -> std::io::Result<Vec<u8>> {
    let retained = snapshot_len.min(max_bytes);
    let retained = usize::try_from(retained)
        .map_err(|_| bounded_input_error("terminal_tail_limit_invalid"))?;
    let start = snapshot_len.saturating_sub(max_bytes);
    reader.seek(std::io::SeekFrom::Start(start))?;
    let mut tail = vec![0_u8; retained];
    reader.read_exact(&mut tail)?;
    Ok(tail)
}

fn infer_zsh_terminal_cols(path: &std::path::Path, max_bytes: u64) -> std::io::Result<Option<u16>> {
    if max_bytes > MAX_ANSI_GEOMETRY_SCAN_BYTES {
        return Err(bounded_input_error("terminal_tail_limit_invalid"));
    }
    let (mut file, snapshot_len) = open_regular_snapshot(path)?;
    let tail = read_tail_snapshot(&mut file, snapshot_len, max_bytes)?;
    if file.metadata()?.len() < snapshot_len {
        return Err(bounded_input_error("terminal_tail_shrank"));
    }
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

/// ANSI tail cutoff를 UTF-8/escape 경계에 맞추고 ground-state newline 뒤를 우선한다.
/// full-screen TUI처럼 LF 없이 CR/CSI만 쓰는 구간도 안전한 다음 경계부터 복원한다.
fn seek_ansi_replay_tail(
    file: &mut std::fs::File,
    snapshot_len: u64,
    max_bytes: u64,
) -> std::io::Result<u64> {
    if max_bytes > MAX_ANSI_REPLAY_BYTES {
        return Err(bounded_input_error("ansi_replay_limit_invalid"));
    }
    if snapshot_len <= max_bytes {
        std::io::Seek::seek(file, std::io::SeekFrom::Start(0))?;
        return Ok(0);
    }
    let cutoff = snapshot_len.saturating_sub(max_bytes);
    let start = storage::seek_ansi_tail_boundary_snapshot(file, cutoff, snapshot_len, true)?;
    if start > snapshot_len || file.metadata()?.len() < snapshot_len {
        return Err(bounded_input_error("ansi_replay_snapshot_changed"));
    }
    std::io::Seek::seek(file, std::io::SeekFrom::Start(start))?;
    Ok(start)
}

/// live-트림 후보 한 세션의 스냅샷 (pure 선택 로직용).
#[derive(Debug, Clone, Copy, PartialEq)]
struct LiveTrimEntry {
    id: SessionId,
    estimated_bytes: usize,
    history_lines: usize,
    visible: bool,
}

/// 전역 예산 초과 시 다음에 트림할 세션과 목표 줄 수를 고른다(순수 함수 — 단위 테스트
/// 대상). 안 보이는 세션을 무거운(estimated_bytes) 순으로 먼저, 그래도 초과면 보이는
/// 세션을 최후수단으로. `LIVE_TRIM_FLOOR_LINES` 초과인 세션만 후보이고, 목표는 히스토리
/// 절반(FLOOR 하한). 예산 이내이거나 더 줄일 세션이 없으면 None(호출 루프가 종료).
fn select_next_live_trim(
    entries: &[LiveTrimEntry],
    cache_bytes: usize,
    budget: usize,
) -> Option<(SessionId, usize)> {
    if cache_bytes <= budget {
        return None;
    }
    for visible_pass in [false, true] {
        if let Some(entry) = entries
            .iter()
            .filter(|e| e.visible == visible_pass && e.history_lines > LIVE_TRIM_FLOOR_LINES)
            .max_by_key(|e| e.estimated_bytes)
        {
            let target = (entry.history_lines / 2).max(LIVE_TRIM_FLOOR_LINES);
            return Some((entry.id, target));
        }
    }
    None
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
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber as TracingSubscriber};

    #[derive(Clone)]
    struct TraceCapture {
        fields: Arc<Mutex<Vec<String>>>,
    }

    impl TracingSubscriber for TraceCapture {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            event.record(&mut TraceVisitor {
                fields: Arc::clone(&self.fields),
            });
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    struct TraceVisitor {
        fields: Arc<Mutex<Vec<String>>>,
    }

    impl Visit for TraceVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.fields
                .lock()
                .unwrap()
                .push(format!("{}={value}", field.name()));
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields
                .lock()
                .unwrap()
                .push(format!("{}={value:?}", field.name()));
        }
    }

    fn zlib_bytes(input: &[u8]) -> Vec<u8> {
        use std::io::Write as _;

        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(input).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn memory_archive_inflate_accepts_exact_cap_and_rejects_plus_one() {
        let exact = vec![b'x'; 4 * 1024];
        assert_eq!(
            inflate_archived_bounded(&zlib_bytes(&exact), exact.len()).unwrap(),
            exact
        );

        let plus_one = vec![b'y'; 4 * 1024 + 1];
        assert_eq!(
            inflate_archived_bounded(&zlib_bytes(&plus_one), 4 * 1024),
            Err(ArchiveInflateError::OutputLimit)
        );
        assert_eq!(
            inflate_archived_bounded(&zlib_bytes(b""), MAX_ARCHIVED_SCROLLBACK_RESTORE_BYTES + 1),
            Err(ArchiveInflateError::OutputLimit)
        );
    }

    #[test]
    fn failed_memory_archive_inflate_retains_no_partial_state() {
        let oversized = zlib_bytes(&vec![b'z'; 1025]);
        for _ in 0..128 {
            assert_eq!(
                inflate_archived_bounded(&oversized, 1024),
                Err(ArchiveInflateError::OutputLimit)
            );
        }
        assert_eq!(
            inflate_archived_bounded(b"not-zlib", 1024),
            Err(ArchiveInflateError::InvalidStream)
        );
        assert_eq!(
            inflate_archived_bounded(&zlib_bytes(b"healthy"), 1024).unwrap(),
            b"healthy"
        );
    }

    #[test]
    fn terminal_tail_snapshot_ignores_append_and_rejects_short_read() {
        let snapshot = b"old-snapshot";
        let mut grown = snapshot.to_vec();
        grown.extend_from_slice(b"-concurrent-append");
        let mut reader = std::io::Cursor::new(grown);
        assert_eq!(
            read_tail_snapshot(&mut reader, snapshot.len() as u64, 8).unwrap(),
            &snapshot[snapshot.len() - 8..]
        );

        let mut short = std::io::Cursor::new(b"short".as_slice());
        assert!(read_tail_snapshot(&mut short, 6, 6).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn terminal_restore_rejects_symlink_and_nonregular_input() {
        use std::os::unix::fs::symlink;

        let dir =
            std::env::temp_dir().join(format!("deppy-runtime-tail-input-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.ansi");
        let link = dir.join("link.ansi");
        std::fs::write(&target, b"safe").unwrap();
        let _ = std::fs::remove_file(&link);
        symlink(&target, &link).unwrap();

        assert!(infer_zsh_terminal_cols(&link, 4).is_err());
        assert!(infer_zsh_terminal_cols(&dir, 4).is_err());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn terminal_restore_production_reads_remain_bounded() {
        let production = include_str!("in_process.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        assert!(
            !production.contains("read_to_end"),
            "production terminal restore must not read to EOF into a growing Vec"
        );
        assert!(
            production.contains("std::io::Read::take(&mut file, expected_bytes)"),
            "ANSI replay must remain bounded to one metadata snapshot"
        );
        assert!(
            production.contains("MAX_ARCHIVED_SCROLLBACK_RESTORE_BYTES"),
            "memory archive restore must retain an explicit decompressed-byte ceiling"
        );
        #[cfg(unix)]
        assert!(
            production.contains("libc::O_NOFOLLOW | libc::O_NONBLOCK"),
            "terminal restore must prevent symlink/FIFO swaps at open"
        );
    }

    #[test]
    fn runtime_failure_trace_uses_only_static_low_cardinality_fields() {
        const MARKER: &str = "HOSTILE_RUNTIME_ERROR_PATH_COMMAND_MARKER";
        let fields = Arc::new(Mutex::new(Vec::new()));
        let subscriber = TraceCapture {
            fields: Arc::clone(&fields),
        };
        tracing::subscriber::with_default(subscriber, || {
            trace_runtime_failure("spawn_agent", "pty_spawn_failed", anyhow::anyhow!(MARKER));
        });

        let captured = fields.lock().unwrap().join("\n");
        assert!(captured.contains("kind=runtime"), "{captured}");
        assert!(captured.contains("phase=spawn_agent"), "{captured}");
        assert!(
            captured.contains("error_code=pty_spawn_failed"),
            "{captured}"
        );
        assert!(!captured.contains(MARKER), "{captured}");
    }

    #[test]
    fn production_runtime_diagnostics_never_format_raw_error_chains() {
        let production = include_str!("in_process.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        for forbidden in [
            "{e}",
            "{e:#}",
            "{error}",
            "{error:#}",
            ".arg(\"error\"",
            ".arg(\"path\"",
            ".arg(\"command\"",
            ".diagnostic(format!",
        ] {
            assert!(!production.contains(forbidden), "found {forbidden}");
        }
        assert!(production.contains("trace_runtime_failure"));
        assert!(production.contains("sanitized_spawn_failure"));
    }

    struct RecordingResolver {
        calls: Mutex<Vec<String>>,
        value: Option<String>,
    }

    impl RuntimeSecretResolver for RecordingResolver {
        fn resolve(&self, logical_credential_id: &str) -> anyhow::Result<RuntimeSecret> {
            self.calls
                .lock()
                .expect("resolver calls lock")
                .push(logical_credential_id.to_owned());
            match &self.value {
                Some(value) => Ok(RuntimeSecret::new(value.clone())),
                None => anyhow::bail!("physical-slot-never-leak: injected resolver failure"),
            }
        }
    }

    struct CompleteSetResolver {
        calls: Mutex<Vec<String>>,
        unsafe_id: Option<&'static str>,
    }

    impl RuntimeSecretResolver for CompleteSetResolver {
        fn resolve(&self, logical_credential_id: &str) -> anyhow::Result<RuntimeSecret> {
            self.calls
                .lock()
                .expect("resolver calls lock")
                .push(logical_credential_id.to_owned());
            let value = if self.unsafe_id == Some(logical_credential_id) {
                "x"
            } else {
                "runtime-secret-value"
            };
            Ok(RuntimeSecret::new(value.to_owned()))
        }
    }

    fn admission_worker(
        resolver: Arc<dyn RuntimeSecretResolver>,
        name: &str,
    ) -> (Worker, std::sync::mpsc::Receiver<RuntimeEvent>) {
        let (_command_tx, command_rx) = sync_channel(1);
        let (event_tx, event_rx) = sync_channel(16);
        let subscribers = Arc::new(Mutex::new(vec![Subscriber {
            events: event_tx,
            overflowed: Arc::default(),
            viewports: Arc::default(),
            input_pressures: Arc::default(),
            resource_usage: Arc::default(),
            wake: None,
            render_bound: false,
        }]));
        let logs_root = test_logs_root(name);
        (
            Worker {
                command_rx,
                subscribers,
                batch: Duration::from_millis(5),
                shell: spec("/bin/true", &[]),
                default_env_plain: Vec::new(),
                default_env_secrets: Vec::new(),
                workspace_id: "workspace".to_owned(),
                next_id: 1,
                sessions: std::collections::HashMap::new(),
                session_redaction_leases: std::collections::HashMap::new(),
                seed_redaction_lease: None,
                secret_resolver: resolver,
                logs: std::collections::HashMap::new(),
                detectors: std::collections::HashMap::new(),
                status_overrides: std::collections::HashMap::new(),
                run_logs_root: logs_root.join("run"),
                logs_root,
                redaction: RedactionService::new(),
                mux: MuxState::new(),
                tab_counter: 0,
                persist: None,
                exited_order: std::collections::VecDeque::new(),
                max_exited_backends: DEFAULT_MAX_EXITED_BACKENDS,
                cache_budget_bytes: TERMINAL_GLOBAL_CACHE_BUDGET_BYTES,
                archived: std::collections::HashMap::new(),
                archived_order: std::collections::VecDeque::new(),
                archived_on_disk: std::collections::HashSet::new(),
                archive_disk_bytes: 0,
                hidden_scrollback: std::collections::HashSet::new(),
                render_active: true,
                suspended: false,
                resource_monitor: ProcessResourceMonitor::new(
                    ProcessResourceMonitorConfig::default(),
                ),
                pressured_sessions: std::collections::HashSet::new(),
                remote_viewing: std::collections::HashMap::new(),
                shutdown_requested: Arc::default(),
            },
            event_rx,
        )
    }

    fn correlated_agent_command() -> RuntimeCommand {
        RuntimeCommand::SpawnAgent {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
            agent_config_id: Some("agent-correlation".to_owned()),
            command: "/bin/true".to_owned(),
            args: Vec::new(),
            env_plain: Vec::new(),
            env_secrets: vec![("TOKEN".to_owned(), "credential".to_owned())],
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        }
    }

    #[test]
    fn worker_defensively_rejects_invalid_agent_and_resolves_correlation_without_secret() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: Some("runtime-secret-value".to_owned()),
        });
        let (mut worker, event_rx) = admission_worker(resolver.clone(), "invalid-worker-command");
        let mut command = correlated_agent_command();
        let RuntimeCommand::SpawnAgent {
            command: program, ..
        } = &mut command
        else {
            unreachable!()
        };
        *program = "x".repeat(32 * 1024 + 1);

        worker.handle_command(command);

        assert!(resolver.calls.lock().unwrap().is_empty());
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::SpawnFailed {
                kind: SpawnKind::Agent,
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::AgentSpawnResolved {
                agent_config_id,
                session: None,
            } if agent_config_id.as_str() == "agent-correlation"
        )));
    }

    #[test]
    fn session_257_is_rejected_before_id_secret_or_spawn() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: Some("runtime-secret-value".to_owned()),
        });
        let (mut worker, event_rx) = admission_worker(resolver.clone(), "session-cap");
        for raw_id in 1..=RUNTIME_SESSION_CAP {
            let id = SessionId(raw_id as u64);
            let mut empty = std::io::empty();
            worker.sessions.insert(
                id,
                Session::restore_archived(
                    id,
                    session::SessionKind::Shell,
                    1,
                    1,
                    0,
                    Some(0),
                    &mut empty,
                ),
            );
        }
        let next_id = worker.next_id;

        for _ in 0..3 {
            worker.handle_command(correlated_agent_command());
        }

        assert_eq!(worker.sessions.len(), RUNTIME_SESSION_CAP);
        assert_eq!(worker.next_id, next_id);
        assert!(resolver.calls.lock().unwrap().is_empty());
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    RuntimeEvent::AgentSpawnResolved { session: None, .. }
                ))
                .count(),
            3
        );
    }

    #[test]
    fn pane_title_storage_canonicalizes_256_huge_spare_allocations() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _event_rx) = admission_worker(resolver, "pane-title-capacity");
        worker.subscribers.lock().unwrap().clear();

        for index in 0..RUNTIME_SESSION_CAP {
            let pane = MuxPaneId(format!("pane-{index}"));
            worker
                .mux
                .panes
                .insert(pane.clone(), MuxPane::new(pane.clone(), String::new()));
            let mut title = String::with_capacity(64 * 1024);
            title.push_str(&"x".repeat(4 * 1024));
            worker.handle_command(RuntimeCommand::RenamePane { pane, title });
        }

        let retained = worker
            .mux
            .panes
            .values()
            .map(|pane| pane.title.capacity())
            .sum::<usize>();
        assert!(retained <= RUNTIME_SESSION_CAP * 4 * 1024);
    }

    #[test]
    fn default_env_and_cwd_are_canonicalized_before_worker_storage() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _event_rx) = admission_worker(resolver, "worker-state-capacity");
        let mut env_plain = Vec::with_capacity(4_096);
        let mut key = String::with_capacity(64 * 1024);
        key.push_str("KEY");
        let mut value = String::with_capacity(64 * 1024);
        value.push_str("value");
        env_plain.push((key, value));
        worker.handle_command(RuntimeCommand::SetSessionDefaultEnv {
            env_plain,
            env_secrets: Vec::with_capacity(4_096),
        });
        assert_eq!(
            worker.default_env_plain.capacity(),
            worker.default_env_plain.len()
        );
        assert_eq!(
            worker.default_env_plain[0].0.capacity(),
            worker.default_env_plain[0].0.len()
        );
        assert_eq!(
            worker.default_env_plain[0].1.capacity(),
            worker.default_env_plain[0].1.len()
        );
        assert_eq!(worker.default_env_secrets.capacity(), 0);

        let mut cwd = PathBuf::with_capacity(64 * 1024);
        cwd.push("cwd");
        worker.handle_command(RuntimeCommand::SetShellCwd(Some(cwd)));
        let stored = worker.shell.cwd.as_ref().unwrap();
        assert_eq!(
            stored.capacity(),
            stored.as_os_str().as_encoded_bytes().len()
        );
    }

    #[test]
    fn runtime_cache_share_accepts_values_below_global_32mib_minimum() {
        assert_eq!(clamp_runtime_cache_budget_bytes(0), 1024 * 1024);
        assert_eq!(
            clamp_runtime_cache_budget_bytes(8 * 1024 * 1024),
            8 * 1024 * 1024
        );
        assert_eq!(
            clamp_runtime_cache_budget_bytes(usize::MAX),
            2048 * 1024 * 1024
        );
    }

    #[test]
    fn pane_restore_dotenv_accepts_exact_limits_and_rejects_over_limit_without_partial_env() {
        fn padded(entry: &str, target_bytes: usize) -> String {
            assert!(entry.len() <= target_bytes);
            let mut content = entry.to_owned();
            content.push_str(&"#".repeat(target_bytes - entry.len()));
            content
        }

        let dir = std::env::temp_dir().join(format!(
            "deppy-pane-dotenv-limits-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let half = crate::dotenv::DOTENV_TOTAL_BYTES_MAX / 2;
        std::fs::write(dir.join(".env"), padded("PORT=3000\n", half)).unwrap();
        std::fs::write(dir.join(".env.local"), padded("PORT=4000\n", half)).unwrap();
        assert_eq!(
            Worker::restored_dotenv(&dir),
            vec![("PORT".to_owned(), "4000".to_owned())]
        );

        std::fs::write(dir.join(".env.local"), padded("PORT=4000\n", half + 1)).unwrap();
        assert!(Worker::restored_dotenv(&dir).is_empty());

        std::fs::write(
            dir.join(".env"),
            "A=1\n".repeat(crate::dotenv::DOTENV_ENTRIES_MAX / 2),
        )
        .unwrap();
        std::fs::write(
            dir.join(".env.local"),
            "A=2\n".repeat(crate::dotenv::DOTENV_ENTRIES_MAX / 2 + 1),
        )
        .unwrap();
        assert!(Worker::restored_dotenv(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_store() -> Arc<dyn SecretStore> {
        Arc::new(secret::KeyringSecretStore)
    }

    fn test_logs_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deppy-rt-logs-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "deppy-rt-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn create_persist_db(db_path: &std::path::Path, workspace_id: &str) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE workspaces (id TEXT PRIMARY KEY, name TEXT, path TEXT,
                 created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
             CREATE TABLE agent_configs (id TEXT PRIMARY KEY);",
        )
        .unwrap();
        conn.execute("INSERT INTO workspaces (id) VALUES (?1)", [workspace_id])
            .unwrap();
        conn.execute("INSERT INTO agent_configs (id) VALUES ('cfg-sf03')", [])
            .unwrap();
        conn.execute_batch(persist::MIGRATION_SQL).unwrap();
    }

    fn persisted_single_pane_window(
        session_id: &str,
        title: &str,
        cwd: Option<String>,
    ) -> persist::WindowState {
        let pane_id = MuxPaneId::new();
        let tab_id = MuxTabId::new();
        persist::WindowState {
            id: deppy_core::MuxWindowId::new(),
            title: Some("restore".to_owned()),
            active_tab: Some(tab_id.clone()),
            tabs: vec![persist::TabState {
                id: tab_id,
                title: "restore".to_owned(),
                layout: mux::LayoutNode::Pane(pane_id.clone()),
                active_pane: Some(pane_id.clone()),
                panes: vec![persist::PaneState {
                    id: pane_id,
                    session_id: Some(session_id.to_owned()),
                    title: title.to_owned(),
                    pane_kind: mux::PaneKind::Terminal,
                    cwd,
                }],
            }],
        }
    }

    fn seed_persisted_session_pane(
        db_path: &std::path::Path,
        workspace_id: &str,
        session_id: &str,
        session_kind: &str,
        agent_id: Option<&str>,
        status: &str,
        cwd: &str,
    ) {
        let mut conn = rusqlite::Connection::open(db_path).unwrap();
        let row = persist::SessionRow {
            id: session_id.to_owned(),
            workspace_id: workspace_id.to_owned(),
            session_kind: session_kind.to_owned(),
            agent_id: agent_id.map(str::to_owned),
            title: "restored".to_owned(),
            command: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), "echo restored".to_owned()],
            cwd: cwd.to_owned(),
            status: status.to_owned(),
            last_log_offset: 0,
        };
        persist::upsert_session(&conn, &row).unwrap();
        let window = persisted_single_pane_window(session_id, "restored", Some(cwd.to_owned()));
        persist::save_window_layout(&mut conn, workspace_id, &window).unwrap();
    }

    fn restore_workspace_from_fixture(
        db_path: &std::path::Path,
        logs_root: &std::path::Path,
        workspace_id: &str,
        shell: CommandSpec,
    ) -> Arc<MuxSnapshot> {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            logs_root.to_path_buf(),
            RedactionService::new(),
            shell,
            Some(crate::persistence::PersistConfig {
                db_path: db_path.to_path_buf(),
                workspace_id: workspace_id.to_owned(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::RestoreWorkspace)
            .unwrap();
        let mux = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| tab.panes.iter())
                    .any(|pane| pane.session_id.is_some()) =>
            {
                Some(snapshot.clone())
            }
            _ => None,
        });
        drop(client);
        mux
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
            render_bound: false,
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

    /// 유휴 worker pump가 session target `Vec`를 만들지 않고 정확한 2초 샘플 경계에서만
    /// collection closure를 한 번 호출하는지 가짜 시각과 호출 카운터로 고정한다.
    #[test]
    fn resource_targets_are_collected_only_once_at_each_exact_due_deadline() {
        let interval = Duration::from_secs(2);
        let mut monitor = ProcessResourceMonitor::new(ProcessResourceMonitorConfig {
            sample_interval: interval,
            high_cpu_percent: f32::MAX,
            high_rss_bytes: u64::MAX,
        });
        let start = Instant::now();
        let collections = std::cell::Cell::new(0usize);
        let collect = || {
            collections.set(collections.get() + 1);
            Vec::with_capacity(1)
        };

        // Construction preserves the existing immediate first sample and CPU-baseline semantics.
        let first_targets = collect_resource_targets_if_due(&monitor, start, collect)
            .expect("first resource sample must be immediately due");
        let (first, _) = monitor
            .sample_if_due_with_sessions_at(start, &first_targets)
            .expect("first resource sample must emit");
        assert!(first.cpu_percent.is_none());

        collections.set(0);
        let just_before_due = start + interval - Duration::from_nanos(1);
        for _ in 0..300 {
            assert!(collect_resource_targets_if_due(&monitor, just_before_due, collect).is_none());
        }
        assert_eq!(collections.get(), 0);

        let exact_due = start + interval;
        let due_targets = collect_resource_targets_if_due(&monitor, exact_due, collect)
            .expect("exact resource deadline must be due");
        assert_eq!(collections.get(), 1);
        let _ = monitor.sample_if_due_with_sessions_at(exact_due, &due_targets);

        collections.set(0);
        let just_before_next_due = exact_due + interval - Duration::from_nanos(1);
        for _ in 0..300 {
            assert!(
                collect_resource_targets_if_due(&monitor, just_before_next_due, collect).is_none()
            );
        }
        assert_eq!(collections.get(), 0);
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
        let (tx, rx) = sync_channel(1);
        let command_budget = Arc::new(RuntimeCommandQueueBudget::default());
        let client = InProcessRuntimeClient {
            command_tx: Some(tx),
            command_budget: Arc::clone(&command_budget),
            subscribers: Arc::default(),
            worker: None,
            worker_thread: None,
            shutdown_flag: Arc::default(),
        };
        client
            .send_command(RuntimeCommand::SetWorkspaceState(
                WorkspaceRuntimeState::Active,
            ))
            .unwrap();
        let retained_after_first = command_budget.retained_bytes();
        let err = client
            .send_command(RuntimeCommand::SetWorkspaceState(
                WorkspaceRuntimeState::Warm,
            ))
            .unwrap_err();
        assert!(err.to_string().contains("명령 큐 가득참"));
        assert_eq!(command_budget.retained_bytes(), retained_after_first);
        drop(rx);
        assert_eq!(command_budget.retained_bytes(), 0);
    }

    #[test]
    fn command_queue_byte_budget_accepts_exact_rejects_repeated_plus_one_and_recovers() {
        let budget = Arc::new(RuntimeCommandQueueBudget::default());
        let exact = budget.reserve(RUNTIME_COMMAND_QUEUE_BYTES_MAX).unwrap();
        assert_eq!(budget.retained_bytes(), RUNTIME_COMMAND_QUEUE_BYTES_MAX);
        for _ in 0..128 {
            assert!(budget.reserve(1).is_err());
            assert_eq!(budget.retained_bytes(), RUNTIME_COMMAND_QUEUE_BYTES_MAX);
        }
        drop(exact);
        assert_eq!(budget.retained_bytes(), 0);

        let command_overhead = std::mem::size_of::<RuntimeCommand>();
        let payload_bytes = RUNTIME_COMMAND_QUEUE_BYTES_MAX / 2 - command_overhead;
        let first = prepare_queued_command(
            RuntimeCommand::WriteInput {
                session: SessionId(1),
                bytes: vec![0; payload_bytes],
            },
            &budget,
        )
        .unwrap();
        let second = prepare_queued_command(
            RuntimeCommand::WriteInput {
                session: SessionId(1),
                bytes: vec![0; payload_bytes],
            },
            &budget,
        )
        .unwrap();
        assert_eq!(budget.retained_bytes(), RUNTIME_COMMAND_QUEUE_BYTES_MAX);
        assert!(
            prepare_queued_command(
                RuntimeCommand::SetWorkspaceState(WorkspaceRuntimeState::Active),
                &budget,
            )
            .is_err()
        );
        drop(first);
        let recovered = prepare_queued_command(
            RuntimeCommand::SetWorkspaceState(WorkspaceRuntimeState::Active),
            &budget,
        )
        .unwrap();
        drop(second);
        drop(recovered);
        assert_eq!(budget.retained_bytes(), 0);

        let received = prepare_queued_command(
            RuntimeCommand::SetWorkspaceState(WorkspaceRuntimeState::Warm),
            &budget,
        )
        .unwrap();
        assert!(budget.retained_bytes() > 0);
        let command = received.into_command();
        assert_eq!(budget.retained_bytes(), 0);
        drop(command);
    }

    #[test]
    fn every_runtime_sender_uses_shared_preparation_and_nonclone_byte_reservation() {
        let production = include_str!("in_process.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        assert_eq!(
            production.matches("prepare_queued_command(command").count(),
            3
        );
        assert!(!production.contains("try_send(command)"));
        assert!(!production.contains("#[derive(Clone)]\nstruct QueuedRuntimeCommand"));
        assert!(production.contains("command.into_command()"));
        let queue_preparation = production
            .split("fn prepare_queued_command")
            .nth(1)
            .unwrap()
            .split("struct Worker")
            .next()
            .unwrap();
        assert!(
            queue_preparation
                .contains("prepare_runtime_command_for_retention_internal(&mut command)")
        );
        let worker_handler = production.split("fn handle_command").nth(1).unwrap();
        let worker_handler_prefix = worker_handler.split("match command").next().unwrap();
        assert!(
            worker_handler_prefix
                .contains("prepare_runtime_command_for_retention_internal(&mut command)")
        );
        assert!(!worker_handler_prefix.contains("validate_host_command"));
        assert!(!worker_handler_prefix.contains("canonicalize_host_command"));
    }

    #[test]
    fn command_sink_rejects_invalid_agent_before_queue_or_secret_resolution() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: Some("runtime-secret-value".to_owned()),
        });
        let mut client = InProcessRuntimeClient::try_new_with_resolver(
            5,
            resolver.clone(),
            test_logs_root("invalid-command-sink"),
            RedactionService::new(),
            None,
            None,
            Vec::new(),
        )
        .unwrap();
        let mut invalid = correlated_agent_command();
        let RuntimeCommand::SpawnAgent { command, .. } = &mut invalid else {
            unreachable!()
        };
        *command = "x".repeat(32 * 1024 + 1);

        client.command_sink().unwrap()(invalid);
        std::thread::sleep(Duration::from_millis(25));

        assert!(resolver.calls.lock().unwrap().is_empty());
        assert_eq!(client.command_budget.retained_bytes(), 0);
        client.shutdown();
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
            1_000,
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
    fn freeze_resume는_결과_이벤트를_회신한다() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("freeze"),
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
            .send_command(RuntimeCommand::FreezeSession { session })
            .unwrap();
        // Probe.seen은 누적되므로 각 대기는 기대 frozen 값을 정확히 매칭한다.
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SessionFreezeChanged {
                session: s,
                frozen: true,
            } if *s == session => Some(()),
            _ => None,
        });
        client
            .send_command(RuntimeCommand::ResumeSession { session })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SessionFreezeChanged {
                session: s,
                frozen: false,
            } if *s == session => Some(()),
            _ => None,
        });
        // 이미 종료된/부재 세션에 보내면 무해히 무시된다(이벤트 없음) — kill로 정리.
        client
            .send_command(RuntimeCommand::KillSession { session })
            .unwrap();
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

    /// 셸 통합 1단계: 출력에 OSC 133;A 마크 2개를 심으면 ScrollToPrompt가
    /// 기존 Scroll과 같은 경로(dirty → Viewport)로 프롬프트 사이를 오간다.
    #[test]
    #[cfg(unix)]
    fn scroll_to_prompt는_프롬프트_마크_사이를_오간다() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("prompt-jump"),
            RedactionService::new(),
            spec(
                "/bin/sh",
                &[
                    "-c",
                    concat!(
                        "printf '\\033]133;A\\007prompt-1\\n'; ",
                        "i=0; while [ $i -lt 40 ]; do echo fill-$i; i=$((i+1)); done; ",
                        "printf '\\033]133;A\\007prompt-2\\n'; sleep 30"
                    ),
                ],
            ),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 5,
                scrollback_lines: 1000,
            })
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        // 출력이 모두 반영될 때까지 — 마지막 마크 뒤 텍스트가 화면에 보인다.
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport {
                session: s,
                snapshot,
                ..
            } if *s == session
                && (0..5).any(|row| snapshot_text(snapshot, row).contains("prompt-2")) =>
            {
                Some(())
            }
            _ => None,
        });

        // 이전 프롬프트(prompt-1, 첫 라인)로 점프 → 스크롤된 Viewport가 흐른다.
        probe.seen.clear();
        client
            .send_command(RuntimeCommand::ScrollToPrompt {
                session,
                direction: -1,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport {
                session: s,
                snapshot,
                ..
            } if *s == session
                && snapshot.scroll_offset > 0
                && snapshot_text(snapshot, 0).contains("prompt-1") =>
            {
                Some(())
            }
            _ => None,
        });

        // 다음 프롬프트(prompt-2)로 — 맨 아래 복귀.
        probe.seen.clear();
        client
            .send_command(RuntimeCommand::ScrollToPrompt {
                session,
                direction: 1,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport {
                session: s,
                snapshot,
                ..
            } if *s == session && snapshot.scroll_offset == 0 => Some(()),
            _ => None,
        });
    }

    /// 셸 통합 2단계: C/D 마크를 심은 세션에 ExtractLastOutput을 보내면
    /// LastOutputExtracted로 C~D 범위 텍스트가 돌아온다 (ScrollToPrompt 통합 테스트 관례).
    #[test]
    #[cfg(unix)]
    fn extract_last_output는_마지막_명령_출력을_돌려준다() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("last-output"),
            RedactionService::new(),
            spec(
                "/bin/sh",
                &[
                    "-c",
                    concat!(
                        "printf '\\033]133;A\\007$ cmd\\n\\033]133;C\\007'; ",
                        "echo out-1; echo out-2; ",
                        "printf '\\033]133;D;0\\007\\033]133;A\\007ready\\n'; sleep 30"
                    ),
                ],
            ),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 5,
                scrollback_lines: 1000,
            })
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        // 마지막 마크 뒤 텍스트까지 반영된 뒤에 추출한다.
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport {
                session: s,
                snapshot,
                ..
            } if *s == session
                && (0..5).any(|row| snapshot_text(snapshot, row).contains("ready")) =>
            {
                Some(())
            }
            _ => None,
        });

        client
            .send_command(RuntimeCommand::ExtractLastOutput { session })
            .unwrap();
        let (text, truncated) = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::LastOutputExtracted {
                session: s,
                text,
                truncated,
            } if *s == session => Some((text.clone(), *truncated)),
            _ => None,
        });
        assert_eq!(text, "out-1\nout-2");
        assert!(!truncated);
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
        const MARKER: &str = "HOSTILE_SPAWN_PATH_COMMAND_MARKER";
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("nofail"),
            RedactionService::new(),
            spec(MARKER, &[]),
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
        let message = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SpawnFailed {
                kind: SpawnKind::Shell,
                message,
            } => Some(message.clone()),
            _ => None,
        });
        assert_eq!(message.message_id, "runtime.spawn_failed.shell");
        assert_eq!(message.arg_value("error_code"), Some("pty_spawn_failed"));
        assert_eq!(message.diagnostic.as_deref(), Some("pty_spawn_failed"));
        assert!(!format!("{message:?}").contains(MARKER));
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
    #[cfg(unix)]
    fn spawn_agent는_workspace_기본_env를_병합하고_launch값을_우선한다() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("agent-default-env"),
            RedactionService::new(),
            pty::default_shell(),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SetSessionDefaultEnv {
                env_plain: vec![
                    ("WORKSPACE_ONLY".into(), "workspace".into()),
                    ("ENV_PRIORITY".into(), "workspace".into()),
                ],
                env_secrets: Vec::new(),
            })
            .unwrap();
        client
            .send_command(RuntimeCommand::SpawnAgent {
                agent_config_id: Some("agent-default-env".into()),
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "echo W=$WORKSPACE_ONLY P=$ENV_PRIORITY".into()],
                env_plain: vec![("ENV_PRIORITY".into(), "launch".into())],
                env_secrets: Vec::new(),
                waiting_regex: None,
                approval_regex: None,
                error_regex: None,
                done_regex: None,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::Viewport { snapshot, .. }
                if snapshot_text(snapshot, 0).contains("W=workspace P=launch") =>
            {
                Some(())
            }
            _ => None,
        });
    }

    #[test]
    #[cfg(unix)]
    fn factory_resolves_complete_secret_set_before_spawn_and_fails_closed_on_redaction_rejection() {
        let resolver = Arc::new(CompleteSetResolver {
            calls: Mutex::new(Vec::new()),
            unsafe_id: Some("logical-too-short"),
        });
        let redaction = RedactionService::new();
        let factory = crate::InProcessRuntimeHostFactory::new(resolver.clone(), redaction.clone());
        let mut host = crate::RuntimeHostFactory::create(
            &factory,
            crate::RuntimeHostConfig {
                output_batch_ms: 5,
                logs_root: test_logs_root("factory-secret-set-fail-closed"),
                persist: None,
                cwd: None,
                extra_env: Vec::new(),
            },
        )
        .unwrap();
        let mut probe = Probe::new(host.subscribe_with_wake(Arc::new(|| {})));

        host.submit(RuntimeCommand::SpawnAgent {
            agent_config_id: None,
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
            command: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), "sleep 30".to_owned()],
            env_plain: Vec::new(),
            env_secrets: vec![
                ("SAFE".to_owned(), "logical-safe".to_owned()),
                ("UNSAFE".to_owned(), "logical-too-short".to_owned()),
            ],
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        })
        .unwrap();

        let message = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::SpawnFailed {
                kind: SpawnKind::Agent,
                message,
            } => Some(message.clone()),
            _ => None,
        });
        assert_eq!(message.message_id, "runtime.spawn_failed.agent_secret");
        assert!(message.args.is_empty());
        assert!(message.diagnostic.is_none());
        assert_eq!(
            *resolver.calls.lock().expect("resolver calls lock"),
            vec!["logical-safe", "logical-too-short"]
        );
        assert_eq!(redaction.corpus_stats().active_leases, 0);
        assert!(
            !probe
                .seen
                .iter()
                .any(|event| matches!(event, RuntimeEvent::AgentSpawned { .. }))
        );
        host.shutdown();
    }

    #[test]
    #[cfg(unix)]
    fn factory_retains_checked_redaction_lease_for_session_lifetime() {
        let resolver = Arc::new(CompleteSetResolver {
            calls: Mutex::new(Vec::new()),
            unsafe_id: None,
        });
        let redaction = RedactionService::new();
        let factory = crate::InProcessRuntimeHostFactory::new(resolver, redaction.clone());
        let mut host = crate::RuntimeHostFactory::create(
            &factory,
            crate::RuntimeHostConfig {
                output_batch_ms: 5,
                logs_root: test_logs_root("factory-secret-session-lease"),
                persist: None,
                cwd: None,
                extra_env: Vec::new(),
            },
        )
        .unwrap();
        let mut probe = Probe::new(host.subscribe_with_wake(Arc::new(|| {})));

        host.submit(RuntimeCommand::SpawnAgent {
            agent_config_id: None,
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
            command: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), "sleep 30".to_owned()],
            env_plain: Vec::new(),
            env_secrets: vec![
                ("ONE".to_owned(), "logical-one".to_owned()),
                ("TWO".to_owned(), "logical-two".to_owned()),
            ],
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        })
        .unwrap();
        let session = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::AgentSpawned { session } => Some(*session),
            _ => None,
        });
        assert_eq!(redaction.corpus_stats().active_leases, 1);

        host.submit(RuntimeCommand::KillSession { session })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while redaction.corpus_stats().active_leases != 0 {
            assert!(
                Instant::now() < deadline,
                "session redaction lease was not released"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        host.shutdown();
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
        // resolve 실패 → SpawnFailed. Adapter error/credential coordinates are
        // deliberately absent because a physical keyring slot may be present.
        let message = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SpawnFailed { message, .. } => Some(message.clone()),
            _ => None,
        });
        assert_eq!(message.message_id, "runtime.spawn_failed.agent_secret");
        assert!(message.args.is_empty());
        assert!(message.diagnostic.is_none());
        assert!(
            !format!("{message:?}").contains("누출되면 안 됨")
                && !format!("{message:?}").contains("cred-없음"),
            "failed spawn payload must not include command args or credential coordinates"
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
    fn valid_spawn_agent_emits_legacy_then_exact_correlation_once() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("agent-correlation-success"),
            RedactionService::new(),
            pty::default_shell(),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnAgent {
                agent_config_id: Some("agent-cfg-success".to_owned()),
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                command: "/bin/sh".to_owned(),
                args: vec!["-c".to_owned(), "sleep 1".to_owned()],
                env_plain: Vec::new(),
                env_secrets: Vec::new(),
                waiting_regex: None,
                approval_regex: None,
                error_regex: None,
                done_regex: None,
            })
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::AgentSpawnResolved {
                agent_config_id,
                session: Some(session),
            } if agent_config_id.as_str() == "agent-cfg-success" => Some(*session),
            _ => None,
        });

        let correlations: Vec<_> = probe
            .seen
            .iter()
            .enumerate()
            .filter_map(|(index, event)| {
                matches!(event, RuntimeEvent::AgentSpawnResolved { .. }).then_some(index)
            })
            .collect();
        assert_eq!(correlations.len(), 1);
        assert!(matches!(
            probe.seen.get(correlations[0].saturating_sub(1)),
            Some(RuntimeEvent::AgentSpawned { session: spawned }) if *spawned == session
        ));
    }

    #[test]
    #[cfg(unix)]
    fn valid_spawn_failure_emits_legacy_then_exact_correlation_once() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let client = InProcessRuntimeClient::try_with_shell_and_resolver(
            5,
            resolver.clone(),
            test_logs_root("agent-correlation-failure"),
            RedactionService::new(),
            pty::default_shell(),
            None,
        )
        .unwrap();
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnAgent {
                agent_config_id: Some("agent-cfg-failure".to_owned()),
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                command: "/bin/echo".to_owned(),
                args: Vec::new(),
                env_plain: Vec::new(),
                env_secrets: vec![("SECRET".to_owned(), "logical-slot".to_owned())],
                waiting_regex: None,
                approval_regex: None,
                error_regex: None,
                done_regex: None,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::AgentSpawnResolved {
                agent_config_id,
                session: None,
            } if agent_config_id.as_str() == "agent-cfg-failure" => Some(()),
            _ => None,
        });

        let correlations: Vec<_> = probe
            .seen
            .iter()
            .enumerate()
            .filter_map(|(index, event)| {
                matches!(event, RuntimeEvent::AgentSpawnResolved { .. }).then_some(index)
            })
            .collect();
        assert_eq!(correlations.len(), 1);
        assert!(matches!(
            probe.seen.get(correlations[0].saturating_sub(1)),
            Some(RuntimeEvent::SpawnFailed {
                kind: SpawnKind::Agent,
                ..
            })
        ));
        let failure_message = probe.seen.iter().find_map(|event| match event {
            RuntimeEvent::SpawnFailed { message, .. } => Some(message),
            _ => None,
        });
        assert!(failure_message.is_some_and(|message| {
            message.args.is_empty()
                && message.diagnostic.is_none()
                && !format!("{message:?}").contains("physical-slot-never-leak")
                && !format!("{message:?}").contains("logical-slot")
        }));
        assert_eq!(
            *resolver.calls.lock().expect("resolver calls lock"),
            vec!["logical-slot"]
        );
        assert!(
            !probe
                .seen
                .iter()
                .any(|event| matches!(event, RuntimeEvent::AgentSpawned { .. }))
        );
    }

    #[test]
    fn invalid_legacy_spawn_is_rejected_before_enqueue_or_secret_resolution() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: Some("must-not-resolve".to_owned()),
        });
        let client = InProcessRuntimeClient::try_with_shell_and_resolver(
            5,
            resolver.clone(),
            test_logs_root("agent-invalid-correlation"),
            RedactionService::new(),
            pty::default_shell(),
            None,
        )
        .unwrap();
        let events = client.subscribe();
        let error = client
            .send_command(RuntimeCommand::SpawnAgent {
                agent_config_id: Some(String::new()),
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                command: "/bin/echo".to_owned(),
                args: Vec::new(),
                env_plain: Vec::new(),
                env_secrets: vec![("SECRET".to_owned(), "logical-slot".to_owned())],
                waiting_regex: None,
                approval_regex: None,
                error_regex: None,
                done_regex: None,
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "runtime_command_spawn_agent_invalid");
        assert!(
            resolver
                .calls
                .lock()
                .expect("resolver calls lock")
                .is_empty()
        );
        assert!(!events.drain().iter().any(|event| matches!(
            event,
            RuntimeEvent::AgentSpawned { .. } | RuntimeEvent::AgentSpawnResolved { .. }
        )));
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
    fn oversized_input_is_rejected_before_the_pty_queue() {
        let mut client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("input-pressure"),
            RedactionService::new(),
            pty::default_shell(),
            None,
        );
        assert!(
            client
                .send_command(RuntimeCommand::WriteInput {
                    session: SessionId(1),
                    bytes: vec![b'x'; pty::PtyInputQueuePolicy::default().max_bytes + 1],
                })
                .is_err()
        );
        assert_eq!(client.command_budget.retained_bytes(), 0);
        client.shutdown();
    }

    #[test]
    #[cfg(unix)]
    fn input_pressure_events_are_coalesced_per_session() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("input-pressure-coalesce"),
            RedactionService::new(),
            spec("/bin/sh", &["-c", "sleep 30"]),
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
        let saturated = vec![b'x'; pty::PtyInputQueuePolicy::default().max_bytes];
        client
            .send_command(RuntimeCommand::WriteInput {
                session,
                bytes: saturated.clone(),
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        for _ in 0..8 {
            client
                .send_command(RuntimeCommand::WriteInput {
                    session,
                    bytes: saturated.clone(),
                })
                .unwrap();
            std::thread::sleep(Duration::from_millis(20));
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

    /// hook이 보고한 턴 시작은 입력과 동등한 리셋 신호다 — latch된 결과 상태(여기선
    /// error regex 오탐)가 그 pane에 직접 타이핑할 때까지 남던 문제(백로그 2).
    #[test]
    #[cfg(unix)]
    fn note_turn_start는_latch된_error를_해제한다() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("status-turn-start"),
            RedactionService::new(),
            pty::default_shell(),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        // FATAL 뒤에 무매치 라인을 화면 꼬리만큼 밀어 넣어 화면 재매치를 배제한다 —
        // 남는 Error는 stream latch뿐이다.
        client
            .send_command(RuntimeCommand::SpawnAgent {
                agent_config_id: None,
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                command: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "echo FATAL; for i in 1 2 3 4 5 6; do echo line$i; done; sleep 30".into(),
                ],
                env_plain: Vec::new(),
                env_secrets: Vec::new(),
                waiting_regex: None,
                approval_regex: None,
                error_regex: Some("FATAL".into()),
                done_regex: None,
            })
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::SessionStatusChanged {
                session,
                status: session::SessionStatus::Error,
            } => Some(*session),
            _ => None,
        });
        probe.seen.clear();
        client
            .send_command(RuntimeCommand::NoteTurnStart { session })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::SessionStatusChanged {
                session: changed,
                status: session::SessionStatus::Running,
            } if *changed == session => Some(()),
            _ => None,
        });
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
    /// 비상 플러시(로드맵 C2)가 persist 파이프 유무와 무관하게 워커를 막거나
    /// 죽이지 않는다 — 플러시 자체의 커밋 정확성은 persistence.rs 단위 테스트 소관.
    #[cfg(unix)]
    #[test]
    fn emergency_persist_flush는_워커를_막지_않는다() {
        init_mock_store();
        let dir = std::env::temp_dir().join(format!("deppy-rtemflush-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("metadata.sqlite3");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE workspaces (id TEXT PRIMARY KEY, name TEXT, path TEXT,
                     created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
                 CREATE TABLE agent_configs (id TEXT PRIMARY KEY);
                 INSERT INTO workspaces (id) VALUES ('ws-emflush');",
            )
            .unwrap();
            conn.execute_batch(persist::MIGRATION_SQL).unwrap();
        }
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("emergency-flush"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path,
                workspace_id: "ws-emflush".into(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::EmergencyPersistFlush)
            .unwrap();
        // 플러시 뒤에도 워커가 정상 동작한다 — 후속 spawn이 처리되어야 한다.
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
    }

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

    /// A1 리뷰 P2: 증분 캐시가 전체 디렉터리 스캔 없이 예산 초과를 정확히 감지한다.
    /// 예산 내 기록은 gc 불필요(false, 스캔 없이 증분만), 초과 확정 시에만 true(전체 스캔).
    #[test]
    fn 증분_캐시는_스캔없이_예산초과만_gc를_요구한다() {
        let budget = 100u64;
        // 예산 내 — 스캔 없이 증분만 (gc 불필요)
        assert!(!archive_cache_needs_gc(50, 40, budget));
        assert!(
            !archive_cache_needs_gc(0, 100, budget),
            "경계(=예산)는 초과 아님"
        );
        assert!(!archive_cache_needs_gc(99, 1, budget));
        // 예산 초과 확정 — 이때만 gc(전체 스캔+제거) 요구
        assert!(archive_cache_needs_gc(50, 51, budget));
        assert!(archive_cache_needs_gc(budget, 1, budget));
        // 오버플로 안전 (saturating) — 초과로 판정
        assert!(archive_cache_needs_gc(u64::MAX, 1, budget));
    }

    #[test]
    fn archive_gc_failure_marks_cached_usage_unknown() {
        let (mut worker, _event_rx) = admission_worker(
            Arc::new(RecordingResolver {
                calls: Mutex::new(Vec::new()),
                value: None,
            }),
            "archive-gc-failure",
        );
        let invalid_root = worker.logs_root.join("not-a-directory");
        std::fs::create_dir_all(&worker.logs_root).unwrap();
        std::fs::write(&invalid_root, b"file").unwrap();
        worker.logs_root = invalid_root;
        worker.archive_disk_bytes = storage::scrollback_archive::ARCHIVE_DISK_BUDGET_BYTES;

        assert!(!worker.account_archive_write(1));

        assert_eq!(
            worker.archive_disk_bytes,
            storage::scrollback_archive::ARCHIVE_DISK_USAGE_UNKNOWN
        );
    }

    #[cfg(unix)]
    #[test]
    fn gc_evicted_triggering_archive_does_not_leave_disk_marker() {
        use std::os::unix::fs::PermissionsExt;

        let (mut worker, _event_rx) = admission_worker(
            Arc::new(RecordingResolver {
                calls: Mutex::new(Vec::new()),
                value: None,
            }),
            "archive-trigger-evicted",
        );
        std::fs::create_dir_all(&worker.logs_root).unwrap();
        let old_path = worker.logs_root.join("old").join("scrollback.zlib");
        std::fs::create_dir_all(old_path.parent().unwrap()).unwrap();
        let old_file = std::fs::File::create(&old_path).unwrap();
        old_file
            .set_len(storage::scrollback_archive::ARCHIVE_DISK_BUDGET_BYTES)
            .unwrap();
        old_file
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000))
            .unwrap();
        std::fs::set_permissions(
            old_path.parent().unwrap(),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        let key = "new";
        let written_len = storage::scrollback_archive::write(
            &worker.logs_root,
            key,
            &storage::scrollback_archive::ArchiveMeta {
                kind: 1,
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                exit_code: Some(0),
            },
            b"new archive",
        )
        .unwrap();
        worker.archive_disk_bytes = storage::scrollback_archive::ARCHIVE_DISK_BUDGET_BYTES;
        let session = SessionId(99);

        worker.finish_archive_write(session, key, written_len);

        std::fs::set_permissions(
            old_path.parent().unwrap(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(!storage::scrollback_archive::exists(&worker.logs_root, key));
        assert!(
            !worker.archived_on_disk.contains(&session),
            "a GC-evicted triggering archive must not leave a false disk marker"
        );
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

    #[cfg(unix)]
    #[test]
    fn unknown_archive_usage_blocks_new_writes_and_makes_gc_progress() {
        init_mock_store();
        let dir = unique_test_dir("archive-scan-over-limit");
        let db_path = dir.join("metadata.sqlite3");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE workspaces (id TEXT PRIMARY KEY, name TEXT, path TEXT,
                     created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
                 CREATE TABLE agent_configs (id TEXT PRIMARY KEY);
                 INSERT INTO workspaces (id) VALUES ('ws-archive-limit');",
            )
            .unwrap();
            conn.execute_batch(persist::MIGRATION_SQL).unwrap();
        }
        let logs_root = dir.join("logs");
        std::fs::create_dir_all(&logs_root).unwrap();
        for index in 0..=4_096 {
            let session_dir = logs_root.join(format!("noise-{index}"));
            std::fs::create_dir_all(&session_dir).unwrap();
            std::fs::write(session_dir.join("scrollback.zlib"), b"").unwrap();
        }
        let entries_before = std::fs::read_dir(&logs_root).unwrap().count();
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            logs_root.clone(),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path: db_path.clone(),
                workspace_id: "ws-archive-limit".into(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(spawn_agent_cmd("echo blocked-archive", None, None))
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::SessionExited { .. } => Some(()),
            _ => None,
        });
        std::thread::sleep(Duration::from_millis(200));

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let uuid: String = conn
            .query_row("SELECT id FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert!(
            !storage::scrollback_archive::exists(&logs_root, &uuid),
            "unknown disk usage must fail closed before writing"
        );
        assert!(
            std::fs::read_dir(&logs_root).unwrap().count() < entries_before,
            "failed admission must still make bounded GC progress"
        );
        drop(client);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn newly_over_limit_archive_scan_rolls_back_the_triggering_write() {
        init_mock_store();
        let dir = unique_test_dir("archive-scan-growth-over-limit");
        let db_path = dir.join("metadata.sqlite3");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE workspaces (id TEXT PRIMARY KEY, name TEXT, path TEXT,
                     created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
                 CREATE TABLE agent_configs (id TEXT PRIMARY KEY);
                 INSERT INTO workspaces (id) VALUES ('ws-archive-growth');",
            )
            .unwrap();
            conn.execute_batch(persist::MIGRATION_SQL).unwrap();
        }
        let logs_root = dir.join("logs");
        let seeded_archive = logs_root.join("seed").join("scrollback.zlib");
        std::fs::create_dir_all(seeded_archive.parent().unwrap()).unwrap();
        std::fs::File::create(&seeded_archive)
            .unwrap()
            .set_len(storage::scrollback_archive::ARCHIVE_DISK_BUDGET_BYTES)
            .unwrap();
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            logs_root.clone(),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path: db_path.clone(),
                workspace_id: "ws-archive-growth".into(),
            }),
        );
        for index in 0..=4_096 {
            let session_dir = logs_root.join(format!("noise-{index}"));
            std::fs::create_dir_all(&session_dir).unwrap();
            std::fs::write(session_dir.join("scrollback.zlib"), b"").unwrap();
        }
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(spawn_agent_cmd("echo rollback-archive", None, None))
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::SessionExited { .. } => Some(()),
            _ => None,
        });
        std::thread::sleep(Duration::from_millis(200));

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let uuid: String = conn
            .query_row("SELECT id FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert!(
            !storage::scrollback_archive::exists(&logs_root, &uuid),
            "failed post-write GC must roll back the triggering archive"
        );
        drop(client);
        std::fs::remove_dir_all(dir).unwrap();
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

    #[cfg(unix)]
    #[test]
    fn archived_restore_invalid_metadata_preserves_persistent_row_for_fallback() {
        init_mock_store();
        let dir = unique_test_dir("sf03-invalid-archive");
        let db_path = dir.join("metadata.sqlite3");
        let logs_root = dir.join("logs");
        let workspace_id = "ws-sf03-invalid";
        let persistent_id = "persisted-agent-invalid";
        create_persist_db(&db_path, workspace_id);
        seed_persisted_session_pane(
            &db_path,
            workspace_id,
            persistent_id,
            "agent",
            Some("cfg-sf03"),
            persist::SESSION_STATUS_EXITED,
            "/tmp",
        );
        storage::scrollback_archive::write(
            &logs_root,
            persistent_id,
            &storage::scrollback_archive::ArchiveMeta {
                kind: 1,
                cols: 500,
                rows: 500,
                scrollback_lines: 100,
                exit_code: Some(0),
            },
            b"invalid dimensions should fall back without consuming row",
        )
        .unwrap();

        restore_workspace_from_fixture(&db_path, &logs_root, workspace_id, spec("/bin/cat", &[]));

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        let pane_session: String = conn
            .query_row("SELECT session_id FROM mux_panes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "fallback must not create a second session row");
        assert_eq!(pane_session, persistent_id);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn archived_restore_truncated_stream_preserves_persistent_row_for_fallback() {
        init_mock_store();
        let dir = unique_test_dir("sf03-truncated-archive");
        let db_path = dir.join("metadata.sqlite3");
        let logs_root = dir.join("logs");
        let workspace_id = "ws-sf03-truncated";
        let persistent_id = "persisted-agent-truncated";
        create_persist_db(&db_path, workspace_id);
        seed_persisted_session_pane(
            &db_path,
            workspace_id,
            persistent_id,
            "agent",
            Some("cfg-sf03"),
            persist::SESSION_STATUS_EXITED,
            "/tmp",
        );
        storage::scrollback_archive::write(
            &logs_root,
            persistent_id,
            &storage::scrollback_archive::ArchiveMeta {
                kind: 1,
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                exit_code: Some(0),
            },
            &vec![b'x'; 4096],
        )
        .unwrap();
        let archive = storage::scrollback_archive::archive_path(&logs_root, persistent_id).unwrap();
        let truncated_len = archive.metadata().unwrap().len().saturating_sub(4);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&archive)
            .unwrap()
            .set_len(truncated_len)
            .unwrap();

        let mux = restore_workspace_from_fixture(
            &db_path,
            &logs_root,
            workspace_id,
            spec("/bin/cat", &[]),
        );

        let restored_sessions: Vec<SessionId> = mux
            .tabs
            .iter()
            .flat_map(|tab| tab.panes.iter())
            .filter_map(|pane| pane.session_id)
            .collect();
        assert_eq!(
            restored_sessions.len(),
            1,
            "fallback must expose exactly one restored runtime session"
        );
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        let pane_session: String = conn
            .query_row("SELECT session_id FROM mux_panes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "fallback must not create a second session row");
        assert_eq!(pane_session, persistent_id);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn archived_restore_failed_rebind_leaves_no_runtime_marker() {
        let dir = unique_test_dir("sf03-rebind-fail-marker");
        let db_path = dir.join("metadata.sqlite3");
        let logs_root = dir.join("logs");
        let workspace_id = "ws-sf03-rebind-fail";
        let persistent_id = "persisted-agent-rebind-fail";
        create_persist_db(&db_path, workspace_id);
        seed_persisted_session_pane(
            &db_path,
            workspace_id,
            persistent_id,
            "agent",
            Some("cfg-sf03"),
            persist::SESSION_STATUS_EXITED,
            "/tmp",
        );
        storage::scrollback_archive::write(
            &logs_root,
            persistent_id,
            &storage::scrollback_archive::ArchiveMeta {
                kind: 1,
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                exit_code: Some(0),
            },
            b"valid archive prepared before failed rebind",
        )
        .unwrap();

        let (mut worker, _event_rx) = admission_worker(
            Arc::new(RecordingResolver {
                calls: Mutex::new(Vec::new()),
                value: None,
            }),
            "sf03-rebind-fail-marker",
        );
        worker.logs_root = logs_root.clone();
        worker.persist = Some(
            crate::persistence::PersistPipe::open(&crate::persistence::PersistConfig {
                db_path: db_path.clone(),
                workspace_id: workspace_id.to_owned(),
            })
            .unwrap(),
        );
        assert!(
            worker
                .persist
                .as_mut()
                .unwrap()
                .session_rebound_archived(SessionId(99), persistent_id),
            "test setup must consume the restored persistence row"
        );
        let pane_state = persist::PaneState {
            id: MuxPaneId::new(),
            session_id: Some(persistent_id.to_owned()),
            title: "restored".to_owned(),
            pane_kind: mux::PaneKind::Terminal,
            cwd: Some("/tmp".to_owned()),
        };

        assert!(!worker.restore_archived_pane(&pane_state, persistent_id));
        let failed_session = SessionId(1);
        assert!(!worker.sessions.contains_key(&failed_session));
        assert!(!worker.exited_order.contains(&failed_session));
        assert!(!worker.mux.panes.contains_key(&pane_state.id));
        assert!(
            !worker.archived_on_disk.contains(&failed_session),
            "failed rebind must not leave a disk archive marker without a session"
        );
        std::fs::remove_dir_all(&dir).ok();
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

    #[cfg(unix)]
    #[test]
    fn restore_cwd_uses_persisted_session_cwd_for_shell_spawn() {
        init_mock_store();
        let dir = unique_test_dir("sf03-restore-cwd");
        let cwd = dir.join("cwd-target");
        std::fs::create_dir_all(&cwd).unwrap();
        let db_path = dir.join("metadata.sqlite3");
        let logs_root = dir.join("logs");
        let workspace_id = "ws-sf03-cwd";
        let persistent_id = "persisted-shell-cwd";
        let cwd_text = cwd.to_string_lossy().into_owned();
        create_persist_db(&db_path, workspace_id);
        seed_persisted_session_pane(
            &db_path,
            workspace_id,
            persistent_id,
            "shell",
            None,
            persist::SESSION_STATUS_RUNNING,
            &cwd_text,
        );

        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            logs_root,
            RedactionService::new(),
            spec("/bin/pwd", &[]),
            Some(crate::persistence::PersistConfig {
                db_path,
                workspace_id: workspace_id.to_owned(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::RestoreWorkspace)
            .unwrap();
        let text = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::Viewport { snapshot, .. } => {
                let text = snapshot
                    .visible_cells
                    .iter()
                    .filter(|cell| !cell.wide_spacer)
                    .map(|cell| cell.c)
                    .collect::<String>();
                text.contains(&cwd_text).then_some(text)
            }
            _ => None,
        });
        assert!(text.contains(&cwd_text), "{text}");
        drop(client);
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

    /// 스냅샷 전체 행에서 문자열을 찾는다 — 입력 echo가 행을 넘겨도 매칭되게.
    fn snapshot_contains(snapshot: &terminal::TerminalViewportSnapshot, needle: &str) -> bool {
        (0..snapshot.rows as usize).any(|row| snapshot_text(snapshot, row).contains(needle))
    }

    /// P5a: 원격 시청 lease가 hidden tab 세션의 Viewport를 흐르게 하고,
    /// 해제(viewing=false) 즉시 다시 멈춘다 (§14.4 union 게이트).
    #[cfg(unix)]
    #[test]
    fn 원격_시청_lease는_hidden_세션_viewport를_흐르게_하고_해제시_멈춘다() {
        init_mock_store();
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("remote-view"),
            RedactionService::new(),
            spec("/bin/sh", &["-c", "sleep 30"]),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        // A spawn (tab 1) → B spawn (tab 2 활성 → A hidden)
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let session_a = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.len() == 2 => Some(()),
            _ => None,
        });
        // 기준선: hidden A에 입력(tty echo로 화면 변화) → Viewport가 나오면 안 된다
        client
            .send_command(RuntimeCommand::WriteInput {
                session: session_a,
                bytes: b"hiddenwrite\r".to_vec(),
            })
            .unwrap();
        let until = Instant::now() + Duration::from_millis(800);
        while Instant::now() < until {
            probe.seen.extend(probe.rx.drain());
            std::thread::sleep(Duration::from_millis(20));
        }
        let hidden_leak = probe.seen.iter().any(|e| {
            matches!(e, RuntimeEvent::Viewport { session, snapshot, .. }
                if *session == session_a && snapshot_contains(snapshot, "hiddenwrite"))
        });
        assert!(!hidden_leak, "hidden 세션 Viewport가 lease 없이 생성됨");

        // lease 시작 → 즉시 현재 화면 push (다음 출력을 기다리지 않는다)
        client
            .send_command(RuntimeCommand::SetRemoteViewing {
                session: session_a,
                viewing: true,
                ttl_ms: 60_000,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport {
                session, snapshot, ..
            } if *session == session_a && snapshot_contains(snapshot, "hiddenwrite") => Some(()),
            _ => None,
        });
        // 시청 중 새 출력 → pump 경로로 Viewport 흐름
        client
            .send_command(RuntimeCommand::WriteInput {
                session: session_a,
                bytes: b"livewrite\r".to_vec(),
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport {
                session, snapshot, ..
            } if *session == session_a && snapshot_contains(snapshot, "livewrite") => Some(()),
            _ => None,
        });

        // 해제 → 이후 출력은 다시 차단 (tombstone — trailing 승격 없음)
        client
            .send_command(RuntimeCommand::SetRemoteViewing {
                session: session_a,
                viewing: false,
                ttl_ms: 0,
            })
            .unwrap();
        client
            .send_command(RuntimeCommand::WriteInput {
                session: session_a,
                bytes: b"afterstop\r".to_vec(),
            })
            .unwrap();
        let until = Instant::now() + Duration::from_millis(800);
        while Instant::now() < until {
            probe.seen.extend(probe.rx.drain());
            std::thread::sleep(Duration::from_millis(20));
        }
        let after_stop_leak = probe.seen.iter().any(|e| {
            matches!(e, RuntimeEvent::Viewport { session, snapshot, .. }
                if *session == session_a && snapshot_contains(snapshot, "afterstop"))
        });
        assert!(!after_stop_leak, "lease 해제 후에도 Viewport가 생성됨");
    }

    /// P5a: Warm(§14.1)에서도 lease 세션은 스냅샷을 생성하고, TTL 만료로
    /// 갱신이 끊기면 자동 원복된다 (WS 절단·브리지 사망 백스톱).
    #[cfg(unix)]
    #[test]
    fn 원격_시청은_warm에서도_생성되고_ttl_만료로_중단된다() {
        init_mock_store();
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("remote-view-warm"),
            RedactionService::new(),
            spec("/bin/sh", &["-c", "printf WARMRV; sleep 30"]),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
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
        let session_a = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        // Warm이라 Viewport 0 (기존 warm 테스트가 보장) — lease 시작 즉시 push된다
        client
            .send_command(RuntimeCommand::SetRemoteViewing {
                session: session_a,
                viewing: true,
                ttl_ms: 900,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport {
                session, snapshot, ..
            } if *session == session_a && snapshot_contains(snapshot, "WARMRV") => Some(()),
            _ => None,
        });
        // TTL(900ms) 경과 → lease 자동 원복 → 이후 출력은 차단
        std::thread::sleep(Duration::from_millis(1200));
        client
            .send_command(RuntimeCommand::WriteInput {
                session: session_a,
                bytes: b"afterexpiry\r".to_vec(),
            })
            .unwrap();
        let until = Instant::now() + Duration::from_millis(800);
        while Instant::now() < until {
            probe.seen.extend(probe.rx.drain());
            std::thread::sleep(Duration::from_millis(20));
        }
        let expired_leak = probe.seen.iter().any(|e| {
            matches!(e, RuntimeEvent::Viewport { session, snapshot, .. }
                if *session == session_a && snapshot_contains(snapshot, "afterexpiry"))
        });
        assert!(
            !expired_leak,
            "TTL 만료 후에도 Viewport가 생성됨 — lease 백스톱 회귀"
        );
    }

    /// P5a: kill된 세션의 lease는 정리되고, 죽은 세션 id로 온 stale lease 커맨드는
    /// 무시된다 (유령 lease 방지).
    #[cfg(unix)]
    #[test]
    fn kill된_세션의_lease는_정리되고_stale_커맨드는_무시된다() {
        init_mock_store();
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("remote-view-kill"),
            RedactionService::new(),
            spec("/bin/sh", &["-c", "sleep 30"]),
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
        let session_a = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        client
            .send_command(RuntimeCommand::SetRemoteViewing {
                session: session_a,
                viewing: true,
                ttl_ms: 60_000,
            })
            .unwrap();
        // spawn의 MuxUpdated(A attach)가 먼저 관측된 뒤 kill을 보낸다 — 이후
        // "A가 detach된 MuxUpdated"가 kill 처리 완료의 신호가 된다.
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| tab.panes.iter())
                    .any(|pane| pane.session_id == Some(session_a)) =>
            {
                Some(())
            }
            _ => None,
        });
        client
            .send_command(RuntimeCommand::KillSession { session: session_a })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| tab.panes.iter())
                    .all(|pane| pane.session_id != Some(session_a)) =>
            {
                Some(())
            }
            _ => None,
        });
        // kill 이후 stale lease 커맨드 — 무시되어야 하고 Viewport도 없어야 한다
        probe.seen.clear();
        client
            .send_command(RuntimeCommand::SetRemoteViewing {
                session: session_a,
                viewing: true,
                ttl_ms: 60_000,
            })
            .unwrap();
        let until = Instant::now() + Duration::from_millis(600);
        while Instant::now() < until {
            probe.seen.extend(probe.rx.drain());
            std::thread::sleep(Duration::from_millis(20));
        }
        let ghost = probe
            .seen
            .iter()
            .any(|e| matches!(e, RuntimeEvent::Viewport { session, .. } if *session == session_a));
        assert!(!ghost, "kill된 세션에 유령 lease Viewport가 생성됨");
    }

    /// P5 리뷰 P1: command_sink 클론(웹 브리지)이 채널을 살려둬도 shutdown이
    /// join까지 완료된다 — 앱 종료 데드락 회귀 방지.
    #[cfg(unix)]
    #[test]
    fn command_sink이_살아있어도_shutdown이_완료된다() {
        init_mock_store();
        let mut client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("sink-shutdown"),
            RedactionService::new(),
            spec("/bin/sh", &["-c", "sleep 30"]),
            None,
        );
        let sink = client.command_sink().expect("command_sink");
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let joiner = std::thread::spawn(move || {
            client.shutdown();
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(15))
            .expect("shutdown 미완료 — command_sink 클론이 worker join을 막음 (P5 리뷰 P1)");
        joiner.join().unwrap();
        drop(sink); // sink는 shutdown 완료 시점까지 살아 있었다 — 그게 이 테스트의 조건
    }

    /// P5 리뷰 P1: 원격 전용 Viewport(Warm/hidden lease 스냅샷)는 render_bound
    /// 구독자(GUI=repaint)를 깨우지 않는다 — 시청이 데스크톱 repaint를 유발하면 안 됨.
    /// 백그라운드 구독자(웹 브리지)는 프레임 라우팅을 위해 깨어나야 한다.
    #[cfg(unix)]
    #[test]
    fn 원격_전용_viewport는_render_bound_구독자를_깨우지_않는다() {
        use std::sync::atomic::{AtomicU64, Ordering};
        init_mock_store();
        // 연속 출력 세션 — Warm + lease면 모든 Viewport가 원격 전용이다.
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("wake-gate"),
            RedactionService::new(),
            spec("/bin/sh", &["-c", "while :; do printf x; sleep 0.05; done"]),
            None,
        );
        let gui_wakes = Arc::new(AtomicU64::new(0));
        let bg_wakes = Arc::new(AtomicU64::new(0));
        let gui_counter = Arc::clone(&gui_wakes);
        let bg_counter = Arc::clone(&bg_wakes);
        let _gui_rx = client.subscribe_with_wake(Arc::new(move || {
            gui_counter.fetch_add(1, Ordering::SeqCst);
        }));
        let _bg_rx = client.subscribe_with_wake_background(Arc::new(move || {
            bg_counter.fetch_add(1, Ordering::SeqCst);
        }));
        let mut probe = Probe::new(client.subscribe());
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
        let session = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        client
            .send_command(RuntimeCommand::SetRemoteViewing {
                session,
                viewing: true,
                ttl_ms: 60_000,
            })
            .unwrap();
        // 스트림이 흐르기 시작한 것 확인 후 측정 창 — 스폰기 상태 이벤트 노이즈를 배제
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport { session: s, .. } if *s == session => Some(()),
            _ => None,
        });
        std::thread::sleep(Duration::from_millis(300));
        gui_wakes.store(0, Ordering::SeqCst);
        bg_wakes.store(0, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(800));
        let gui = gui_wakes.load(Ordering::SeqCst);
        let bg = bg_wakes.load(Ordering::SeqCst);
        assert!(
            bg >= 5,
            "백그라운드 구독자가 원격 viewport에 깨어나지 않음 (bg={bg})"
        );
        // ResourceUsage(~2s 주기) 등 비-viewport wake 1~2회는 허용 — viewport로 인한
        // 연속 wake(수십 회)만 없으면 된다.
        assert!(
            gui <= 2,
            "render_bound 구독자가 원격 전용 viewport에 깨어남 (gui={gui}, bg={bg}) — Warm repaint 회귀 (P5 리뷰 P1)"
        );
    }

    /// 스크롤백 열람: Scroll 커맨드의 dirty가 union 게이트를 타고 lease 세션
    /// (Warm/hidden)의 Viewport로 흐른다 — 폰 스크롤의 runtime 경로 고정.
    #[cfg(unix)]
    #[test]
    fn scroll은_lease_세션의_viewport를_흐르게_한다() {
        init_mock_store();
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("lease-scroll"),
            RedactionService::new(),
            spec("/bin/sh", &["-c", "sleep 30"]),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
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
        let session = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        client
            .send_command(RuntimeCommand::SetRemoteViewing {
                session,
                viewing: true,
                ttl_ms: 60_000,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport { session: s, .. } if *s == session => Some(()),
            _ => None,
        });
        // 스크롤 → mark_full_dirty → 다음 pump이 union 게이트로 Viewport를 emit한다
        probe.seen.clear();
        client
            .send_command(RuntimeCommand::Scroll { session, delta: 5 })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport { session: s, .. } if *s == session => Some(()),
            _ => None,
        });
    }

    /// P5 리뷰 P3: lease 갱신(재전송)은 만료 연장만 — 전 대상 스냅샷 재push를
    /// 유발하지 않는다 (15s마다 풀 keyframe 낭비 + Warm wake 소음 방지).
    #[cfg(unix)]
    #[test]
    fn lease_갱신은_스냅샷을_재push하지_않는다() {
        init_mock_store();
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("lease-renew"),
            RedactionService::new(),
            spec("/bin/sh", &["-c", "sleep 30"]),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
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
        let session = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        // 신규 lease → 즉시 초기 push 1회
        client
            .send_command(RuntimeCommand::SetRemoteViewing {
                session,
                viewing: true,
                ttl_ms: 60_000,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport { session: s, .. } if *s == session => Some(()),
            _ => None,
        });
        // 갱신(재전송) — 조용한 세션이라 새 출력이 없으니 Viewport도 없어야 한다
        probe.seen.clear();
        client
            .send_command(RuntimeCommand::SetRemoteViewing {
                session,
                viewing: true,
                ttl_ms: 60_000,
            })
            .unwrap();
        let until = Instant::now() + Duration::from_millis(700);
        while Instant::now() < until {
            probe.seen.extend(probe.rx.drain());
            std::thread::sleep(Duration::from_millis(20));
        }
        let renewal_push = probe
            .seen
            .iter()
            .any(|e| matches!(e, RuntimeEvent::Viewport { session: s, .. } if *s == session));
        assert!(
            !renewal_push,
            "lease 갱신이 스냅샷을 재push함 (P5 리뷰 P3 회귀)"
        );
    }

    fn live_entry(id: u64, bytes: usize, history: usize, visible: bool) -> super::LiveTrimEntry {
        super::LiveTrimEntry {
            id: SessionId(id),
            estimated_bytes: bytes,
            history_lines: history,
            visible,
        }
    }

    #[test]
    fn select_next_live_trim은_hidden을_heaviest순으로_먼저_고른다() {
        let entries = [
            live_entry(1, 50, 1000, true),  // visible, 무거움 — 최후수단이라 나중
            live_entry(2, 40, 1000, false), // hidden, heaviest → 먼저
            live_entry(3, 30, 1000, false), // hidden, 더 가벼움
        ];
        assert_eq!(
            super::select_next_live_trim(&entries, 120, 100),
            Some((SessionId(2), 500)) // 1000/2
        );
    }

    #[test]
    fn select_next_live_trim은_hidden이_전부_floor면_visible을_최후수단으로() {
        let entries = [
            live_entry(1, 50, 1000, true),
            live_entry(2, 40, super::LIVE_TRIM_FLOOR_LINES, false), // 이미 FLOOR
        ];
        assert_eq!(
            super::select_next_live_trim(&entries, 120, 100),
            Some((SessionId(1), 500))
        );
    }

    #[test]
    fn select_next_live_trim은_예산이내면_none() {
        let entries = [live_entry(1, 50, 1000, false)];
        assert_eq!(super::select_next_live_trim(&entries, 50, 100), None);
        assert_eq!(super::select_next_live_trim(&entries, 100, 100), None); // 경계 ==
    }

    #[test]
    fn select_next_live_trim은_전부_floor면_초과여도_none() {
        let entries = [
            live_entry(1, 50, super::LIVE_TRIM_FLOOR_LINES, false),
            live_entry(2, 50, super::LIVE_TRIM_FLOOR_LINES, true),
        ];
        assert_eq!(super::select_next_live_trim(&entries, 200, 100), None);
    }

    #[test]
    fn select_next_live_trim_target은_floor아래로_안내려간다() {
        // history = FLOOR+10 → /2는 FLOOR 미만 → FLOOR로 클램프
        let entries = [live_entry(1, 50, super::LIVE_TRIM_FLOOR_LINES + 10, false)];
        assert_eq!(
            super::select_next_live_trim(&entries, 120, 100),
            Some((SessionId(1), super::LIVE_TRIM_FLOOR_LINES))
        );
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
        let snapshot_len = file.metadata().unwrap().len();
        let start =
            super::seek_ansi_replay_tail(&mut file, snapshot_len, recent.len() as u64 + 8).unwrap();
        assert!(start > 0);
        let mut restored = Vec::new();
        file.read_to_end(&mut restored).unwrap();
        assert_eq!(restored, recent);
        assert!(restored.len() as u64 <= recent.len() as u64 + 8);

        // full-screen TUI처럼 LF가 없는 giant 구간도 최근 tail은 보존한다.
        std::fs::write(&path, vec![b'x'; 128]).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let snapshot_len = file.metadata().unwrap().len();
        assert_eq!(
            super::seek_ansi_replay_tail(&mut file, snapshot_len, 16).unwrap(),
            112
        );
        let mut restored = Vec::new();
        file.read_to_end(&mut restored).unwrap();
        assert_eq!(restored, vec![b'x'; 16]);

        // LF 없는 tail의 cutoff가 CSI나 UTF-8 문자 중간이어도 안전한 다음 경계부터 읽는다.
        let data = b"old\x1b[38;2;12;34;56mVISIBLE";
        std::fs::write(&path, data).unwrap();
        let requested_start = 9usize;
        let visible_start = data
            .windows(b"VISIBLE".len())
            .position(|window| window == b"VISIBLE")
            .unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let snapshot_len = file.metadata().unwrap().len();
        assert_eq!(
            super::seek_ansi_replay_tail(
                &mut file,
                snapshot_len,
                (data.len() - requested_start) as u64,
            )
            .unwrap(),
            visible_start as u64
        );
        let mut restored = Vec::new();
        file.read_to_end(&mut restored).unwrap();
        assert_eq!(restored, b"VISIBLE");

        let data = "old한글-tail".as_bytes();
        std::fs::write(&path, data).unwrap();
        let requested_start = "old".len() + 1;
        let mut file = std::fs::File::open(&path).unwrap();
        let snapshot_len = file.metadata().unwrap().len();
        let start = super::seek_ansi_replay_tail(
            &mut file,
            snapshot_len,
            (data.len() - requested_start) as u64,
        )
        .unwrap();
        assert_eq!(start, "old한".len() as u64);
        let mut restored = Vec::new();
        file.read_to_end(&mut restored).unwrap();
        assert_eq!(std::str::from_utf8(&restored).unwrap(), "글-tail");
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
