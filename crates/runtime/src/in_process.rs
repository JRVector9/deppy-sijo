//! v0 구현체 (설계문서 2.3). worker thread가 세션들을 소유한다.
//! 세션 로직(PTY+terminal+lifecycle)은 session crate 소관 (PR-08).

use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use deppy_core::{MuxPaneId, MuxTabId};
use mux::{FocusManager, MuxPane, MuxSnapshot, MuxTab, MuxWindow, PaneSnapshot, TabSnapshot};
use std::path::PathBuf;

use pty::CommandSpec;
#[cfg(test)]
use secret::SecretStore;
use secret::{RedactionLease, RedactionService, StreamRedactor};
use session::{Session, StatusDetector, StatusPatterns, agent_exit_sentinel_path};
use storage::SessionLogWriter;
use terminal::{TERMINAL_GLOBAL_CACHE_BUDGET_BYTES, TerminalCacheClass, TerminalCacheEvent};

use crate::client::{
    LOCAL_EVENT_QUEUE_CAP, RuntimeClient, RuntimeCommandSendError, RuntimeCommandSink,
    RuntimeEventReceiver, RuntimeEventStream,
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

const INPUT_REPLY_MAX_BYTES: usize = 8192;

/// Local-only output produced after one guarded input batch. No wire or disk copy.
/// The worker holds only a weak reference; the caller closes it on cancellation.
#[derive(Clone)]
pub struct InputReplyProbe(Arc<Mutex<InputReplyBuffer>>);

#[derive(Default)]
struct InputReplyBuffer {
    closed: bool,
    bytes: Vec<u8>,
}

impl InputReplyProbe {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(InputReplyBuffer::default())))
    }

    pub fn close(&self) {
        if let Ok(mut buffer) = self.0.lock() {
            buffer.closed = true;
            buffer.bytes = Vec::new();
        }
    }

    pub fn text(&self) -> String {
        self.0
            .lock()
            .ok()
            .filter(|buffer| !buffer.closed)
            .map(|buffer| String::from_utf8_lossy(&buffer.bytes).into_owned())
            .unwrap_or_default()
    }
}

impl InputReplyBuffer {
    fn append(&mut self, chunk: &[u8]) {
        if self.closed {
            return;
        }
        if chunk.len() >= INPUT_REPLY_MAX_BYTES {
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&chunk[chunk.len() - INPUT_REPLY_MAX_BYTES..]);
        } else {
            let discard = (self.bytes.len() + chunk.len()).saturating_sub(INPUT_REPLY_MAX_BYTES);
            self.bytes.drain(..discard);
            self.bytes.extend_from_slice(chunk);
        }
    }
}

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
                let (archive_disk_bytes, archive_root_identity) =
                    scan_archive_usage_snapshot(&logs_root);
                Worker {
                    command_rx,
                    subscribers: worker_subscribers,
                    batch: Duration::from_millis(output_batch_ms.max(1)),
                    shell,
                    default_env_plain: Vec::new(),
                    default_env_secrets: Vec::new(),
                    default_api_secrets: Vec::new(),
                    environment_revision: None,
                    secret_versions: Vec::new(),
                    dotenv_source: None,
                    // needsInput hook 키를 워크스페이스 스코프로 만들기 위해 workspace_id를
                    // 워커에 보관한다(SessionId는 워커마다 1부터라 전역 유일하지 않음 — codex High).
                    workspace_id: persist
                        .as_ref()
                        .map(|c| c.workspace_id.clone())
                        .unwrap_or_default(),
                    next_id: 1,
                    sessions: std::collections::HashMap::new(),
                    input_reply_probes: std::collections::HashMap::new(),
                    resize_epoch: 0,
                    resize_records: std::collections::HashMap::new(),
                    session_redaction_leases: std::collections::HashMap::new(),
                    seed_redaction_lease: None,
                    logs: std::collections::HashMap::new(),
                    detectors: std::collections::HashMap::new(),
                    agent_exit_watch: std::collections::HashMap::new(),
                    status_overrides: std::collections::HashMap::new(),
                    logs_root,
                    run_logs_root,
                    redaction,
                    secret_resolver: resolver,
                    mux: MuxState::new(),
                    tab_counter: 0,
                    persist: persist_pipe,
                    persist_db_path: persist.as_ref().map(|config| config.db_path.clone()),
                    lazy_restore: None,
                    exited_order: std::collections::VecDeque::new(),
                    max_exited_backends: DEFAULT_MAX_EXITED_BACKENDS,
                    cache_budget_bytes: TERMINAL_GLOBAL_CACHE_BUDGET_BYTES,
                    scrollback_policy: None,
                    scrollback_results: std::collections::HashMap::new(),
                    scrollback_trimmed: 0,
                    pending_scrollback_ceilings: std::collections::HashMap::new(),
                    scrollback_batching: false,
                    scrollback_ack_pending: false,
                    scrollback_restored: false,
                    archived: std::collections::HashMap::new(),
                    archived_order: std::collections::VecDeque::new(),
                    archived_on_disk: std::collections::HashMap::new(),
                    archive_failed: std::collections::HashSet::new(),
                    archive_disk_bytes,
                    archive_root_identity,
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
        self.enqueue_command(command, None)
    }
}

impl InProcessRuntimeClient {
    /// Local, correlated, bounded screen read. Does not enable GUI rendering or a remote lease.
    pub fn inspect_screen(&self, session: SessionId) -> anyhow::Result<Receiver<Option<String>>> {
        let (reply, receiver) = sync_channel(1);
        let mut queued = prepare_queued_command(
            RuntimeCommand::ExtractLastOutput { session },
            &self.command_budget,
        )?;
        queued.screen_reply = Some(reply);
        self.command_tx
            .as_ref()
            .ok_or(RuntimeCommandSendError::Disconnected)?
            .try_send(queued)
            .map_err(|error| match error {
                TrySendError::Full(_) => RuntimeCommandSendError::Backpressure,
                TrySendError::Disconnected(_) => RuntimeCommandSendError::Disconnected,
            })?;
        if let Some(worker) = &self.worker_thread {
            worker.unpark();
        }
        Ok(receiver)
    }

    pub fn send_guarded_input(
        &self,
        session: SessionId,
        operation_id: String,
        bytes: Vec<u8>,
        admission: crate::InputAdmission,
    ) -> anyhow::Result<()> {
        self.enqueue_command(
            RuntimeCommand::WriteInputTracked {
                session,
                operation_id,
                bytes,
            },
            Some(admission),
        )
    }

    /// Guard remains held through one reservation of all paste/submit parts.
    pub fn send_guarded_input_batch(
        &self,
        session: SessionId,
        operation_id: String,
        parts: Vec<Vec<u8>>,
        admission: crate::InputAdmission,
    ) -> anyhow::Result<()> {
        self.enqueue_command(
            RuntimeCommand::WriteInputBatchTracked {
                session,
                operation_id,
                parts,
            },
            Some(admission),
        )
    }

    /// Watch only fresh output after actual PTY acceptance. Requires the agent guard's
    /// bounded drain of preceding output; rejection never activates the probe.
    pub fn send_guarded_input_batch_with_reply(
        &self,
        session: SessionId,
        operation_id: String,
        parts: Vec<Vec<u8>>,
        admission: crate::InputAdmission,
    ) -> anyhow::Result<InputReplyProbe> {
        anyhow::ensure!(
            admission.agent_guard().is_some(),
            "input_reply_requires_agent_guard"
        );
        let probe = InputReplyProbe::new();
        let mut queued = prepare_queued_command(
            RuntimeCommand::WriteInputBatchTracked {
                session,
                operation_id,
                parts,
            },
            &self.command_budget,
        )?;
        queued.admission = Some(admission);
        queued.input_reply = Some(probe.clone());
        self.command_tx
            .as_ref()
            .ok_or(RuntimeCommandSendError::Disconnected)?
            .try_send(queued)
            .map_err(|error| match error {
                TrySendError::Full(_) => RuntimeCommandSendError::Backpressure,
                TrySendError::Disconnected(_) => RuntimeCommandSendError::Disconnected,
            })?;
        if let Some(worker) = &self.worker_thread {
            worker.unpark();
        }
        Ok(probe)
    }

    /// Returns ownership only when the command was never admitted to the worker queue.
    /// Success means channel admission, not PTY acceptance. No automatic retry occurs here.
    pub fn send_command_owned(
        &self,
        command: RuntimeCommand,
    ) -> Result<(), (anyhow::Error, Box<RuntimeCommand>)> {
        let queued = prepare_queued_command_owned(command, &self.command_budget)?;
        let Some(tx) = self.command_tx.as_ref() else {
            return Err((
                RuntimeCommandSendError::Disconnected.into(),
                Box::new(queued.into_command()),
            ));
        };
        match tx.try_send(queued) {
            Ok(()) => {
                if let Some(worker_thread) = &self.worker_thread {
                    worker_thread.unpark();
                }
                Ok(())
            }
            Err(TrySendError::Full(queued)) => Err((
                RuntimeCommandSendError::Backpressure.into(),
                Box::new(queued.into_command()),
            )),
            Err(TrySendError::Disconnected(queued)) => Err((
                RuntimeCommandSendError::Disconnected.into(),
                Box::new(queued.into_command()),
            )),
        }
    }

    fn enqueue_command(
        &self,
        command: RuntimeCommand,
        admission: Option<crate::InputAdmission>,
    ) -> anyhow::Result<()> {
        let mut queued = prepare_queued_command(command, &self.command_budget)?;
        queued.admission = admission;
        let Some(tx) = self.command_tx.as_ref() else {
            return Err(RuntimeCommandSendError::Disconnected.into());
        };
        match tx.try_send(queued) {
            Ok(()) => {
                if let Some(worker_thread) = &self.worker_thread {
                    worker_thread.unpark();
                }
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(RuntimeCommandSendError::Backpressure.into()),
            Err(TrySendError::Disconnected(_)) => Err(RuntimeCommandSendError::Disconnected.into()),
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
                Err(TrySendError::Full(_)) => Err(RuntimeCommandSendError::Backpressure.into()),
                Err(TrySendError::Disconnected(_)) => {
                    Err(RuntimeCommandSendError::Disconnected.into())
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
            return Err(RuntimeCommandSendError::Backpressure.into());
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
    // Local-only: never serialize a revocable permission or lose it over a wire.
    admission: Option<crate::InputAdmission>,
    screen_reply: Option<SyncSender<Option<String>>>,
    input_reply: Option<InputReplyProbe>,
}

impl QueuedRuntimeCommand {
    fn into_command(self) -> RuntimeCommand {
        let Self {
            command,
            reservation,
            admission: _,
            screen_reply: _,
            input_reply: _,
        } = self;
        drop(reservation);
        command
    }
}

fn prepare_queued_command(
    command: RuntimeCommand,
    budget: &Arc<RuntimeCommandQueueBudget>,
) -> anyhow::Result<QueuedRuntimeCommand> {
    prepare_queued_command_owned(command, budget).map_err(|(error, _)| error)
}

fn prepare_queued_command_owned(
    mut command: RuntimeCommand,
    budget: &Arc<RuntimeCommandQueueBudget>,
) -> Result<QueuedRuntimeCommand, (anyhow::Error, Box<RuntimeCommand>)> {
    let retention =
        match crate::command::prepare_runtime_command_for_retention_internal(&mut command) {
            Ok(retention) => retention,
            Err(error) => return Err((error.into(), Box::new(command))),
        };
    let reservation = match budget.reserve(retention.retained_bytes()) {
        Ok(reservation) => reservation,
        Err(error) => return Err((error, Box::new(command))),
    };
    Ok(QueuedRuntimeCommand {
        command,
        reservation,
        admission: None,
        screen_reply: None,
        input_reply: None,
    })
}

struct LazyWorkspaceRestore {
    pending_panes: Vec<persist::PaneState>,
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
    default_api_secrets: Vec<(String, String)>,
    environment_revision: Option<u64>,
    secret_versions: Vec<(String, String)>,
    dotenv_source: Option<crate::dotenv::DotenvSourceSelection>,
    /// 이 워커의 workspace id — needsInput hook 키(`{workspace_id}:{session_id}`)에 쓴다.
    workspace_id: String,
    next_id: u64,
    /// 다중 세션 (PR-08 Session Runtime). 세션 로직은 session crate 소관.
    sessions: std::collections::HashMap<SessionId, Session>,
    input_reply_probes: std::collections::HashMap<SessionId, Weak<Mutex<InputReplyBuffer>>>,
    /// Checked redaction leases are retained for exactly as long as their live/readable session.
    /// A session may need more than one lease when restored dotenv values supplement the resolved
    /// default credential set. Removal/archive drops the complete set and starts grace expiry.
    resize_epoch: u64,
    resize_records: std::collections::HashMap<SessionId, crate::resize::ResizeRecord>,
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
    /// 에이전트 exit sentinel 감시 목록 — `agent_launcher::wrap_agent_then_shell`이 남기는
    /// 파일 경로(래퍼 PID로 결정). 값이 나타나면 detector에 진짜 종료 코드를 latch하고
    /// 항목을 지운다(1회성). 폴백 셸이 아니라 에이전트 자신이 끝난 순간을 잡는다 —
    /// SessionExited(폴백 셸이 exit 칠 때)보다 훨씬 먼저 온다.
    agent_exit_watch: std::collections::HashMap<SessionId, PathBuf>,
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
    persist_db_path: Option<PathBuf>,
    /// 첫 targeted restore가 설치한 bounded pane catalog. worker 명령 직렬화로 한 번에
    /// 하나만 materialize하며, 성공한 항목은 제거하고 실패한 항목은 재시도를 위해 남긴다.
    lazy_restore: Option<LazyWorkspaceRestore>,
    /// backend를 유지 중인 exited 세션들 (종료 순서 — 오래된 것이 앞). §14.3 cap.
    exited_order: std::collections::VecDeque<SessionId>,
    /// exited 백엔드 LRU 상한 (SetTerminalCachePolicy로 변경 — 설정 UI).
    max_exited_backends: usize,
    /// 이 runtime에 배정된 프로세스 전역 터미널 캐시 바이트 예산의 share.
    cache_budget_bytes: usize,
    /// 최신 정책 한 개와 현재 세션 수로 제한한 적용 결과만 보관한다.
    scrollback_policy: Option<(u64, u32)>,
    scrollback_results: std::collections::HashMap<SessionId, terminal::ScrollbackApplyResult>,
    scrollback_trimmed: u64,
    // 시작 시 bounded catalog(최대256 pane)에 있던 영속 identity만 추적한다.
    pending_scrollback_ceilings: std::collections::HashMap<String, usize>,
    scrollback_batching: bool,
    scrollback_ack_pending: bool,
    scrollback_restored: bool,
    /// 압축 아카이브 — 백엔드를 내린 exited 세션의 zlib(ANSI) 덤프. pane이 다시
    /// 보이면 복원(inflate)한다 (§14.3 확장, 2026-07-11).
    archived: std::collections::HashMap<SessionId, ArchivedScrollback>,
    /// 아카이브 삽입 순서 (오래된 것이 앞 — 총 바이트 예산 초과 시 제거 순서)
    archived_order: std::collections::VecDeque<SessionId>,
    /// 디스크 아카이브(scrollback.zlib)가 있는 세션들 (PR-A1) — 메모리 아카이브가
    /// 예산 축출돼도 디스크에서 복원 가능함을 fs stat 없이 판정한다.
    archived_on_disk: std::collections::HashMap<SessionId, usize>,
    /// 변하지 않는 exited 화면의 실패를 매 pump마다 재직렬화하지 않는다.
    archive_failed: std::collections::HashSet<SessionId>,
    /// 디스크 아카이브 총 바이트의 증분 캐시 (A1 리뷰 P2). 워커 시작 시 1회 스캔으로
    /// 시드하고, 기록 성공마다 그 파일 크기만 더한다. 예산 초과가 확정될 때만 gc를
    /// 호출(그때만 전체 디렉터리 스캔+제거)해 매 exit 전체 스캔 비용을 없앤다.
    archive_disk_bytes: u64,
    /// `archive_disk_bytes`를 측정한 logs_root의 dev/inode. root가 교체되면 증분값을
    /// 폐기하고 fresh GC 스캔이 성공하기 전까지 신규 기록을 회계하지 않는다.
    archive_root_identity: Option<storage::scrollback_archive::ArchiveRootIdentity>,
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

fn scan_archive_usage_snapshot(
    logs_root: &std::path::Path,
) -> (
    u64,
    Option<storage::scrollback_archive::ArchiveRootIdentity>,
) {
    let Ok(before) = storage::scrollback_archive::root_identity(logs_root) else {
        return (
            storage::scrollback_archive::ARCHIVE_DISK_USAGE_UNKNOWN,
            None,
        );
    };
    let total = storage::scrollback_archive::scan_total(logs_root);
    let Ok(after) = storage::scrollback_archive::root_identity(logs_root) else {
        return (
            storage::scrollback_archive::ARCHIVE_DISK_USAGE_UNKNOWN,
            None,
        );
    };
    if before == after {
        (total, after)
    } else {
        (
            storage::scrollback_archive::ARCHIVE_DISK_USAGE_UNKNOWN,
            after,
        )
    }
}

fn gc_archive_usage_snapshot(
    logs_root: &std::path::Path,
    budget: u64,
) -> anyhow::Result<(
    u64,
    Option<storage::scrollback_archive::ArchiveRootIdentity>,
)> {
    let before = storage::scrollback_archive::root_identity(logs_root)?;
    let total = storage::scrollback_archive::gc(logs_root, budget)?;
    let after = storage::scrollback_archive::root_identity(logs_root)?;
    anyhow::ensure!(before == after, "scrollback_archive_root_replaced");
    Ok((total, after))
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

struct SessionPumpEffects {
    events: Vec<(RuntimeEvent, bool)>,
    log_offsets: Vec<(SessionId, u64)>,
    status_updates: Vec<(SessionId, session::SessionStatus)>,
    exited_classes: Vec<(SessionId, TerminalCacheClass)>,
    deferred_logs: Vec<(SessionId, Vec<u8>)>,
    final_viewports: Vec<(SessionId, bool)>,
    deferred_ptys: Vec<Box<dyn pty::PtySession>>,
    activity: PumpActivity,
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

/// Redacted bytes are batched only inside one bounded PTY pump, never across ticks or sessions.
/// The last partial batch is flushed before lifecycle/output decisions and persisted offsets.
const REDACTED_LOG_BATCH_BYTES: usize = 32 * 1024;
#[derive(Default)]
struct RedactedLogBatch {
    bytes: Vec<u8>,
}
impl RedactedLogBatch {
    fn push(
        &mut self,
        mut bytes: &[u8],
        mut append: impl FnMut(&[u8]) -> anyhow::Result<u64>,
    ) -> anyhow::Result<Option<u64>> {
        let mut last = None;
        while !bytes.is_empty() {
            if self.bytes.capacity() == 0 {
                self.bytes.reserve_exact(REDACTED_LOG_BATCH_BYTES);
            }
            let take = (REDACTED_LOG_BATCH_BYTES - self.bytes.len()).min(bytes.len());
            self.bytes.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.bytes.len() == REDACTED_LOG_BATCH_BYTES {
                last = self.finish(&mut append)?;
            }
        }
        Ok(last)
    }
    fn finish(
        &mut self,
        mut append: impl FnMut(&[u8]) -> anyhow::Result<u64>,
    ) -> anyhow::Result<Option<u64>> {
        if self.bytes.is_empty() {
            return Ok(None);
        }
        let result = append(&self.bytes).map(Some);
        // A failed batch is not replayed: write_all may have partially written its prefix.
        self.bytes.clear();
        result
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
            }
            | RuntimeCommand::SpawnAgentBeside {
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
        validate_generation: bool,
    ) -> anyhow::Result<(Vec<RuntimeSecret>, Option<RedactionLease>)> {
        let mut resolved = Vec::with_capacity(logical_ids.len());
        for logical_id in logical_ids {
            let (value, generation) = self.secret_resolver.resolve_versioned(&logical_id)?;
            if validate_generation
                && let Some((_, expected)) = self
                    .secret_versions
                    .iter()
                    .find(|(id, _)| id == &logical_id)
            {
                anyhow::ensure!(
                    generation.as_ref() == Some(expected),
                    "runtime_secret_generation_changed"
                );
            }
            resolved.push(value);
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
        let (resolved, lease) = self.resolve_secret_set(logical_ids, true)?;
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
        let default_secret_count = self.default_env_secrets.len() + self.default_api_secrets.len();
        let mut all_secrets = self.default_env_secrets.clone();
        all_secrets.extend(self.default_api_secrets.iter().cloned());
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

    fn emit_resize_result(
        &self,
        session: SessionId,
        token: crate::ResizeToken,
        result: Result<crate::ResizeStamp, crate::ResizeFailure>,
    ) {
        self.emit(match result {
            Ok(stamp) => RuntimeEvent::ResizeApplied { session, stamp },
            Err(reason) => RuntimeEvent::ResizeFailed {
                session,
                token,
                reason,
            },
        });
    }

    fn invalidate_resize(&mut self, session: SessionId) {
        let Some(record) = self.resize_records.get_mut(&session) else {
            return;
        };
        record.result = Err(crate::ResizeFailure::Superseded);
        let Some(epoch) = self.resize_epoch.checked_add(1) else {
            record.stamp = None;
            return;
        };
        self.resize_epoch = epoch;
        record.stamp = self
            .sessions
            .get(&session)
            .and_then(Session::grid_dimensions)
            .map(|(cols, rows)| crate::ResizeStamp {
                epoch,
                owner_epoch: record.token.owner_epoch,
                token: None,
                cols,
                rows,
            });
    }

    fn apply_tracked_resize(
        &mut self,
        session: SessionId,
        token: crate::ResizeToken,
        cols: u16,
        rows: u16,
    ) {
        use crate::resize::{ResizeDecision, ResizeRecord};
        use crate::{ResizeFailure, ResizeStamp};
        if !self.sessions.contains_key(&session) {
            self.emit_resize_result(session, token, Err(ResizeFailure::MissingSession));
            return;
        }
        if !self.resize_records.contains_key(&session) && token.owner_epoch != 1 {
            self.emit_resize_result(session, token, Err(ResizeFailure::Superseded));
            return;
        }
        if let Some(record) = self.resize_records.get(&session)
            && let ResizeDecision::Replay(result) = record.classify(token, (cols, rows))
        {
            self.emit_resize_result(session, token, result);
            if result.is_ok()
                || matches!(
                    result,
                    Err(ResizeFailure::Conflict | ResizeFailure::Superseded)
                )
            {
                if let Some(active) = self.sessions.get_mut(&session) {
                    active.mark_full_dirty();
                }
                self.push_watched_viewports();
            }
            return;
        }
        let Some(epoch) = self.resize_epoch.checked_add(1) else {
            self.emit_resize_result(session, token, Err(ResizeFailure::CounterExhausted));
            return;
        };
        self.resize_epoch = epoch;
        let active = self
            .sessions
            .get_mut(&session)
            .expect("위에서 세션 존재 확인");
        let applied = active.resize_checked(cols, rows);
        let actual = active.grid_dimensions();
        let result = match applied {
            Ok(applied) => {
                if let Some(event) = applied.cache_event {
                    trace_terminal_cache_event(session, event);
                }
                Ok(ResizeStamp {
                    epoch,
                    owner_epoch: token.owner_epoch,
                    token: Some(token),
                    cols: applied.cols,
                    rows: applied.rows,
                })
            }
            Err(error) => Err(match error {
                session::ResizeError::InvalidSize => ResizeFailure::SizeMismatch,
                session::ResizeError::Backend => ResizeFailure::Backend,
                session::ResizeError::Pty => ResizeFailure::Pty,
                session::ResizeError::Dimensions => ResizeFailure::Dimensions,
                session::ResizeError::SizeMismatch => ResizeFailure::SizeMismatch,
            }),
        };
        let stamp = actual.map(|(cols, rows)| ResizeStamp {
            epoch,
            owner_epoch: token.owner_epoch,
            token: result.is_ok().then_some(token),
            cols,
            rows,
        });
        let mut record = self
            .resize_records
            .get(&session)
            .copied()
            .unwrap_or_else(|| ResizeRecord::new(token, (cols, rows), result, stamp));
        if let Err(reason) = record.change_owner(token) {
            self.emit_resize_result(session, token, Err(reason));
            return;
        }
        record.target = (cols, rows);
        record.result = result;
        record.stamp = stamp;
        self.resize_records.insert(session, record);
        if result.is_ok() {
            self.save_terminal_size(session, cols, rows);
        }
        crate::signal_memory_released();
        self.emit_resize_result(session, token, result);
        self.push_watched_viewports();
    }

    fn remove_session(&mut self, session: SessionId) -> Option<Session> {
        self.invalidate_resize(session);
        self.scrollback_results.remove(&session);
        self.archive_failed.remove(&session);
        self.session_redaction_leases.remove(&session);
        let removed = self.sessions.remove(&session);
        if removed.is_some() {
            self.emit_scrollback_result();
        }
        removed
    }

    fn requested_scrollback(&self, fallback: usize) -> usize {
        self.scrollback_policy
            .map_or(fallback, |(_, requested)| requested as usize)
    }

    /// 보관 중인 archive의 낮아진 한도는 이후 증가 요청으로 되살리지 않는다.
    fn restored_scrollback(&self, session: SessionId, stored: usize) -> usize {
        self.requested_scrollback(stored).min(stored).min(
            self.archived_on_disk
                .get(&session)
                .copied()
                .unwrap_or(usize::MAX),
        )
    }

    fn insert_session(&mut self, id: SessionId, mut session: Session) {
        if let Some((_, requested)) = self.scrollback_policy {
            let result = session.set_scrollback_limit(requested as usize);
            self.record_scrollback_result(id, result);
        }
        self.sessions.insert(id, session);
        self.invalidate_resize(id);
        // 여러 pane을 한 명령으로 복원해도 다음 backend를 만들기 전에 예산을 적용한다.
        if self.terminal_cache_bytes() > self.cache_budget_bytes {
            let mut visible = self.mux.watched_sessions();
            visible.extend(self.remote_viewing.keys().copied());
            if self.trim_live_over_budget(&visible) {
                crate::signal_memory_released();
            }
        }
        self.emit_scrollback_result();
    }

    fn record_scrollback_result(&mut self, id: SessionId, result: terminal::ScrollbackApplyResult) {
        if let terminal::ScrollbackApplyResult::Applied { trimmed, .. } = result {
            self.scrollback_trimmed = self.scrollback_trimmed.saturating_add(trimmed as u64);
        }
        self.scrollback_results.insert(id, result);
    }

    fn emit_scrollback_result(&mut self) {
        if self.scrollback_batching {
            self.scrollback_ack_pending = true;
            return;
        }
        let Some((generation, requested)) = self.scrollback_policy else {
            return;
        };
        let mut applied = 0u16;
        let mut unsupported = 0u16;
        let mut effective_min = usize::MAX;
        for (id, result) in &self.scrollback_results {
            match result {
                terminal::ScrollbackApplyResult::Applied { effective, .. } => {
                    applied = applied.saturating_add(1);
                    let current = self.sessions.get(id).map_or(*effective, |session| {
                        session.cache_footprint().scrollback_limit_lines
                    });
                    effective_min = effective_min.min(current);
                }
                terminal::ScrollbackApplyResult::Unsupported => {
                    unsupported = unsupported.saturating_add(1)
                }
            }
        }
        self.emit(RuntimeEvent::ScrollbackLimitApplied {
            generation,
            requested,
            applied,
            unsupported,
            trimmed: self.scrollback_trimmed,
            // 감사/복구 로그를 삭제하지 않으므로 재시작 후 영속 삭제를 보장하지 않는다.
            durable: false,
            restored: self.scrollback_restored,
            effective_min: if applied == 0 {
                0
            } else {
                effective_min as u32
            },
        });
    }

    fn apply_scrollback_policy(&mut self, generation: u64, requested: u32) {
        if let Some((current, value)) = self.scrollback_policy
            && (generation < current || (generation == current && requested != value))
        {
            return;
        }
        if self.scrollback_policy != Some((generation, requested)) {
            if let Some(pipe) = &self.persist {
                for key in pipe.pending_session_ids() {
                    self.pending_scrollback_ceilings
                        .entry(key.to_owned())
                        .or_insert(requested as usize);
                }
            }
            for limit in self.pending_scrollback_ceilings.values_mut() {
                *limit = (*limit).min(requested as usize);
            }
            self.scrollback_policy = Some((generation, requested));
            self.scrollback_results.clear();
            self.scrollback_trimmed = 0;
            let results = self
                .sessions
                .iter_mut()
                .map(|(id, session)| (*id, session.set_scrollback_limit(requested as usize)))
                .collect::<Vec<_>>();
            for (id, result) in results {
                self.record_scrollback_result(id, result);
            }
            for entry in self.archived.values_mut() {
                entry.scrollback_lines = entry.scrollback_lines.min(requested as usize);
            }
            for limit in self.archived_on_disk.values_mut() {
                *limit = (*limit).min(requested as usize);
            }
            self.push_watched_viewports();
        }
        // ACK 유실 후 같은 요청을 재전송해도 다시 압축하거나 기록을 추가 삭제하지 않는다.
        self.emit_scrollback_result();
    }

    fn spawn_session(
        &self,
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
        let session = Session::spawn_with_spec_and_output_wake(
            id,
            kind,
            spec,
            cols,
            rows,
            self.requested_scrollback(scrollback_lines),
            output_wake,
        )?;
        // 실제 spawn 성공 뒤에만 기록한다. 복원 아카이브에는 이 이벤트를 만들지 않는다.
        self.emit(RuntimeEvent::EnvironmentApplied {
            session: Some(id),
            revision: self.environment_revision.filter(|_| {
                self.default_env_secrets
                    .iter()
                    .chain(&self.default_api_secrets)
                    .all(|(_, id)| self.secret_versions.iter().any(|(pinned, _)| pinned == id))
            }),
        });
        Ok(session)
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
                        self.handle_queued_command(command);
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
                    self.handle_queued_command(command);
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
        let event = match event {
            RuntimeEvent::Viewport {
                session,
                snapshot,
                bracketed_paste,
            } if self
                .resize_records
                .get(&session)
                .and_then(|record| record.stamp)
                .is_some() =>
            {
                let mut stamp = self.resize_records[&session]
                    .stamp
                    .expect("위에서 stamp 확인");
                if (stamp.cols, stamp.rows) != (snapshot.cols, snapshot.rows) {
                    stamp.token = None;
                    stamp.cols = snapshot.cols;
                    stamp.rows = snapshot.rows;
                }
                RuntimeEvent::ViewportTracked {
                    session,
                    snapshot,
                    bracketed_paste,
                    stamp,
                }
            }
            event => event,
        };
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
                if let Some((session, _, _, _)) = event.viewport() {
                    if Arc::strong_count(&subscriber.viewports) <= 1 {
                        return false;
                    }
                    {
                        let mut slot = subscriber.viewports.lock().expect("viewport slot lock");
                        let prev = slot.insert(session, event.clone());
                        // 미소비 이전 스냅샷의 dirty 델타를 합친다 — 안 그러면 그 행들이
                        // renderer 재shaping에서 빠져 stale로 남는다 (event.rs 헬퍼 주석).
                        if let Some(prev) = prev
                            && let Some(current) = slot.get_mut(&session)
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

    fn handle_queued_command(&mut self, mut queued: QueuedRuntimeCommand) {
        if let Some(reply) = queued.screen_reply.take() {
            if let RuntimeCommand::ExtractLastOutput { session } = queued.command {
                let effects = self.collect_session_pump_effects(&[session], false, true);
                self.finish_session_pump_effects(effects);
                let screen = self
                    .sessions
                    .get(&session)
                    .filter(|active| {
                        active.lifecycle().is_running() && active.pending_output_bytes() == 0
                    })
                    .map(|active| {
                        let text = active.screen_text();
                        let mut start = text.len().saturating_sub(8192);
                        while !text.is_char_boundary(start) {
                            start += 1;
                        }
                        text[start..].to_owned()
                    });
                let _ = reply.try_send(screen);
            }
            return;
        }
        let admission = queued.admission.take();
        let input_reply = queued.input_reply.take();
        let command = queued.into_command();
        if let Some(admission) = admission {
            match command {
                RuntimeCommand::WriteInputTracked {
                    session,
                    operation_id,
                    bytes,
                } => {
                    let result = self.admit_input_checked(session, &bytes, Some(&admission));
                    self.emit(RuntimeEvent::InputAdmitted {
                        session,
                        operation_id,
                        result,
                    });
                }
                RuntimeCommand::WriteInputBatchTracked {
                    session,
                    operation_id,
                    parts,
                } => {
                    let slices = parts.iter().map(Vec::as_slice).collect::<Vec<_>>();
                    let result = self.admit_input_batch_checked(session, &slices, Some(&admission));
                    if result.is_ok()
                        && let Some(probe) = input_reply
                    {
                        self.input_reply_probes.retain(|id, weak| {
                            self.sessions.contains_key(id) && weak.strong_count() > 0
                        });
                        // Session creation already enforces RUNTIME_SESSION_CAP.
                        debug_assert!(self.input_reply_probes.len() <= RUNTIME_SESSION_CAP);
                        if let Some(previous) = self
                            .input_reply_probes
                            .insert(session, Arc::downgrade(&probe.0))
                            && let Some(previous) = previous.upgrade()
                        {
                            InputReplyProbe(previous).close();
                        }
                    }
                    self.emit(RuntimeEvent::InputAdmitted {
                        session,
                        operation_id,
                        result,
                    });
                }
                _ => self.reject_invalid_command(&command),
            }
        } else {
            self.handle_command(command);
        }
    }

    fn handle_command(&mut self, command: RuntimeCommand) {
        // 하나의 복원 명령이 만든 세션 전체를 집계한 뒤 완료를 알린다.
        self.scrollback_batching = true;
        self.handle_command_inner(command);
        self.scrollback_batching = false;
        if self.scrollback_ack_pending {
            self.scrollback_ack_pending = false;
            self.emit_scrollback_result();
        }
    }

    fn admit_input(
        &mut self,
        session: SessionId,
        bytes: &[u8],
    ) -> Result<(), pty::PtyInputRejectReason> {
        self.admit_input_checked(session, bytes, None)
    }

    fn admit_input_checked(
        &mut self,
        session: SessionId,
        bytes: &[u8],
        admission: Option<&crate::InputAdmission>,
    ) -> Result<(), pty::PtyInputRejectReason> {
        self.admit_input_batch_checked(session, &[bytes], admission)
    }

    fn admit_input_batch_checked(
        &mut self,
        session: SessionId,
        parts: &[&[u8]],
        admission: Option<&crate::InputAdmission>,
    ) -> Result<(), pty::PtyInputRejectReason> {
        self.admit_input_batch_checked_with_log_sink(
            session,
            parts,
            admission,
            SessionLog::append_redacted_output,
        )
    }

    fn admit_input_batch_checked_with_log_sink(
        &mut self,
        session: SessionId,
        parts: &[&[u8]],
        admission: Option<&crate::InputAdmission>,
        append: impl FnMut(&mut SessionLog, &[u8]) -> anyhow::Result<u64>,
    ) -> Result<(), pty::PtyInputRejectReason> {
        if !self.sessions.contains_key(&session) {
            return Err(pty::PtyInputRejectReason::SessionClosed);
        }
        let at_micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_micros().min(i64::MAX as u128) as i64)
            .unwrap_or(0);
        let outcome = if let Some(admission) = admission {
            let mut refreshed = None;
            let admitted = admission.admit(|| {
                let refresh_output =
                    admission.requires_bracketed_paste() || admission.agent_guard().is_some();
                if refresh_output {
                    refreshed = Some(self.collect_session_pump_effects(&[session], false, true));
                }
                let active = self
                    .sessions
                    .get_mut(&session)
                    .ok_or(pty::PtyInputRejectReason::SessionClosed)?;
                if refresh_output && active.pending_output_bytes() > 0 {
                    return Err(pty::PtyInputRejectReason::AdmissionDenied);
                }
                // The authorization callback can wait for its own lock. Query live foreground,
                // draft and dialog state inside its accepted callback, immediately before retention.
                if admission.requires_bracketed_paste() && !active.bracketed_paste() {
                    return Err(pty::PtyInputRejectReason::AdmissionDenied);
                }
                if let Some(guard) = admission.agent_guard()
                    && !guard.allows(active, self.detectors.get(&session))
                {
                    return Err(pty::PtyInputRejectReason::AdmissionDenied);
                }
                // A bounded refresh/guard can take time, and more output can arrive while
                // it runs. Refuse known-unsent rather than spin or use that stale screen.
                if admission.deadline_elapsed()
                    || (refresh_output && active.pending_output_bytes() > 0)
                {
                    return Err(pty::PtyInputRejectReason::AdmissionDenied);
                }
                Ok(active.write_input_batch(parts))
            });
            if let Some(effects) = refreshed {
                self.finish_session_pump_effects_with_log_sink(effects, append);
            }
            admitted.ok_or(pty::PtyInputRejectReason::AdmissionDenied)??
        } else {
            self.sessions
                .get_mut(&session)
                .expect("session exists")
                .write_input_batch(parts)
        };
        // The admission lock is gone before detectors/events invoke UI wakes.
        match outcome {
            Some(pty::PtyInputEnqueueResult::Accepted) => {
                // These two native Codex configuration keys cannot recall or introduce text.
                // The guard already verified an empty prompt and no existing draft. Keep all
                // ordinary/history/manual input evidence unchanged; never clear a real draft.
                let effort_key = matches!(parts, [key] if *key == b"\x1b[1;2A" || *key == b"\x1b[1;2B")
                    && admission
                        .and_then(crate::InputAdmission::agent_guard)
                        .is_some_and(|guard| {
                            guard.provider == crate::AgentPromptKind::Codex
                                && guard.intent == crate::AgentInputIntent::AutomaticPrompt
                        });
                let submitted = !effort_key
                    && self.detectors.get_mut(&session).is_some_and(|detector| {
                        let mut submitted = false;
                        for part in parts {
                            submitted |= detector.on_user_input(part);
                        }
                        submitted
                    });
                if submitted && at_micros > 0 {
                    self.emit(RuntimeEvent::SessionInputSubmitted { session, at_micros });
                }
                Ok(())
            }
            Some(pty::PtyInputEnqueueResult::Backpressured { pressure }) => {
                self.pressured_sessions.insert(session);
                self.emit(RuntimeEvent::PtyInputPressure { session, pressure });
                Err(pty::PtyInputRejectReason::QueueFull)
            }
            Some(pty::PtyInputEnqueueResult::Rejected { pressure }) => {
                let reason = pressure.reason;
                self.emit(RuntimeEvent::PtyInputPressure { session, pressure });
                Err(reason)
            }
            None => Err(pty::PtyInputRejectReason::WriterUnavailable),
        }
    }

    fn handle_command_inner(&mut self, mut command: RuntimeCommand) {
        // All production senders already use this primitive before queue retention. Reapplying it
        // here is an idempotent defense for direct/internal producers and preserves fail-closed
        // worker semantics without duplicating validation or canonicalization rules.
        if crate::command::prepare_runtime_command_for_retention_internal(&mut command).is_err() {
            self.reject_invalid_command(&command);
            return;
        }
        let (command, agent_split_target) = command.into_agent_spawn_target();
        match command {
            RuntimeCommand::SpawnAgentBeside { .. } => {
                unreachable!("split launch normalized above")
            }
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
                match self.spawn_session(
                    id,
                    session::SessionKind::Shell,
                    &spec, // 테스트 주입 가능해야 하므로 default_shell 헬퍼 대신 spec 직접
                    cols,
                    rows,
                    scrollback_lines,
                ) {
                    Ok(new_session) => {
                        self.insert_session(id, new_session);
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
                                None,
                                None,
                                None,
                                None,
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
                if agent_split_target
                    .as_ref()
                    .is_some_and(|pane| self.mux.tab_of_pane(pane).is_none())
                {
                    self.emit(RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Agent,
                        message: MessagePayload::new("runtime.split.target_missing"),
                    });
                    self.emit_agent_spawn_resolved(correlation_id, None);
                    return;
                }
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
                match self.spawn_session(
                    id,
                    session::SessionKind::Agent,
                    &spec,
                    cols,
                    rows,
                    scrollback_lines,
                ) {
                    Ok(new_session) => {
                        self.insert_session(id, new_session);
                        self.retain_session_redaction_leases(id, redaction_leases);
                        let patterns = StatusPatterns::compile(
                            waiting_regex.as_deref(),
                            approval_regex.as_deref(),
                            error_regex.as_deref(),
                            done_regex.as_deref(),
                        );
                        // regex가 없어도 idle heuristic(3단)은 동작해야 한다 — 상시 설치
                        self.detectors.insert(id, StatusDetector::new(patterns));
                        self.register_agent_exit_watch(id);
                        if let Some(target) = agent_split_target {
                            if !self.attach_agent_beside(id, AGENT_TITLE_ID, &target) {
                                self.remove_session(id);
                                self.detectors.remove(&id);
                                self.agent_exit_watch.remove(&id);
                                self.emit(RuntimeEvent::SpawnFailed {
                                    kind: SpawnKind::Agent,
                                    message: MessagePayload::new("runtime.split.target_lost"),
                                });
                                self.emit_agent_spawn_resolved(correlation_id, None);
                                return;
                            }
                        } else {
                            self.attach_in_new_tab(id, AGENT_TITLE_ID);
                        }
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
                                waiting_regex,
                                approval_regex,
                                error_regex,
                                done_regex,
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
                secret_versions,
                environment_revision,
                dotenv_source,
                api_secrets,
                env_plain,
                env_secrets,
            } => {
                // 이후 SpawnShell/SpawnAgent부터 적용 — 기존 세션은 건드리지 않는다.
                self.default_env_plain = env_plain;
                self.default_env_secrets = env_secrets;
                self.default_api_secrets = api_secrets;
                self.dotenv_source = dotenv_source;
                self.environment_revision = environment_revision;
                self.secret_versions = secret_versions;
                self.emit(RuntimeEvent::EnvironmentApplied {
                    session: None,
                    revision: environment_revision,
                });
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
            RuntimeCommand::SetScrollbackLimit {
                generation,
                requested,
            } => self.apply_scrollback_policy(generation, requested),
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
                        || self.archived_on_disk.contains_key(&session);
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
                let _ = self.admit_input(session, &bytes);
            }
            RuntimeCommand::RequestMuxSnapshot => self.emit_current_mux_snapshot(),
            RuntimeCommand::WriteTerminalInput { session, input } => {
                if input.requires_current_modes() {
                    // Commands run before the normal output pump. Parse the target's queued
                    // modes with the existing strict budget; never spin behind an output flood.
                    let effects = self.collect_session_pump_effects(&[session], false, true);
                    self.finish_session_pump_effects(effects);
                    if self
                        .sessions
                        .get(&session)
                        .is_some_and(|active| active.pending_output_bytes() > 0)
                    {
                        let policy = pty::PtyInputQueuePolicy::default();
                        self.emit(RuntimeEvent::PtyInputPressure {
                            session,
                            pressure: pty::PtyInputPressure {
                                attempted_bytes: input.payload().len(),
                                queued_bytes: 0,
                                queued_messages: 0,
                                max_bytes: policy.max_bytes,
                                max_messages: policy.max_messages,
                                reason: pty::PtyInputRejectReason::AdmissionDenied,
                            },
                        });
                        return;
                    }
                }
                let Some(active) = self.sessions.get(&session) else {
                    return;
                };
                if let Some(bytes) =
                    input.encode(active.bracketed_paste(), active.application_cursor())
                {
                    let _ = self.admit_input(session, &bytes);
                }
            }
            RuntimeCommand::WriteInputTracked {
                session,
                operation_id,
                bytes,
            } => {
                let result = self.admit_input(session, &bytes);
                self.emit(RuntimeEvent::InputAdmitted {
                    session,
                    operation_id,
                    result,
                });
            }
            RuntimeCommand::WriteInputBatchTracked {
                session,
                operation_id,
                parts,
            } => {
                let slices = parts.iter().map(Vec::as_slice).collect::<Vec<_>>();
                let result = self.admit_input_batch_checked(session, &slices, None);
                self.emit(RuntimeEvent::InputAdmitted {
                    session,
                    operation_id,
                    result,
                });
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
            RuntimeCommand::ResizeTracked {
                session,
                token,
                cols,
                rows,
            } => {
                self.apply_tracked_resize(session, token, cols, rows);
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
                    self.invalidate_resize(session);
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
                match self.resolve_secret_set(credential_ids, false) {
                    Ok((_resolved, lease)) => self.seed_redaction_lease = lease,
                    Err(_) => {
                        tracing::warn!("redaction seed rejected");
                    }
                }
            }
            RuntimeCommand::KillSession { session } => {
                self.kill_session_owned(session, true);
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
                if !self.suspended && (self.sessions.is_empty() || self.lazy_restore.is_some()) {
                    self.restore_saved_layout();
                }
                self.scrollback_restored = true;
                self.emit_scrollback_result();
            }
            RuntimeCommand::RestoreWorkspacePane { pane } => {
                if !self.suspended {
                    self.restore_saved_pane(&pane);
                }
                self.scrollback_restored = true;
                self.emit_scrollback_result();
            }
            RuntimeCommand::DurableEventBarrier { correlation_id } => {
                self.emit(RuntimeEvent::DurableEventBarrierReached { correlation_id });
            }
            RuntimeCommand::InspectUnattachedSessions => {
                let count = u16::try_from(self.unattached_session_ids().len()).unwrap_or(u16::MAX);
                self.emit(RuntimeEvent::UnattachedSessionsInspected { count });
            }
            RuntimeCommand::KillUnattachedSessions => {
                let candidates = self.unattached_session_ids();
                let mut killed = 0_u16;
                for session in candidates {
                    killed =
                        killed.saturating_add(u16::from(self.kill_session_owned(session, false)));
                }
                if killed > 0 {
                    self.emit_mux_and_watched();
                    crate::signal_memory_released();
                }
                self.emit(RuntimeEvent::UnattachedSessionsKilled { count: killed });
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
            RuntimeCommand::RespawnArchivedAgent {
                session,
                extra_args,
                cols,
                rows,
                scrollback_lines,
            } => {
                if self.suspended {
                    self.emit(RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Agent,
                        message: MessagePayload::new("runtime.spawn_failed.suspended"),
                    });
                    return;
                }
                self.respawn_archived_agent(session, extra_args, cols, rows, scrollback_lines);
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
    fn replay_saved_ansi(&mut self, persistent_id: &str, session: &mut Session) {
        self.replay_saved_ansi_ext(persistent_id, session, true);
    }

    fn replay_saved_ansi_ext(
        &mut self,
        persistent_id: &str,
        session: &mut Session,
        finish_boundary: bool,
    ) {
        let future = self.requested_scrollback(session.cache_footprint().scrollback_limit_lines);
        // 이 실행에서 낮춘 뒤 아직 복원하지 않은 로그도 낮은 한도로 딱 한 번 재생한다.
        session.set_scrollback_limit(
            future.min(
                self.pending_scrollback_ceilings
                    .get(persistent_id)
                    .copied()
                    .unwrap_or(future),
            ),
        );
        Self::replay_saved_ansi_raw(&self.logs_root, persistent_id, session, finish_boundary);
        session.set_scrollback_limit(future);
    }

    fn replay_saved_ansi_raw(
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
        let mut secrets = self.default_env_secrets.clone();
        secrets.extend(self.default_api_secrets.iter().cloned());
        crate::command::validate_env_entries_with_base(
            &self.shell.env,
            &self.default_env_plain,
            &secrets,
        )?;
        let mut spec = self.shell.clone();
        spec.env
            .push(("DEPPY_SESSION_ID".to_owned(), self.session_key(id)));
        // 워크스페이스 기본 env(.env 자동 주입). The complete secret set is resolved and
        // protected before spawn; partial injection would silently change command semantics.
        spec.env.extend(self.default_env_plain.iter().cloned());
        let (secret_env, leases) = self.resolve_secret_env(secrets)?;
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

    fn attach_agent_beside(
        &mut self,
        session: SessionId,
        title_prefix: &str,
        target: &MuxPaneId,
    ) -> bool {
        let Some(tab_id) = self.mux.tab_of_pane(target) else {
            return false;
        };
        let pane_id = MuxPaneId::new();
        if !self.mux.tabs.get_mut(&tab_id).is_some_and(|tab| {
            tab.split_pane(target, mux::SplitDirection::Horizontal, pane_id.clone())
        }) {
            return false;
        }
        self.tab_counter += 1;
        let mut pane = MuxPane::new(
            pane_id.clone(),
            format!("{title_prefix} {}", self.tab_counter),
        );
        pane.session_id = Some(session);
        self.mux.panes.insert(pane_id.clone(), pane);
        self.mux.window.active_tab = Some(tab_id);
        self.mux.focus.focus(pane_id);
        true
    }

    /// 복원(PR-14)이 spawn하는 fresh 셸의 scrollback 기본값 — 실제 config 값은
    /// 복원 경로에 없어 app::config::TerminalConfig 기본값(10_000)과 맞춘 상수를 쓴다.
    const RESTORE_SCROLLBACK_LINES: usize = 10_000;

    /// bounded startup snapshot을 정확히 한 번 소비해 tab/layout과 session 없는 pane
    /// skeleton을 설치한다. PaneState는 최대 256개로 persist loader에서 이미 제한된다.
    fn prepare_saved_layout_skeleton(&mut self) -> bool {
        if self.lazy_restore.is_some() {
            return true;
        }
        let (tabs, active_tab) = match &mut self.persist {
            Some(pipe) => pipe.take_saved_layout(),
            None => return false,
        };
        if tabs.is_empty() {
            return false;
        }
        let max_suffix = tabs
            .iter()
            .flat_map(|tab| {
                std::iter::once(tab.title.as_str())
                    .chain(tab.panes.iter().map(|p| p.title.as_str()))
            })
            .filter_map(title_suffix)
            .max()
            .unwrap_or(0);
        let mut pending_panes = Vec::new();
        for tab in tabs {
            let persist::TabState {
                id,
                title,
                layout,
                active_pane,
                panes,
            } = tab;
            for pane_state in panes {
                if let Some(key) = &pane_state.session_id {
                    let requested = self.requested_scrollback(Self::RESTORE_SCROLLBACK_LINES);
                    self.pending_scrollback_ceilings
                        .entry(key.clone())
                        .or_insert(requested);
                }
                self.mux.panes.insert(
                    pane_state.id.clone(),
                    MuxPane::new(pane_state.id.clone(), pane_state.title.clone()),
                );
                pending_panes.push(pane_state);
            }
            let restored = MuxTab {
                id,
                title,
                layout,
                active_pane,
            };
            self.mux.window.add_tab(restored.id.clone());
            self.mux.tabs.insert(restored.id.clone(), restored);
        }
        self.tab_counter = self.tab_counter.max(max_suffix);
        if active_tab.is_some() {
            self.mux.window.active_tab = active_tab;
        }
        self.mux.fix_focus();
        self.lazy_restore = Some(LazyWorkspaceRestore { pending_panes });
        true
    }

    fn restore_saved_pane(&mut self, pane: &MuxPaneId) {
        if self.lazy_restore.is_none()
            && (!self.sessions.is_empty() || !self.prepare_saved_layout_skeleton())
        {
            return;
        }
        if self
            .mux
            .panes
            .get(pane)
            .is_some_and(|pane| pane.session_id.is_some())
        {
            return;
        }
        let pane_state = self.lazy_restore.as_mut().and_then(|restore| {
            restore
                .pending_panes
                .iter()
                .position(|candidate| candidate.id == *pane)
                .map(|position| (position, restore.pending_panes.remove(position)))
        });
        let restored_session = match pane_state {
            Some((position, pane_state)) => match self.restore_pane(&pane_state) {
                Some(session) => Some(session),
                None => {
                    if let Some(restore) = &mut self.lazy_restore {
                        restore.pending_panes.insert(position, pane_state);
                    }
                    None
                }
            },
            None => None,
        };
        if self
            .lazy_restore
            .as_ref()
            .is_some_and(|restore| restore.pending_panes.is_empty())
        {
            self.lazy_restore = None;
        }
        self.emit_mux_and_watched();
        if let Some(session) = restored_session {
            self.emit_restored_session(session);
        }
    }

    /// 이전 실행이 저장한 mux layout을 완전히 복원한다. targeted restore가 먼저
    /// 진행됐으면 남은 catalog만 worker에서 순서대로 materialize하고, 실패한 항목은
    /// 원래 영속 session association을 유지한 채 다음 복원 시도를 위해 보존한다.
    fn restore_saved_layout(&mut self) {
        if self.lazy_restore.is_none() && !self.prepare_saved_layout_skeleton() {
            return;
        }
        let pending_panes = self
            .lazy_restore
            .take()
            .map(|restore| restore.pending_panes)
            .unwrap_or_default();
        let mut restored_sessions = Vec::new();
        let mut failed_panes = Vec::new();
        for pane in pending_panes {
            match self.restore_pane(&pane) {
                Some(session) => restored_sessions.push(session),
                None => failed_panes.push(pane),
            }
        }
        if !failed_panes.is_empty() {
            self.lazy_restore = Some(LazyWorkspaceRestore {
                pending_panes: failed_panes,
            });
        }
        self.mux.fix_focus();
        self.emit_mux_and_watched();
        for session in restored_sessions {
            self.emit_restored_session(session);
        }
    }

    fn emit_restored_session(&self, session: SessionId) {
        if let Some(session::SessionLifecycle::Exited { exit_code }) =
            self.sessions.get(&session).map(Session::lifecycle)
        {
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

    /// Bounded dotenv projection for restored panes. Any missing/invalid/over-limit input is an
    /// empty fail-closed projection; restoration never applies a partial first-file result.
    fn restored_dotenv_for_session(&self, dir: &std::path::Path) -> Vec<(String, String)> {
        // 버전이 있는 기본환경은 앱 worker가 확정한 snapshot이다. 재조회로 실행값과 버전을 갈라놓지 않는다.
        if self.environment_revision.is_some() {
            return Vec::new();
        }
        let entries = match &self.dotenv_source {
            Some(source) => source
                .root
                .as_ref()
                .and_then(|root| {
                    crate::dotenv::read_dotenv_files_bounded(root, &source.files)
                        .ok()
                        .flatten()
                })
                .unwrap_or_default(),
            None => Self::restored_dotenv(dir),
        };
        entries
            .into_iter()
            .filter(|(key, _)| {
                // 앱이 확정한 라이브 반영 제어값은 복원 파일로 덮어쓰지 않는다.
                key != "DEPPY_ENV_LIVE_RELOAD"
                    && key != "DEPPY_PROJECT_ROOT"
                    && !self
                        .default_api_secrets
                        .iter()
                        .any(|(binding, _)| binding == key)
            })
            .collect()
    }

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
    fn restore_pane(&mut self, pane_state: &persist::PaneState) -> Option<SessionId> {
        if !self.session_capacity_available() {
            let pane = MuxPane::new(pane_state.id.clone(), pane_state.title.clone());
            self.mux.panes.insert(pane_state.id.clone(), pane);
            tracing::warn!(error_code = "runtime_session_limit", "복원 세션 상한 도달");
            return None;
        }
        if let Some(persistent_id) = pane_state.session_id.as_deref() {
            let was_agent = self
                .persist
                .as_ref()
                .and_then(|pipe| pipe.restored_session_kind(persistent_id))
                .is_some_and(|kind| kind == "agent");
            if was_agent && self.restore_archived_pane(pane_state, persistent_id) {
                return self
                    .mux
                    .panes
                    .get(&pane_state.id)
                    .and_then(|pane| pane.session_id);
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
                return None;
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
            let dotenv = self.restored_dotenv_for_session(dir);
            match self.acquire_dotenv_redaction_lease(&dotenv) {
                Ok(Some(lease)) => redaction_leases.push(lease),
                Ok(None) => {}
                Err(_) => {
                    tracing::warn!(pane_id = %pane_state.id.0, "복원 중 dotenv redaction 준비 실패");
                    self.mux.panes.insert(pane_state.id.clone(), pane);
                    return None;
                }
            }
            // 워크스페이스 기본 env보다 뒤에 붙어 pane 폴더 값이 이긴다.
            // API와 충돌하는 값은 redaction 준비 전에 제외했다.
            spec.env.extend(dotenv);
        }
        let spawn_cwd = Self::spawn_cwd_string(&spec.cwd);
        let (restore_cols, restore_rows) = pane_state
            .session_id
            .as_deref()
            .map(|persistent_id| Self::restored_terminal_size(&self.logs_root, persistent_id))
            .unwrap_or((DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS));
        let restored_session = match self.spawn_session(
            id,
            session::SessionKind::Shell,
            &spec,
            restore_cols,
            restore_rows,
            Self::RESTORE_SCROLLBACK_LINES,
        ) {
            Ok(mut new_session) => {
                if let Some(persistent_id) = pane_state.session_id.as_deref() {
                    self.replay_saved_ansi(persistent_id, &mut new_session);
                }
                self.insert_session(id, new_session);
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
                            None,
                            None,
                            None,
                            None,
                        );
                    }
                }
                self.open_session_log(id);
                Some(id)
            }
            Err(error) => {
                trace_runtime_failure("restore_shell_spawn", "pty_spawn_failed", error);
                None
            }
        };
        self.mux.panes.insert(pane_state.id.clone(), pane);
        restored_session
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
                    self.requested_scrollback(meta.scrollback_lines as usize)
                        .min(meta.scrollback_lines as usize)
                        .min(
                            self.pending_scrollback_ceilings
                                .get(persistent_id)
                                .copied()
                                .unwrap_or(usize::MAX),
                        ),
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
                    self.requested_scrollback(Self::RESTORE_SCROLLBACK_LINES),
                    None,
                    &mut std::io::empty(),
                );
                // 열람 전용 — 모드 경계 리셋 생략(alt-screen 화면 보존, codex 리뷰 P2)
                self.replay_saved_ansi_ext(persistent_id, &mut session, false);
                (session, false)
            }
        };
        if let Some(pipe) = &mut self.persist
            && !pipe.session_rebound_archived(id, persistent_id)
        {
            return false;
        }
        let restored_limit = restored.cache_footprint().scrollback_limit_lines;
        self.insert_session(id, restored);
        if restored_from_disk {
            self.archived_on_disk.insert(id, restored_limit);
        }
        self.exited_order.push_back(id);
        let mut pane = MuxPane::new(pane_state.id.clone(), pane_state.title.clone());
        pane.session_id = Some(id);
        self.mux.panes.insert(pane_state.id.clone(), pane);
        tracing::info!(persistent_id, session = id.0, "agent pane 열람 전용 복원");
        true
    }

    /// `RespawnArchivedAgent` 실패 공통 경로 — 대상이 archived agent pane이 아니거나
    /// (라이브 세션·미존재 세션·pane 미결속·agent 아님) 준비 단계에서 막힌 경우.
    /// 이 시점까진 기존 상태를 전혀 건드리지 않았으므로 그냥 실패만 알리면 된다
    /// (열람 전용 화면·pane 결속·영속 행 모두 그대로).
    fn fail_respawn_archived_agent(&self, correlation_id: Option<AgentConfigCorrelationId>) {
        self.emit(RuntimeEvent::SpawnFailed {
            kind: SpawnKind::Agent,
            message: MessagePayload::new("runtime.spawn_failed.invalid_command"),
        });
        self.emit_agent_spawn_resolved(correlation_id, None);
    }

    /// 열람 전용(archived)으로 복원된 agent pane을 그 자리에서 재실행한다 (PR-2).
    /// 사용자가 명시적으로 pane의 「다시 실행」을 눌렀을 때만 온다.
    ///
    /// 순서가 안전의 전부다: 새 프로세스가 **성공적으로 spawn된 뒤에만** 이전 archived
    /// 세션/아카이브/영속 행을 건드린다. 그 전에 실패하면(대상 부적합, 용량 초과, env
    /// 준비 실패, PTY spawn 실패) 이전 상태는 한 바이트도 바뀌지 않는다 — 재실행 실패가
    /// "열람 전용으로 복원됐던 화면"을 잃게 만드는 경로는 없다.
    fn respawn_archived_agent(
        &mut self,
        session: SessionId,
        extra_args: Vec<String>,
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
    ) {
        let Some(pane_id) = self
            .mux
            .panes
            .iter()
            .find(|(_, pane)| pane.session_id == Some(session))
            .map(|(id, _)| id.clone())
        else {
            self.fail_respawn_archived_agent(None);
            return;
        };
        // 재실행 대상은 라이브 프로세스가 없는(열람 전용) 세션만 — PTY가 붙어 있는
        // 세션을 덮어쓰지 않는다. 세션이 아예 없어도(레이스로 이미 정리됨) 마찬가지.
        let is_archived = self
            .sessions
            .get(&session)
            .is_some_and(|current| !current.lifecycle().is_running());
        if !is_archived {
            self.fail_respawn_archived_agent(None);
            return;
        }
        // 영속 행이 있어야 재실행 스펙(command/args/cwd)을 안다 — session_rebound_archived가
        // restored_rows에서 self.rows로 이미 옮겨뒀으므로 archived agent pane이면 항상 있다
        // (persistence.rs:196 결속 계약). kind가 agent가 아니면(레거시 shell 등) 대상이 아니다.
        // regex 4종도 같은 행에서 함께 읽는다 — spawn 시점에 굳혀 저장된 값이라
        // agent_configs를 다시 조회하지 않아도 된다(§ MIGRATION_SESSION_REGEX).
        let Some((
            persistent_id,
            command,
            mut args,
            cwd_value,
            agent_config_id,
            waiting_regex,
            approval_regex,
            error_regex,
            done_regex,
        )) = self.persist.as_ref().and_then(|pipe| {
            let row = pipe.session_row(session)?;
            (row.session_kind == "agent").then(|| {
                (
                    row.id.clone(),
                    row.command.clone(),
                    row.args.clone(),
                    row.cwd.clone(),
                    row.agent_id.clone(),
                    row.waiting_regex.clone(),
                    row.approval_regex.clone(),
                    row.error_regex.clone(),
                    row.done_regex.clone(),
                )
            })
        })
        else {
            self.fail_respawn_archived_agent(None);
            return;
        };
        let correlation_id = agent_config_id
            .as_ref()
            .filter(|id| crate::command::agent_config_id_is_valid(id))
            .cloned()
            .map(AgentConfigCorrelationId::from_validated);
        if !self.session_capacity_available() {
            self.reject_session_capacity(SpawnKind::Agent, correlation_id);
            return;
        }
        args.extend(extra_args);
        let (mut env, redaction_leases) = match self.prepare_agent_env(Vec::new(), Vec::new()) {
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
        // 저장된 cwd가 더 이상 유효한 디렉터리가 아니면(삭제/이동) 워크스페이스 기본
        // cwd로 폴백한다 — restore_pane의 pane별 cwd 복원과 같은 규칙.
        let cwd = Some(std::path::PathBuf::from(cwd_value))
            .filter(|p| crate::command::validate_runtime_path(p).is_ok() && p.is_dir())
            .or_else(|| self.shell.cwd.clone());
        let id = SessionId(self.next_id);
        self.next_id += 1;
        env.push(("DEPPY_SESSION_ID".to_owned(), self.session_key(id)));
        let spec = CommandSpec {
            program: command,
            args,
            env,
            cwd,
        };
        match self.spawn_session(
            id,
            session::SessionKind::Agent,
            &spec,
            cols,
            rows,
            scrollback_lines,
        ) {
            Ok(mut new_session) => {
                // 이전(열람 전용) 화면을 잃지 않는다 — 셸 respawn 복원(restore_pane)과
                // 같은 연속성 패턴: 새 세션의 scrollback에 이전 redacted ANSI를 먼저
                // 재생한 뒤, 이번 tick부터 도착하는 라이브 PTY 출력이 그 뒤를 잇는다.
                if let Some(previous) = self.sessions.get_mut(&session) {
                    let had_output = previous.cache_footprint().history_lines > 0
                        || !previous.screen_text().trim().is_empty();
                    // 직렬화 미지원/출력상한과 무관하게 현재 backend 소유권을 이어받는다.
                    new_session.inherit_terminal_from(previous);
                    if had_output && let Err(error) = new_session.finish_ansi_replay() {
                        trace_runtime_failure(
                            "respawn_replay",
                            "respawn_replay_finish_failed",
                            error,
                        );
                    }
                }
                self.insert_session(id, new_session);
                self.retain_session_redaction_leases(id, redaction_leases);
                // 세션 행에 함께 저장해둔 spawn 시점 regex를 그대로 복원한다 —
                // agent_configs를 다시 조회하지 않는다(그 사이 설정이 바뀌었거나
                // 삭제됐어도 이 세션은 처음 띄울 때 규칙을 그대로 쓴다. command/args/cwd가
                // 이미 같은 방식으로 spawn 시점 값을 보존하는 것과 동일한 불변식).
                self.detectors.insert(
                    id,
                    StatusDetector::new(StatusPatterns::compile(
                        waiting_regex.as_deref(),
                        approval_regex.as_deref(),
                        error_regex.as_deref(),
                        done_regex.as_deref(),
                    )),
                );
                self.register_agent_exit_watch(id);

                // 여기서부터는 성공이 확정됐을 때만 실행된다 — 이전 archived 상태 정리.
                self.remove_session(session);
                self.exited_order.retain(|s| *s != session);
                self.archived.remove(&session);
                self.archived_on_disk.remove(&session);
                self.hidden_scrollback.remove(&session);
                self.remote_viewing.remove(&session);
                self.status_overrides.remove(&session);
                self.detectors.remove(&session);
                if let Some(path) = self.agent_exit_watch.remove(&session) {
                    let _ = std::fs::remove_file(&path);
                }

                // 새 탭이 아니라 그 pane에 — attach_in_new_tab을 쓰면 안 된다 (새 탭 생성).
                if let Some(pane) = self.mux.panes.get_mut(&pane_id) {
                    pane.session_id = Some(id);
                }
                if let Some(pipe) = &mut self.persist {
                    pipe.session_respawned(session, id);
                }
                // 오래된 디스크 스크롤백 아카이브(scrollback.zlib) 무효화 — 그대로 두면
                // write_scrollback_archive의 exists() 가드가 "이미 있음"으로 skip해
                // 새 세션이 종료돼도 아카이브가 영영 이전(재실행 전) 화면인 채로 남는다
                // (exited grid 불변 가정을 재실행이 깨므로, 재실행 쪽이 명시적으로
                // 무효화해야 한다). 실패해도 치명적이지 않다 — redacted.ansi.log tail
                // 폴백이 다음 복원에서 그 자리를 대신한다.
                match storage::scrollback_archive::remove(&self.logs_root, &persistent_id) {
                    Ok(_) => {
                        self.archive_disk_bytes =
                            storage::scrollback_archive::ARCHIVE_DISK_USAGE_UNKNOWN;
                    }
                    Err(error) => trace_runtime_failure(
                        "respawn_archive_invalidate",
                        "scrollback_archive_remove_failed",
                        error,
                    ),
                }

                self.open_session_log(id);
                self.emit_mux_snapshot();
                self.emit(RuntimeEvent::AgentSpawned { session: id });
                self.emit_agent_spawn_resolved(correlation_id, Some(id));
                self.push_watched_viewports();
            }
            Err(error) => {
                // 실패 — 이전 archived 세션/pane/영속 행/디스크 아카이브 전부 그대로.
                trace_runtime_failure("respawn_archived_agent", "pty_spawn_failed", error);
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
        match self.spawn_session(
            id,
            session::SessionKind::Shell,
            &spec,
            80,
            24,
            scrollback_lines,
        ) {
            Ok(new_session) => {
                self.insert_session(id, new_session);
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
                        None,
                        None,
                        None,
                        None,
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
                    // 방어: split 실패 시 고아 pane/세션을 남기지 않는다.
                    // `detectors`도 반드시 함께 지운다 — 다른 정리 지점(close_pane,
                    // kill_session_owned, close_tab)은 전부 지우는데 여기만 빠져 있었다
                    // (2026-08-20 코드 리뷰). SessionId는 단조 증가라 한 번 새면 그
                    // 항목은 영영 남는다.
                    self.mux.panes.remove(&pane_id);
                    self.remove_session(id);
                    self.detectors.remove(&id);
                    self.status_overrides.remove(&id);
                    self.close_session_log(id, "killed", None);
                    self.emit(RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Shell,
                        message: MessagePayload::new("runtime.split.target_lost"),
                    });
                    return;
                }
                // A queued split retains its exact target even if another tab became active.
                self.mux.window.active_tab = Some(tab_id);
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
        if let Some(restore) = &mut self.lazy_restore {
            restore.pending_panes.retain(|pane| pane.id != pane_id);
            if restore.pending_panes.is_empty() {
                self.lazy_restore = None;
            }
        }
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

    fn unattached_session_ids(&self) -> Vec<SessionId> {
        let now = Instant::now();
        let attached = self
            .mux
            .panes
            .values()
            .filter_map(|pane| pane.session_id)
            .collect::<std::collections::HashSet<_>>();
        self.sessions
            .keys()
            .copied()
            .filter(|session| {
                !attached.contains(session)
                    && self
                        .remote_viewing
                        .get(session)
                        .is_none_or(|expiry| *expiry <= now)
            })
            .take(RUNTIME_SESSION_CAP)
            .collect()
    }

    fn kill_session_owned(&mut self, session: SessionId, emit_mux: bool) -> bool {
        let existed = self.sessions.contains_key(&session);
        self.final_drain(session);
        // Session drop → PtySession Drop이 process group 정리를 보장한다.
        self.remove_session(session);
        self.exited_order.retain(|candidate| *candidate != session);
        self.hidden_scrollback.remove(&session);
        self.remote_viewing.remove(&session);
        self.detectors.remove(&session);
        self.status_overrides.remove(&session);
        self.close_session_log(session, "killed", None);
        if let Some(pipe) = &mut self.persist {
            pipe.session_exited(session);
        }
        // 세션을 잃은 pane은 attach 해제 (pane/session 분리 — 5.2).
        for pane in self.mux.panes.values_mut() {
            if pane.session_id == Some(session) {
                pane.session_id = None;
            }
        }
        if emit_mux {
            self.emit_mux_and_watched();
        }
        existed
    }

    fn close_tab(&mut self, tab_id: MuxTabId) {
        let Some(tab) = self.mux.tabs.remove(&tab_id) else {
            return;
        };
        let pane_ids = tab.panes();
        if let Some(restore) = &mut self.lazy_restore {
            restore
                .pending_panes
                .retain(|pane| !pane_ids.contains(&pane.id));
            if restore.pending_panes.is_empty() {
                self.lazy_restore = None;
            }
        }
        for pane_id in pane_ids {
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
        let mut batch = RedactedLogBatch::default();
        let mut drained = 0usize;
        for _ in 0..FINAL_DRAIN_MAX_PUMPS {
            let result = active.pump(|chunk| {
                if let Some(log) = log.as_mut() {
                    let redacted = log.redactor.redact_chunk(chunk);
                    match batch.push(&redacted, |bytes| log.append_redacted_output(bytes)) {
                        Ok(Some(offset)) => latest_offset = Some(offset),
                        Ok(None) => {}
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
        if let Some(log) = log.as_mut() {
            match batch.finish(|bytes| log.append_redacted_output(bytes)) {
                Ok(Some(offset)) => latest_offset = Some(offset),
                Ok(None) => {}
                Err(error) => trace_runtime_failure(
                    "session_log_final_append",
                    "session_log_append_failed",
                    error,
                ),
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

    fn save_lazy_merged_layout(&self) {
        let (Some(restore), Some(db_path), Some(pipe)) = (
            self.lazy_restore.as_ref(),
            self.persist_db_path.as_ref(),
            self.persist.as_ref(),
        ) else {
            return;
        };
        let saved = (|| -> anyhow::Result<()> {
            let mut conn = rusqlite::Connection::open(db_path)?;
            conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
            conn.pragma_update(None, "foreign_keys", true)?;
            conn.busy_timeout(Duration::from_secs(5))?;
            let window_id = conn.query_row(
                "SELECT id FROM mux_windows
                 WHERE workspace_id = ?1
                 ORDER BY created_at, id
                 LIMIT 1",
                [&self.workspace_id],
                |row| row.get::<_, String>(0),
            )?;
            let state = persist::WindowState {
                id: deppy_core::MuxWindowId(window_id),
                title: None,
                active_tab: self.mux.window.active_tab.clone(),
                tabs: self
                    .mux
                    .window
                    .tabs
                    .iter()
                    .filter_map(|tab_id| self.mux.tabs.get(tab_id))
                    .map(|tab| persist::TabState {
                        id: tab.id.clone(),
                        title: tab.title.clone(),
                        layout: tab.layout.clone(),
                        active_pane: tab.active_pane.clone(),
                        panes: tab
                            .layout
                            .panes()
                            .into_iter()
                            .filter_map(|pane_id| self.mux.panes.get(&pane_id))
                            .map(|pane| {
                                let pending = restore
                                    .pending_panes
                                    .iter()
                                    .find(|pending| pending.id == pane.id);
                                persist::PaneState {
                                    id: pane.id.clone(),
                                    session_id: pending
                                        .and_then(|pending| pending.session_id.clone())
                                        .or_else(|| {
                                            pane.session_id.and_then(|session| {
                                                pipe.session_log_key(session).map(str::to_owned)
                                            })
                                        }),
                                    title: pane.title.clone(),
                                    pane_kind: pane.pane_kind,
                                    cwd: pending.and_then(|pending| pending.cwd.clone()),
                                }
                            })
                            .collect(),
                    })
                    .collect(),
            };
            persist::save_window_layout(&mut conn, &self.workspace_id, &state)
        })();
        if saved.is_err() {
            tracing::warn!(
                error_code = "lazy_restore_layout_save_failed",
                "부분 복원 mux layout 영속 실패"
            );
        }
    }

    /// mux 스냅샷만 emit (+ 영속 저장). spawn 경로는 이걸 먼저 부르고
    /// Spawned 이벤트를 보낸 뒤 [`Self::push_watched_viewports`]를 불러야
    /// "slot에 Viewport가 있으면 그 세션의 Spawned가 같은 drain에 포함"이라는
    /// RuntimeEventReceiver::drain의 happens-before 계약이 유지된다 (codex 리뷰).
    fn emit_mux_snapshot(&mut self) {
        // 활성/보관 세션 소유 범위를 벗어난 token/owner 이력을 누적하지 않는다.
        self.resize_records.retain(|id, _| {
            self.sessions.contains_key(id)
                || self.archived.contains_key(id)
                || self.archived_on_disk.contains_key(id)
                || self
                    .mux
                    .panes
                    .values()
                    .any(|pane| pane.session_id == Some(*id))
        });
        // mux 구조가 바뀐 지점 — 가시성 전이에 맞춰 scrollback cap 조정 (§14.3)
        self.reconcile_visibility();
        if self.lazy_restore.is_some() {
            self.save_lazy_merged_layout();
        } else if let Some(pipe) = &mut self.persist {
            pipe.save_layout(&self.mux.window, &self.mux.tabs, &self.mux.panes);
        }
        self.emit_current_mux_snapshot();
    }

    // Queries only publish current live membership. A cold worker may still have
    // saved panes awaiting deferred restoration; querying must never save its empty mux.
    fn emit_current_mux_snapshot(&self) {
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
                && (self.archived.contains_key(session)
                    || self.archived_on_disk.contains_key(session))
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

    /// 방금 스폰한 세션에 exit sentinel 감시를 건다. pid를 못 구하면(플랫폼 제약,
    /// portable-pty가 identity를 못 준 경우 등) 조용히 건너뛴다 — 실패해도 기존 동작
    /// (idle heuristic·SessionExited)이 그대로 남는다. custom agent처럼
    /// `wrap_agent_then_shell`을 거치지 않은 세션도 걸리지만, 그 sentinel은 영영 안
    /// 나타날 뿐이고 세션 종료 시 항목을 함께 지우므로(아래 exited 처리) 누수되지 않는다.
    #[cfg(unix)]
    fn register_agent_exit_watch(&mut self, id: SessionId) {
        let Some(pid) = self
            .sessions
            .get(&id)
            .and_then(|session| session.process_identity().pid)
        else {
            return;
        };
        let path = agent_exit_sentinel_path(&std::env::temp_dir(), pid);
        // 이 pid를 재사용한 옛 프로세스가 남긴 파일이 있으면(극히 드묾) 오판을 막기 위해
        // 먼저 지운다 — session::agent_exit_sentinel_path 문서의 재사용 경고와 짝.
        let _ = std::fs::remove_file(&path);
        self.agent_exit_watch.insert(id, path);
    }

    #[cfg(not(unix))]
    fn register_agent_exit_watch(&mut self, _id: SessionId) {}

    /// exit sentinel이 나타났으면 detector에 진짜 종료 코드를 latch한다 —
    /// SessionStatusChanged/SessionStatusViewChanged는 뒤이은 evaluate()가 평소처럼
    /// emit한다(새 이벤트 타입 불필요, 기존 notifications 배선을 그대로 탄다).
    /// 대부분의 tick은 아직 안 끝난 것뿐이라 못 찾는 게 정상 — 조용히 다음 tick으로.
    fn poll_agent_exit_sentinels(&mut self) {
        if self.agent_exit_watch.is_empty() {
            return;
        }
        let mut resolved: Vec<(SessionId, PathBuf, u32)> = Vec::new();
        for (session, path) in &self.agent_exit_watch {
            if let Ok(content) = std::fs::read_to_string(path)
                && let Ok(code) = content.trim().parse::<u32>()
            {
                resolved.push((*session, path.clone(), code));
            }
        }
        for (session, path, code) in resolved {
            self.agent_exit_watch.remove(&session);
            let _ = std::fs::remove_file(&path);
            if let Some(detector) = self.detectors.get_mut(&session) {
                detector.note_exit_sentinel(code);
            }
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
        // 폴백 셸이 이어받기 전에(=SessionExited보다 훨씬 먼저) 에이전트 자신의 진짜
        // 종료 코드를 반영한다 — 아래 evaluate()가 이번 tick에 바로 새 상태를 emit한다.
        self.poll_agent_exit_sentinels();
        let sessions = self.sessions.keys().copied().collect::<Vec<_>>();
        let effects = self.collect_session_pump_effects(&sessions, allow_viewport, false);
        self.finish_session_pump_effects(effects)
    }

    /// The same stream/parser/detector/log lifecycle serves normal ticks and the guarded
    /// target-only refresh. Effects are emitted only after the input authorization lock exits.
    fn collect_session_pump_effects(
        &mut self,
        sessions: &[SessionId],
        allow_viewport: bool,
        input_guard: bool,
    ) -> SessionPumpEffects {
        let watched = self.mux.watched_sessions();
        // 원격 시청 lease 세션 — GUI 가시성과 무관하게 Viewport 대상 (P5a).
        let remote_viewed = if input_guard {
            let now = Instant::now();
            sessions
                .iter()
                .copied()
                .filter(|id| {
                    self.remote_viewing
                        .get(id)
                        .is_some_and(|expiry| *expiry > now)
                })
                .collect()
        } else {
            self.remote_viewed_sessions()
        };
        // (이벤트, gui_viewport) — Viewport만 원격 전용 여부를 구분한다 (P5 리뷰 P1).
        let mut events: Vec<(RuntimeEvent, bool)> = Vec::new();
        let mut log_offsets = Vec::new();
        let mut status_updates = Vec::new();
        let mut exited_classes = Vec::new();
        let mut deferred_logs = Vec::new();
        let mut final_viewports = Vec::new();
        let mut deferred_ptys = Vec::new();
        let mut activity = PumpActivity::default();
        for session in sessions {
            let Some(active) = self.sessions.get_mut(session) else {
                continue;
            };
            let active_id = active.id();
            let reply = self
                .input_reply_probes
                .get(&active_id)
                .and_then(Weak::upgrade);
            if reply.is_none() {
                self.input_reply_probes.remove(&active_id);
            }
            let mut log = self.logs.get_mut(&active_id);
            let mut detector = self.detectors.get_mut(&active_id);
            let mut batch = RedactedLogBatch::default();
            let mut latest_log_offset = None;
            let mut deferred_log = Vec::new();
            let mut on_output = |chunk: &[u8]| {
                if let Some(reply) = &reply
                    && let Ok(mut buffer) = reply.lock()
                {
                    buffer.append(chunk);
                }
                if input_guard && log.is_some() {
                    // Allocate only for actual guarded output with an open log. One flat
                    // buffer charges at most the strict pump cap, independent of chunk count.
                    if deferred_log.capacity() == 0 {
                        deferred_log.reserve_exact(Session::INPUT_GUARD_OUTPUT_MAX_BYTES);
                    }
                    deferred_log.extend_from_slice(chunk);
                } else if let Some(log) = log.as_mut() {
                    // redaction 후에만 디스크에 닿는다 (7장 — raw 평문 저장 금지)
                    let redacted = log.redactor.redact_chunk(chunk);
                    match batch.push(&redacted, |bytes| log.append_redacted_output(bytes)) {
                        Ok(Some(offset)) => latest_log_offset = Some(offset),
                        Ok(None) => {}
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
            };
            let (result, deferred_pty) = if input_guard {
                active.pump_for_input_guard(&mut on_output)
            } else {
                (active.pump(&mut on_output), None)
            };
            if let Some(pty) = deferred_pty {
                deferred_ptys.push(pty);
            }
            if !input_guard && let Some(log) = log.as_mut() {
                match batch.finish(|bytes| log.append_redacted_output(bytes)) {
                    Ok(Some(offset)) => latest_log_offset = Some(offset),
                    Ok(None) => {}
                    Err(error) => trace_runtime_failure(
                        "session_log_append",
                        "session_log_append_failed",
                        error,
                    ),
                }
            }
            if let Some(offset) = latest_log_offset {
                log_offsets.push((active_id, offset));
            }
            if !deferred_log.is_empty() {
                deferred_logs.push((active_id, deferred_log));
            }
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
                } else if result.just_exited {
                    // There is no later paced tick after pane cleanup. Publish the final
                    // watched frame in finish, outside any input authorization lock.
                    let gui_viewport = self.render_active && watched.contains(&active.id());
                    final_viewports.push((active.id(), gui_viewport));
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
                exited_classes.push((active.id(), class));
            }
            if result.just_exited
                && let session::SessionLifecycle::Exited { exit_code } = active.lifecycle()
            {
                let status = if exit_code == Some(0) {
                    session::SessionStatus::Done
                } else {
                    session::SessionStatus::Error
                };
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
        SessionPumpEffects {
            events,
            log_offsets,
            status_updates,
            exited_classes,
            deferred_logs,
            final_viewports,
            deferred_ptys,
            activity,
        }
    }

    fn finish_session_pump_effects(&mut self, effects: SessionPumpEffects) -> PumpActivity {
        self.finish_session_pump_effects_with_log_sink(effects, SessionLog::append_redacted_output)
    }

    fn finish_session_pump_effects_with_log_sink(
        &mut self,
        effects: SessionPumpEffects,
        mut append: impl FnMut(&mut SessionLog, &[u8]) -> anyhow::Result<u64>,
    ) -> PumpActivity {
        let SessionPumpEffects {
            mut events,
            mut log_offsets,
            status_updates,
            exited_classes,
            deferred_logs,
            final_viewports,
            deferred_ptys,
            mut activity,
        } = effects;
        // Sessions already detached these exited writers. Escalation and worker joins
        // happen only here, after InputAdmission released permit/credential locks.
        drop(deferred_ptys);
        for (session, gui_viewport) in final_viewports {
            if let Some(active) = self.sessions.get_mut(&session)
                && let Some(snapshot) = active.take_snapshot()
            {
                events.push((
                    RuntimeEvent::Viewport {
                        session,
                        snapshot: Arc::new(snapshot),
                        bracketed_paste: active.bracketed_paste(),
                    },
                    gui_viewport,
                ));
                activity.viewport_emitted = true;
            }
        }
        for (session, raw) in deferred_logs {
            let Some(log) = self.logs.get_mut(&session) else {
                continue;
            };
            let redacted = log.redactor.redact_chunk(&raw);
            let mut batch = RedactedLogBatch::default();
            let mut latest_log_offset = None;
            match batch.push(&redacted, |bytes| append(log, bytes)) {
                Ok(Some(offset)) => latest_log_offset = Some(offset),
                Ok(None) => {}
                Err(error) => {
                    trace_runtime_failure("session_log_append", "session_log_append_failed", error)
                }
            }
            match batch.finish(|bytes| append(log, bytes)) {
                Ok(Some(offset)) => latest_log_offset = Some(offset),
                Ok(None) => {}
                Err(error) => {
                    trace_runtime_failure("session_log_append", "session_log_append_failed", error)
                }
            }
            if let Some(offset) = latest_log_offset {
                log_offsets.push((session, offset));
            }
        }
        for (session, class) in exited_classes {
            if let Some(active) = self.sessions.get_mut(&session)
                && active.cache_class() != class
            {
                if let Some(event) = active.set_cache_class(class) {
                    trace_terminal_cache_event(session, event);
                }
                if class == TerminalCacheClass::Exited {
                    crate::signal_memory_released();
                }
            }
            if class != TerminalCacheClass::Hidden {
                self.hidden_scrollback.remove(&session);
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
            // sentinel이 끝내 안 나타났으면(에이전트가 아니었거나, 쓰기 실패 등) 감시
            // 항목과 혹시 남은 파일을 함께 정리한다 — temp dir에 흔적을 남기지 않는다.
            if let Some(path) = self.agent_exit_watch.remove(&session) {
                let _ = std::fs::remove_file(&path);
            }
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
        // 세션 종료 → pane 자동 닫힘 (tmux 관례 — exit하면 pane이 접히고 이웃이
        // 공간을 차지, 2026-07-05 사용자 요청). 예전엔 agent pane을 결과 상태(✅/❌)와
        // scrollback 열람을 위해 제외했지만, 에이전트 실행은 항상
        // `agent_launcher::wrap_agent_then_shell`로 감싸여 있다: 에이전트가 끝나면 그
        // 자리에서 `exec`로 평범한 셸이 이어받는다. 즉 agent 세션의 SessionExited가
        // 온다는 것 자체가 "그 폴백 셸에서 사용자가 exit을 쳤다"는 뜻이고, 결과
        // 배지·scrollback은 exit을 치기 전에 이미 pane에서 다 봤다 — shim이 세션을
        // 에이전트 프로세스보다 오래 살리는 한 그 근거는 계속 성립한다. 그래서 이제
        // agent도 셸과 동일하게 닫는다(2026-08-19, "exit하면 pane 닫힘"과 "이어서
        // 하기는 재기동해도 동작"을 동시에 요구 — 후자는 work history가 pane 생존과
        // 무관하게 archived 바인딩만으로 재개하도록 별도로 처리한다, agent_resume 참고).
        //
        // 주의: `SessionRestored`(재시작 시 열람 전용으로 복원된 pane, restore_pane→
        // restore_archived_pane)는 이 `exited_sessions`에 절대 섞이지 않는다 — 이
        // 리스트는 이번 tick `pump_sessions`가 만든 `events`에서 SessionExited만 뽑은
        // 것이고, SessionRestored는 그 이벤트 목록에 들어간 적이 없는 별도 경로에서
        // emit된다. 여기서 SessionRestored까지 닫으면 앱을 켜자마자 복원 pane이 전부
        // 사라진다 — 아래 회귀 테스트가 이를 고정한다.
        //
        // SessionExited emit **후**라 UI는 같은 drain에서 exit 알림을 먼저 받고
        // MuxUpdated로 pane 제거를 본다 (채널 FIFO). close_pane의 세션 정리는 위
        // exited 처리와 겹쳐도 멱등(no-op)이다.
        for session in exited_sessions {
            self.close_exited_pane(session);
        }
        activity
    }

    fn close_exited_pane(&mut self, session: SessionId) {
        self.close_exited_pane_with_budget(session, ARCHIVED_SCROLLBACK_BUDGET_BYTES);
    }

    fn close_exited_pane_with_budget(&mut self, session: SessionId, compressed_budget: usize) {
        let pane = self
            .mux
            .panes
            .values()
            .find(|p| p.session_id == Some(session))
            .map(|p| p.id.clone());
        if let Some(pane) = pane {
            if !self.archived_on_disk.contains_key(&session) {
                if self.archive_failed.contains(&session) {
                    return;
                }
                if let Some(live) = self.sessions.get_mut(&session) {
                    match Self::make_archive_entry_with_budget(live, compressed_budget) {
                        Ok(entry) => {
                            self.archived_order.retain(|id| *id != session);
                            self.archived_order.push_back(session);
                            self.archived.insert(session, entry);
                            self.trim_archived_budget();
                        }
                        Err(terminal::ScrollbackSerializeError::Unsupported) => {}
                        Err(error) => {
                            // 자동 exit의 보존 수단이 모두 실패한 경우만 화면을 남긴다.
                            // 명시적 ClosePane은 이 경로를 거치지 않고 기존대로 폐기한다.
                            self.archive_failed.insert(session);
                            tracing::warn!(
                                session = session.0,
                                ?error,
                                "자동 종료 아카이브 실패 — 화면 보존"
                            );
                            return;
                        }
                    }
                }
            }
            self.close_pane(pane);
        }
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
            if self.archive_failed.contains(&session) {
                continue;
            }
            let Some(live) = self.sessions.get_mut(&session) else {
                continue;
            };
            let estimated_bytes = live.cache_footprint().estimated_bytes;
            // 압축 아카이브 시도 — 성공하면 pane을 유지하고 다시 보일 때 복원한다
            let entry = match Self::make_archive_entry(live) {
                Ok(entry) => Some(entry),
                Err(terminal::ScrollbackSerializeError::Unsupported) => None,
                Err(error) => {
                    // 지원 backend는 완전한 최신 tail이 보존되기 전에 제거하지 않는다.
                    self.archive_failed.insert(session);
                    tracing::warn!(
                        session = session.0,
                        ?error,
                        "아카이브 실패 — 최신 화면 보존"
                    );
                    continue;
                }
            };
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
    /// 미지원과 보존 실패를 구분해 지원 backend의 최신 화면을 버리지 않는다.
    fn make_archive_entry(
        live: &mut Session,
    ) -> Result<ArchivedScrollback, terminal::ScrollbackSerializeError> {
        Self::make_archive_entry_with_budget(live, ARCHIVED_SCROLLBACK_BUDGET_BYTES)
    }

    fn make_archive_entry_with_budget(
        live: &mut Session,
        compressed_budget: usize,
    ) -> Result<ArchivedScrollback, terminal::ScrollbackSerializeError> {
        // 압축률이 낮아도 새 entry 자체가 LRU 예산을 넘지 않게 한다. 매 실패마다
        // 실제 history가 절반 이하로 줄어들어 중첩 직렬화/압축 재시도도 유계다.
        for _ in 0..=terminal::policy::SCROLLBACK_LINES_MAX.ilog2() + 1 {
            let dump =
                live.serialize_scrollback_for_archive(MAX_ARCHIVED_SCROLLBACK_RESTORE_BYTES)?;
            let footprint = live.cache_footprint();
            let exit_code = match live.lifecycle() {
                session::SessionLifecycle::Exited { exit_code } => exit_code,
                session::SessionLifecycle::Running => None,
            };
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            std::io::Write::write_all(&mut encoder, &dump)
                .map_err(|_| terminal::ScrollbackSerializeError::Unavailable)?;
            let compressed = encoder
                .finish()
                .map_err(|_| terminal::ScrollbackSerializeError::Unavailable)?;
            if compressed.len() > compressed_budget {
                let history = live.cache_footprint().history_lines;
                if history == 0 {
                    return Err(terminal::ScrollbackSerializeError::LimitExceeded);
                }
                live.trim_scrollback(history / 2);
                if live.cache_footprint().history_lines >= history {
                    return Err(terminal::ScrollbackSerializeError::LimitExceeded);
                }
                continue;
            }
            return Ok(ArchivedScrollback {
                kind: live.kind(),
                cols: footprint.columns as u16,
                rows: footprint.screen_lines as u16,
                scrollback_lines: footprint.scrollback_limit_lines,
                exit_code,
                compressed,
            });
        }
        Err(terminal::ScrollbackSerializeError::LimitExceeded)
    }

    /// 아카이브 총 바이트가 예산을 넘으면 오래된 것부터 제거한다 (LRU).
    fn trim_archived_budget(&mut self) {
        let mut total: usize = self.archived.values().map(|a| a.compressed.len()).sum();
        while total > ARCHIVED_SCROLLBACK_BUDGET_BYTES || self.archived.len() > RUNTIME_SESSION_CAP
        {
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
        if !self.refresh_archive_root_identity() {
            return;
        }
        if self.archive_disk_bytes == storage::scrollback_archive::ARCHIVE_DISK_USAGE_UNKNOWN
            && !self.reconcile_archive_disk_usage(
                storage::scrollback_archive::ARCHIVE_DISK_BUDGET_BYTES,
            )
        {
            return;
        }
        if storage::scrollback_archive::exists(&self.logs_root, &key) {
            let limit = self
                .sessions
                .get(&session)
                .map(|live| live.cache_footprint().scrollback_limit_lines)
                .unwrap_or(self.requested_scrollback(terminal::policy::SCROLLBACK_LINES_MAX));
            self.archived_on_disk.entry(session).or_insert(limit);
            return;
        }
        let Some(live) = self.sessions.get_mut(&session) else {
            return;
        };
        // 빈 grid는 기록 생략 (VS Code v1.69 노이즈 억제 차용)
        let footprint = live.cache_footprint();
        if footprint.history_lines == 0 && live.screen_text().trim().is_empty() {
            return;
        }
        let Ok(dump) = live.serialize_scrollback_for_archive(
            storage::scrollback_archive::MAX_UNCOMPRESSED_BYTES as usize,
        ) else {
            return; // 미지원/리소스 실패는 여기서 backend를 제거하지 않는다.
        };
        let footprint = live.cache_footprint();
        if dump.len() > storage::scrollback_archive::MAX_UNCOMPRESSED_BYTES as usize {
            return;
        }
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
        match storage::scrollback_archive::write_receipt(&self.logs_root, &key, &meta, &redacted) {
            Ok(receipt) => self.finish_archive_write(session, &key, receipt),
            Err(error) => trace_runtime_failure(
                "scrollback_archive_write",
                "scrollback_archive_write_failed",
                error,
            ),
        }
    }

    fn finish_archive_write(
        &mut self,
        session: SessionId,
        key: &str,
        receipt: storage::scrollback_archive::ArchiveWriteReceipt,
    ) {
        let accounted = self.account_archive_write(receipt.bytes());
        let current =
            match storage::scrollback_archive::written_is_current(&self.logs_root, key, &receipt) {
                Ok(current) => current,
                Err(error) => {
                    trace_runtime_failure(
                        "scrollback_archive_revalidate",
                        "scrollback_archive_revalidate_failed",
                        error,
                    );
                    false
                }
            };
        if accounted && current {
            let limit = self
                .sessions
                .get(&session)
                .map(|live| live.cache_footprint().scrollback_limit_lines)
                .unwrap_or(self.requested_scrollback(terminal::policy::SCROLLBACK_LINES_MAX));
            self.archived_on_disk.insert(session, limit);
        } else {
            if !current {
                self.archive_disk_bytes = storage::scrollback_archive::ARCHIVE_DISK_USAGE_UNKNOWN;
                self.archive_root_identity =
                    storage::scrollback_archive::root_identity(&self.logs_root)
                        .ok()
                        .flatten();
            }
            if let Err(error) =
                storage::scrollback_archive::remove_written(&self.logs_root, key, receipt)
            {
                trace_runtime_failure(
                    "scrollback_archive_rollback",
                    "scrollback_archive_rollback_failed",
                    error,
                );
            }
        }
    }

    fn refresh_archive_root_identity(&mut self) -> bool {
        match storage::scrollback_archive::root_identity(&self.logs_root) {
            Ok(current) => {
                if current != self.archive_root_identity {
                    self.archive_root_identity = current;
                    self.archive_disk_bytes =
                        storage::scrollback_archive::ARCHIVE_DISK_USAGE_UNKNOWN;
                }
                true
            }
            Err(error) => {
                trace_runtime_failure(
                    "scrollback_archive_root_identity",
                    "scrollback_archive_root_identity_failed",
                    error,
                );
                self.archive_disk_bytes = storage::scrollback_archive::ARCHIVE_DISK_USAGE_UNKNOWN;
                false
            }
        }
    }

    fn reconcile_archive_disk_usage(&mut self, budget: u64) -> bool {
        match gc_archive_usage_snapshot(&self.logs_root, budget) {
            Ok((total, identity)) => {
                self.archive_disk_bytes = total;
                self.archive_root_identity = identity;
                true
            }
            Err(error) => {
                trace_runtime_failure(
                    "scrollback_archive_gc",
                    "scrollback_archive_gc_failed",
                    error,
                );
                self.archive_disk_bytes = storage::scrollback_archive::ARCHIVE_DISK_USAGE_UNKNOWN;
                false
            }
        }
    }

    /// 디스크 아카이브 기록 후 증분 예산 캐시를 갱신한다 (A1 리뷰 P2). 예산 내면
    /// 전체 스캔 없이 크기만 더하고, 초과가 확정될 때만 gc(전체 스캔+오래된 것부터
    /// 제거)를 호출해 캐시를 실제 총량으로 재동기화한다. 이로써 매 exit의 GC 비용이
    /// "지금까지 존재한 세션 수"에 비례하는 문제를 없앤다.
    fn account_archive_write(&mut self, written_len: u64) -> bool {
        let budget = storage::scrollback_archive::ARCHIVE_DISK_BUDGET_BYTES;
        if !self.refresh_archive_root_identity() {
            return false;
        }
        if archive_cache_needs_gc(self.archive_disk_bytes, written_len, budget) {
            self.reconcile_archive_disk_usage(budget)
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
                    self.restored_scrollback(session, meta.scrollback_lines as usize),
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
            self.requested_scrollback(scrollback_lines)
                .min(scrollback_lines),
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
        self.insert_session(session, restored);
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
    #[test]
    fn input_reply_is_bounded_and_close_stops_capture() {
        let probe = InputReplyProbe::new();
        probe
            .0
            .lock()
            .unwrap()
            .append(&vec![b'x'; INPUT_REPLY_MAX_BYTES * 3]);
        assert_eq!(probe.text().len(), INPUT_REPLY_MAX_BYTES);
        probe.0.lock().unwrap().append("한글😀".as_bytes());
        assert!(probe.text().ends_with("한글😀"));
        assert_eq!(probe.0.lock().unwrap().bytes.len(), INPUT_REPLY_MAX_BYTES);
        probe.close();
        probe.0.lock().unwrap().append(b"after cancellation");
        assert!(probe.text().is_empty());
        assert_eq!(probe.0.lock().unwrap().bytes.capacity(), 0);
    }

    #[test]
    fn pr8_pump_log_batch_limits_small_chunk_writes_without_losing_order() {
        let mut batch = RedactedLogBatch::default();
        let mut written = Vec::new();
        let mut calls = 0usize;
        let chunks: Vec<Vec<u8>> = (0..512)
            .map(|n| format!("{n:04}:{}\n", "x".repeat(122)).into_bytes())
            .collect();
        let expected: Vec<u8> = chunks.iter().flatten().copied().collect();
        for chunk in &chunks {
            batch
                .push(chunk, |bytes| {
                    calls += 1;
                    written.extend_from_slice(bytes);
                    Ok(written.len() as u64)
                })
                .unwrap();
        }
        batch
            .finish(|bytes| {
                calls += 1;
                written.extend_from_slice(bytes);
                Ok(written.len() as u64)
            })
            .unwrap();
        eprintln!("PR8 runtime512x128B append calls={calls}");
        assert_eq!(written, expected);
        assert!(
            calls <= 2,
            "pump issued {calls} small writes instead of bounded batches"
        );
    }

    #[test]
    fn pr8_measure_real_log_writer_batch_five_samples() {
        let chunks: Vec<Vec<u8>> = (0..512)
            .map(|n| format!("{n:04}:{}\n", "x".repeat(122)).into_bytes())
            .collect();
        let expected: Vec<u8> = chunks.iter().flatten().copied().collect();
        let mut samples: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
        for sample in 0..5 {
            for (mode, timings) in samples.iter_mut().enumerate() {
                let root = std::env::temp_dir().join(format!(
                    "deppy-pr8-batch-{}-{sample}-{mode}",
                    uuid::Uuid::new_v4()
                ));
                let mut writer = SessionLogWriter::open(&root, SessionId(1)).unwrap();
                let mut calls = 0;
                let started = Instant::now();
                if mode == 0 {
                    // Actual pre-PR8 call pattern, current storage: isolate only batching's effect.
                    for chunk in &chunks {
                        calls += 1;
                        writer.append_output(chunk).unwrap();
                    }
                } else {
                    let mut batch = RedactedLogBatch::default();
                    let mut append = |bytes: &[u8]| -> anyhow::Result<u64> {
                        calls += 1;
                        writer.append_output(bytes)?;
                        Ok(0)
                    };
                    for chunk in &chunks {
                        batch.push(chunk, &mut append).unwrap();
                    }
                    batch.finish(&mut append).unwrap();
                }
                writer.flush();
                let us = started.elapsed().as_secs_f64() * 1e6;
                assert_eq!(
                    std::fs::read(root.join("1/redacted.ansi.log")).unwrap(),
                    expected
                );
                assert_eq!(
                    std::fs::read(root.join("1/redacted.plain.txt")).unwrap(),
                    expected
                );
                assert_eq!(calls, if mode == 0 { 512 } else { 2 });
                eprintln!(
                    "PR8 realWriter sample{sample} mode{mode} append_output={calls} us={us:.3}"
                );
                timings.push(us);
                drop(writer);
                std::fs::remove_dir_all(root).unwrap();
            }
        }
        for (mode, timings) in samples.iter_mut().enumerate() {
            timings.sort_by(f64::total_cmp);
            eprintln!("PR8 realWriter median5 mode{mode} us={:.3}", timings[2]);
        }
    }

    #[test]
    fn pr8_partial_log_batch_failure_is_not_replayed() {
        let mut batch = RedactedLogBatch::default();
        batch
            .push(b"old", |_| panic!("must remain buffered"))
            .unwrap();
        let mut written = Vec::new();
        assert!(
            batch
                .finish(|bytes| {
                    written.extend_from_slice(&bytes[..1]);
                    anyhow::bail!("partial write")
                })
                .is_err()
        );
        assert!(
            batch
                .finish(|_| panic!("must not replay failed bytes"))
                .unwrap()
                .is_none()
        );
        batch
            .push("한글😀\x1b[31mnew".as_bytes(), |_| {
                panic!("must remain buffered")
            })
            .unwrap();
        batch
            .finish(|bytes| {
                written.extend_from_slice(bytes);
                Ok(written.len() as u64)
            })
            .unwrap();
        assert_eq!(written, "o한글😀\x1b[31mnew".as_bytes());
        assert!(batch.bytes.capacity() <= REDACTED_LOG_BATCH_BYTES);
    }

    use super::*;
    use crate::command::{SplitDirection, WorkspaceRuntimeState};
    use crate::test_secret_store::test_store;
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
    fn live_scrollback_시작시_한도로_복원하고_증가후_새출력을_보존한다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _) = admission_worker(resolver, "live-startup-ceiling");
        let path = SessionLogWriter::ansi_path(&worker.logs_root, "persistent-session").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            (0..1000).map(|i| format!("old{i}\r\n")).collect::<String>(),
        )
        .unwrap();
        worker.apply_scrollback_policy(1, 100);
        let id = SessionId(1);
        let mut session = Session::restore_archived(
            id,
            session::SessionKind::Shell,
            20,
            5,
            100,
            Some(0),
            &mut &b""[..],
        );
        worker.replay_saved_ansi("persistent-session", &mut session);
        assert!(session.cache_footprint().history_lines <= 100);
        worker.insert_session(id, session);
        worker.apply_scrollback_policy(2, 5000);
        assert!(worker.sessions[&id].cache_footprint().history_lines <= 100);
        let new = (0..1000).map(|i| format!("new{i}\r\n")).collect::<String>();
        worker
            .sessions
            .get_mut(&id)
            .unwrap()
            .replay_ansi(&mut new.as_bytes())
            .unwrap();
        assert!(worker.sessions[&id].cache_footprint().history_lines >= 1000);
        let dump = String::from_utf8(worker.sessions[&id].serialize_scrollback().unwrap()).unwrap();
        assert!(dump.contains("new100"));
        assert!(!dump.contains("old100\r"));
        std::fs::remove_dir_all(&worker.logs_root).unwrap();
    }

    #[test]
    fn live_scrollback_독립로그는_재시작시_현재설정으로_재생하며_영속삭제를_약속하지_않는다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _) = admission_worker(resolver.clone(), "live-restart-boundary");
        let path = SessionLogWriter::ansi_path(&worker.logs_root, "persistent-session").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text = (0..1000).map(|i| format!("old{i}\r\n")).collect::<String>();
        std::fs::write(&path, &text).unwrap();
        worker.apply_scrollback_policy(1, 100);
        worker.apply_scrollback_policy(2, 5000);
        let root = worker.logs_root.clone();
        drop(worker);
        let (mut restarted, events) = admission_worker(resolver, "live-restarted-boundary");
        restarted.logs_root = root.clone();
        restarted.apply_scrollback_policy(1, 5000);
        let mut session = Session::restore_archived(
            SessionId(1),
            session::SessionKind::Shell,
            20,
            5,
            5000,
            Some(0),
            &mut &b""[..],
        );
        restarted.replay_saved_ansi("persistent-session", &mut session);
        assert!(session.cache_footprint().history_lines > 100);
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
        assert!(matches!(
            events.try_recv().unwrap(),
            RuntimeEvent::ScrollbackLimitApplied { durable: false, .. }
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archive_limit_자동_exit의_보존실패만_pane을_남기고_명시적_close는_폐기한다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _) = admission_worker(resolver, "archive-auto-exit-failed");
        let id = SessionId(1);
        worker.sessions.insert(
            id,
            Session::restore_archived(
                id,
                session::SessionKind::Shell,
                20,
                5,
                100,
                Some(0),
                &mut &b"LATEST"[..],
            ),
        );
        worker.attach_in_new_tab(id, SHELL_TITLE_ID);
        worker.close_exited_pane_with_budget(id, 0);
        assert!(worker.sessions[&id].screen_text().contains("LATEST"));
        assert!(worker.archive_failed.contains(&id));
        let pane = worker
            .mux
            .panes
            .values()
            .find(|pane| pane.session_id == Some(id))
            .unwrap()
            .id
            .clone();
        worker.close_exited_pane(id);
        assert!(
            worker.sessions.contains_key(&id),
            "실패한 불변 화면은 매 pump 재시도하지 않는다"
        );
        worker.close_pane(pane);
        assert!(!worker.sessions.contains_key(&id));
        assert!(!worker.archive_failed.contains(&id));
    }

    #[test]
    fn archive_limit_닫힌_pane의_memory_tail도_개수와_바이트_lru를_따른다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _) = admission_worker(resolver, "archive-auto-exit-lru");
        for index in 0..=RUNTIME_SESSION_CAP {
            let id = SessionId(index as u64);
            worker.archived_order.push_back(id);
            worker.archived.insert(
                id,
                ArchivedScrollback {
                    kind: session::SessionKind::Shell,
                    cols: 20,
                    rows: 5,
                    scrollback_lines: 100,
                    exit_code: Some(0),
                    compressed: vec![0],
                },
            );
        }
        worker.trim_archived_budget();
        assert_eq!(worker.archived.len(), RUNTIME_SESSION_CAP);
        assert!(!worker.archived.contains_key(&SessionId(0)));
        let newest = SessionId(RUNTIME_SESSION_CAP as u64 + 1);
        worker.archived_order.push_back(newest);
        worker.archived.insert(
            newest,
            ArchivedScrollback {
                kind: session::SessionKind::Shell,
                cols: 20,
                rows: 5,
                scrollback_lines: 100,
                exit_code: Some(0),
                compressed: vec![0; ARCHIVED_SCROLLBACK_BUDGET_BYTES],
            },
        );
        worker.trim_archived_budget();
        assert_eq!(worker.archived.len(), 1);
        assert!(
            worker.archived.contains_key(&newest),
            "최신 tail을 남기고 가장 오래된 entry부터 제거한다"
        );
    }

    #[test]
    fn archive_limit_자동_exit도_disk가_없으면_memory_tail을_먼저_보존한다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _) = admission_worker(resolver, "archive-auto-exit");
        let id = SessionId(1);
        let text = (0..1000)
            .map(|i| format!("line{i}\r\n"))
            .collect::<String>()
            + "LATEST";
        worker.sessions.insert(
            id,
            Session::restore_archived(
                id,
                session::SessionKind::Shell,
                20,
                5,
                5000,
                Some(0),
                &mut text.as_bytes(),
            ),
        );
        worker.attach_in_new_tab(id, SHELL_TITLE_ID);
        worker.close_exited_pane(id);
        assert!(
            worker.archived.contains_key(&id),
            "자동 close도 보존 성공 전에 supported backend를 버리면 안 된다"
        );
        assert!(!worker.sessions.contains_key(&id));
        assert!(
            !worker
                .mux
                .panes
                .values()
                .any(|pane| pane.session_id == Some(id)),
            "정상 exit의 pane 닫힘 계약은 유지한다"
        );
        let dump = inflate_archived_bounded(
            &worker.archived[&id].compressed,
            MAX_ARCHIVED_SCROLLBACK_RESTORE_BYTES,
        )
        .unwrap();
        assert!(dump.windows(6).any(|window| window == b"LATEST"));
    }

    #[test]
    fn archive_limit_압축후_예산도_맞춰_새_archive가_즉시_축출되지_않는다() {
        let text = (0..1000)
            .map(|i| format!("row{i:04}-{}\r\n", i * 7919))
            .collect::<String>()
            + "LATEST";
        let mut live = Session::restore_archived(
            SessionId(1),
            session::SessionKind::Shell,
            30,
            5,
            5000,
            Some(0),
            &mut text.as_bytes(),
        );
        let entry = Worker::make_archive_entry_with_budget(&mut live, 512).unwrap();
        assert!(entry.compressed.len() <= 512);
        let dump =
            inflate_archived_bounded(&entry.compressed, MAX_ARCHIVED_SCROLLBACK_RESTORE_BYTES)
                .unwrap();
        assert!(dump.windows(6).any(|window| window == b"LATEST"));
    }

    #[test]
    fn archive_limit_초과한_100k_이력도_pane과_최신_tail을_보존한다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _) = admission_worker(resolver, "archive-oversize-tail");
        let id = SessionId(1);
        // 셀마다 색이 바뀌어 정상 100k 이력의 ANSI만 32MiB를 초과한다.
        let line = "\x1b[38;2;1;2;3mA\x1b[38;2;4;5;6mB".repeat(10) + "\r\n";
        let text = line.repeat(100_000) + "\x1b[0mLATEST-END";
        let live = Session::restore_archived(
            id,
            session::SessionKind::Shell,
            20,
            5,
            100_000,
            Some(0),
            &mut text.as_bytes(),
        );
        assert!(
            live.serialize_scrollback().is_none(),
            "실제 32MiB 초과 fixture"
        );
        worker.sessions.insert(id, live);
        worker.attach_in_new_tab(id, SHELL_TITLE_ID);
        worker.attach_in_new_tab(SessionId(2), SHELL_TITLE_ID);
        worker.exited_order.push_back(id);
        worker.max_exited_backends = 0;
        worker.archive_over_cap();
        assert!(
            worker.archived.contains_key(&id),
            "초과는 미지원처럼 버리면 안 된다"
        );
        assert!(
            worker
                .mux
                .panes
                .values()
                .any(|pane| pane.session_id == Some(id))
        );
        assert!(!worker.sessions.contains_key(&id));
        let entry = &worker.archived[&id];
        let dump =
            inflate_archived_bounded(&entry.compressed, MAX_ARCHIVED_SCROLLBACK_RESTORE_BYTES)
                .unwrap();
        assert!(
            dump.windows(b"LATEST-END".len())
                .any(|window| window == b"LATEST-END")
        );
        worker.inflate_archived(id);
        assert!(worker.sessions[&id].screen_text().contains("LATEST-END"));

        // 같은 초과 fixture를 디스크 경로에도 넣고 독립 감사 로그가 바뀌지 않는지 확인한다.
        let db_path = worker.logs_root.join("metadata.sqlite3");
        create_persist_db(&db_path, "archive-tail");
        let mut pipe = crate::persistence::PersistPipe::open(&crate::persistence::PersistConfig {
            db_path,
            workspace_id: "archive-tail".to_owned(),
        })
        .unwrap();
        pipe.session_spawned(
            id,
            "shell",
            None,
            "archive-tail",
            "/bin/sh",
            &[],
            "/tmp",
            None,
            None,
            None,
            None,
        );
        let key = pipe.session_log_key(id).unwrap().to_owned();
        worker.persist = Some(pipe);
        let audit = SessionLogWriter::ansi_path(&worker.logs_root, &key).unwrap();
        std::fs::create_dir_all(audit.parent().unwrap()).unwrap();
        std::fs::write(&audit, b"AUDIT-UNCHANGED").unwrap();
        worker.sessions.insert(
            id,
            Session::restore_archived(
                id,
                session::SessionKind::Shell,
                20,
                5,
                100_000,
                Some(0),
                &mut text.as_bytes(),
            ),
        );
        worker.write_scrollback_archive(id);
        assert!(worker.archived_on_disk.contains_key(&id));
        let mut archive = storage::scrollback_archive::open(&worker.logs_root, &key)
            .unwrap()
            .unwrap();
        let mut disk_dump = Vec::new();
        std::io::Read::read_to_end(&mut archive, &mut disk_dump).unwrap();
        assert!(archive.finish());
        assert!(disk_dump.len() <= MAX_ARCHIVED_SCROLLBACK_RESTORE_BYTES);
        assert!(disk_dump.windows(10).any(|window| window == b"LATEST-END"));
        assert_eq!(std::fs::read(audit).unwrap(), b"AUDIT-UNCHANGED");
    }

    #[test]
    fn live_scrollback_증가후_새세션의_archive를_이전최소값으로_자르지_않는다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _) = admission_worker(resolver, "live-new-archive");
        worker.apply_scrollback_policy(1, 100);
        worker.apply_scrollback_policy(2, 5000);
        let id = SessionId(1);
        let text = (0..1000).map(|i| format!("new{i}\r\n")).collect::<String>();
        let mut session = Session::restore_archived(
            id,
            session::SessionKind::Shell,
            20,
            5,
            5000,
            Some(0),
            &mut text.as_bytes(),
        );
        let history = session.cache_footprint().history_lines;
        let entry = Worker::make_archive_entry(&mut session).unwrap();
        worker.archived.insert(id, entry);
        worker.inflate_archived(id);
        assert_eq!(
            worker.sessions[&id].cache_footprint().history_lines,
            history
        );
    }

    #[test]
    fn live_scrollback_세션삭제는_동일세대_ack_집계를_갱신한다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, events) = admission_worker(resolver, "live-remove-count");
        worker.apply_scrollback_policy(1, 100);
        let id = SessionId(1);
        worker.insert_session(
            id,
            Session::restore_archived(
                id,
                session::SessionKind::Shell,
                20,
                5,
                100,
                Some(0),
                &mut &b""[..],
            ),
        );
        while events.try_recv().is_ok() {}
        worker.remove_session(id);
        assert!(matches!(
            events.try_recv().unwrap(),
            RuntimeEvent::ScrollbackLimitApplied {
                generation: 1,
                applied: 0,
                unsupported: 0,
                ..
            }
        ));
    }

    #[test]
    fn live_scrollback_새_spawn은_최신값을_쓰고_pending_identity만_낮은값을_유지한다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _) = admission_worker(resolver, "live-future-spawn");
        worker.shell = spec("/bin/sh", &["-c", "true"]);
        worker
            .pending_scrollback_ceilings
            .insert("old".to_owned(), 100);
        worker.apply_scrollback_policy(1, 5000);
        assert_eq!(worker.pending_scrollback_ceilings["old"], 100);
        let (spec, _leases) = worker.shell_with_session(SessionId(1)).unwrap();
        let new = worker
            .spawn_session(SessionId(1), session::SessionKind::Shell, &spec, 20, 5, 100)
            .unwrap();
        assert_eq!(new.cache_footprint().scrollback_limit_lines, 5000);
    }

    #[test]
    fn live_scrollback_검토_세션편입마다_전역_예산을_확인한다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _) = admission_worker(resolver, "live-incremental-budget");
        let text = (0..10000)
            .map(|i| format!("line{i:05}-abcdefghijk\r\n"))
            .collect::<String>();
        for index in 1..=3 {
            let id = SessionId(index);
            let session = Session::restore_archived(
                id,
                session::SessionKind::Shell,
                40,
                5,
                50000,
                Some(0),
                &mut text.as_bytes(),
            );
            if index == 1 {
                worker.cache_budget_bytes = session.cache_footprint().estimated_bytes * 3 / 2;
            }
            worker.insert_session(id, session);
            assert!(
                worker.terminal_cache_bytes() <= worker.cache_budget_bytes,
                "다음세션복원전에예산을맞춰야한다"
            );
        }
    }

    #[test]
    fn live_scrollback_낡은세대와_동일세대_충돌을_무시한다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _) = admission_worker(resolver, "live-generation");
        worker.apply_scrollback_policy(2, 5000);
        worker.apply_scrollback_policy(1, 100);
        assert_eq!(worker.requested_scrollback(1000), 5000);
        worker.apply_scrollback_policy(2, 100);
        assert_eq!(worker.requested_scrollback(1000), 5000);
    }

    #[test]
    fn live_scrollback_현재_세션에_적용한_뒤_ack한다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, events) = admission_worker(resolver, "live-scrollback");
        let id = SessionId(1);
        let text = (0..500).map(|i| format!("line{i}\r\n")).collect::<String>();
        worker.sessions.insert(
            id,
            Session::restore_archived(
                id,
                session::SessionKind::Shell,
                20,
                5,
                1000,
                Some(0),
                &mut text.as_bytes(),
            ),
        );
        worker.handle_command(RuntimeCommand::SetScrollbackLimit {
            generation: 7,
            requested: 100,
        });
        assert_eq!(worker.sessions[&id].cache_footprint().history_lines, 100);
        let mut found = false;
        while let Ok(event) = events.try_recv() {
            if let RuntimeEvent::ScrollbackLimitApplied {
                generation,
                requested,
                applied,
                unsupported,
                trimmed,
                ..
            } = event
            {
                assert_eq!(
                    (generation, requested, applied, unsupported),
                    (7, 100, 1, 0)
                );
                assert!(trimmed > 0);
                found = true;
            }
        }
        assert!(found, "실제 적용 결과 ACK가 필요하다");
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
        let archive_root_identity = storage::scrollback_archive::root_identity(&logs_root)
            .ok()
            .flatten();
        (
            Worker {
                command_rx,
                subscribers,
                batch: Duration::from_millis(5),
                shell: spec("/bin/true", &[]),
                default_env_plain: Vec::new(),
                default_env_secrets: Vec::new(),
                default_api_secrets: Vec::new(),
                environment_revision: None,
                secret_versions: Vec::new(),
                dotenv_source: None,
                workspace_id: "workspace".to_owned(),
                next_id: 1,
                sessions: std::collections::HashMap::new(),
                input_reply_probes: std::collections::HashMap::new(),
                resize_epoch: 0,
                resize_records: std::collections::HashMap::new(),
                session_redaction_leases: std::collections::HashMap::new(),
                seed_redaction_lease: None,
                secret_resolver: resolver,
                logs: std::collections::HashMap::new(),
                detectors: std::collections::HashMap::new(),
                agent_exit_watch: std::collections::HashMap::new(),
                status_overrides: std::collections::HashMap::new(),
                run_logs_root: logs_root.join("run"),
                logs_root,
                redaction: RedactionService::new(),
                mux: MuxState::new(),
                tab_counter: 0,
                persist: None,
                persist_db_path: None,
                lazy_restore: None,
                exited_order: std::collections::VecDeque::new(),
                max_exited_backends: DEFAULT_MAX_EXITED_BACKENDS,
                cache_budget_bytes: TERMINAL_GLOBAL_CACHE_BUDGET_BYTES,
                scrollback_policy: None,
                scrollback_results: std::collections::HashMap::new(),
                scrollback_trimmed: 0,
                pending_scrollback_ceilings: std::collections::HashMap::new(),
                scrollback_batching: false,
                scrollback_ack_pending: false,
                scrollback_restored: false,
                archived: std::collections::HashMap::new(),
                archived_order: std::collections::VecDeque::new(),
                archived_on_disk: std::collections::HashMap::new(),
                archive_failed: std::collections::HashSet::new(),
                archive_disk_bytes: 0,
                archive_root_identity,
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

    struct UnattachedHarness {
        worker: Worker,
        events: std::sync::mpsc::Receiver<RuntimeEvent>,
        _viewports: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>>,
        _input_pressures: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>>,
        _resource_usage: Arc<Mutex<Option<RuntimeEvent>>>,
    }

    impl UnattachedHarness {
        fn new(name: &str) -> Self {
            let resolver = Arc::new(RecordingResolver {
                calls: Mutex::new(Vec::new()),
                value: Some("runtime-secret-value".to_owned()),
            });
            let (mut worker, events) = admission_worker(resolver, name);
            worker.shell = spec("/bin/cat", &[]);
            let (viewports, input_pressures, resource_usage) = {
                let subscribers = worker.subscribers.lock().expect("subscribers lock");
                let subscriber = subscribers.first().expect("test subscriber");
                (
                    Arc::clone(&subscriber.viewports),
                    Arc::clone(&subscriber.input_pressures),
                    Arc::clone(&subscriber.resource_usage),
                )
            };
            Self {
                worker,
                events,
                _viewports: viewports,
                _input_pressures: input_pressures,
                _resource_usage: resource_usage,
            }
        }

        fn spawn_attached(&mut self) -> SessionId {
            self.worker.handle_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            });
            let events = self.events.try_iter().collect::<Vec<_>>();
            if let Some(session) = events.iter().find_map(|event| match event {
                RuntimeEvent::ShellSpawned { session } => Some(*session),
                _ => None,
            }) {
                return session;
            }
            let failure = events.iter().find_map(|event| match event {
                RuntimeEvent::SpawnFailed { message, .. } => Some(message),
                _ => None,
            });
            panic!(
                "real worker must emit ShellSpawned; sessions={}, failure={failure:?}",
                self.worker.sessions.len()
            );
        }

        fn spawn_unattached(&mut self) -> SessionId {
            let session = self.spawn_attached();
            let pane = self
                .worker
                .mux
                .panes
                .values_mut()
                .find(|pane| pane.session_id == Some(session))
                .expect("spawned session must have a pane");
            pane.session_id = None;
            session
        }

        fn set_remote_viewing(&mut self, session: SessionId, viewing: bool) {
            self.set_remote_viewing_ttl(session, viewing, 60_000);
        }

        fn set_remote_viewing_ttl(&mut self, session: SessionId, viewing: bool, ttl_ms: u32) {
            self.worker
                .handle_command(RuntimeCommand::SetRemoteViewing {
                    session,
                    viewing,
                    ttl_ms,
                });
            let _ = self.events.try_iter().count();
        }

        fn expire_remote_viewing_without_pump(&mut self, session: SessionId) {
            *self
                .worker
                .remote_viewing
                .get_mut(&session)
                .expect("remote viewing lease") = Instant::now();
        }

        fn attach_new_pane(&mut self, session: SessionId) {
            self.worker.attach_in_new_tab(session, SHELL_TITLE_ID);
        }

        fn inspect_unattached(&mut self) -> u16 {
            self.worker
                .handle_command(RuntimeCommand::InspectUnattachedSessions);
            self.events
                .try_iter()
                .find_map(|event| match event {
                    RuntimeEvent::UnattachedSessionsInspected { count } => Some(count),
                    _ => None,
                })
                .expect("inspect command must emit a result")
        }

        fn kill_unattached(&mut self) -> u16 {
            self.worker
                .handle_command(RuntimeCommand::KillUnattachedSessions);
            self.events
                .try_iter()
                .find_map(|event| match event {
                    RuntimeEvent::UnattachedSessionsKilled { count } => Some(count),
                    _ => None,
                })
                .expect("kill command must emit a result")
        }

        fn session_exists(&self, session: SessionId) -> bool {
            self.worker.sessions.contains_key(&session)
        }
    }

    #[test]
    #[cfg(unix)]
    fn inspect_unattached_excludes_mux_and_remote_viewed_sessions() {
        let mut harness = UnattachedHarness::new("inspect-unattached");
        let attached = harness.spawn_attached();
        let remote = harness.spawn_unattached();
        harness.set_remote_viewing(remote, true);
        let orphan = harness.spawn_unattached();

        assert_eq!(harness.inspect_unattached(), 1);
        assert!(harness.session_exists(attached));
        assert!(harness.session_exists(remote));
        assert!(harness.session_exists(orphan));
    }

    #[test]
    #[cfg(unix)]
    fn inspect_unattached_treats_zero_ttl_remote_lease_as_expired_in_same_burst() {
        let mut harness = UnattachedHarness::new("inspect-zero-ttl");
        let candidate = harness.spawn_unattached();
        harness.set_remote_viewing(candidate, true);
        harness.set_remote_viewing_ttl(candidate, true, 0);

        assert_eq!(harness.inspect_unattached(), 1);
        assert!(harness.session_exists(candidate));
    }

    #[test]
    #[cfg(unix)]
    fn kill_unattached_treats_expired_remote_lease_as_candidate_without_pump() {
        let mut harness = UnattachedHarness::new("kill-expired-lease");
        let candidate = harness.spawn_unattached();
        harness.set_remote_viewing(candidate, true);
        harness.expire_remote_viewing_without_pump(candidate);

        assert_eq!(harness.kill_unattached(), 1);
        assert!(!harness.session_exists(candidate));
    }

    #[test]
    #[cfg(unix)]
    fn kill_unattached_recomputes_after_a_session_becomes_attached() {
        let mut harness = UnattachedHarness::new("reattach-before-kill");
        let candidate = harness.spawn_unattached();
        assert_eq!(harness.inspect_unattached(), 1);
        harness.attach_new_pane(candidate);

        assert_eq!(harness.kill_unattached(), 0);
        assert!(harness.session_exists(candidate));
    }

    #[test]
    #[cfg(unix)]
    fn kill_unattached_removes_only_current_local_candidates() {
        let mut harness = UnattachedHarness::new("kill-unattached");
        let attached = harness.spawn_attached();
        let orphan = harness.spawn_unattached();

        assert_eq!(harness.kill_unattached(), 1);
        assert!(harness.session_exists(attached));
        assert!(!harness.session_exists(orphan));
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
    #[cfg(unix)]
    fn agent_right_split_preserves_anchor_tab_even_when_another_tab_is_active() {
        let mut harness = UnattachedHarness::new("agent-right-split");
        let (worker, events) = (&mut harness.worker, &harness.events);
        worker.shell = spec("/bin/cat", &[]);
        worker.handle_command(RuntimeCommand::SpawnShell {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
        });
        let original = worker.mux.snapshot(|_| None);
        let original_tab = original.active_tab.clone().unwrap();
        let original_pane = original.focused_pane.clone().unwrap();
        let original_session = original.tabs[0].panes[0].session_id;
        events.try_iter().for_each(drop);
        worker.handle_command(RuntimeCommand::SpawnShell {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
        });
        assert_ne!(worker.mux.window.active_tab, Some(original_tab.clone()));
        events.try_iter().for_each(drop);

        let mut launch = correlated_agent_command();
        if let RuntimeCommand::SpawnAgent { command, .. } = &mut launch {
            *command = "/bin/cat".to_owned();
        }
        worker.handle_command(launch.with_right_split(original_pane.clone()).unwrap());

        let resolved = events.try_iter().collect::<Vec<_>>();
        let failure = resolved.iter().find_map(|event| match event {
            RuntimeEvent::SpawnFailed { message, .. } => Some(message.message_id.as_str()),
            _ => None,
        });
        assert!(
            resolved.iter().any(|event| matches!(
                event,
                RuntimeEvent::AgentSpawnResolved {
                    session: Some(_),
                    ..
                }
            )),
            "agent must resolve successfully; failure={failure:?}"
        );
        let snapshot = worker.mux.snapshot(|_| None);
        assert_eq!(snapshot.tabs.len(), 2, "launch must not create a third tab");
        assert_eq!(snapshot.active_tab, Some(original_tab.clone()));
        let tab = snapshot
            .tabs
            .iter()
            .find(|tab| tab.id == original_tab)
            .unwrap();
        assert_eq!(tab.panes.len(), 2);
        assert_eq!(tab.panes[0].id, original_pane);
        assert_eq!(tab.panes[0].session_id, original_session);
        let right = tab.panes[1].id.clone();
        assert_eq!(snapshot.focused_pane, Some(right.clone()));
        assert!(
            matches!(&tab.layout, mux::LayoutNode::Split { direction: mux::SplitDirection::Horizontal, first, second, .. }
            if **first == mux::LayoutNode::Pane(original_pane) && **second == mux::LayoutNode::Pane(right))
        );
        assert_eq!(worker.sessions.len(), 3);
    }

    #[test]
    #[cfg(unix)]
    fn shell_right_split_reveals_anchor_tab_when_another_tab_became_active() {
        let mut harness = UnattachedHarness::new("shell-right-split-tab-focus");
        let (worker, events) = (&mut harness.worker, &harness.events);
        worker.shell = spec("/bin/cat", &[]);
        worker.handle_command(RuntimeCommand::SpawnShell {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
        });
        let before = worker.mux.snapshot(|_| None);
        let target_tab = before.active_tab.clone().unwrap();
        let target_pane = before.focused_pane.clone().unwrap();
        let target_session = before.tabs[0].panes[0].session_id;
        events.try_iter().for_each(drop);
        worker.handle_command(RuntimeCommand::SpawnShell {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
        });
        assert_ne!(worker.mux.window.active_tab, Some(target_tab.clone()));
        events.try_iter().for_each(drop);

        worker.handle_command(RuntimeCommand::SplitPane {
            pane: target_pane.clone(),
            direction: mux::SplitDirection::Horizontal,
            scrollback_lines: 100,
        });
        assert!(
            events
                .try_iter()
                .any(|event| matches!(event, RuntimeEvent::ShellSpawned { .. })),
            "owned shell must spawn successfully"
        );
        let after = worker.mux.snapshot(|_| None);
        assert_eq!(
            after.tabs.len(),
            2,
            "right split must not create another tab"
        );
        assert_eq!(
            after.active_tab,
            Some(target_tab.clone()),
            "new right pane must be visible even after active tab changed"
        );
        let target = after.tabs.iter().find(|tab| tab.id == target_tab).unwrap();
        assert_eq!(target.panes.len(), 2);
        assert_eq!(target.panes[0].id, target_pane);
        assert_eq!(target.panes[0].session_id, target_session);
        assert_eq!(after.focused_pane, Some(target.panes[1].id.clone()));
        assert!(matches!(&target.layout,
            mux::LayoutNode::Split { direction: mux::SplitDirection::Horizontal, first, second, .. }
            if **first == mux::LayoutNode::Pane(target.panes[0].id.clone())
                && **second == mux::LayoutNode::Pane(target.panes[1].id.clone())));
        assert_eq!(worker.sessions.len(), 3);
    }

    #[test]
    fn agent_right_split_stale_target_rejects_before_secret_or_process_creation() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: Some("runtime-secret-value".to_owned()),
        });
        let (mut worker, events) = admission_worker(resolver.clone(), "agent-right-split-stale");
        let initial_id = worker.next_id;
        worker.handle_command(
            correlated_agent_command()
                .with_right_split(MuxPaneId("closed-pane".to_owned()))
                .unwrap(),
        );
        assert!(resolver.calls.lock().unwrap().is_empty());
        assert_eq!(worker.next_id, initial_id);
        assert!(worker.sessions.is_empty());
        assert!(worker.mux.tabs.is_empty());
        assert!(events.try_iter().any(|event| matches!(
            event,
            RuntimeEvent::AgentSpawnResolved { session: None, .. }
        )));
    }

    #[test]
    #[cfg(unix)]
    fn agent_right_split_spawn_failure_keeps_original_layout_and_session() {
        let mut harness = UnattachedHarness::new("agent-right-split-failure");
        let (worker, events) = (&mut harness.worker, &harness.events);
        worker.shell = spec("/bin/cat", &[]);
        worker.handle_command(RuntimeCommand::SpawnShell {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
        });
        events.try_iter().for_each(drop);
        let before = worker.mux.snapshot(|_| None);
        let target = before.focused_pane.clone().unwrap();
        let mut command = correlated_agent_command();
        if let RuntimeCommand::SpawnAgent { command, .. } = &mut command {
            *command = "/definitely-missing-deppy-test-agent".to_owned();
        }
        worker.handle_command(command.with_right_split(target).unwrap());
        assert_eq!(worker.mux.snapshot(|_| None), before);
        assert_eq!(worker.sessions.len(), 1);
        assert!(events.try_iter().any(|event| matches!(
            event,
            RuntimeEvent::AgentSpawnResolved { session: None, .. }
        )));
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
    #[cfg(unix)]
    fn credential_env_실제_agent_프로세스에_연결한_키를_전달한다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: Some("api-test-secret-value".into()),
        });
        let client = InProcessRuntimeClient::try_with_shell_and_resolver(
            5,
            resolver,
            test_logs_root("credential-env-spawn"),
            RedactionService::new(),
            pty::default_shell(),
            None,
        )
        .unwrap();
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SetSessionDefaultEnv {
                environment_revision: None,
                secret_versions: Vec::new(),
                dotenv_source: None,
                env_plain: Vec::new(),
                env_secrets: Vec::new(),
                api_secrets: vec![("SERVICE_KEY".into(), "api-test-credential".into())],
            })
            .unwrap();
        client
            .send_command(RuntimeCommand::SpawnAgent {
                agent_config_id: None,
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                command: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "test \"$SERVICE_KEY\" = api-test-secret-value && printf API_BINDING_OK".into(),
                ],
                env_plain: Vec::new(),
                env_secrets: Vec::new(),
                waiting_regex: None,
                approval_regex: None,
                error_regex: None,
                done_regex: None,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::Viewport { snapshot, .. }
                if snapshot_text(snapshot, 0).contains("API_BINDING_OK") =>
            {
                Some(())
            }
            _ => None,
        });
    }

    #[test]
    fn environment_application_캡처후_비밀회전은_이전버전_spawn을_막는다() {
        struct RotatingResolver(std::sync::atomic::AtomicU64);
        impl RuntimeSecretResolver for RotatingResolver {
            fn resolve(&self, id: &str) -> anyhow::Result<RuntimeSecret> {
                self.resolve_versioned(id).map(|(value, _)| value)
            }
            fn resolve_versioned(
                &self,
                _: &str,
            ) -> anyhow::Result<(RuntimeSecret, Option<String>)> {
                let generation = self.0.load(std::sync::atomic::Ordering::SeqCst);
                Ok((
                    RuntimeSecret::new(format!("fake-test-value-{generation}")),
                    Some(format!("slot-{generation}")),
                ))
            }
        }
        let resolver = Arc::new(RotatingResolver(std::sync::atomic::AtomicU64::new(1)));
        let (mut worker, events) = admission_worker(resolver.clone(), "environment-rotation");
        let defaults = |generation| RuntimeCommand::SetSessionDefaultEnv {
            secret_versions: vec![("logical".into(), format!("slot-{generation}"))],
            environment_revision: Some(generation),
            dotenv_source: None,
            api_secrets: vec![("API_TOKEN".into(), "logical".into())],
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
        };
        worker.handle_command(defaults(1));
        while events.try_recv().is_ok() {}
        assert!(worker.prepare_agent_env(Vec::new(), Vec::new()).is_ok());
        resolver.0.store(2, std::sync::atomic::Ordering::SeqCst);
        assert!(worker.prepare_agent_env(Vec::new(), Vec::new()).is_err());
        assert!(worker.shell_with_session(SessionId(1)).is_err());
        worker.handle_command(RuntimeCommand::SpawnShell {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
        });
        assert!(matches!(
            events.try_recv().unwrap(),
            RuntimeEvent::SpawnFailed { .. }
        ));
        assert!(events.try_recv().is_err());
        assert!(
            worker
                .resolve_secret_set(vec!["logical".into()], false)
                .is_ok()
        );
        worker.handle_command(defaults(2));
        assert!(worker.prepare_agent_env(Vec::new(), Vec::new()).is_ok());
    }

    #[test]
    fn environment_application_버전은_실제_spawn_성공에만_붙는다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, events) = admission_worker(resolver, "environment-applied");
        worker.handle_command(RuntimeCommand::SetSessionDefaultEnv {
            secret_versions: Vec::new(),
            environment_revision: Some(7),
            dotenv_source: None,
            api_secrets: Vec::new(),
            env_plain: vec![("FLAG".into(), "captured".into())],
            env_secrets: Vec::new(),
        });
        assert!(matches!(
            events.try_recv().unwrap(),
            RuntimeEvent::EnvironmentApplied {
                session: None,
                revision: Some(7)
            }
        ));
        worker.shell = spec("/bin/cat", &[]);
        let (shell, _leases) = worker.shell_with_session(SessionId(9)).unwrap();
        assert!(shell.env.contains(&("FLAG".into(), "captured".into())));
        let child = worker
            .spawn_session(
                SessionId(9),
                session::SessionKind::Shell,
                &shell,
                80,
                24,
                100,
            )
            .unwrap();
        assert!(matches!(
            events.try_recv().unwrap(),
            RuntimeEvent::EnvironmentApplied {
                session: Some(SessionId(9)),
                revision: Some(7)
            }
        ));
        drop(child);
        let invalid = spec("/nonexistent-deppy-test-command", &[]);
        assert!(
            worker
                .spawn_session(
                    SessionId(10),
                    session::SessionKind::Shell,
                    &invalid,
                    80,
                    24,
                    100
                )
                .is_err()
        );
        assert!(events.try_recv().is_err());
        let dir = std::env::temp_dir().join(format!("deppy-applied-file-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "FLAG=changed-after-capture\n").unwrap();
        assert!(worker.restored_dotenv_for_session(&dir).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn workspace_sources_복원은_pane_cwd보다_프로젝트_선택파일을_사용한다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _events) = admission_worker(resolver, "source-restore");
        let dir =
            std::env::temp_dir().join(format!("deppy-source-restore-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "PORT=default\n").unwrap();
        std::fs::write(dir.join(".env.dev"), "PORT=selected\n").unwrap();
        worker.dotenv_source = Some(crate::dotenv::DotenvSourceSelection {
            root: Some(dir.clone()),
            files: vec![".env.dev".into()],
        });
        assert_eq!(
            worker.restored_dotenv_for_session(&dir.join("other")),
            vec![("PORT".into(), "selected".into())]
        );
        worker.dotenv_source.as_mut().unwrap().files.clear();
        assert!(worker.restored_dotenv_for_session(&dir).is_empty());
        worker.dotenv_source.as_mut().unwrap().root = None;
        assert!(worker.restored_dotenv_for_session(&dir).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn credential_env_새_셸과_agent는_api연결을_주입하고_launch가_우선한다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: Some("bound-secret-value".into()),
        });
        let (mut worker, _events) = admission_worker(resolver, "credential-env-precedence");
        worker.handle_command(RuntimeCommand::SetSessionDefaultEnv {
            environment_revision: None,
            secret_versions: Vec::new(),
            dotenv_source: None,
            env_plain: vec![("SERVICE_KEY".into(), "file-value".into())],
            env_secrets: Vec::new(),
            api_secrets: vec![("SERVICE_KEY".into(), "bound-credential".into())],
        });
        let (env, _leases) = worker.prepare_agent_env(Vec::new(), Vec::new()).unwrap();
        assert_eq!(
            env.iter()
                .rev()
                .find(|(key, _)| key == "SERVICE_KEY")
                .unwrap()
                .1,
            "bound-secret-value"
        );
        let (env, _leases) = worker
            .prepare_agent_env(
                vec![("SERVICE_KEY".into(), "launch-value".into())],
                Vec::new(),
            )
            .unwrap();
        assert_eq!(
            env.iter()
                .rev()
                .find(|(key, _)| key == "SERVICE_KEY")
                .unwrap()
                .1,
            "launch-value"
        );
        let (shell, _leases) = worker.shell_with_session(SessionId(1)).unwrap();
        assert_eq!(
            shell
                .env
                .iter()
                .rev()
                .find(|(key, _)| key == "SERVICE_KEY")
                .unwrap()
                .1,
            "bound-secret-value"
        );
        let dir = std::env::temp_dir().join(format!("deppy-api-restore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".env"),
            "SERVICE_KEY=x\nPORT=1000\nDEPPY_ENV_LIVE_RELOAD=1\nDEPPY_PROJECT_ROOT=/tmp\n",
        )
        .unwrap();
        let restored = worker.restored_dotenv_for_session(&dir);
        assert_eq!(restored, vec![("PORT".into(), "1000".into())]);
        assert!(
            worker
                .acquire_dotenv_redaction_lease(&restored)
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(dir).unwrap();
        worker.handle_command(RuntimeCommand::SetSessionDefaultEnv {
            environment_revision: None,
            secret_versions: Vec::new(),
            dotenv_source: None,
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            api_secrets: Vec::new(),
        });
        assert!(
            worker
                .prepare_agent_env(Vec::new(), Vec::new())
                .unwrap()
                .0
                .is_empty()
        );
    }

    #[test]
    fn credential_env_키를_읽지못하면_agent_환경을_부분반환하지_않는다() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _events) = admission_worker(resolver, "credential-env-failed");
        worker.handle_command(RuntimeCommand::SetSessionDefaultEnv {
            environment_revision: None,
            secret_versions: Vec::new(),
            dotenv_source: None,
            env_plain: vec![("PORT".into(), "1000".into())],
            env_secrets: Vec::new(),
            api_secrets: vec![("SERVICE_KEY".into(), "missing".into())],
        });
        assert!(worker.prepare_agent_env(Vec::new(), Vec::new()).is_err());
        assert!(worker.shell_with_session(SessionId(1)).is_err());
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
            environment_revision: None,
            secret_versions: Vec::new(),
            dotenv_source: None,
            api_secrets: Vec::new(),
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

    #[test]
    fn pr18_secret_store_resolver_uses_only_its_injected_fixture() {
        let store = test_store();
        let empty_store = test_store();
        store
            .set_secret(
                "resolver-fixture",
                &secret::SecretString::new("private-value".into()),
            )
            .unwrap();
        let resolver = SecretStoreResolver(Arc::clone(&store));
        let value = resolver.resolve("resolver-fixture").unwrap();
        assert_eq!(value.as_secret_string().expose(), "private-value");
        assert_eq!(format!("{value:?}"), "RuntimeSecret(REDACTED)");
        assert!(
            SecretStoreResolver(empty_store)
                .resolve("resolver-fixture")
                .is_err()
        );
        store.delete_secret("resolver-fixture").unwrap();
        assert!(resolver.resolve("resolver-fixture").is_err());
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
        conn.execute_batch(persist::MIGRATION_SESSION_REGEX)
            .unwrap();
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
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        };
        persist::upsert_session(&conn, &row).unwrap();
        let window = persisted_single_pane_window(session_id, "restored", Some(cwd.to_owned()));
        persist::save_window_layout(&mut conn, workspace_id, &window).unwrap();
    }

    /// `seed_persisted_session_pane`과 같은 모양이지만 command/args를 직접 지정한다 —
    /// RespawnArchivedAgent가 저장된 launch spec을 그대로 쓰는지 관측하려면 고정된
    /// `/bin/sh -c "echo restored"`로는 부족하다(인자 순서를 눈으로 확인할 수 없다).
    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)]
    fn seed_persisted_agent_session_pane(
        db_path: &std::path::Path,
        workspace_id: &str,
        session_id: &str,
        agent_id: &str,
        command: &str,
        args: Vec<String>,
        status: &str,
        cwd: &str,
    ) {
        let mut conn = rusqlite::Connection::open(db_path).unwrap();
        let row = persist::SessionRow {
            id: session_id.to_owned(),
            workspace_id: workspace_id.to_owned(),
            session_kind: "agent".to_owned(),
            agent_id: Some(agent_id.to_owned()),
            title: "respawn-target".to_owned(),
            command: command.to_owned(),
            args,
            cwd: cwd.to_owned(),
            status: status.to_owned(),
            last_log_offset: 0,
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        };
        persist::upsert_session(&conn, &row).unwrap();
        let window =
            persisted_single_pane_window(session_id, "respawn-target", Some(cwd.to_owned()));
        persist::save_window_layout(&mut conn, workspace_id, &window).unwrap();
    }

    fn seed_persisted_two_pane_window(
        db_path: &std::path::Path,
        workspace_id: &str,
        first_kind: &str,
    ) -> (MuxPaneId, MuxPaneId) {
        let mut conn = rusqlite::Connection::open(db_path).unwrap();
        let first_session = "restore-pane-first";
        let second_session = "restore-pane-second";
        for (id, kind) in [(first_session, first_kind), (second_session, "shell")] {
            persist::upsert_session(
                &conn,
                &persist::SessionRow {
                    id: id.to_owned(),
                    workspace_id: workspace_id.to_owned(),
                    session_kind: kind.to_owned(),
                    agent_id: (kind == "agent").then(|| "cfg-sf03".to_owned()),
                    title: id.to_owned(),
                    command: "/bin/cat".to_owned(),
                    args: Vec::new(),
                    cwd: "/tmp".to_owned(),
                    status: if kind == "agent" {
                        persist::SESSION_STATUS_EXITED.to_owned()
                    } else {
                        persist::SESSION_STATUS_RUNNING.to_owned()
                    },
                    last_log_offset: 0,
                    waiting_regex: None,
                    approval_regex: None,
                    error_regex: None,
                    done_regex: None,
                },
            )
            .unwrap();
        }
        let first_pane = MuxPaneId("restore-pane-first".to_owned());
        let second_pane = MuxPaneId("restore-pane-second".to_owned());
        let first_tab = MuxTabId("restore-tab-first".to_owned());
        let second_tab = MuxTabId("restore-tab-second".to_owned());
        let window = persist::WindowState {
            id: deppy_core::MuxWindowId::new(),
            title: Some("lazy restore".to_owned()),
            active_tab: Some(first_tab.clone()),
            tabs: vec![
                persist::TabState {
                    id: first_tab.clone(),
                    title: "first".to_owned(),
                    layout: mux::LayoutNode::Pane(first_pane.clone()),
                    active_pane: Some(first_pane.clone()),
                    panes: vec![persist::PaneState {
                        id: first_pane.clone(),
                        session_id: Some(first_session.to_owned()),
                        title: "first".to_owned(),
                        pane_kind: mux::PaneKind::Terminal,
                        cwd: Some("/tmp".to_owned()),
                    }],
                },
                persist::TabState {
                    id: second_tab,
                    title: "second".to_owned(),
                    layout: mux::LayoutNode::Pane(second_pane.clone()),
                    active_pane: Some(second_pane.clone()),
                    panes: vec![persist::PaneState {
                        id: second_pane.clone(),
                        session_id: Some(second_session.to_owned()),
                        title: "second".to_owned(),
                        pane_kind: mux::PaneKind::Terminal,
                        cwd: Some("/tmp".to_owned()),
                    }],
                },
            ],
        };
        persist::save_window_layout(&mut conn, workspace_id, &window).unwrap();
        (first_pane, second_pane)
    }

    #[cfg(unix)]
    fn lazy_restore_durable_event_barrier_fixture(
        name: &str,
    ) -> (InProcessRuntimeClient, Probe, MuxPaneId, MuxPaneId, PathBuf) {
        let dir = unique_test_dir(name);
        let db_path = dir.join("metadata.sqlite3");
        let workspace_id = format!("ws-{name}");
        create_persist_db(&db_path, &workspace_id);
        let (first_pane, second_pane) =
            seed_persisted_two_pane_window(&db_path, &workspace_id, "shell");
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            dir.join("logs"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path,
                workspace_id,
            }),
        );
        let probe = Probe::new(client.subscribe());
        (client, probe, first_pane, second_pane, dir)
    }

    #[cfg(unix)]
    #[test]
    fn direct_mux_query_before_cold_restore_preserves_persisted_panes() {
        let name = "direct-mux-query-before-restore";
        let (client, mut probe, first_pane, second_pane, dir) =
            lazy_restore_durable_event_barrier_fixture(name);
        let conn = rusqlite::Connection::open(dir.join("metadata.sqlite3")).unwrap();
        let workspace_id = format!("ws-{name}");
        let saved = persist::load_workspace_restore_bounded(&conn, &workspace_id)
            .unwrap()
            .window
            .expect("seeded saved window");
        assert_eq!(saved.tabs.len(), 2);
        client
            .send_command(RuntimeCommand::RequestMuxSnapshot)
            .unwrap();
        client
            .send_command(RuntimeCommand::DurableEventBarrier {
                correlation_id: 401,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(5), |event| {
            matches!(
                event,
                RuntimeEvent::DurableEventBarrierReached {
                    correlation_id: 401
                }
            )
            .then_some(())
        });
        assert!(
            probe.seen.iter().any(|event| matches!(event,
                RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.is_empty()
            )),
            "the query must publish the empty live mux without materializing saved panes"
        );
        let after_query = persist::load_workspace_restore_bounded(&conn, &workspace_id)
            .unwrap()
            .window
            .expect("snapshot query erased the saved window");
        assert_eq!(
            after_query, saved,
            "read-only query erased persisted tabs/panes before Catalog could observe them"
        );

        // The catalog still observes saved panes and can request deferred restoration.
        client
            .send_command(RuntimeCommand::RestoreWorkspacePane {
                pane: first_pane.clone(),
            })
            .unwrap();
        let restored = probe.wait_for(Duration::from_secs(5), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .any(|pane| pane.id == first_pane && pane.session_id.is_some()) =>
            {
                Some(snapshot.clone())
            }
            _ => None,
        });
        assert!(
            restored
                .tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .any(|pane| pane.id == second_pane && pane.session_id.is_none())
        );
        drop(conn);
        drop(client);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn restore_workspace_pane_durable_event_barrier_follows_requested_mux_result() {
        let (client, mut probe, first_pane, _second_pane, dir) =
            lazy_restore_durable_event_barrier_fixture("restore-durable-barrier-order");

        client
            .send_command(RuntimeCommand::RestoreWorkspacePane {
                pane: first_pane.clone(),
            })
            .unwrap();
        client
            .send_command(RuntimeCommand::DurableEventBarrier { correlation_id: 41 })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| {
            matches!(
                event,
                RuntimeEvent::DurableEventBarrierReached { correlation_id: 41 }
            )
            .then_some(())
        });

        let restore_index = probe
            .seen
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::MuxUpdated { snapshot }
                        if snapshot.tabs.iter().flat_map(|tab| &tab.panes).any(|pane|
                            pane.id == first_pane && pane.session_id.is_some())
                )
            })
            .expect("requested restore mux result");
        let barrier_index = probe
            .seen
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::DurableEventBarrierReached { correlation_id: 41 }
                )
            })
            .expect("exact barrier");
        assert!(restore_index < barrier_index);

        drop(client);
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn stale_mux_can_precede_restore_while_durable_event_barrier_stays_after_requested_result() {
        let (client, mut probe, first_pane, second_pane, dir) =
            lazy_restore_durable_event_barrier_fixture("restore-durable-barrier-stale-mux");

        client
            .send_command(RuntimeCommand::RestoreWorkspacePane { pane: first_pane })
            .unwrap();
        client
            .send_command(RuntimeCommand::DurableEventBarrier { correlation_id: 50 })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| {
            matches!(
                event,
                RuntimeEvent::DurableEventBarrierReached { correlation_id: 50 }
            )
            .then_some(())
        });

        client
            .send_command(RuntimeCommand::RestoreWorkspacePane { pane: second_pane })
            .unwrap();
        client
            .send_command(RuntimeCommand::DurableEventBarrier { correlation_id: 51 })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| {
            matches!(
                event,
                RuntimeEvent::DurableEventBarrierReached { correlation_id: 51 }
            )
            .then_some(())
        });

        let stale_mux_index = probe
            .seen
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::MuxUpdated { snapshot }
                        if snapshot.tabs.iter().flat_map(|tab| &tab.panes)
                            .filter(|pane| pane.session_id.is_some()).count() == 1
                )
            })
            .expect("stale mux before requested restore");
        let first_barrier_index = probe
            .seen
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::DurableEventBarrierReached { correlation_id: 50 }
                )
            })
            .expect("first barrier");
        let requested_mux_index = probe
            .seen
            .iter()
            .enumerate()
            .find_map(|(index, event)| {
                (index > first_barrier_index
                    && matches!(
                        event,
                        RuntimeEvent::MuxUpdated { snapshot }
                            if snapshot.tabs.iter().flat_map(|tab| &tab.panes)
                                .filter(|pane| pane.session_id.is_some()).count() == 2
                    ))
                .then_some(index)
            })
            .expect("requested restore mux result");
        let requested_barrier_index = probe
            .seen
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::DurableEventBarrierReached { correlation_id: 51 }
                )
            })
            .expect("requested barrier");
        assert!(stale_mux_index < first_barrier_index);
        assert!(first_barrier_index < requested_mux_index);
        assert!(requested_mux_index < requested_barrier_index);

        drop(client);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn back_to_back_durable_event_barriers_preserve_fifo_and_exact_ids() {
        let client = InProcessRuntimeClient::new(
            5,
            test_store(),
            test_logs_root("back-to-back-barriers"),
            RedactionService::new(),
            None,
            None,
            Vec::new(),
        );
        let mut probe = Probe::new(client.subscribe());

        client
            .send_command(RuntimeCommand::DurableEventBarrier { correlation_id: 70 })
            .unwrap();
        client
            .send_command(RuntimeCommand::DurableEventBarrier { correlation_id: 71 })
            .unwrap();
        probe.wait_for(Duration::from_secs(5), |event| {
            matches!(
                event,
                RuntimeEvent::DurableEventBarrierReached { correlation_id: 71 }
            )
            .then_some(())
        });

        let ids = probe
            .seen
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::DurableEventBarrierReached { correlation_id } => {
                    Some(*correlation_id)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ids, [70, 71]);

        drop(client);
    }

    #[cfg(unix)]
    #[test]
    fn restore_workspace_pane_materializes_only_requested_panes_and_reuses_catalog() {
        let dir = unique_test_dir("lazy-restore-two-pane");
        let db_path = dir.join("metadata.sqlite3");
        let workspace_id = "ws-lazy-restore";
        create_persist_db(&db_path, workspace_id);
        let (first_pane, second_pane) =
            seed_persisted_two_pane_window(&db_path, workspace_id, "shell");
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            dir.join("logs"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path,
                workspace_id: workspace_id.to_owned(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());

        client
            .send_command(RuntimeCommand::RestoreWorkspacePane {
                pane: first_pane.clone(),
            })
            .unwrap();
        let first = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot.tabs.len() == 2
                    && snapshot
                        .tabs
                        .iter()
                        .flat_map(|tab| &tab.panes)
                        .any(|pane| pane.id == first_pane && pane.session_id.is_some()) =>
            {
                Some(snapshot.clone())
            }
            _ => None,
        });
        assert_eq!(
            first
                .tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .filter(|pane| pane.session_id.is_some())
                .count(),
            1
        );
        assert!(first.tabs.iter().any(|tab| {
            tab.layout.contains(&second_pane)
                && tab
                    .panes
                    .iter()
                    .any(|pane| pane.id == second_pane && pane.session_id.is_none())
        }));

        client
            .send_command(RuntimeCommand::RestoreWorkspacePane {
                pane: second_pane.clone(),
            })
            .unwrap();
        let second = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .filter(|pane| pane.session_id.is_some())
                    .count()
                    == 2 =>
            {
                Some(snapshot.clone())
            }
            _ => None,
        });
        assert!(
            second
                .tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .any(|pane| pane.id == second_pane && pane.session_id.is_some())
        );

        drop(client);
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn restore_workspace_pane_unknown_target_creates_no_partial_session() {
        let dir = unique_test_dir("lazy-restore-unknown");
        let db_path = dir.join("metadata.sqlite3");
        let workspace_id = "ws-lazy-restore-unknown";
        create_persist_db(&db_path, workspace_id);
        seed_persisted_two_pane_window(&db_path, workspace_id, "shell");
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            dir.join("logs"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path,
                workspace_id: workspace_id.to_owned(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());

        client
            .send_command(RuntimeCommand::RestoreWorkspacePane {
                pane: MuxPaneId("missing-pane".to_owned()),
            })
            .unwrap();
        let snapshot = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.len() == 2 => {
                Some(snapshot.clone())
            }
            _ => None,
        });
        assert!(
            snapshot
                .tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .all(|pane| pane.session_id.is_none())
        );

        drop(client);
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn restore_workspace_pane_spawn_failure_preserves_persisted_association() {
        let dir = unique_test_dir("lazy-restore-spawn-failure");
        let db_path = dir.join("metadata.sqlite3");
        let workspace_id = "ws-lazy-restore-spawn-failure";
        create_persist_db(&db_path, workspace_id);
        seed_persisted_session_pane(
            &db_path,
            workspace_id,
            "persisted-shell",
            "shell",
            None,
            persist::SESSION_STATUS_RUNNING,
            "/tmp",
        );
        let pane_id = MuxPaneId(
            rusqlite::Connection::open(&db_path)
                .unwrap()
                .query_row("SELECT id FROM mux_panes", [], |row| row.get(0))
                .unwrap(),
        );
        let (mut worker, _event_rx) = admission_worker(
            Arc::new(RecordingResolver {
                calls: Mutex::new(Vec::new()),
                value: None,
            }),
            "lazy-restore-spawn-failure",
        );
        worker.shell = spec("/definitely/missing/deppy-shell", &[]);
        worker.persist = Some(
            crate::persistence::PersistPipe::open(&crate::persistence::PersistConfig {
                db_path: db_path.clone(),
                workspace_id: workspace_id.to_owned(),
            })
            .unwrap(),
        );

        worker.handle_command(RuntimeCommand::RestoreWorkspacePane { pane: pane_id });
        assert!(worker.sessions.is_empty());
        drop(worker);

        let association: Option<String> = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row("SELECT session_id FROM mux_panes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(association.as_deref(), Some("persisted-shell"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn restore_workspace_pane_closed_skeleton_is_not_materialized_by_full_restore() {
        let dir = unique_test_dir("lazy-restore-close-skeleton");
        let db_path = dir.join("metadata.sqlite3");
        let workspace_id = "ws-lazy-restore-close-skeleton";
        create_persist_db(&db_path, workspace_id);
        let (first_pane, second_pane) =
            seed_persisted_two_pane_window(&db_path, workspace_id, "shell");
        let (mut worker, _event_rx) = admission_worker(
            Arc::new(RecordingResolver {
                calls: Mutex::new(Vec::new()),
                value: None,
            }),
            "lazy-restore-close-skeleton",
        );
        worker.shell = spec("/bin/cat", &[]);
        worker.persist = Some(
            crate::persistence::PersistPipe::open(&crate::persistence::PersistConfig {
                db_path,
                workspace_id: workspace_id.to_owned(),
            })
            .unwrap(),
        );

        worker.handle_command(RuntimeCommand::RestoreWorkspacePane {
            pane: MuxPaneId("missing-pane".to_owned()),
        });
        worker.handle_command(RuntimeCommand::ClosePane {
            pane: second_pane.clone(),
        });
        worker.handle_command(RuntimeCommand::RestoreWorkspace);

        assert_eq!(worker.sessions.len(), 1);
        assert!(worker.mux.panes.contains_key(&first_pane));
        assert!(!worker.mux.panes.contains_key(&second_pane));
        assert!(worker.lazy_restore.is_none());
        drop(worker);
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    fn assert_lazy_restore_close_survives_restart(close_tab: bool) {
        let dir = unique_test_dir(if close_tab {
            "lazy-restore-close-tab-restart"
        } else {
            "lazy-restore-close-pane-restart"
        });
        let db_path = dir.join("metadata.sqlite3");
        let workspace_id = if close_tab {
            "ws-lazy-close-tab-restart"
        } else {
            "ws-lazy-close-pane-restart"
        };
        create_persist_db(&db_path, workspace_id);
        let (first_pane, second_pane) =
            seed_persisted_two_pane_window(&db_path, workspace_id, "shell");
        {
            let client = InProcessRuntimeClient::with_shell(
                5,
                test_store(),
                dir.join("logs-first"),
                RedactionService::new(),
                spec("/bin/cat", &[]),
                Some(crate::persistence::PersistConfig {
                    db_path: db_path.clone(),
                    workspace_id: workspace_id.to_owned(),
                }),
            );
            let mut probe = Probe::new(client.subscribe());
            client
                .send_command(RuntimeCommand::RestoreWorkspacePane {
                    pane: MuxPaneId("missing-pane".to_owned()),
                })
                .unwrap();
            probe.wait_for(Duration::from_secs(15), |event| match event {
                RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.len() == 2 => Some(()),
                _ => None,
            });
            let command = if close_tab {
                RuntimeCommand::CloseTab {
                    tab: MuxTabId("restore-tab-second".to_owned()),
                }
            } else {
                RuntimeCommand::ClosePane {
                    pane: second_pane.clone(),
                }
            };
            client.send_command(command).unwrap();
            probe.wait_for(Duration::from_secs(15), |event| match event {
                RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.len() == 1 => Some(()),
                _ => None,
            });
        }

        let persisted_panes = rusqlite::Connection::open(&db_path)
            .unwrap()
            .prepare("SELECT id FROM mux_panes ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(persisted_panes, vec![first_pane.0.clone()]);

        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            dir.join("logs-second"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path,
                workspace_id: workspace_id.to_owned(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::RestoreWorkspace)
            .unwrap();
        let snapshot = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot.tabs.len() == 1
                    && snapshot.tabs[0]
                        .panes
                        .iter()
                        .any(|pane| pane.id == first_pane && pane.session_id.is_some()) =>
            {
                Some(snapshot.clone())
            }
            _ => None,
        });
        assert_eq!(snapshot.tabs.len(), 1);
        assert!(
            !snapshot
                .tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .any(|pane| pane.id == second_pane)
        );
        drop(client);
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn restore_workspace_pane_pending_close_survives_restart() {
        assert_lazy_restore_close_survives_restart(false);
    }

    #[cfg(unix)]
    #[test]
    fn restore_workspace_pane_pending_tab_close_survives_restart() {
        assert_lazy_restore_close_survives_restart(true);
    }

    #[cfg(unix)]
    #[test]
    fn restore_workspace_pane_full_failure_preserves_remaining_association_for_restart() {
        let dir = unique_test_dir("lazy-restore-full-failure-restart");
        let db_path = dir.join("metadata.sqlite3");
        let workspace_id = "ws-lazy-full-failure-restart";
        create_persist_db(&db_path, workspace_id);
        let (agent_pane, shell_pane) =
            seed_persisted_two_pane_window(&db_path, workspace_id, "agent");
        let (mut worker, _event_rx) = admission_worker(
            Arc::new(RecordingResolver {
                calls: Mutex::new(Vec::new()),
                value: None,
            }),
            "lazy-restore-full-failure-restart",
        );
        worker.shell = spec("/definitely/missing/deppy-shell", &[]);
        worker.persist = Some(
            crate::persistence::PersistPipe::open(&crate::persistence::PersistConfig {
                db_path: db_path.clone(),
                workspace_id: workspace_id.to_owned(),
            })
            .unwrap(),
        );
        worker.persist_db_path = Some(db_path.clone());

        worker.handle_command(RuntimeCommand::RestoreWorkspacePane { pane: agent_pane });
        worker.handle_command(RuntimeCommand::RestoreWorkspace);
        assert_eq!(worker.sessions.len(), 1, "remaining shell spawn must fail");
        drop(worker);

        let association: Option<String> = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT session_id FROM mux_panes WHERE id = ?1",
                [&shell_pane.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(association.as_deref(), Some("restore-pane-second"));

        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            dir.join("logs-restart"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path: db_path.clone(),
                workspace_id: workspace_id.to_owned(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::RestoreWorkspace)
            .unwrap();
        let snapshot = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .any(|pane| pane.id == shell_pane && pane.session_id.is_some()) =>
            {
                Some(snapshot.clone())
            }
            _ => None,
        });
        assert!(snapshot.tabs.iter().flat_map(|tab| &tab.panes).any(|pane| {
            pane.id == shell_pane
                && pane.persistent_session_id.as_deref() == Some("restore-pane-second")
        }));
        drop(client);

        let session_count: i64 = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            session_count, 2,
            "retry must not duplicate persisted sessions"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn restore_workspace_pane_then_full_restore_materializes_remaining_once() {
        let dir = unique_test_dir("lazy-restore-full");
        let db_path = dir.join("metadata.sqlite3");
        let workspace_id = "ws-lazy-restore-full";
        create_persist_db(&db_path, workspace_id);
        let (first_pane, _) = seed_persisted_two_pane_window(&db_path, workspace_id, "shell");
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            dir.join("logs"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path,
                workspace_id: workspace_id.to_owned(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());

        client
            .send_command(RuntimeCommand::RestoreWorkspacePane { pane: first_pane })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .filter(|pane| pane.session_id.is_some())
                    .count()
                    == 1 =>
            {
                Some(())
            }
            _ => None,
        });
        client
            .send_command(RuntimeCommand::RestoreWorkspace)
            .unwrap();
        let full = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .filter(|pane| pane.session_id.is_some())
                    .count()
                    == 2 =>
            {
                Some(snapshot.clone())
            }
            _ => None,
        });
        assert_eq!(
            full.tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .filter(|pane| pane.session_id.is_some())
                .count(),
            2
        );

        drop(client);
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn restore_workspace_pane_agent_is_archived_without_agent_start_event() {
        let dir = unique_test_dir("lazy-restore-agent");
        let db_path = dir.join("metadata.sqlite3");
        let workspace_id = "ws-lazy-restore-agent";
        create_persist_db(&db_path, workspace_id);
        let (agent_pane, _) = seed_persisted_two_pane_window(&db_path, workspace_id, "agent");
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            dir.join("logs"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path,
                workspace_id: workspace_id.to_owned(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());

        client
            .send_command(RuntimeCommand::RestoreWorkspacePane {
                pane: agent_pane.clone(),
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .any(|pane| pane.id == agent_pane && pane.session_id.is_some()) =>
            {
                Some(())
            }
            _ => None,
        });
        probe.seen.extend(probe.rx.drain());
        assert!(
            !probe
                .seen
                .iter()
                .any(|event| matches!(event, RuntimeEvent::AgentSpawned { .. }))
        );
        let conn = rusqlite::Connection::open(dir.join("metadata.sqlite3")).unwrap();
        let kind: String = conn
            .query_row(
                "SELECT session_kind FROM sessions WHERE id = 'restore-pane-first'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kind, "agent");

        drop(client);
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn 복원된_archived_agent_pane은_시간이_지나도_자동으로_닫히지_않는다() {
        // 2026-08-19 회귀 가드: agent도 exit 시 pane을 닫도록 바뀌면서, 실수로
        // SessionRestored(재시작 시 열람 전용 복원)까지 그 판정에 섞이면 앱을 켜자마자
        // 모든 복원 pane이 사라진다. restore_archived_pane이 만드는 세션은 pty가
        // 처음부터 None이라 Session::pump의 just_exited가 구조적으로 다시 true가 될 수
        // 없다(pty.is_some() 전제) — 이 테스트는 그 불변식을 실제 여러 pump tick에
        // 걸쳐 관찰로도 고정한다.
        let dir = unique_test_dir("lazy-restore-agent-survives-ticks");
        let db_path = dir.join("metadata.sqlite3");
        let workspace_id = "ws-lazy-restore-agent-survives-ticks";
        create_persist_db(&db_path, workspace_id);
        let (agent_pane, _) = seed_persisted_two_pane_window(&db_path, workspace_id, "agent");
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            dir.join("logs"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path,
                workspace_id: workspace_id.to_owned(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());

        client
            .send_command(RuntimeCommand::RestoreWorkspacePane {
                pane: agent_pane.clone(),
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .any(|pane| pane.id == agent_pane && pane.session_id.is_some()) =>
            {
                Some(())
            }
            _ => None,
        });
        // 여러 pump tick이 지나가게 실제로 기다린 뒤, FocusPane으로 새 MuxUpdated를
        // 끌어내 그 시점 스냅샷에도 pane이 그대로인지 확인한다.
        std::thread::sleep(Duration::from_millis(150));
        client
            .send_command(RuntimeCommand::FocusPane {
                pane: agent_pane.clone(),
            })
            .unwrap();
        let still_present = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot } => Some(
                snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .any(|pane| pane.id == agent_pane && pane.session_id.is_some()),
            ),
            _ => None,
        });
        assert!(
            still_present,
            "복원된 archived agent pane이 자동으로 닫혔다"
        );

        drop(client);
        std::fs::remove_dir_all(dir).ok();
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
        // 1) 모니터 동작 확인 — 첫 ResourceUsage를 이벤트로 기다린다. 원래는 고정
        // 5.2s sleep이 유일한 동기화라 느린 공유 CI 러너에서 모니터 스레드 기아 시
        // count==0으로 실패할 수 있었다 (2026-08-04). 상한 10s는 2s 주기의 5배.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if rx
                .drain()
                .iter()
                .any(|e| matches!(e, RuntimeEvent::ResourceUsage { .. }))
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "ResourceUsage 샘플이 오지 않음 (모니터 미동작?)"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // 2) 코얼레싱 — 드레인하지 않고 샘플 주기 2회+(4.5s)를 본 후 한 번에 드레인.
        //    주기 자체가 벽시계 2s라 창은 유지하되, 첫 샘플 기준으로 앵커돼 있어
        //    마지막 샘플(t0+2s)은 2.5s의 스케줄 여유를 가진다.
        std::thread::sleep(Duration::from_millis(4500));
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
            "드레인 사이 샘플이 최소 1회는 slot에 있어야 함 (모니터 미동작?)"
        );
    }

    #[test]
    fn in_process_command_queue_full은_err로_surface된다() {
        let (tx, rx) = sync_channel(1);
        let command_budget = Arc::new(RuntimeCommandQueueBudget::default());
        let mut client = InProcessRuntimeClient {
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
        assert_eq!(
            err.downcast_ref::<crate::RuntimeCommandSendError>(),
            Some(&crate::RuntimeCommandSendError::Backpressure),
            "host must be able to distinguish retryable pressure from disconnect"
        );
        assert_eq!(command_budget.retained_bytes(), retained_after_first);
        drop(rx);
        assert_eq!(command_budget.retained_bytes(), 0);

        client.command_tx = None;
        let err = client
            .send_command(RuntimeCommand::SetWorkspaceState(
                WorkspaceRuntimeState::Warm,
            ))
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<crate::RuntimeCommandSendError>(),
            Some(&crate::RuntimeCommandSendError::Disconnected),
            "an explicitly shut-down local runtime must preserve typed disconnect"
        );
    }

    #[test]
    fn pr10_owned_sender_returns_original_body_on_channel_and_byte_pressure() {
        let (tx, rx) = sync_channel(1);
        let budget = Arc::new(RuntimeCommandQueueBudget::default());
        let mut client = InProcessRuntimeClient {
            command_tx: Some(tx),
            command_budget: Arc::clone(&budget),
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
        let baseline_bytes = budget.retained_bytes();
        let bytes = vec![b'x'; 1024 * 1024];
        let pointer = bytes.as_ptr();
        let mut command = RuntimeCommand::WriteInput {
            session: SessionId(73),
            bytes,
        };
        for _ in 0..4 {
            let (error, returned) = client.send_command_owned(command).unwrap_err();
            assert_eq!(
                error.downcast_ref::<RuntimeCommandSendError>(),
                Some(&RuntimeCommandSendError::Backpressure)
            );
            command = *returned;
            assert!(
                matches!(&command, RuntimeCommand::WriteInput { session: SessionId(73), bytes }
                if bytes.as_ptr() == pointer && bytes.len() == 1024 * 1024)
            );
            assert_eq!(budget.retained_bytes(), baseline_bytes);
        }
        drop(rx.recv().unwrap());
        let held = budget.reserve(RUNTIME_COMMAND_QUEUE_BYTES_MAX).unwrap();
        let (error, returned) = client.send_command_owned(command).unwrap_err();
        assert_eq!(
            error.downcast_ref::<RuntimeCommandSendError>(),
            Some(&RuntimeCommandSendError::Backpressure)
        );
        assert!(
            matches!(&*returned, RuntimeCommand::WriteInput { bytes, .. } if bytes.as_ptr() == pointer)
        );
        drop(held);
        client.send_command_owned(*returned).unwrap();
        assert!(budget.retained_bytes() >= 1024 * 1024);
        let admitted = rx.recv().unwrap().into_command();
        assert!(
            matches!(&admitted, RuntimeCommand::WriteInput { bytes, .. } if bytes.as_ptr() == pointer)
        );
        assert_eq!(budget.retained_bytes(), 0);
        drop(rx);
        let (error, returned) = client.send_command_owned(admitted).unwrap_err();
        assert_eq!(
            error.downcast_ref::<RuntimeCommandSendError>(),
            Some(&RuntimeCommandSendError::Disconnected)
        );
        assert!(
            matches!(&*returned, RuntimeCommand::WriteInput { bytes, .. } if bytes.as_ptr() == pointer)
        );
        client.command_tx = None;
        let (error, _) = client.send_command_owned(*returned).unwrap_err();
        assert_eq!(
            error.downcast_ref::<RuntimeCommandSendError>(),
            Some(&RuntimeCommandSendError::Disconnected)
        );
    }

    #[test]
    fn command_queue_byte_budget_accepts_exact_rejects_repeated_plus_one_and_recovers() {
        let budget = Arc::new(RuntimeCommandQueueBudget::default());
        let exact = budget.reserve(RUNTIME_COMMAND_QUEUE_BYTES_MAX).unwrap();
        assert_eq!(budget.retained_bytes(), RUNTIME_COMMAND_QUEUE_BYTES_MAX);
        for _ in 0..128 {
            let error = match budget.reserve(1) {
                Ok(_) => panic!("aggregate byte-budget overflow must be rejected"),
                Err(error) => error,
            };
            assert_eq!(
                error.downcast_ref::<crate::RuntimeCommandSendError>(),
                Some(&crate::RuntimeCommandSendError::Backpressure),
                "aggregate byte-budget pressure must remain retryable"
            );
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
        assert_eq!(
            production
                .matches("prepare_queued_command_owned(command")
                .count(),
            2,
            "owned sender and shared wrapper must use the same canonicalization/reservation"
        );
        assert!(!production.contains("try_send(command)"));
        assert!(!production.contains("#[derive(Clone)]\nstruct QueuedRuntimeCommand"));
        assert!(production.contains("queued.into_command()"));
        let wrapper = production
            .split_once("fn prepare_queued_command(")
            .unwrap()
            .1
            .split_once("fn prepare_queued_command_owned(")
            .unwrap()
            .0;
        assert!(wrapper.contains("prepare_queued_command_owned(command, budget)"));
        let queue_preparation = production
            .split_once("fn prepare_queued_command_owned(")
            .unwrap()
            .1
            .split("struct LazyWorkspaceRestore")
            .next()
            .unwrap();
        assert!(queue_preparation.contains("budget.reserve(retention.retained_bytes())"));
        assert!(
            queue_preparation
                .contains("prepare_runtime_command_for_retention_internal(&mut command)")
        );
        let worker_handler = production.split("fn handle_command_inner").nth(1).unwrap();
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
            .filter(|c| !c.wide_spacer())
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
    fn direct_terminal_input_reads_live_modes_before_pty_write() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("direct-input"),
            RedactionService::new(),
            spec(
                "/bin/sh",
                &[
                    "-c",
                    r"stty raw -echo; printf '\033[?1h\033[?2004hREADY\r\n'; dd bs=1 count=16 2>/dev/null | od -An -tx1; sleep 30",
                ],
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
        let session = probe.wait_for(Duration::from_secs(5), |event| match event {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        probe.wait_for(Duration::from_secs(5), |event| match event {
            RuntimeEvent::Viewport { snapshot, .. }
                if snapshot_text(snapshot, 0).contains("READY") =>
            {
                Some(())
            }
            _ => None,
        });
        for input in [
            crate::TerminalInput::Key {
                key: "up".into(),
                ctrl: false,
                alt: false,
                shift: false,
                meta: false,
            },
            crate::TerminalInput::Text {
                text: "x".into(),
                paste: true,
            },
        ] {
            client
                .send_command(RuntimeCommand::WriteTerminalInput { session, input })
                .unwrap();
        }
        probe.wait_for(Duration::from_secs(5), |event| match event {
            RuntimeEvent::Viewport { snapshot, .. }
                if snapshot_text(snapshot, 1)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    == [
                        "1b", "4f", "41", "1b", "5b", "32", "30", "30", "7e", "78", "1b", "5b",
                        "32", "30", "31", "7e",
                    ] =>
            {
                Some(())
            }
            _ => None,
        });
        client
            .send_command(RuntimeCommand::KillSession { session })
            .unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn direct_input_refreshes_queued_mode_changes_without_viewport_wait() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _events) = admission_worker(resolver, "direct-queued-modes");
        let id = SessionId(1);
        let (output_tx, output_rx) = std::sync::mpsc::channel();
        let live = Session::spawn_with_spec_and_output_wake(
            id, session::SessionKind::Shell,
            &spec("/bin/sh", &["-c", r"stty raw -echo; printf '\033[?1h\033[?2004hREADY\r\n'; dd bs=1 count=16 2>/dev/null | od -An -tx1; sleep 30"]),
            80, 24, 100, Arc::new(move || { let _ = output_tx.send(()); }),
        ).unwrap();
        worker.sessions.insert(id, live);
        output_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(worker.sessions[&id].pending_output_bytes() > 0);
        assert!(!worker.sessions[&id].application_cursor());
        assert!(!worker.sessions[&id].bracketed_paste());
        for input in [
            crate::TerminalInput::Key {
                key: "up".into(),
                ctrl: false,
                alt: false,
                shift: false,
                meta: false,
            },
            crate::TerminalInput::Text {
                text: "x".into(),
                paste: true,
            },
        ] {
            worker.handle_command(RuntimeCommand::WriteTerminalInput { session: id, input });
        }
        assert!(
            worker.sessions[&id].application_cursor(),
            "direct command must parse queued DECCKM before encoding"
        );
        assert!(
            worker.sessions[&id].bracketed_paste(),
            "direct command must parse queued DEC2004 before encoding"
        );
        let expected = [
            "1b", "4f", "41", "1b", "5b", "32", "30", "30", "7e", "78", "1b", "5b", "32", "30",
            "31", "7e",
        ];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let effects = worker.collect_session_pump_effects(&[id], false, true);
            worker.finish_session_pump_effects(effects);
            if worker.sessions[&id]
                .screen_text()
                .split_whitespace()
                .collect::<Vec<_>>()
                .windows(expected.len())
                .any(|bytes| bytes == expected)
            {
                break;
            }
            output_rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
        }
    }

    #[test]
    #[cfg(unix)]
    fn direct_input_mode_refresh_is_bounded_and_flood_keeps_interrupt_keys_available() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _events) = admission_worker(resolver, "direct-mode-flood");
        let pressures = Arc::clone(&worker.subscribers.lock().unwrap()[0].input_pressures);
        let id = SessionId(1);
        let (output_tx, output_rx) = std::sync::mpsc::channel();
        let live = Session::spawn_with_spec_and_output_wake(
            id,
            session::SessionKind::Shell,
            &spec("/bin/sh", &["-c", "stty raw -echo; dd if=/dev/zero bs=1024 count=768 2>/dev/null; dd bs=1 count=4 2>/dev/null | od -An -tx1; sleep 30"]),
            80, 24, 100,
            Arc::new(move || { let _ = output_tx.send(()); }),
        ).unwrap();
        let total = 768 * 1024;
        let deadline = Instant::now() + Duration::from_secs(5);
        while live.pending_output_bytes() < total {
            output_rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
        }
        worker.sessions.insert(id, live);
        worker.handle_command(RuntimeCommand::WriteTerminalInput {
            session: id,
            input: crate::TerminalInput::Text {
                text: "wrong-paste".into(),
                paste: true,
            },
        });
        assert_eq!(
            worker.sessions[&id].pending_output_bytes(),
            total - Session::INPUT_GUARD_OUTPUT_MAX_BYTES
        );
        assert!(pressures.lock().unwrap().values().any(|event| matches!(event, RuntimeEvent::PtyInputPressure {
            session,
            pressure: pty::PtyInputPressure { reason: pty::PtyInputRejectReason::AdmissionDenied, queued_bytes: 0, queued_messages: 0, .. },
        } if *session == id)), "bounded mode refresh must report a transient denial");
        for input in [
            crate::TerminalInput::Key {
                key: "c".into(),
                ctrl: true,
                alt: false,
                shift: false,
                meta: false,
            },
            crate::TerminalInput::Key {
                key: "d".into(),
                ctrl: true,
                alt: false,
                shift: false,
                meta: false,
            },
            crate::TerminalInput::Key {
                key: "esc".into(),
                ctrl: false,
                alt: false,
                shift: false,
                meta: false,
            },
            crate::TerminalInput::Text {
                text: "x".into(),
                paste: false,
            },
        ] {
            worker.handle_command(RuntimeCommand::WriteTerminalInput { session: id, input });
        }
        assert!(
            worker.sessions[&id].pending_output_bytes()
                >= total - Session::INPUT_GUARD_OUTPUT_MAX_BYTES,
            "mode-independent input must not drain or wait for the output flood"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let effects = worker.collect_session_pump_effects(&[id], false, true);
            worker.finish_session_pump_effects(effects);
            if worker.sessions[&id]
                .screen_text()
                .split_whitespace()
                .collect::<Vec<_>>()
                .windows(4)
                .any(|bytes| bytes == ["03", "04", "1b", "78"])
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "PTY capture timed out: {:?}",
                worker.sessions[&id].screen_text()
            );
            if worker.sessions[&id].pending_output_bytes() == 0 {
                output_rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap_or_else(|error| {
                        panic!(
                            "PTY capture {error:?}: {:?}",
                            worker.sessions[&id].screen_text()
                        )
                    });
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn 복원된_archived_agent_pane도_scroll로_스크롤백을_볼_수_있다() {
        // 2026-08-19: agent도 exit 시 pane이 자동으로 닫히게 되면서("셸_exit시_
        // pane_자동_닫힘_agent도_동일하게_닫힌다" 참고), §14.3 "종료 후에도
        // scrollback 열람 가능" 계약은 더 이상 "살아있는 pane으로 exit 직후 관찰"로는
        // 검증할 수 없다 — 검증 시도 자체가 그 pane을 없앤다. 이 계약이 실제로 남아
        // 있는 자리는 재시작 시 열람 전용으로 복원된 archived pane이다: 그 세션은
        // pty가 처음부터 None이라 pane 자동 닫힘 대상이 되지 않고(§셸_exit... 테스트의
        // 코드 주석 참고) 무기한 유지된다. 이 테스트는 그 자리에서 Scroll이 여전히
        // Viewport로 응답하는지 고정한다(예전 이름: 종료_후에도_scrollback_열람_가능).
        let dir = unique_test_dir("archived-pane-scroll");
        let db_path = dir.join("metadata.sqlite3");
        let workspace_id = "ws-archived-pane-scroll";
        create_persist_db(&db_path, workspace_id);
        let (agent_pane, _) = seed_persisted_two_pane_window(&db_path, workspace_id, "agent");
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            dir.join("logs"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path,
                workspace_id: workspace_id.to_owned(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::RestoreWorkspacePane {
                pane: agent_pane.clone(),
            })
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot } => snapshot
                .tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .find(|pane| pane.id == agent_pane)
                .and_then(|pane| pane.session_id),
            _ => None,
        });
        // 살아있는 세션의 exit-후-Scroll과 달리, 여기선 애초에 pane이 닫힐 일이 없다
        // — Scroll이 여전히 Viewport로 응답하는지만 확인한다(seed 픽스처는 실제 ANSI
        // 아카이브/로그가 없어 내용 검증은 다른 테스트들의 몫이다).
        probe.seen.clear();
        client
            .send_command(RuntimeCommand::Scroll { session, delta: 1 })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport { session: s, .. } if *s == session => Some(()),
            _ => None,
        });

        drop(client);
        std::fs::remove_dir_all(dir).ok();
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
    fn 셸_exit시_pane_자동_닫힘_agent도_동일하게_닫힌다() {
        // 2026-08-19: agent pane도 exit 시 닫히도록 바뀌었다(wrap_agent_then_shell이
        // 세션을 에이전트보다 오래 살리므로 agent SessionExited = "폴백 셸에서 exit
        // 쳤다"는 뜻 — in_process.rs의 pump_sessions 주석 참고). 이 테스트는 예전
        // "agent는 유지"를 지키던 것을 뒤집어 셸·agent가 동일하게 닫힘을 고정한다.
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("t"),
            RedactionService::new(),
            spec("/bin/echo", &["bye"]),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        // agent(즉시 종료)와 셸(즉시 종료) 둘 다 pane 자동 닫힘을 기대한다
        client
            .send_command(spawn_agent_cmd("true", None, None))
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
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SessionExited { session, .. } if *session == agent => Some(()),
            _ => None,
        });
        // exit 직후의 MuxUpdated에서 셸·agent pane 둘 다 사라진다
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::MuxUpdated { snapshot } => {
                let sessions: Vec<_> = snapshot
                    .tabs
                    .iter()
                    .flat_map(|t| &t.panes)
                    .filter_map(|p| p.session_id)
                    .collect();
                (!sessions.contains(&shell) && !sessions.contains(&agent)).then_some(())
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
        let store = test_store();
        store
            .set_secret(
                "cred-agent-test",
                &secret::SecretString::new("s3cret-value".into()),
            )
            .unwrap();

        let client = InProcessRuntimeClient::with_shell(
            5,
            Arc::clone(&store),
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
        let waited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            probe.wait_for(Duration::from_secs(15), |e| match e {
                RuntimeEvent::Viewport { snapshot, .. }
                    if snapshot_text(snapshot, 0).contains("P=plain-v S=s3cret-value") =>
                {
                    Some(())
                }
                _ => None,
            });
        }));
        if let Err(panic) = waited {
            // Failure-only diagnostics for this synthetic private fixture. Never dump
            // arbitrary event payloads or actual keyring error strings.
            let resolved = store
                .get_secret("cred-agent-test")
                .map(|value| value.expose() == "s3cret-value");
            eprintln!(
                "owned secret fixture resolve_matches={:?}",
                resolved.map_err(|_| "resolve_error")
            );
            for event in &probe.seen {
                match event {
                    RuntimeEvent::Viewport {
                        session, snapshot, ..
                    } => {
                        let row = snapshot_text(snapshot, 0);
                        let expected_any_row = (0..snapshot.rows as usize).any(|row| {
                            snapshot_text(snapshot, row).contains("P=plain-v S=s3cret-value")
                        });
                        eprintln!(
                            "owned fixture Viewport session={} row0={row:?} expected_any_row={expected_any_row}",
                            session.0
                        );
                    }
                    RuntimeEvent::SpawnFailed { kind, message } => {
                        eprintln!(
                            "owned fixture SpawnFailed kind={kind:?} id={} code={:?}",
                            message.message_id,
                            message.arg_value("error_code")
                        );
                    }
                    RuntimeEvent::AgentSpawned { session } => {
                        eprintln!("owned fixture AgentSpawned session={}", session.0);
                    }
                    RuntimeEvent::SessionExited { session, exit_code } => {
                        eprintln!(
                            "owned fixture SessionExited session={} code={exit_code:?}",
                            session.0
                        );
                    }
                    RuntimeEvent::MuxUpdated { .. } => eprintln!("owned fixture MuxUpdated"),
                    _ => eprintln!(
                        "owned fixture other_variant={:?}",
                        std::mem::discriminant(event)
                    ),
                }
            }
            std::panic::resume_unwind(panic);
        }
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
                environment_revision: None,
                secret_versions: Vec::new(),
                dotenv_source: None,
                api_secrets: Vec::new(),
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
                scrollback_policy: None,
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
                scrollback_policy: None,
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
        // with_shell이 run-<ms> 하위 디렉터리를 만든다 — 그 안에서 세션 디렉터리를 찾는다
        let run_dir = std::fs::read_dir(&logs_root)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().starts_with("run-"))
            .expect("run 디렉터리 없음")
            .path();
        let dir = storage::SessionLogWriter::session_dir(&run_dir, session);
        // exit 처리(로그 flush) 완료를 관측 가능한 상태(치환 마커 기록)로 기다린다 —
        // 고정 200ms sleep은 느린 공유 CI 러너에서 flush보다 먼저 읽어 실패할 수 있다
        // (2026-08-04). 상한 5s는 코드베이스 관례(50ms급 작업에 초 단위 상한).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let text = std::fs::read(dir.join("redacted.ansi.log"))
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            if text.contains("[REDACTED]") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "세션 로그 flush가 끝나지 않음"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
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

    /// agent_launcher::wrap_agent_then_shell이 만드는 것과 동등한 스크립트(app crate라
    /// 여기서 직접 참조는 못 하지만 같은 형태)로 exit sentinel 경로를 진짜 PTY로 고정한다.
    /// 폴백 셸은 일부러 오래 살려 둔다(sleep) — SessionExited가 오기 훨씬 전에, 에이전트
    /// 자신의 종료 코드(0이 아님)로 Error가 즉시 반영돼야 한다는 게 이 테스트의 요점.
    #[test]
    #[cfg(unix)]
    fn exit_sentinel은_폴백_셸이_살아있어도_에이전트의_진짜_종료코드를_즉시_반영한다() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("exit-sentinel"),
            RedactionService::new(),
            pty::default_shell(),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        let script = r#""$@"; __deppy_exit=$?; printf '%s' "$__deppy_exit" > "${TMPDIR:-/tmp}/deppy-agent-exit-$$" 2>/dev/null || true; sleep 30"#;
        client
            .send_command(RuntimeCommand::SpawnAgent {
                agent_config_id: None,
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                command: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    script.into(),
                    "deppy-agent-session".into(),
                    "/bin/sh".into(),
                    "-c".into(),
                    "exit 3".into(),
                ],
                env_plain: Vec::new(),
                env_secrets: Vec::new(),
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
            RuntimeEvent::SessionStatusChanged {
                session: s,
                status: session::SessionStatus::Error,
            } if *s == session => Some(()),
            _ => None,
        });
        assert!(
            !probe.seen.iter().any(
                |e| matches!(e, RuntimeEvent::SessionExited { session: s, .. } if *s == session)
            ),
            "폴백 셸이 아직 안 죽었으니 SessionExited보다 먼저 와야 한다"
        );
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
    fn tracked_input_reports_actual_pty_queue_pressure() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("tracked-pressure"),
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
        let session = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        let mut rejected = false;
        for n in 0..4 {
            let id = format!("pressure-{n}");
            client
                .send_command(RuntimeCommand::WriteInputTracked {
                    session,
                    operation_id: id.clone(),
                    bytes: vec![b'x'; pty::PtyInputQueuePolicy::default().max_bytes],
                })
                .unwrap();
            let result = probe.wait_for(Duration::from_secs(15), |e| match e {
                RuntimeEvent::InputAdmitted {
                    operation_id,
                    result,
                    ..
                } if operation_id == &id => Some(*result),
                _ => None,
            });
            if result == Err(pty::PtyInputRejectReason::QueueFull) {
                rejected = true;
                break;
            }
            assert_eq!(result, Ok(()));
        }
        assert!(rejected, "PTY saturation must yield a correlated rejection");
    }

    #[test]
    #[cfg(unix)]
    fn pr13_exit_teardown_releases_authorization_before_revocation_handshake() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, events) = admission_worker(resolver, "pr13-exit-teardown");
        let id = SessionId(1);
        let ready = worker.logs_root.join("descendant-ready");
        let started = worker.logs_root.join("teardown-started");
        let released = worker.logs_root.join("permit-revoked");
        let acknowledged = worker.logs_root.join("descendant-acknowledged");
        let (output_tx, output_rx) = std::sync::mpsc::channel();
        let live = Session::spawn_with_spec_and_output_wake(
            id,
            session::SessionKind::Shell,
            &spec(
                "/bin/sh",
                &[
                    "-c",
                    r#"(trap '' HUP; trap 'printf teardown > "$2"; while [ ! -f "$3" ]; do /bin/sleep 0.005; done; printf acknowledged > "$4"; exit 0' TERM; printf ready > "$1"; while :; do /bin/sleep 0.01; done) & while [ ! -f "$1" ]; do /bin/sleep 0.005; done; printf '\033[?2004lpr13-exit-final'; exit 0"#,
                    "pr13",
                    ready.to_str().unwrap(),
                    started.to_str().unwrap(),
                    released.to_str().unwrap(),
                    acknowledged.to_str().unwrap(),
                ],
            ),
            80,
            24,
            100,
            Arc::new(move || {
                let _ = output_tx.send(());
            }),
        )
        .unwrap();
        worker.sessions.insert(id, live);
        worker.attach_in_new_tab(id, "owned-exit-teardown");
        let viewports = Arc::clone(&worker.subscribers.lock().unwrap()[0].viewports);
        output_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let permit = crate::InputPermit::new();
        let authorizing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inside_authorizer = Arc::clone(&authorizing);
        let admission = crate::InputAdmission::new(
            permit.clone(),
            Instant::now() + Duration::from_secs(5),
            move |write| {
                inside_authorizer.store(true, std::sync::atomic::Ordering::SeqCst);
                write();
                inside_authorizer.store(false, std::sync::atomic::Ordering::SeqCst);
            },
        )
        .with_bracketed_paste_required();
        let controller_started = started.clone();
        let controller_released = released.clone();
        let controller = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !controller_started.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            if !controller_started.exists() {
                return None;
            }
            let teardown_inside_authorization =
                authorizing.load(std::sync::atomic::Ordering::SeqCst);
            permit.revoke();
            std::fs::write(controller_released, b"revoked").unwrap();
            Some(teardown_inside_authorization)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut exited = false;
        while !exited && Instant::now() < deadline {
            assert_eq!(
                worker.admit_input_batch_checked(id, &[b"must-not-send", b"\r"], Some(&admission)),
                Err(pty::PtyInputRejectReason::AdmissionDenied)
            );
            exited = events.try_iter().any(|event| matches!(event, RuntimeEvent::SessionExited { session, .. } if session == id));
            if !exited {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let teardown_inside_authorization = controller.join().unwrap();
        assert!(exited, "owned shell must exit through guarded admission");
        assert_eq!(
            teardown_inside_authorization,
            Some(false),
            "actual descendant teardown ran while authorization was held"
        );
        assert!(
            acknowledged.exists(),
            "permit revocation must release the real descendant before teardown completes"
        );
        let slots = viewports.lock().unwrap();
        let Some(RuntimeEvent::Viewport { snapshot, .. }) = slots.get(&id) else {
            panic!("guarded exit must preserve its final watched viewport");
        };
        assert!(snapshot_text(snapshot, 0).contains("pr13-exit-final"));
        for path in [ready, started, released, acknowledged] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    #[cfg(unix)]
    fn pr13_final_watched_output_survives_paced_fast_exit() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, events) = admission_worker(resolver, "pr13-final-viewport");
        let id = SessionId(1);
        let (output_tx, output_rx) = std::sync::mpsc::channel();
        let mut live = Session::spawn_with_spec_and_output_wake(
            id,
            session::SessionKind::Shell,
            &spec("/bin/sh", &["-c", "printf 'pr13-final-output'"]),
            80,
            24,
            100,
            Arc::new(move || {
                let _ = output_tx.send(());
            }),
        )
        .unwrap();
        live.take_snapshot().unwrap();
        worker.sessions.insert(id, live);
        worker.attach_in_new_tab(id, "owned-final");
        assert!(worker.mux.watched_sessions().contains(&id));
        let viewports = Arc::clone(&worker.subscribers.lock().unwrap()[0].viewports);
        output_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut exited = false;
        while !exited && Instant::now() < deadline {
            worker.pump_sessions(false);
            exited = events.try_iter().any(|event| matches!(event, RuntimeEvent::SessionExited { session, .. } if session == id));
            if !exited {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert!(exited, "owned child must exit through the real shared pump");
        let slots = viewports.lock().unwrap();
        let Some(RuntimeEvent::Viewport { snapshot, .. }) = slots.get(&id) else {
            panic!("paced exit removed the watched session without its final viewport");
        };
        assert!(snapshot_text(snapshot, 0).contains("pr13-final-output"));
    }

    #[test]
    #[cfg(unix)]
    fn pr13_guard_refreshes_queued_dec2004_off_before_admission() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _events) = admission_worker(resolver, "pr13-queued-dec2004");
        let id = SessionId(1);
        let (output_tx, output_rx) = std::sync::mpsc::channel();
        let mut live = Session::spawn_with_spec_and_output_wake(
            id,
            session::SessionKind::Shell,
            &spec(
                "/bin/sh",
                &["-c", r"stty -echo; printf '\033[?2004l'; exec /bin/cat"],
            ),
            80,
            24,
            100,
            Arc::new(move || {
                let _ = output_tx.send(());
            }),
        )
        .unwrap();
        live.replay_ansi(&mut "\x1b[?2004h".as_bytes()).unwrap();
        worker.sessions.insert(id, live);
        output_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            worker.sessions[&id].bracketed_paste(),
            "cached mode is still enabled before admission"
        );
        let admission = crate::InputAdmission::new(
            crate::InputPermit::new(),
            Instant::now() + Duration::from_secs(5),
            |write| write(),
        )
        .with_bracketed_paste_required();
        assert_eq!(
            worker.admit_input_batch_checked(
                id,
                &[b"\x1b[200~line1\nline2\x1b[201~", b"\r"],
                Some(&admission)
            ),
            Err(pty::PtyInputRejectReason::AdmissionDenied),
            "queued mode-off must be applied before retaining any body or submit"
        );
    }

    #[test]
    #[cfg(unix)]
    fn pr13_guard_refreshes_queued_choice_prompt_before_admission() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, events) = admission_worker(resolver, "pr13-queued-choice");
        let id = SessionId(1);
        let (output_tx, output_rx) = std::sync::mpsc::channel();
        let mut live = Session::spawn_with_spec_and_output_wake(
            id,
            session::SessionKind::Agent,
            &spec(
                "/bin/sh",
                &[
                    "-c",
                    r"stty -echo; printf '\r\nEnter to select\r\n❯ '; exec /bin/cat",
                ],
            ),
            80,
            24,
            100,
            Arc::new(move || {
                let _ = output_tx.send(());
            }),
        )
        .unwrap();
        let group = live.process_identity().process_group.unwrap();
        live.replay_ansi(&mut "❯ ".as_bytes()).unwrap();
        worker.sessions.insert(id, live);
        worker.detectors.insert(
            id,
            session::StatusDetector::new(session::StatusPatterns::compile(
                Some("Enter to select"),
                None,
                None,
                None,
            )),
        );
        output_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            !worker.sessions[&id]
                .screen_text()
                .contains("Enter to select"),
            "dialog is queued, not yet parsed"
        );
        let authorizing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inside_authorizer = Arc::clone(&authorizing);
        let during_wake = Arc::clone(&authorizing);
        worker.subscribers.lock().unwrap()[0].wake = Some(Arc::new(move || {
            assert!(
                !during_wake.load(std::sync::atomic::Ordering::SeqCst),
                "output events must wait until authorization released"
            );
        }));
        let admission = crate::InputAdmission::new(
            crate::InputPermit::new(),
            Instant::now() + Duration::from_secs(5),
            move |write| {
                inside_authorizer.store(true, std::sync::atomic::Ordering::SeqCst);
                write();
                inside_authorizer.store(false, std::sync::atomic::Ordering::SeqCst);
            },
        )
        .with_agent_guard(crate::AgentInputGuard {
            foreground_process_group: group,
            provider: crate::AgentPromptKind::Claude,
            intent: crate::AgentInputIntent::ExplicitPrompt,
        });
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"AUTOMATIC", b"\r"], Some(&admission)),
            Err(pty::PtyInputRejectReason::AdmissionDenied),
            "queued dialog must protect the otherwise empty prompt"
        );
        assert_eq!(
            events
                .try_iter()
                .filter(|event| matches!(event, RuntimeEvent::SessionStatusChanged { .. }))
                .count(),
            1
        );
        assert!(
            worker.sessions[&id]
                .screen_text()
                .contains("Enter to select")
        );
        let output_bytes = worker.detectors[&id].stats().stream_bytes;
        assert!(output_bytes > 0);
        worker.pump_sessions(false);
        assert_eq!(
            worker.detectors[&id].stats().stream_bytes,
            output_bytes,
            "normal pump must not replay raw output"
        );
        assert!(
            !events.try_iter().any(|event| matches!(
                event,
                RuntimeEvent::SessionStatusChanged { .. }
                    | RuntimeEvent::SessionInputSubmitted { .. }
            )),
            "next normal pump must not replay status or submit rejected input"
        );
    }

    #[test]
    #[cfg(unix)]
    fn pr13_guard_refreshes_output_arriving_during_authorization_wait() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _events) = admission_worker(resolver, "pr13-auth-output");
        let release = worker.logs_root.join("release-test-output");
        let id = SessionId(1);
        let (output_tx, output_rx) = std::sync::mpsc::channel();
        let mut live = Session::spawn_with_spec_and_output_wake(
            id, session::SessionKind::Shell,
            &spec("/bin/sh", &["-c", r#"stty -echo; while [ ! -f "$1" ]; do sleep 0.01; done; printf '\033[?2004l'; exec /bin/cat"#, "pr13", release.to_str().unwrap()]),
            80, 24, 100,
            Arc::new(move || { let _ = output_tx.send(()); }),
        ).unwrap();
        live.replay_ansi(&mut "\x1b[?2004h".as_bytes()).unwrap();
        worker.sessions.insert(id, live);
        assert_eq!(worker.sessions[&id].pending_output_bytes(), 0);
        let output_rx = Mutex::new(output_rx);
        let release_in_authorizer = release.clone();
        let admission = crate::InputAdmission::new(
            crate::InputPermit::new(),
            Instant::now() + Duration::from_secs(5),
            move |write| {
                std::fs::write(&release_in_authorizer, b"release").unwrap();
                output_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
                write();
            },
        )
        .with_bracketed_paste_required();
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"body", b"\r"], Some(&admission)),
            Err(pty::PtyInputRejectReason::AdmissionDenied)
        );
        assert!(
            !worker.sessions[&id].bracketed_paste(),
            "refresh must run after authorization waited"
        );
        std::fs::remove_file(release).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn pr13_flood_stops_at_strict_budget_without_replaying_output_or_input() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, events) = admission_worker(resolver, "pr13-output-flood");
        let id = SessionId(1);
        let (output_tx, output_rx) = std::sync::mpsc::channel();
        let mut live = Session::spawn_with_spec_and_output_wake(
            id,
            session::SessionKind::Shell,
            &spec(
                "/bin/sh",
                &[
                    "-c",
                    "stty -echo; dd if=/dev/zero bs=1024 count=384 2>/dev/null; exec /bin/cat",
                ],
            ),
            80,
            24,
            100,
            Arc::new(move || {
                let _ = output_tx.send(());
            }),
        )
        .unwrap();
        live.replay_ansi(&mut "\x1b[?2004h".as_bytes()).unwrap();
        let total = 384 * 1024;
        let deadline = Instant::now() + Duration::from_secs(5);
        while live.pending_output_bytes() < total {
            output_rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
        }
        worker.sessions.insert(id, live);
        worker.detectors.insert(
            id,
            session::StatusDetector::new(session::StatusPatterns::compile(None, None, None, None)),
        );
        worker.open_session_log(id);
        let admission = crate::InputAdmission::new(
            crate::InputPermit::new(),
            Instant::now() + Duration::from_secs(5),
            |write| write(),
        )
        .with_bracketed_paste_required();
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"body", b"\r"], Some(&admission)),
            Err(pty::PtyInputRejectReason::AdmissionDenied)
        );
        let fed = worker.detectors[&id].stats().stream_bytes as usize;
        assert!(
            fed > 0 && fed <= 256 * 1024,
            "guard consumed {fed} bytes past its fixed budget"
        );
        assert_eq!(worker.sessions[&id].pending_output_bytes(), total - fed);
        assert!(worker.sessions[&id].pending_output_bytes() > 0);
        worker.pump_sessions(false);
        assert_eq!(worker.sessions[&id].pending_output_bytes(), 0);
        assert_eq!(worker.detectors[&id].stats().stream_bytes, total as u64);
        assert_eq!(worker.logs[&id].last_log_offset, total as u64);
        worker.pump_sessions(false);
        assert_eq!(worker.detectors[&id].stats().stream_bytes, total as u64);
        assert_eq!(worker.logs[&id].last_log_offset, total as u64);
        assert!(
            !events
                .try_iter()
                .any(|event| matches!(event, RuntimeEvent::SessionInputSubmitted { .. }))
        );
    }

    #[test]
    #[cfg(unix)]
    fn pr13_deferred_log_sink_allows_revocation_while_disk_work_is_delayed() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _events) = admission_worker(resolver, "pr13-deferred-log");
        let id = SessionId(1);
        let (output_tx, output_rx) = std::sync::mpsc::channel();
        let mut live = Session::spawn_with_spec_and_output_wake(
            id,
            session::SessionKind::Shell,
            &spec(
                "/bin/sh",
                &["-c", "stty -echo; printf 'owned-log-output'; exec /bin/cat"],
            ),
            80,
            24,
            100,
            Arc::new(move || {
                let _ = output_tx.send(());
            }),
        )
        .unwrap();
        live.replay_ansi(&mut "\x1b[?2004h".as_bytes()).unwrap();
        worker.sessions.insert(id, live);
        worker.open_session_log(id);
        output_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let permit = crate::InputPermit::new();
        let admission = crate::InputAdmission::new(
            permit.clone(),
            Instant::now() + Duration::from_secs(5),
            |write| write(),
        )
        .with_bracketed_paste_required();
        let (sink_started_tx, sink_started_rx) = std::sync::mpsc::channel();
        let (revoked_tx, revoked_rx) = std::sync::mpsc::channel();
        let controller = std::thread::spawn(move || {
            if sink_started_rx.recv_timeout(Duration::from_secs(5)).is_ok() {
                permit.revoke();
                let _ = revoked_tx.send(());
            }
        });
        let mut revoke_completed_in_sink = false;
        let mut writes = 0;
        let result = worker.admit_input_batch_checked_with_log_sink(
            id,
            &[b""],
            Some(&admission),
            |log, bytes| {
                writes += 1;
                sink_started_tx.send(()).unwrap();
                // Bounded delayed-sink handshake: a regression never waits indefinitely.
                revoke_completed_in_sink =
                    revoked_rx.recv_timeout(Duration::from_millis(500)).is_ok();
                log.append_redacted_output(bytes)
            },
        );
        controller.join().unwrap();
        assert_eq!(result, Ok(()));
        assert!(
            revoke_completed_in_sink,
            "disk phase held the permit and blocked cancellation"
        );
        assert_eq!(writes, 1);
        assert_eq!(
            worker.logs[&id].last_log_offset,
            b"owned-log-output".len() as u64
        );
        worker
            .sessions
            .get_mut(&id)
            .unwrap()
            .write_input(b"next-owned-output\n")
            .unwrap();
        output_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let before_log = worker.logs[&id].last_log_offset;
        let mut charged_effects = None;
        let phase_admission = crate::InputAdmission::new(
            crate::InputPermit::new(),
            Instant::now() + Duration::from_secs(5),
            |write| write(),
        );
        phase_admission.admit(|| {
            charged_effects = Some(worker.collect_session_pump_effects(&[id], false, true));
            assert_eq!(
                worker.logs[&id].last_log_offset, before_log,
                "collector wrote to disk while admission was held"
            );
        });
        let charged_effects = charged_effects.unwrap();
        assert_eq!(charged_effects.deferred_logs.len(), 1);
        let raw = &charged_effects.deferred_logs[0].1;
        assert!(!raw.is_empty() && raw.len() <= Session::INPUT_GUARD_OUTPUT_MAX_BYTES);
        assert_eq!(
            raw.capacity(),
            Session::INPUT_GUARD_OUTPUT_MAX_BYTES,
            "actual deferred raw-body charge must be bounded"
        );
        worker.finish_session_pump_effects(charged_effects);
        assert!(worker.logs[&id].last_log_offset > before_log);
        let mut effects = None;
        let admission = crate::InputAdmission::new(
            crate::InputPermit::new(),
            Instant::now() + Duration::from_secs(5),
            |write| write(),
        );
        admission.admit(|| {
            effects = Some(worker.collect_session_pump_effects(&[id], false, true));
        });
        assert!(
            effects.unwrap().deferred_logs.is_empty(),
            "no output allocates no deferred body"
        );
    }

    #[test]
    #[cfg(unix)]
    fn pr13_ordinary_keyboard_does_not_drain_or_wait_for_output() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _events) = admission_worker(resolver, "pr13-keyboard-output");
        let id = SessionId(1);
        let (output_tx, output_rx) = std::sync::mpsc::channel();
        let mut live = Session::spawn_with_spec_and_output_wake(
            id,
            session::SessionKind::Shell,
            &spec(
                "/bin/sh",
                &["-c", r"stty -echo; printf '\033[?2004l'; exec /bin/cat"],
            ),
            80,
            24,
            100,
            Arc::new(move || {
                let _ = output_tx.send(());
            }),
        )
        .unwrap();
        live.replay_ansi(&mut "\x1b[?2004h".as_bytes()).unwrap();
        worker.sessions.insert(id, live);
        output_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let pending = worker.sessions[&id].pending_output_bytes();
        assert!(pending > 0);
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"manual"], None),
            Ok(())
        );
        assert!(worker.sessions[&id].bracketed_paste());
        assert_eq!(worker.sessions[&id].pending_output_bytes(), pending);
    }

    #[test]
    #[cfg(unix)]
    fn pr9_actual_paste_admission_rechecks_dec2004_at_queue_boundary() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _events) = admission_worker(resolver, "pr9-dec2004");
        let id = SessionId(1);
        let mut live = Session::spawn_with_spec(
            id,
            session::SessionKind::Shell,
            &spec("/bin/cat", &[]),
            80,
            24,
            100,
        )
        .unwrap();
        live.replay_ansi(&mut "\x1b[?2004h".as_bytes()).unwrap();
        worker.sessions.insert(id, live);
        let admission = crate::InputAdmission::new(
            crate::InputPermit::new(),
            Instant::now() + Duration::from_secs(5),
            |write| write(),
        )
        .with_bracketed_paste_required();
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b""], Some(&admission)),
            Ok(())
        );
        worker
            .sessions
            .get_mut(&id)
            .unwrap()
            .replay_ansi(&mut "\x1b[?2004l".as_bytes())
            .unwrap();
        assert_eq!(
            worker.admit_input_batch_checked(
                id,
                &[b"\x1b[200~line1\nline2\x1b[201~", b"\r"],
                Some(&admission)
            ),
            Err(pty::PtyInputRejectReason::AdmissionDenied),
            "actual mode-off must refuse whole paste and submit"
        );
    }

    #[test]
    #[cfg(unix)]
    fn followup_codex_effort_keys_do_not_block_next_step_or_prompt() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _events) = admission_worker(resolver, "followup-codex-effort");
        let id = SessionId(1);
        let live = Session::spawn_with_spec(
            id,
            session::SessionKind::Shell,
            &spec(
                "/bin/sh",
                &["-c", "stty -echo -icanon; printf READY; exec /bin/cat"],
            ),
            80,
            24,
            100,
        )
        .unwrap();
        let group = live.process_identity().process_group.unwrap();
        worker.sessions.insert(id, live);
        worker.detectors.insert(
            id,
            session::StatusDetector::new(session::StatusPatterns::compile(None, None, None, None)),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker.sessions[&id].screen_text().contains("READY") {
            let effects = worker.collect_session_pump_effects(&[id], false, true);
            worker.finish_session_pump_effects(effects);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let redraw = |worker: &mut Worker| {
            worker
                .sessions
                .get_mut(&id)
                .unwrap()
                .replay_ansi(&mut "\x1b[2J\x1b[H› ".as_bytes())
                .unwrap();
        };
        let admission =
            crate::InputAdmission::new(crate::InputPermit::new(), deadline, |write| write())
                .with_agent_guard(crate::AgentInputGuard {
                    foreground_process_group: group,
                    provider: crate::AgentPromptKind::Codex,
                    intent: crate::AgentInputIntent::AutomaticPrompt,
                });
        for bytes in [b"\x1b[1;2B".as_slice(), b"\x1b[1;2A".as_slice()] {
            redraw(&mut worker);
            assert_eq!(
                worker.admit_input_batch_checked(id, &[bytes], Some(&admission)),
                Ok(())
            );
            assert!(
                !worker.detectors[&id].has_input_draft(),
                "Codex effort key must not create phantom text"
            );
        }
        redraw(&mut worker);
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"next prompt", b"\r"], Some(&admission)),
            Ok(())
        );
        assert!(!worker.detectors[&id].has_input_draft());
        redraw(&mut worker);
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"\x1b[A"], Some(&admission)),
            Ok(())
        );
        assert!(
            worker.detectors[&id].has_input_draft(),
            "ordinary history arrow can recall draft text"
        );
        redraw(&mut worker);
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"\x1b[1;2B"], Some(&admission)),
            Err(pty::PtyInputRejectReason::AdmissionDenied)
        );
        worker
            .detectors
            .get_mut(&id)
            .unwrap()
            .on_user_input(b"\x03genuine draft");
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"\x1b[1;2B"], Some(&admission)),
            Err(pty::PtyInputRejectReason::AdmissionDenied)
        );
    }

    #[test]
    #[cfg(unix)]
    fn composer_codex_placeholder_accepts_multiline_prompt_in_owned_pty() {
        for prompt in [
            "한글 첫 줄\n두 번째 줄\n세 번째 줄".to_owned(),
            "가".repeat(170),
            "가".repeat(171),
            "가".repeat(2_000),
            format!("{}\n끝", "가".repeat(30_000)),
        ] {
            let prompt = format!("{prompt}COMPOSER-FIXTURE-END");
            let resolver = Arc::new(RecordingResolver {
                calls: Mutex::new(Vec::new()),
                value: None,
            });
            let (mut worker, _events) = admission_worker(resolver, "composer-codex-placeholder");
            let id = SessionId(1);
            let live = Session::spawn_with_spec(
            id,
            session::SessionKind::Shell,
            &spec("/bin/sh", &["-c", r"stty raw -echo; printf '\033[?2004h› \033[2mAsk Codex to do anything\033[0m\r\033[2C'; exec /bin/cat"]),
            80, 24, 100,
        ).unwrap();
            let group = live.process_identity().process_group.unwrap();
            worker.sessions.insert(id, live);
            worker.detectors.insert(
                id,
                session::StatusDetector::new(session::StatusPatterns::compile(
                    None, None, None, None,
                )),
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while !worker.sessions[&id].bracketed_paste() {
                assert!(Instant::now() < deadline);
                worker.collect_session_pump_effects(&[id], false, true);
                std::thread::sleep(Duration::from_millis(1));
            }
            let admission =
                crate::InputAdmission::new(crate::InputPermit::new(), deadline, |write| write())
                    .with_agent_guard(crate::AgentInputGuard {
                        foreground_process_group: group,
                        provider: crate::AgentPromptKind::Codex,
                        intent: crate::AgentInputIntent::ExplicitPrompt,
                    });
            let body = format!("\x1b[200~{prompt}\x1b[201~");
            assert_eq!(
                worker.admit_input_batch_checked(id, &[body.as_bytes(), b"\r"], Some(&admission)),
                Ok(())
            );
            assert!(!worker.detectors[&id].has_input_draft());
            while !worker.sessions[&id]
                .screen_text()
                .contains("COMPOSER-FIXTURE-END")
            {
                assert!(
                    Instant::now() < deadline,
                    "accepted bytes must reach the owned PTY reader"
                );
                worker.collect_session_pump_effects(&[id], false, true);
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    #[test]
    #[cfg(unix)]
    fn composer_ready_prompt_is_not_vetoed_by_previous_stream_status() {
        for (tag, status) in [
            ("WAIT-FIXTURE", session::SessionStatus::Waiting),
            ("APPROVAL-FIXTURE", session::SessionStatus::NeedsApproval),
        ] {
            for (provider, ready) in [
                (crate::AgentPromptKind::Claude, "❯ "),
                (
                    crate::AgentPromptKind::Codex,
                    "› \x1b[2mAsk Codex to do anything\x1b[0m\r\x1b[2C",
                ),
            ] {
                let resolver = Arc::new(RecordingResolver {
                    calls: Mutex::new(Vec::new()),
                    value: None,
                });
                let (mut worker, _events) = admission_worker(resolver, "composer-stale-stream");
                let id = SessionId(1);
                // The former question/approval is gone; a real CLI has redrawn its native
                // input. Its stream status stays latched until accepted user input.
                let script = format!(
                    "stty raw -echo; printf '{tag}\\n\\033[2J\\033[H\\033[?2004h{ready}'; exec /bin/cat"
                );
                let live = Session::spawn_with_spec(
                    id,
                    session::SessionKind::Agent,
                    &spec("/bin/sh", &["-c", &script]),
                    80,
                    24,
                    100,
                )
                .unwrap();
                let group = live.process_identity().process_group.unwrap();
                worker.sessions.insert(id, live);
                worker.detectors.insert(
                    id,
                    session::StatusDetector::new(session::StatusPatterns::compile(
                        Some("WAIT-FIXTURE"),
                        Some("APPROVAL-FIXTURE"),
                        None,
                        None,
                    )),
                );
                let deadline = Instant::now() + Duration::from_secs(5);
                while !worker.sessions[&id].bracketed_paste() {
                    assert!(Instant::now() < deadline);
                    let effects = worker.collect_session_pump_effects(&[id], false, true);
                    worker.finish_session_pump_effects(effects);
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert_eq!(worker.detectors[&id].status(), status);
                assert!(!worker.detectors[&id].has_input_draft());
                let guard = |intent| {
                    crate::InputAdmission::new(crate::InputPermit::new(), deadline, |write| write())
                        .with_agent_guard(crate::AgentInputGuard {
                            foreground_process_group: group,
                            provider,
                            intent,
                        })
                };
                assert_eq!(
                    worker.admit_input_batch_checked(
                        id,
                        &[b""],
                        Some(&guard(crate::AgentInputIntent::AutomaticPrompt))
                    ),
                    Err(pty::PtyInputRejectReason::AdmissionDenied),
                    "automatic sends remain conservative"
                );
                let prompt = format!("{}\nCOMPOSER-READY-END", "긴 문장 ".repeat(2_000));
                let body = format!("\x1b[200~{prompt}\x1b[201~");
                assert_eq!(
                    worker.admit_input_batch_checked(
                        id,
                        &[body.as_bytes(), b"\r"],
                        Some(&guard(crate::AgentInputIntent::ExplicitPrompt))
                    ),
                    Ok(()),
                    "current native input must accept explicit user submit despite previous stream status"
                );
                assert!(!worker.detectors[&id].has_input_draft());
                while !worker.sessions[&id]
                    .screen_text()
                    .contains("COMPOSER-READY-END")
                {
                    assert!(
                        Instant::now() < deadline,
                        "accepted long prompt must reach owned PTY"
                    );
                    let effects = worker.collect_session_pump_effects(&[id], false, true);
                    worker.finish_session_pump_effects(effects);
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn composer_ready_hint_still_protects_current_question_and_accepted_draft() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _events) = admission_worker(resolver, "composer-current-screen");
        let id = SessionId(1);
        let live = Session::spawn_with_spec(
            id,
            session::SessionKind::Agent,
            &spec(
                "/bin/sh",
                &["-c", "stty raw -echo; printf READY; exec /bin/cat"],
            ),
            80,
            24,
            100,
        )
        .unwrap();
        let group = live.process_identity().process_group.unwrap();
        worker.sessions.insert(id, live);
        worker.detectors.insert(
            id,
            session::StatusDetector::new(session::StatusPatterns::compile(
                Some("STREAM-WAIT"),
                None,
                None,
                None,
            )),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker.sessions[&id].screen_text().contains("READY") {
            assert!(Instant::now() < deadline);
            let effects = worker.collect_session_pump_effects(&[id], false, true);
            worker.finish_session_pump_effects(effects);
            std::thread::sleep(Duration::from_millis(1));
        }
        let admission =
            crate::InputAdmission::new(crate::InputPermit::new(), deadline, |write| write())
                .with_agent_guard(crate::AgentInputGuard {
                    foreground_process_group: group,
                    provider: crate::AgentPromptKind::Codex,
                    intent: crate::AgentInputIntent::ExplicitPrompt,
                });
        let hint = "› \x1b[2mAsk Codex to do anything\x1b[0m\r\x1b[2C";
        // A current real question is not just a former stream state. Even a native
        // hint must not override that screen-derived approval observation.
        let current = format!("\x1b[2J\x1b[HWould you like to run fixture? [y/n]\r\n{hint}");
        worker
            .sessions
            .get_mut(&id)
            .unwrap()
            .replay_ansi(&mut current.as_bytes())
            .unwrap();
        let text = worker.sessions[&id].screen_text();
        worker.detectors.get_mut(&id).unwrap().evaluate(Some(&text));
        assert_eq!(
            worker.detectors[&id].status(),
            session::SessionStatus::NeedsApproval
        );
        assert_eq!(
            worker.detectors[&id].status_view(None).source,
            session::StatusSource::ScreenText
        );
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"must-not-send", b"\r"], Some(&admission)),
            Err(pty::PtyInputRejectReason::AdmissionDenied)
        );
        // An old approval can retain StreamRegex precedence even when the current
        // screen also contains an approval. The source label alone is insufficient.
        let detector = worker.detectors.get_mut(&id).unwrap();
        *detector = session::StatusDetector::new(session::StatusPatterns::compile(
            Some("STREAM-WAIT"),
            Some("STREAM-APPROVAL"),
            None,
            None,
        ));
        detector.on_output(b"STREAM-APPROVAL\n");
        detector.evaluate(Some(&text));
        assert_eq!(
            detector.status_view(None).source,
            session::StatusSource::StreamRegex
        );
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"must-not-send", b"\r"], Some(&admission)),
            Err(pty::PtyInputRejectReason::AdmissionDenied),
            "current approval must win even over an equally ranked stream latch"
        );
        // Input recalled before the next screen echo is still authoritative draft
        // evidence. A native placeholder and old stream state cannot erase it.
        worker
            .sessions
            .get_mut(&id)
            .unwrap()
            .replay_ansi(&mut format!("\x1b[2J\x1b[H{hint}").as_bytes())
            .unwrap();
        let detector = worker.detectors.get_mut(&id).unwrap();
        detector.on_input();
        detector.on_output(b"STREAM-WAIT\n");
        detector.on_user_input(b"\x1b[A");
        assert!(detector.has_input_draft());
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"must-not-send", b"\r"], Some(&admission)),
            Err(pty::PtyInputRejectReason::AdmissionDenied)
        );
    }

    #[test]
    #[cfg(unix)]
    fn composer_explicit_prompt_accepts_redrawn_empty_editor_after_cleared_native_draft() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _events) = admission_worker(resolver, "composer-cleared-native-draft");
        let id = SessionId(1);
        let mut live = Session::spawn_with_spec(
            id,
            session::SessionKind::Shell,
            &spec("/bin/cat", &[]),
            80,
            24,
            100,
        )
        .unwrap();
        let group = live.process_identity().process_group.unwrap();
        live.replay_ansi(&mut "❯ ".as_bytes()).unwrap();
        worker.sessions.insert(id, live);
        worker.detectors.insert(
            id,
            session::StatusDetector::new(session::StatusPatterns::compile(None, None, None, None)),
        );
        let admission = crate::InputAdmission::new(
            crate::InputPermit::new(),
            Instant::now() + Duration::from_secs(5),
            |write| write(),
        )
        .with_agent_guard(crate::AgentInputGuard {
            foreground_process_group: group,
            provider: crate::AgentPromptKind::Claude,
            intent: crate::AgentInputIntent::ExplicitPrompt,
        });

        // The native editor may erase text without a submit byte (e.g. backspace/Ctrl+U).
        let detector = worker.detectors.get_mut(&id).unwrap();
        detector.on_user_input(b"old native draft\x15");
        assert!(detector.has_input_draft());
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"new prompt", b"\r"], Some(&admission)),
            Err(pty::PtyInputRejectReason::AdmissionDenied),
            "without a fresh native redraw, accepted-input evidence still protects the draft"
        );
        let draft_redraw = b"\x1b[2J\x1b[H\xe2\x9d\xaf still here";
        worker
            .sessions
            .get_mut(&id)
            .unwrap()
            .replay_ansi(&mut draft_redraw.as_slice())
            .unwrap();
        worker
            .detectors
            .get_mut(&id)
            .unwrap()
            .on_output(draft_redraw);
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"new prompt", b"\r"], Some(&admission)),
            Err(pty::PtyInputRejectReason::AdmissionDenied),
            "a visible native draft must never be overwritten"
        );
        // Claude can leave a continuation line after Ctrl+U erases the first
        // line. The cursor row then looks empty while the editor is not.
        let multiline_redraw =
            "\x1b[2J\x1b[H❯ \r\n  remaining native draft\r\n────────────────────\x1b[1;3H";
        worker
            .sessions
            .get_mut(&id)
            .unwrap()
            .replay_ansi(&mut multiline_redraw.as_bytes())
            .unwrap();
        worker
            .detectors
            .get_mut(&id)
            .unwrap()
            .on_output(multiline_redraw.as_bytes());
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"new prompt", b"\r"], Some(&admission)),
            Err(pty::PtyInputRejectReason::AdmissionDenied),
            "a draft on another native editor row must still block composer submit"
        );
        let redraw = b"\x1b[2J\x1b[H\xe2\x9d\xaf ";
        worker
            .sessions
            .get_mut(&id)
            .unwrap()
            .replay_ansi(&mut redraw.as_slice())
            .unwrap();
        worker.detectors.get_mut(&id).unwrap().on_output(redraw);
        let automatic = crate::InputAdmission::new(
            crate::InputPermit::new(),
            Instant::now() + Duration::from_secs(5),
            |write| write(),
        )
        .with_agent_guard(crate::AgentInputGuard {
            foreground_process_group: group,
            provider: crate::AgentPromptKind::Claude,
            intent: crate::AgentInputIntent::AutomaticPrompt,
        });
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"auto", b"\r"], Some(&automatic)),
            Err(pty::PtyInputRejectReason::AdmissionDenied),
            "automation must still refuse accepted-draft evidence"
        );
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"new prompt", b"\r"], Some(&admission)),
            Ok(()),
            "a fresh, positively empty native editor must admit the deliberate composer send"
        );
    }

    #[test]
    #[cfg(unix)]
    fn pr2_actual_admission_preserves_existing_tui_draft_and_dialog() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _events) = admission_worker(resolver, "pr2-input-draft");
        let id = SessionId(1);
        let mut live = Session::spawn_with_spec(
            id,
            session::SessionKind::Shell,
            &spec("/bin/cat", &[]),
            80,
            24,
            100,
        )
        .unwrap();
        let group = live.process_identity().process_group.unwrap();
        live.replay_ansi(&mut "❯ ".as_bytes()).unwrap();
        worker.sessions.insert(id, live);
        worker.detectors.insert(
            id,
            session::StatusDetector::new(session::StatusPatterns::compile(None, None, None, None)),
        );
        let admission = crate::InputAdmission::new(
            crate::InputPermit::new(),
            Instant::now() + Duration::from_secs(5),
            |write| write(),
        )
        .with_agent_guard(crate::AgentInputGuard {
            foreground_process_group: group,
            provider: crate::AgentPromptKind::Claude,
            intent: crate::AgentInputIntent::ExplicitPrompt,
        });
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b""], Some(&admission)),
            Ok(()),
            "positive empty cursor-row and foreground control"
        );
        for input in [
            b"\x1b[".as_slice(),
            b"D\x1b[I\x7f",
            b"\x1b[200~\x1b[201~",
            b"\x14",
        ] {
            worker.detectors.get_mut(&id).unwrap().on_user_input(input);
        }
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b""], Some(&admission)),
            Ok(()),
            "empty prompt remains usable after split navigation and empty bracketed paste"
        );
        worker
            .detectors
            .get_mut(&id)
            .unwrap()
            .on_user_input(b"\x1b[A");
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b""], Some(&admission)),
            Err(pty::PtyInputRejectReason::AdmissionDenied),
            "history recall protects input before screen echo"
        );
        worker
            .detectors
            .get_mut(&id)
            .unwrap()
            .on_user_input(b"\x03");
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"draft"], None),
            Ok(())
        );
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"AUTOMATIC", b"\r"], Some(&admission)),
            Err(pty::PtyInputRejectReason::AdmissionDenied),
            "must not append to unsubmitted local draft"
        );
        let append = crate::InputAdmission::new(
            crate::InputPermit::new(),
            Instant::now() + Duration::from_secs(5),
            |write| write(),
        )
        .with_agent_guard(crate::AgentInputGuard {
            foreground_process_group: group,
            provider: crate::AgentPromptKind::Claude,
            intent: crate::AgentInputIntent::ExplicitAppend,
        });
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b" SELECTED"], Some(&append)),
            Ok(()),
            "deliberate no-submit append preserves existing manual draft"
        );
        assert!(worker.detectors[&id].has_input_draft());
        worker
            .detectors
            .get_mut(&id)
            .unwrap()
            .on_user_input(b"\x03");
        worker
            .sessions
            .get_mut(&id)
            .unwrap()
            .replay_ansi(&mut "\r\n❯ Yes, allow\r\nEnter to select".as_bytes())
            .unwrap();
        worker
            .detectors
            .get_mut(&id)
            .unwrap()
            .evaluate(Some("❯ Yes, allow\nEnter to select"));
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"AUTOMATIC", b"\r"], Some(&admission)),
            Err(pty::PtyInputRejectReason::AdmissionDenied),
            "dialog cannot receive natural prompt"
        );
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b" SELECTED"], Some(&append)),
            Err(pty::PtyInputRejectReason::AdmissionDenied),
            "explicit append still respects dialog safety"
        );
    }

    #[test]
    #[cfg(unix)]
    fn pr2_automatic_unknown_and_wrong_foreground_fail_closed_but_explicit_other_is_useful() {
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            value: None,
        });
        let (mut worker, _events) = admission_worker(resolver, "pr2-unknown");
        let id = SessionId(1);
        let mut live = Session::spawn_with_spec(
            id,
            session::SessionKind::Shell,
            &spec("/bin/cat", &[]),
            80,
            24,
            100,
        )
        .unwrap();
        let group = live.process_identity().process_group.unwrap();
        live.replay_ansi(&mut "unknown provider input".as_bytes())
            .unwrap();
        worker.sessions.insert(id, live);
        worker.detectors.insert(
            id,
            session::StatusDetector::new(session::StatusPatterns::compile(None, None, None, None)),
        );
        let guard = |intent, foreground| {
            crate::InputAdmission::new(
                crate::InputPermit::new(),
                Instant::now() + Duration::from_secs(5),
                |write| write(),
            )
            .with_agent_guard(crate::AgentInputGuard {
                foreground_process_group: foreground,
                provider: crate::AgentPromptKind::Other,
                intent,
            })
        };
        assert_eq!(
            worker.admit_input_batch_checked(
                id,
                &[b""],
                Some(&guard(crate::AgentInputIntent::AutomaticPrompt, group))
            ),
            Err(pty::PtyInputRejectReason::AdmissionDenied)
        );
        assert_eq!(
            worker.admit_input_batch_checked(
                id,
                &[b""],
                Some(&guard(crate::AgentInputIntent::ExplicitPrompt, group))
            ),
            Ok(())
        );
        assert_eq!(
            worker.admit_input_batch_checked(
                id,
                &[b""],
                Some(&guard(
                    crate::AgentInputIntent::ExplicitAppend,
                    group.saturating_add(1)
                ))
            ),
            Err(pty::PtyInputRejectReason::AdmissionDenied)
        );
        assert_eq!(
            worker.admit_input_batch_checked(
                id,
                &[b"explicit prompt", b"\r"],
                Some(&guard(crate::AgentInputIntent::ExplicitPrompt, group))
            ),
            Ok(())
        );
        assert!(!worker.detectors[&id].has_input_draft());
        // Manual normal-shell input is unaffected by automatic provider readiness.
        assert_eq!(
            worker.admit_input_batch_checked(id, &[b"manual"], None),
            Ok(())
        );
        assert!(worker.detectors[&id].has_input_draft());
    }

    #[test]
    #[cfg(unix)]
    fn pr1_guarded_batch_has_one_permission_check_and_correlated_acceptance() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("pr1-batch"),
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
        let session = probe.wait_for(Duration::from_secs(5), |event| match event {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&checks);
        let permit = crate::InputPermit::new();
        let admission = crate::InputAdmission::new(
            permit.clone(),
            Instant::now() + Duration::from_secs(5),
            move |write| {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                write();
            },
        );
        client
            .send_guarded_input_batch(
                session,
                "pr1:accepted".into(),
                vec![
                    "\x1b[200~한글\n😀\x1b[201~".as_bytes().to_vec(),
                    b"\r".to_vec(),
                ],
                admission,
            )
            .unwrap();
        let result = probe.wait_for(Duration::from_secs(5), |event| match event {
            RuntimeEvent::InputAdmitted {
                operation_id,
                result,
                ..
            } if operation_id == "pr1:accepted" => Some(*result),
            _ => None,
        });
        assert_eq!(result, Ok(()));
        assert_eq!(checks.load(std::sync::atomic::Ordering::SeqCst), 1);
        let submitted = probe
            .seen
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::SessionInputSubmitted { .. }))
            .count();
        assert_eq!(
            submitted, 1,
            "only the submit CR outside bracketed paste marks submission"
        );
        permit.revoke();
        let admission =
            crate::InputAdmission::new(permit, Instant::now() + Duration::from_secs(5), |write| {
                write()
            });
        client
            .send_guarded_input_batch(
                session,
                "pr1:denied".into(),
                vec![b"body".to_vec(), b"\r".to_vec()],
                admission,
            )
            .unwrap();
        let result = probe.wait_for(Duration::from_secs(5), |event| match event {
            RuntimeEvent::InputAdmitted {
                operation_id,
                result,
                ..
            } if operation_id == "pr1:denied" => Some(*result),
            _ => None,
        });
        assert_eq!(result, Err(pty::PtyInputRejectReason::AdmissionDenied));
        assert_eq!(
            probe
                .seen
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::SessionInputSubmitted { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn pr1_stale_session_batch_is_correlated_rejection_without_submission() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("pr1-stale"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::WriteInputBatchTracked {
                session: SessionId(u64::MAX),
                operation_id: "pr1:stale".into(),
                parts: vec![b"body".to_vec(), b"\r".to_vec()],
            })
            .unwrap();
        let result = probe.wait_for(Duration::from_secs(5), |event| match event {
            RuntimeEvent::InputAdmitted {
                operation_id,
                result,
                ..
            } if operation_id == "pr1:stale" => Some(*result),
            _ => None,
        });
        assert_eq!(result, Err(pty::PtyInputRejectReason::SessionClosed));
        assert!(
            !probe
                .seen
                .iter()
                .any(|event| matches!(event, RuntimeEvent::SessionInputSubmitted { .. }))
        );
    }

    #[test]
    #[cfg(unix)]
    fn fleet_review_fix_submission_event_requires_real_admission_and_no_paste_newline() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("fleet-submit"),
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
        let session = probe.wait_for(Duration::from_secs(5), |event| match event {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        for bytes in [
            b"typed".as_slice(),
            b"\x1b[20",
            b"0~first\nsecond\r",
            b"\x1b[201~",
        ] {
            client
                .send_command(RuntimeCommand::WriteInput {
                    session,
                    bytes: bytes.to_vec(),
                })
                .unwrap();
        }
        client
            .send_command(RuntimeCommand::WriteInputTracked {
                session: SessionId(u64::MAX),
                operation_id: "rejected-submit".into(),
                bytes: b"\r".to_vec(),
            })
            .unwrap();
        client
            .send_command(RuntimeCommand::DurableEventBarrier { correlation_id: 11 })
            .unwrap();
        probe.wait_for(Duration::from_secs(5), |event| {
            matches!(
                event,
                RuntimeEvent::DurableEventBarrierReached { correlation_id: 11 }
            )
            .then_some(())
        });
        assert!(
            !probe
                .seen
                .iter()
                .any(|event| matches!(event, RuntimeEvent::SessionInputSubmitted { .. }))
        );
        client
            .send_command(RuntimeCommand::WriteInput {
                session,
                bytes: b"\r".to_vec(),
            })
            .unwrap();
        client
            .send_command(RuntimeCommand::DurableEventBarrier { correlation_id: 12 })
            .unwrap();
        probe.wait_for(Duration::from_secs(5), |event| {
            matches!(
                event,
                RuntimeEvent::DurableEventBarrierReached { correlation_id: 12 }
            )
            .then_some(())
        });
        let submissions: Vec<_> = probe
            .seen
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::SessionInputSubmitted { session, at_micros } => {
                    Some((*session, *at_micros))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            submissions.len(),
            1,
            "accepted Enter must emit exactly one timestamp-only boundary"
        );
        assert_eq!(submissions[0].0, session);
        assert!(submissions[0].1 > 0);
    }

    #[test]
    #[cfg(unix)]
    fn tracked_input_reports_real_pty_admission_rejection_and_output() {
        let mut client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("tracked-admission"),
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
            .send_command(RuntimeCommand::WriteInputTracked {
                session,
                operation_id: "accepted".into(),
                bytes: b"TRACKED_ROUNDTRIP\r".to_vec(),
            })
            .unwrap();
        let result = probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::InputAdmitted {
                operation_id,
                result,
                ..
            } if operation_id == "accepted" => Some(*result),
            _ => None,
        });
        assert_eq!(result, Ok(()));
        probe.wait_for(Duration::from_secs(15), |e| {
            e.viewport()
                .and_then(|(_, s, _, _)| snapshot_contains(s, "TRACKED_ROUNDTRIP").then_some(()))
        });
        client
            .send_command(RuntimeCommand::WriteInputTracked {
                session: SessionId(u64::MAX),
                operation_id: "missing".into(),
                bytes: b"ignored".to_vec(),
            })
            .unwrap();
        assert_eq!(
            probe.wait_for(Duration::from_secs(15), |e| match e {
                RuntimeEvent::InputAdmitted {
                    operation_id,
                    result,
                    ..
                } if operation_id == "missing" => Some(*result),
                _ => None,
            }),
            Err(pty::PtyInputRejectReason::SessionClosed)
        );
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
        let pressure_for_session = |event: &RuntimeEvent| {
            matches!(
                event,
                RuntimeEvent::PtyInputPressure {
                    session: pressure_session,
                    ..
                } if *pressure_session == session
            )
        };
        // 첫 쓰기(정확히 max_bytes)는 큐를 채우지만 압박은 내지 않는다 — 압박은
        // 가득 찬 큐에 대한 다음 쓰기에서 난다. 고정 100ms sleep은 워커가 첫 쓰기를
        // 큐로 옮길 때까지의 유일한 동기화라 느린 공유 CI 러너에서 실패할 수 있어,
        // 첫 압박 이벤트("큐 가득"의 관측 가능한 신호)까지 쓰기를 반복한다 (2026-08-04).
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        // A rejected runtime admission has no PTY effect. Retry only that
        // explicit outcome; accepted writes are never retried here.
        let send_saturated = || loop {
            match client.send_command(RuntimeCommand::WriteInput {
                session,
                bytes: saturated.clone(),
            }) {
                Ok(()) => break,
                Err(error)
                    if matches!(
                        error.downcast_ref::<RuntimeCommandSendError>(),
                        Some(RuntimeCommandSendError::Backpressure)
                    ) =>
                {
                    assert!(Instant::now() < deadline, "runtime admission stayed full");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("runtime input admission failed: {error:#}"),
            }
        };
        let mut sent = 0usize;
        loop {
            send_saturated();
            sent += 1;
            if probe.rx.drain().iter().any(pressure_for_session) {
                break;
            }
            // 커맨드 큐 예산(4MB 쓰기 수 건 분)이 차지 않도록, 워커가 이전 명령을
            // 소비한 뒤 다음 쓰기를 보낸다 — 이 대기가 곧 큐 적체를 기다리는 것이다.
            while client.command_budget.retained_bytes() != 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "워커가 WriteInput을 소비하지 못함"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(sent < 64, "압박 이벤트가 오지 않음");
        }
        // 코얼레싱 확인 — 추가 쓰기 8건을 같은 방식으로 전부 소비시킨 뒤 압박이
        // 더 발행되지 않아야 한다. command_budget이 0이면 워커가 명령을 전부 소비한
        // 것이고, 그 시점에 발행될 이벤트는 이미 채널에 있다.
        for _ in 0..8 {
            send_saturated();
            while client.command_budget.retained_bytes() != 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "워커가 WriteInput을 소비하지 못함"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let events = probe.rx.drain();
        let pressure_count = events.iter().filter(|e| pressure_for_session(e)).count();
        // 워커는 backpressured된 쓰기마다 발행하지만, 구독 채널은 세션당 최신 1 slot으로
        // 코얼레싱한다 (ResourceUsage와 같은 latest-value 규칙) — 드레인에는 1건만 남는다.
        assert_eq!(pressure_count, 1, "세션당 압박 이벤트 코얼레싱 회귀");
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
            conn.execute_batch(persist::MIGRATION_SESSION_REGEX)
                .unwrap();
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
            conn.execute_batch(persist::MIGRATION_SESSION_REGEX)
                .unwrap();
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
        // agent 세션으로 검증. 2026-08-19: agent도 exit 시 pane이 자동으로 닫혀
        // layout에서 사라지므로("셸_exit시_pane_자동_닫힘_agent도_동일하게_닫힌다"
        // 참고), "pane→session_id 참조가 저장됐는지"는 세션이 살아 있는 동안(exit
        // 전에) 확인해야 한다 — attach_in_new_tab 직후 emit_mux_snapshot이 이미
        // save_layout을 거치므로 AgentSpawned를 받은 시점에 이미 반영돼 있다.
        // "exit 상태가 영속되는지"는 그 뒤 exit을 기다려 별도로 확인한다.
        client
            .send_command(spawn_agent_cmd("echo persist-ok", None, None))
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::AgentSpawned { .. } => Some(()),
            _ => None,
        });
        // mux layout: window/tab/pane가 저장되고 pane이 영속 session id를 참조
        // (아직 살아 있는 시점 — exit 후에는 pane 자체가 닫혀 확인할 수 없다)
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let windows = persist::load_window_layouts(&conn, "ws-rt").unwrap();
            assert_eq!(windows.len(), 1);
            assert_eq!(windows[0].tabs.len(), 1);
            let pane_session = windows[0].tabs[0].panes[0].session_id.clone().unwrap();
            let persisted_id: String = conn
                .query_row("SELECT id FROM sessions", [], |r| r.get(0))
                .unwrap();
            assert_eq!(pane_session, persisted_id);
        }
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::SessionExited { .. } => Some(()),
            _ => None,
        });
        // exit 기록(persist pump) 완료를 DB 상태로 관측한다 — 고정 100ms sleep은
        // 느린 공유 CI 러너에서 pump보다 먼저 읽어 실패할 수 있다 (2026-08-04).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status = rusqlite::Connection::open(&db_path)
                .unwrap()
                .query_row("SELECT status FROM sessions", [], |r| r.get::<_, String>(0))
                .ok();
            if status.as_deref() == Some("exited") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "exit 상태가 영속되지 않음"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let (kind, command, status): (String, String, String) = conn
            .query_row(
                "SELECT session_kind, command, status FROM sessions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        // config id 없는 SpawnAgent는 스키마 CHECK(agent kind ⇒ agent_id 필수) 때문에
        // "shell" kind로 기록된다 — 런타임 SessionKind는 여전히 Agent다(위에서 이미
        // 확인한 layout 참조와 무관하게 exit 시 pane은 닫힌다).
        assert_eq!(kind, "shell");
        assert_eq!(command, "/bin/sh");
        assert_eq!(status, "exited");
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
    fn archive_cache_revalidates_replaced_logs_root_before_incrementing() {
        let (mut worker, _event_rx) = admission_worker(
            Arc::new(RecordingResolver {
                calls: Mutex::new(Vec::new()),
                value: None,
            }),
            "archive-root-cache-replacement",
        );
        worker.archive_disk_bytes = 0;
        worker.archive_root_identity =
            storage::scrollback_archive::root_identity(&worker.logs_root).unwrap();
        let old_root = worker.logs_root.with_extension("old");
        std::fs::rename(&worker.logs_root, &old_root).unwrap();
        let seeded = worker.logs_root.join("seed").join("scrollback.zlib");
        std::fs::create_dir_all(seeded.parent().unwrap()).unwrap();
        let seeded_file = std::fs::File::create(&seeded).unwrap();
        seeded_file
            .set_len(storage::scrollback_archive::ARCHIVE_DISK_BUDGET_BYTES)
            .unwrap();
        seeded_file
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000))
            .unwrap();
        let key = "trigger";
        let receipt = storage::scrollback_archive::write_receipt(
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
        let written = receipt.bytes();
        let session = SessionId(100);

        worker.finish_archive_write(session, key, receipt);

        assert!(
            !seeded.exists(),
            "fresh GC must account the replacement root"
        );
        assert!(storage::scrollback_archive::exists(&worker.logs_root, key));
        assert!(worker.archived_on_disk.contains_key(&session));
        assert_eq!(worker.archive_disk_bytes, written);
        assert_eq!(
            worker.archive_root_identity,
            storage::scrollback_archive::root_identity(&worker.logs_root).unwrap()
        );
        std::fs::remove_dir_all(&worker.logs_root).unwrap();
        std::fs::remove_dir_all(old_root).unwrap();
    }

    #[test]
    fn oversized_archive_dump_is_rejected_before_redaction() {
        let production = include_str!("in_process.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        let length_check = production
            .find("dump.len() > storage::scrollback_archive::MAX_UNCOMPRESSED_BYTES")
            .expect("archive dump length must be checked before redaction");
        let redaction = production
            .find("let mut redactor = self.redaction.stream_redactor()")
            .unwrap();

        assert!(length_check < redaction);
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
        let receipt = storage::scrollback_archive::write_receipt(
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

        worker.finish_archive_write(session, key, receipt);

        std::fs::set_permissions(
            old_path.parent().unwrap(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(!storage::scrollback_archive::exists(&worker.logs_root, key));
        assert!(
            !worker.archived_on_disk.contains_key(&session),
            "a GC-evicted triggering archive must not leave a false disk marker"
        );
    }

    /// PR-A1: 세션 exit 시 최종 grid가 디스크 아카이브(scrollback.zlib)로 기록되고,
    /// 메타·내용이 라운드트립된다 (suspend/재시작 생존의 원천).
    #[cfg(unix)]
    #[test]
    fn exit시_scrollback_아카이브가_디스크에_기록된다() {
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
            conn.execute_batch(persist::MIGRATION_SESSION_REGEX)
                .unwrap();
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
        // 아카이브 기록(exit 처리) 완료를 관측 가능한 상태로 기다린다 — 고정 200ms
        // sleep은 느린 공유 CI 러너에서 워커보다 먼저 읽어 실패할 수 있다 (2026-08-04).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (meta, dump) = loop {
            let uuid = rusqlite::Connection::open(&db_path)
                .unwrap()
                .query_row("SELECT id FROM sessions", [], |r| r.get::<_, String>(0))
                .ok();
            let archived = uuid
                .as_deref()
                .and_then(|uuid| storage::scrollback_archive::read(&logs_root, uuid).ok())
                .flatten();
            if let Some(archived) = archived {
                break archived;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "아카이브가 기록되지 않음"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(meta.kind, 1, "SpawnAgent 세션은 agent kind");
        assert_eq!(meta.exit_code, Some(0));
        let text = String::from_utf8_lossy(&dump);
        assert!(text.contains("archive-roundtrip-marker"), "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unknown_archive_usage_blocks_new_writes_and_makes_gc_progress() {
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
            conn.execute_batch(persist::MIGRATION_SESSION_REGEX)
                .unwrap();
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
        // GC 진행(exit 처리) 완료를 관측 가능한 상태로 기다린다 — 고정 200ms sleep은
        // 느린 공유 CI 러너에서 워커보다 먼저 검사해 실패할 수 있다 (2026-08-04).
        // 진입 차단된 아카이브는 애초에 기록되지 않으므로 !exists는 GC 관측 후에도 유효.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::fs::read_dir(&logs_root).unwrap().count() < entries_before {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "GC 진행이 관측되지 않음"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

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
            conn.execute_batch(persist::MIGRATION_SESSION_REGEX)
                .unwrap();
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
        // 롤백(exit 처리의 post-write GC) 완료를 관측 가능한 상태로 기다린다 — 고정
        // 200ms sleep은 느린 공유 CI 러너에서 롤백보다 먼저 검사해 실패할 수 있다
        // (2026-08-04). 롤백 회귀(파일 잔존)는 상한에서 실패한다.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let uuid = rusqlite::Connection::open(&db_path)
                .unwrap()
                .query_row("SELECT id FROM sessions", [], |row| row.get::<_, String>(0))
                .ok();
            if uuid
                .as_deref()
                .is_some_and(|uuid| !storage::scrollback_archive::exists(&logs_root, uuid))
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "failed post-write GC must roll back the triggering archive"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(client);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// PR-A2: agent pane은 재시작 후 respawn 대신 열람 전용 복원된다 —
    /// 아카이브 1차 → (파일 삭제 시) 로그 tail 폴백, 재결속으로 2회 왕복에도
    /// pane↔UUID 연결이 유지된다.
    #[cfg(unix)]
    #[test]
    fn 재시작시_agent_pane은_열람전용으로_복원된다() {
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
            conn.execute_batch(persist::MIGRATION_SESSION_REGEX)
                .unwrap();
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
                .filter(|c| !c.wide_spacer())
                .map(|c| c.c)
                .collect()
        };

        // 1) agent(config id 있음 — DB kind 'agent') 실행 → 워커 종료(app 종료 흉내)
        //
        // 2026-08-19: 예전엔 여기서 SessionExited를 기다렸지만, agent도 exit 시 pane이
        // 자동으로 닫히는 지금은 그러면 mux_panes 행이 재시작 전에 이미 지워져 복원할
        // 게 없어진다(테스트 취지 파괴) — 그리고 실제 프로덕션에서도 agent는 항상
        // wrap_agent_then_shell로 감싸여 있어, 진짜 agent 세션이 SessionExited를 내는
        // 시점은 이미 사용자가 그 pane에서 명시적으로 exit을 친 뒤다(그때는 셸처럼
        // pane이 사라지는 게 맞다 — in_process.rs의 pump_sessions 주석 참고). 그래서
        // "재시작 시 열람 전용 복원"이 실제로 의미 있는 시나리오는 "pane이 아직 살아
        // 있는 도중 앱이 통째로 꺼진(비정상 종료/그냥 종료) 경우"다 — 세션을 절대 exit
        // 시키지 않고 워커를 그냥 drop해 그 상황을 흉내낸다. 스크롤백 아카이브는
        // Worker 종료 루프가 "아직 running인 agent"도 예외 없이 기록한다(위 1286행
        // 부근 "running 셸은 제외 — running agent는 기록" 주석 참고) — 그래서 exit을
        // 기다리지 않아도 아카이브가 남는다.
        {
            let client = make_client();
            let mut probe = Probe::new(client.subscribe());
            let mut cmd = spawn_agent_cmd("printf 'a2-restore-marker\\n'; sleep 30", None, None);
            if let RuntimeCommand::SpawnAgent {
                agent_config_id, ..
            } = &mut cmd
            {
                *agent_config_id = Some("cfg-1".into());
            }
            client.send_command(cmd).unwrap();
            // 마커가 화면에 반영될 때까지 — 세션은 sleep 30으로 계속 살아 있다.
            probe.wait_for(Duration::from_secs(15), |e| match e {
                RuntimeEvent::Viewport { snapshot, .. }
                    if viewport_text(snapshot).contains("a2-restore-marker") =>
                {
                    Some(())
                }
                _ => None,
            });
            // client/probe를 여기서 drop — InProcessRuntimeClient::shutdown이
            // worker.join()으로 종료 루프(아카이브 기록 포함)를 동기 대기한다.
        }
        // join이 동기라 이론상 이 시점에 아카이브가 이미 존재해야 하지만, 원래
        // 테스트(2026-08-04 주석)가 겪은 느린 공유 CI 러너의 파일시스템 가시성 지연을
        // 그대로 방어해 둔다.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let uuid = rusqlite::Connection::open(&db_path)
                .unwrap()
                .query_row("SELECT id FROM sessions", [], |r| r.get::<_, String>(0))
                .ok();
            let archived = uuid
                .as_deref()
                .is_some_and(|uuid| storage::scrollback_archive::exists(&logs_root, uuid));
            if archived {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "아카이브가 기록되지 않음"
            );
            std::thread::sleep(Duration::from_millis(20));
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
            !worker.archived_on_disk.contains_key(&failed_session),
            "failed rebind must not leave a disk archive marker without a session"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// PR-2: 열람 전용으로 복원된 agent pane을 사용자가 재실행하면 (1) 저장된
    /// command/args 뒤에 extra_args가 붙고, (2) 새 탭이 아니라 그 pane 자리를
    /// 유지하고, (3) 같은 영속 UUID를 재사용해 행이 늘지 않는다.
    #[cfg(unix)]
    #[test]
    fn respawn_archived_agent_reuses_pane_persistent_id_and_appends_extra_args() {
        let dir = unique_test_dir("respawn-success");
        let db_path = dir.join("metadata.sqlite3");
        let logs_root = dir.join("logs");
        let workspace_id = "ws-respawn-success";
        let persistent_id = "respawn-success-session";
        create_persist_db(&db_path, workspace_id);
        seed_persisted_agent_session_pane(
            &db_path,
            workspace_id,
            persistent_id,
            "cfg-sf03",
            "/bin/echo",
            vec!["stored-arg".to_owned()],
            persist::SESSION_STATUS_EXITED,
            "/tmp",
        );

        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            logs_root.clone(),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path: db_path.clone(),
                workspace_id: workspace_id.to_owned(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::RestoreWorkspace)
            .unwrap();
        let restored = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .any(|pane| pane.session_id.is_some()) =>
            {
                Some(snapshot.clone())
            }
            _ => None,
        });
        assert_eq!(restored.tabs.len(), 1, "fixture는 tab 1개/pane 1개");
        let pane = restored
            .tabs
            .iter()
            .flat_map(|tab| &tab.panes)
            .find(|pane| pane.session_id.is_some())
            .unwrap();
        let pane_id = pane.id.clone();
        let archived_session = pane.session_id.unwrap();
        assert_eq!(pane.persistent_session_id.as_deref(), Some(persistent_id));

        client
            .send_command(RuntimeCommand::RespawnArchivedAgent {
                session: archived_session,
                extra_args: vec!["extra-arg".to_owned()],
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();

        let new_session = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::AgentSpawned { session } => Some(*session),
            _ => None,
        });
        assert_ne!(
            new_session, archived_session,
            "재실행은 새 runtime SessionId를 할당해야 한다"
        );

        let text = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::Viewport {
                session, snapshot, ..
            } if *session == new_session => {
                let text = snapshot_text(snapshot, 0);
                text.contains("stored-arg").then_some(text)
            }
            _ => None,
        });
        assert_eq!(
            text, "stored-arg extra-arg",
            "저장된 args 뒤에 extra_args가 그대로 붙어야 한다"
        );

        let after = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .any(|pane| pane.id == pane_id && pane.session_id == Some(new_session)) =>
            {
                Some(snapshot.clone())
            }
            _ => None,
        });
        assert_eq!(
            after.tabs.len(),
            1,
            "새 탭이 생기면 안 된다 — 같은 pane 자리를 유지해야 한다"
        );
        let after_pane = after
            .tabs
            .iter()
            .flat_map(|tab| &tab.panes)
            .find(|pane| pane.id == pane_id)
            .unwrap();
        assert_eq!(
            after_pane.persistent_session_id.as_deref(),
            Some(persistent_id),
            "같은 영속 UUID를 재사용해 로그/아카이브 연속성을 유지해야 한다"
        );

        drop(probe);
        drop(client);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let rows = persist::load_sessions(&conn, workspace_id).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "행이 늘면 안 된다 — 같은 UUID를 재사용해야 한다"
        );
        assert_eq!(rows[0].id, persistent_id);
        assert_eq!(
            rows[0].args,
            vec!["stored-arg".to_owned()],
            "영속 launch spec(args)에 extra_args가 누적되면 안 된다"
        );
        drop(conn);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// PR-2: 재실행이 PTY spawn 단계에서 실패하면 이전 archived pane/세션/영속 행이
    /// 그대로 보존돼야 한다 — 「다시 실행」 실패가 열람 전용 화면을 잃게 하면 안 된다.
    #[cfg(unix)]
    #[test]
    fn respawn_archived_agent_failure_preserves_previous_archived_state() {
        let dir = unique_test_dir("respawn-failure");
        let db_path = dir.join("metadata.sqlite3");
        let logs_root = dir.join("logs");
        let workspace_id = "ws-respawn-failure";
        let persistent_id = "respawn-failure-session";
        create_persist_db(&db_path, workspace_id);
        // spawn_실패_이벤트와 같은 기법 — 존재하지 않는 실행 파일로 PTY spawn 자체를
        // 확실히 실패시킨다(파일 부재를 exec 단계에서 검증하는 실제 실패 경로).
        seed_persisted_agent_session_pane(
            &db_path,
            workspace_id,
            persistent_id,
            "cfg-sf03",
            "HOSTILE_SPAWN_PATH_COMMAND_MARKER",
            Vec::new(),
            persist::SESSION_STATUS_EXITED,
            "/tmp",
        );

        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            logs_root.clone(),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path: db_path.clone(),
                workspace_id: workspace_id.to_owned(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::RestoreWorkspace)
            .unwrap();
        let restored = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .any(|pane| pane.session_id.is_some()) =>
            {
                Some(snapshot.clone())
            }
            _ => None,
        });
        let pane = restored
            .tabs
            .iter()
            .flat_map(|tab| &tab.panes)
            .find(|pane| pane.session_id.is_some())
            .unwrap();
        let pane_id = pane.id.clone();
        let archived_session = pane.session_id.unwrap();

        client
            .send_command(RuntimeCommand::RespawnArchivedAgent {
                session: archived_session,
                extra_args: Vec::new(),
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::SpawnFailed {
                kind: SpawnKind::Agent,
                ..
            } => Some(()),
            _ => None,
        });
        // 실패 후 AgentSpawned이 뒤늦게 오지 않는지도 확인 — 조금 더 드레인한다.
        std::thread::sleep(Duration::from_millis(150));
        probe.seen.extend(probe.rx.drain());
        assert!(
            !probe
                .seen
                .iter()
                .any(|event| matches!(event, RuntimeEvent::AgentSpawned { .. })),
            "실패한 재실행이 AgentSpawned를 내면 안 된다"
        );

        drop(probe);
        drop(client);

        // 이전 archived 세션/pane 결속이 그대로다 — 실패가 화면을 잃게 만들지 않는다.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let rows = persist::load_sessions(&conn, workspace_id).unwrap();
        assert_eq!(rows.len(), 1, "행이 늘면 안 된다");
        assert_eq!(rows[0].id, persistent_id);
        assert_eq!(
            rows[0].status,
            persist::SESSION_STATUS_EXITED,
            "실패한 재실행은 status를 running으로 바꾸면 안 된다"
        );
        assert_eq!(rows[0].command, "HOSTILE_SPAWN_PATH_COMMAND_MARKER");
        let pane_session: String = conn
            .query_row(
                "SELECT session_id FROM mux_panes WHERE id = ?1",
                [&pane_id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            pane_session, persistent_id,
            "실패한 재실행은 pane↔영속 세션 결속을 바꾸면 안 된다"
        );
        drop(conn);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// PR-2: archived pane이 아닌 대상(라이브 세션·미존재 세션)에는 안전하게 실패만
    /// 하고 아무것도 건드리지 않는다.
    #[cfg(unix)]
    #[test]
    fn respawn_archived_agent_ignores_non_archived_targets() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("respawn-ignore-live"),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            None,
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(spawn_agent_cmd("sleep 5", None, None))
            .unwrap();
        let live_session = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::AgentSpawned { session } => Some(*session),
            _ => None,
        });

        // 1) 라이브(실행 중) 세션 — archived가 아니므로 거부돼야 한다.
        client
            .send_command(RuntimeCommand::RespawnArchivedAgent {
                session: live_session,
                extra_args: Vec::new(),
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        // 2) 존재한 적 없는 session id — pane이 없으므로 거부돼야 한다.
        client
            .send_command(RuntimeCommand::RespawnArchivedAgent {
                session: SessionId(999_999),
                extra_args: Vec::new(),
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        // DurableEventBarrier로 위 두 명령의 이벤트가 이미 큐에 들어왔음을 보장한 뒤
        // 센다 — 같은 조건을 반복 wait_for하면 누적 history의 첫 매치만 보고
        // 두 번째 실패를 놓칠 수 있다(codex 리뷰 방지 관례와 동일한 이유).
        client
            .send_command(RuntimeCommand::DurableEventBarrier { correlation_id: 7 })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::DurableEventBarrierReached { correlation_id: 7 } => Some(()),
            _ => None,
        });
        probe.seen.extend(probe.rx.drain());

        let failed_count = probe
            .seen
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    RuntimeEvent::SpawnFailed {
                        kind: SpawnKind::Agent,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            failed_count, 2,
            "라이브 세션과 미존재 세션 각각 안전하게 실패해야 한다"
        );
        let spawned_count = probe
            .seen
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::AgentSpawned { .. }))
            .count();
        assert_eq!(
            spawned_count, 1,
            "원래 라이브 agent 하나만 spawn된 채여야 한다 — 재실행이 끼어들면 안 된다"
        );

        drop(probe);
        drop(client);
    }

    /// 복원 UX (PR-14, 설계문서 §11.1~11.5·§14): 첫 worker가 만든 셸 2개 + split
    /// 1개(tab 2개/pane 3개) 구조가 종료 후 새 worker 시작 시 fresh 셸로 복원되는지
    /// 확인한다. 임시 파일 DB(WAL) — worker 자체 연결의 다중 프로세스 재시작 시나리오를
    /// in-memory보다 정확히 재현한다.
    #[cfg(unix)]
    #[test]
    fn 재시작시_저장된_layout이_복원된다() {
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
            conn.execute_batch(persist::MIGRATION_SESSION_REGEX)
                .unwrap();
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
                    .filter(|cell| !cell.wide_spacer())
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
            conn.execute_batch(persist::MIGRATION_SESSION_REGEX)
                .unwrap();
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

    /// 셸 세션 화면 복원(2026-08-19): SSH로 원격에 붙어 vim·htop·tmux 같은
    /// alt-screen 프로그램을 보던 중 앱이 재시작돼도 그 화면이 통째로 사라지면
    /// 안 된다. restore_pane의 셸 respawn 경로가 alt-screen을 그냥 finish_ansi_replay로
    /// 끝내버리면(§ finish_ansi_replay 원래 동작) 이 내용을 되찾을 길이 없다 —
    /// scrollback 검색으로 보존을 확인하고, 동시에 fresh 셸이 즉시 입력 가능한지도
    /// 같이 검증한다("화면 보존"과 "셸 재사용성" 둘 다).
    #[cfg(unix)]
    #[test]
    fn 재시작시_alt_screen이었던_셸_pane도_화면이_보존된다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-rt-altscreen-restore-{}-{}",
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
                 INSERT INTO workspaces (id) VALUES ('ws-alt');",
            )
            .unwrap();
            conn.execute_batch(persist::MIGRATION_SQL).unwrap();
            conn.execute_batch(persist::MIGRATION_SESSION_REGEX)
                .unwrap();
        }
        let persist_config = || crate::persistence::PersistConfig {
            db_path: db_path.clone(),
            workspace_id: "ws-alt".into(),
        };

        {
            // 원격 TUI를 흉내: alt-screen에 들어가 마커 텍스트를 찍고, exit 없이(연결이
            // 끊긴 채) 그대로 둔다 — 앱이 재시작될 때 흔한 "TUI 화면에 멈춰있던" 상태.
            let client = InProcessRuntimeClient::with_shell(
                5,
                test_store(),
                logs_root.clone(),
                RedactionService::new(),
                spec(
                    "/bin/sh",
                    &[
                        "-c",
                        r"printf '\033[?1049hREMOTE-VIM-BUFFER'; exec /bin/cat",
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
            probe.wait_for(Duration::from_secs(15), |event| match event {
                RuntimeEvent::Viewport { snapshot, .. }
                    if snapshot.is_alt_screen
                        && snapshot.visible_cells.iter().any(|cell| cell.c == 'R') =>
                {
                    Some(())
                }
                _ => None,
            });
        }

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
        let restored_session = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::Viewport {
                session, snapshot, ..
            } if !snapshot.is_alt_screen => Some(*session),
            _ => None,
        });

        // 1) 화면 보존 — alt-screen 내용이 scrollback에서 찾아져야 한다(위로 스크롤하면 보임).
        client
            .send_command(RuntimeCommand::SearchScrollback {
                session: restored_session,
                query: "REMOTE-VIM-BUFFER".into(),
                max_matches: 10,
            })
            .unwrap();
        let found = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::ScrollbackSearchResult {
                session, result, ..
            } if *session == restored_session => Some(!result.matches.is_empty()),
            _ => None,
        });
        assert!(found, "alt-screen 화면이 scrollback에 보존돼야 함");

        // 2) 셸 재사용성 — fresh 셸(/bin/cat)이 살아있어 입력이 그대로 에코된다.
        client
            .send_command(RuntimeCommand::WriteInput {
                session: restored_session,
                bytes: b"FRESH-INPUT-ECHO\n".to_vec(),
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::Viewport {
                session, snapshot, ..
            } if *session == restored_session
                && snapshot
                    .visible_cells
                    .iter()
                    .any(|cell| cell.c == 'E' && !cell.wide_spacer()) =>
            {
                snapshot
                    .visible_cells
                    .iter()
                    .map(|cell| cell.c)
                    .collect::<String>()
                    .contains("FRESH-INPUT-ECHO")
                    .then_some(())
            }
            _ => None,
        });

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
    fn tracked_resize_worker는_실제크기_ack과_멱등_stamp를_반환한다() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            test_logs_root("resize-tracked"),
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
        let session = probe.wait_for(Duration::from_secs(3), |event| match event {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        let token = crate::ResizeToken {
            owner: [1; 16],
            generation: 1,
            owner_epoch: 1,
        };
        let command = RuntimeCommand::ResizeTracked {
            session,
            token,
            cols: 101,
            rows: 31,
        };
        client.send_command(command.clone()).unwrap();
        let first = probe.wait_for(Duration::from_secs(2), |event| match event {
            RuntimeEvent::ResizeApplied { session: id, stamp } if *id == session => Some(*stamp),
            _ => None,
        });
        assert_eq!(
            (first.cols, first.rows, first.token),
            (101, 31, Some(token))
        );
        client.send_command(command).unwrap();
        let retry = probe.wait_for(Duration::from_secs(2), |event| match event {
            RuntimeEvent::ResizeApplied { session: id, stamp } if *id == session => Some(*stamp),
            _ => None,
        });
        assert_eq!(first, retry);
        let next = crate::ResizeToken {
            owner: [2; 16],
            generation: 1,
            owner_epoch: 2,
        };
        client
            .send_command(RuntimeCommand::ResizeTracked {
                session,
                token: next,
                cols: 110,
                rows: 40,
            })
            .unwrap();
        let applied = probe.wait_for(Duration::from_secs(2), |event| match event {
            RuntimeEvent::ResizeApplied { stamp, .. } if stamp.token == Some(next) => Some(*stamp),
            _ => None,
        });
        client
            .send_command(RuntimeCommand::ResizeTracked {
                session,
                token,
                cols: 101,
                rows: 31,
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(2), |event| matches!(event,
            RuntimeEvent::ResizeFailed { token: rejected, reason: crate::ResizeFailure::Superseded, .. } if *rejected == token).then_some(()));
        let actual = probe.wait_for(Duration::from_secs(2), |event| match event {
            RuntimeEvent::ViewportTracked {
                snapshot, stamp, ..
            } if *stamp == applied => Some((snapshot.cols, snapshot.rows)),
            _ => None,
        });
        assert_eq!(
            actual,
            (110, 40),
            "늦은 이전 owner는 실제 backend도 되돌리지 않는다"
        );
    }

    #[cfg(unix)]
    #[test]
    fn subscribe_with_wake는_상태이벤트에_깨운다() {
        use std::sync::atomic::{AtomicUsize, Ordering};
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
        // 스트림이 흐르기 시작한 것 확인 — wake는 발행 시점에 동기 호출되므로, 첫 원격
        // Viewport 관측 시점에 스폰기 노이즈의 wake는 이미 반영돼 있다. 여기서 0으로
        // 초기화하면 노이즈가 결정적으로 배제된다 (2026-08-04: 고정 300ms settle +
        // 800ms 측정 창은 느린 공유 CI 러너에서 스트림 처리 지연으로 bg<5가 될 수 있었다).
        probe.wait_for(Duration::from_secs(15), |e| match e {
            RuntimeEvent::Viewport { session: s, .. } if *s == session => Some(()),
            _ => None,
        });
        gui_wakes.store(0, Ordering::SeqCst);
        bg_wakes.store(0, Ordering::SeqCst);
        // bg wake 5회 도달을 관측한다 — 50ms 간격 스트림이 살아 있으면 도달하고,
        // 깨어나지 않는 회귀는 10s 상한에서 실패한다.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while bg_wakes.load(Ordering::SeqCst) < 5 {
            assert!(
                std::time::Instant::now() < deadline,
                "백그라운드 구독자가 원격 viewport에 깨어나지 않음"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let gui = gui_wakes.load(Ordering::SeqCst);
        let bg = bg_wakes.load(Ordering::SeqCst);
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
            conn.execute_batch(persist::MIGRATION_SESSION_REGEX)
                .unwrap();
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

    /// MIGRATION_SESSION_REGEX 불변식의 핵심: 세션 spawn 시점에 저장해둔 error_regex가
    /// RespawnArchivedAgent를 거친 뒤에도 살아 있어야 한다. `RuntimeCommand`에는 regex가
    /// 없으므로(시그니처 고정) — 재실행된 세션이 실제로 "FATAL_MARKER" 출력을 error로
    /// 감지한다면, 그건 오직 세션 행에서 복원한 regex로 detector를 만들었다는 뜻이다.
    /// (idle heuristic만 있었다면 이 출력은 Running으로 남고 Error는 오지 않는다.)
    #[cfg(unix)]
    #[test]
    fn respawn_archived_agent는_저장된_error_regex로_상태를_감지한다() {
        let dir = unique_test_dir("respawn-regex");
        let db_path = dir.join("metadata.sqlite3");
        let logs_root = dir.join("logs");
        let workspace_id = "ws-respawn-regex";
        let persistent_id = "respawn-regex-session";
        create_persist_db(&db_path, workspace_id);
        seed_persisted_agent_session_pane(
            &db_path,
            workspace_id,
            persistent_id,
            "cfg-sf03",
            "/bin/sh",
            vec!["-c".to_owned(), "echo FATAL_MARKER; sleep 30".to_owned()],
            persist::SESSION_STATUS_EXITED,
            "/tmp",
        );
        // seed 헬퍼는 regex 파라미터가 없다(다른 테스트에 영향 없게) — 저장된 행에
        // 직접 세팅한다. 실제 경로에서는 SpawnAgent 처리 시 session_spawned가 채운다.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "UPDATE sessions SET error_regex = 'FATAL_MARKER' WHERE id = ?1",
                [persistent_id],
            )
            .unwrap();
        }

        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
            logs_root.clone(),
            RedactionService::new(),
            spec("/bin/cat", &[]),
            Some(crate::persistence::PersistConfig {
                db_path: db_path.clone(),
                workspace_id: workspace_id.to_owned(),
            }),
        );
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::RestoreWorkspace)
            .unwrap();
        let restored = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::MuxUpdated { snapshot }
                if snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .any(|pane| pane.session_id.is_some()) =>
            {
                Some(snapshot.clone())
            }
            _ => None,
        });
        let archived_session = restored
            .tabs
            .iter()
            .flat_map(|tab| &tab.panes)
            .find_map(|pane| pane.session_id)
            .unwrap();

        client
            .send_command(RuntimeCommand::RespawnArchivedAgent {
                session: archived_session,
                extra_args: Vec::new(),
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let new_session = probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::AgentSpawned { session } => Some(*session),
            _ => None,
        });

        // 타임아웃 시 wait_for 자체가 panic한다 — 저장된 error_regex를 복원하지
        // 못했다면(=None,None,None,None으로 detector가 만들어졌다면) 이 출력은
        // idle heuristic상 Running으로 남고 Error 이벤트는 영영 오지 않는다.
        probe.wait_for(Duration::from_secs(15), |event| match event {
            RuntimeEvent::SessionStatusChanged {
                session,
                status: session::SessionStatus::Error,
            } if *session == new_session => Some(()),
            _ => None,
        });

        drop(probe);
        drop(client);
        std::fs::remove_dir_all(&dir).ok();
    }
}
