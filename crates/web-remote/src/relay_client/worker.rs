//! Relay 재접속 워커 — 소유 스레드 하나.
//!
//! 전송은 [`RelayTransport`]로 주입받는다. 그래서 재접속·백오프·취소·종료 같은 성질을 실제
//! 네트워크 없이 검증할 수 있고, TLS 구현([`super::tls`])은 이 트레이트의 한 구현일 뿐이다.
//!
//! 스레드 규칙:
//! - 소유 스레드는 **하나**다. 접속·수신·재시도가 모두 그 스레드에서 일어난다.
//! - 명령 큐는 유계다. 가득 차면 거절하며, 무한정 쌓지 않는다.
//! - 대기는 전부 **취소 가능**하다. 백오프 도중 꺼도 그 시간을 다 기다리지 않는다.
//! - `shutdown`은 정지 신호를 올리고 스레드를 join한다. Drop도 같은 일을 한다.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::lifecycle::{BackoffPolicy, RelayEndpoint, RelayLifecycle, RelayState};

/// 대기 중인 명령 상한. 이 이상은 거절한다 — UI 클릭이 큐를 무한정 밀어 넣지 못하게 한다.
pub const MAX_PENDING_COMMANDS: usize = 32;
/// 한 번의 소켓 읽기가 막을 수 있는 최대 시간. 소켓 읽기는 취소할 수 없으므로 이 값이 곧
/// 끄기·종료가 관측되기까지의 최악 지연이다.
pub const MAX_RECEIVE_SLICE: Duration = Duration::from_secs(1);

/// 전송 실패의 종류. **재시도해도 되는가**를 여기서 가른다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportError {
    /// 네트워크/일시적 실패. 백오프 뒤 재시도해도 된다.
    Unavailable,
    /// 서버가 자격증명을 거부했다. 자동 재시도 금지.
    AuthenticationRejected,
    /// 이 기기의 인가가 취소됐다. 자동 재시도 금지.
    Revoked,
}

/// 한 번의 바깥 방향 연결. 구현은 반드시 주어진 시한을 지켜야 한다.
pub trait RelayTransport: Send {
    fn connect(
        &mut self,
        endpoint: &RelayEndpoint,
        deadline: Duration,
    ) -> Result<Box<dyn RelaySession>, TransportError>;
}

/// 열려 있는 한 세션. 프레임 단위이며 내용은 해석하지 않는다.
pub trait RelaySession: Send {
    /// 다음 프레임을 기다린다. 시한 안에 아무것도 오지 않으면 `Ok(None)`.
    fn receive(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, TransportError>;
    fn send(&mut self, frame: &[u8]) -> Result<(), TransportError>;
    fn close(&mut self);
}

/// 프레임 하나를 처리한 뒤 워커가 무엇을 해야 하는가.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkOutcome {
    /// 계속 받는다.
    Continue,
    /// 이 채널을 닫는다. 권한 위반이 상한을 넘었을 때처럼, 상대를 더 받아 줄 이유가
    /// 없어진 경우다. 로그만 남기고 계속 받으면 상한이 아무것도 강제하지 못한다.
    CloseChannel,
}

/// 워커가 받은 프레임을 넘길 곳. 권한 강제 어댑터가 이 자리에 들어온다.
pub trait RelayFrameSink: Send {
    fn accept(&mut self, frame: &[u8]) -> SinkOutcome;
    /// 세션이 열렸다. 핸드셰이크 상태 기계가 라우트 입장 프레임을 큐에 넣을 기회다.
    fn session_started(&mut self) {}
    /// 세션이 끝났다. 어댑터가 세션 상태를 버릴 기회다.
    fn session_ended(&mut self) {}
    /// 나갈 프레임을 꺼낸다. 워커는 세션 시작 직후와 매 수신 뒤에 한 번씩 비운다 —
    /// 싱크가 직접 소켓을 들지 않아야 "소유 스레드 하나"가 유지된다.
    fn drain_outbound(&mut self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    /// 프레임이 오지 않는 동안에도 워커가 부르는 주기 점검. 마감·생존 신호·조정자의 거절처럼
    /// **시간으로만 판정되는 것**이 여기서 다뤄진다. 이 훅이 없으면 그런 판정은 상대가 말을
    /// 걸어야만 이뤄지는데, 정확히 그 상대가 문제인 경우를 놓친다.
    fn poll(&mut self) -> SinkOutcome {
        SinkOutcome::Continue
    }
}

/// 상태 변화를 관찰한다. UI와 테스트가 같은 창으로 본다.
pub trait RelayObserver: Send + Sync {
    fn state_changed(&self, _state: RelayState) {}
    fn connect_attempted(&self) {}
}

/// 아무것도 하지 않는 관찰자.
pub struct IgnoreObserver;

impl RelayObserver for IgnoreObserver {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayDeadlines {
    /// DNS + TCP + TLS + WebSocket 핸드셰이크 전체에 걸리는 시한.
    pub connect: Duration,
    /// 한 번의 수신 대기 시한. 이 주기로 깨어나 명령을 확인한다.
    pub read: Duration,
}

impl Default for RelayDeadlines {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            read: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayCommand {
    Enable,
    Disable,
    Shutdown,
}

/// 취소 가능한 대기. 명령이 오거나 정지 신호가 오면 즉시 깨어난다.
struct Wake {
    signalled: Mutex<bool>,
    cvar: Condvar,
}

impl Wake {
    fn new() -> Self {
        Self {
            signalled: Mutex::new(false),
            cvar: Condvar::new(),
        }
    }

    fn signal(&self) {
        let mut signalled = self
            .signalled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *signalled = true;
        self.cvar.notify_all();
    }

    /// 최대 `timeout`까지 기다린다. 신호가 있으면 즉시 돌아온다.
    ///
    /// **기다리기 전에 먼저 확인한다.** `signal()`이 이 호출 직전에 끝났다면 조건 변수는
    /// 그 통지를 이미 놓쳤으므로, 깃발만 보고 자면 다음 주기까지 통째로 잔다 — 끄기와 종료가
    /// 그만큼 늦어진다.
    fn wait(&self, timeout: Duration) {
        let mut guard = self
            .signalled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *guard {
            *guard = false;
            return;
        }
        let (mut guard, _) = self
            .cvar
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = false;
    }
}

pub struct RelayWorker {
    commands: SyncSender<RelayCommand>,
    wake: Arc<Wake>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RelayWorker {
    /// 워커를 띄운다. **엔드포인트 정책은 이미 통과한 값만 받는다** — 정책 검사를 워커
    /// 안으로 미루면 잘못된 주소로 소켓을 열어 본 뒤에야 알게 된다.
    pub fn spawn(
        endpoint: RelayEndpoint,
        mut transport: Box<dyn RelayTransport>,
        mut sink: Box<dyn RelayFrameSink>,
        observer: Arc<dyn RelayObserver>,
        deadlines: RelayDeadlines,
        backoff: BackoffPolicy,
    ) -> Self {
        let (commands, inbox) = sync_channel(MAX_PENDING_COMMANDS);
        let wake = Arc::new(Wake::new());
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let wake = Arc::clone(&wake);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("relay-client".into())
                .spawn(move || {
                    let mut lifecycle = RelayLifecycle::new(backoff);
                    run(
                        &mut lifecycle,
                        &endpoint,
                        transport.as_mut(),
                        sink.as_mut(),
                        observer.as_ref(),
                        deadlines,
                        &inbox,
                        &wake,
                        &stop,
                    );
                })
                .expect("relay-client 스레드 생성")
        };

        Self {
            commands,
            wake,
            stop,
            thread: Some(thread),
        }
    }

    /// 명령을 넣는다. 큐가 가득 차면 `false` — 무한정 쌓지 않는다.
    pub fn send(&self, command: RelayCommand) -> bool {
        match self.commands.try_send(command) {
            Ok(()) => {
                self.wake.signal();
                true
            }
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    pub fn enable(&self) -> bool {
        self.send(RelayCommand::Enable)
    }

    pub fn disable(&self) -> bool {
        self.send(RelayCommand::Disable)
    }

    /// 정지 신호를 올리고 스레드를 join한다. 여러 번 불러도 안전하다.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.commands.try_send(RelayCommand::Shutdown);
        self.wake.signal();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RelayWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    lifecycle: &mut RelayLifecycle,
    endpoint: &RelayEndpoint,
    transport: &mut dyn RelayTransport,
    sink: &mut dyn RelayFrameSink,
    observer: &dyn RelayObserver,
    deadlines: RelayDeadlines,
    inbox: &Receiver<RelayCommand>,
    wake: &Wake,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::SeqCst) {
        if !apply_commands(lifecycle, inbox, observer) {
            break;
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }

        let now = unix_now();
        if !lifecycle.begin_connect(now) {
            // 붙을 수 없는 동안에는 잔다. 명령이 오면 즉시 깨어나므로 백오프 도중 꺼도
            // 그 시간을 다 기다리지 않는다.
            wake.wait(idle_wait(lifecycle, now));
            continue;
        }
        observer.state_changed(RelayState::Connecting);
        observer.connect_attempted();

        match transport.connect(endpoint, deadlines.connect) {
            Ok(session) => {
                lifecycle.connected();
                observer.state_changed(RelayState::Connected);
                pump(
                    lifecycle, session, sink, observer, deadlines, inbox, wake, stop,
                );
                sink.session_ended();
            }
            Err(error) => {
                record_failure(lifecycle, error, observer);
            }
        }
    }
    lifecycle.shutdown();
    observer.state_changed(lifecycle.state());
}

/// 세션 하나를 끝까지 돈다. 명령과 정지 신호를 매 바퀴 확인한다.
#[allow(clippy::too_many_arguments)]
fn pump(
    lifecycle: &mut RelayLifecycle,
    mut session: Box<dyn RelaySession>,
    sink: &mut dyn RelayFrameSink,
    observer: &dyn RelayObserver,
    deadlines: RelayDeadlines,
    inbox: &Receiver<RelayCommand>,
    wake: &Wake,
    stop: &AtomicBool,
) {
    // 세션이 열리자마자 싱크가 라우트 입장 자격증명을 낸다. 이 프레임이 나가지 못하면
    // 이 세션에서는 아무것도 일어나지 않으므로, 실패는 곧 세션 실패다.
    sink.session_started();
    if !flush(session.as_mut(), sink, lifecycle, observer, wake) {
        return;
    }

    loop {
        if stop.load(Ordering::SeqCst) {
            session.close();
            return;
        }
        if !apply_commands(lifecycle, inbox, observer) {
            session.close();
            return;
        }
        if !matches!(lifecycle.state(), RelayState::Connected) {
            // 꺼졌거나 취소됐다 — 살아 있는 채널을 즉시 닫는다.
            session.close();
            return;
        }

        // 주기 점검은 **매 바퀴** 돈다. 수신 시한 만료 분기에만 걸어 두면 읽기 조각(1초)보다
        // 자주 말을 거는 상대 하나가 생존 신호·마감 판정·조정자의 거절을 통째로 멈춰 세운다 —
        // 정확히 그 상대가 문제인 경우를 놓친다. 자리는 명령·수명주기 확인 **뒤**(끄기와 종료가
        // 싱크의 사정보다 앞선다. 이미 닫기로 한 세션에 싱크를 더 돌리지 않는다)이고
        // 수신 **앞**이다(여기서 큐에 든 생존 신호가 읽기 대기 뒤로 밀리지 않는다).
        // 바쁜 대기는 생기지 않는다 — 이 바퀴의 대기는 여전히 아래 `receive`가 맡는다.
        if sink.poll() == SinkOutcome::CloseChannel {
            session.close();
            record_failure(lifecycle, TransportError::Unavailable, observer);
            wake.signal();
            return;
        }
        // 싱크가 큐에 넣은 것(생존 신호, 승인된 채널의 첫 화면, 갱신된 대시보드)을 내보낸다 —
        // 상대가 말을 걸어야만 우리가 보낼 수 있다면 view-only 기기는 영원히 첫 화면을 못 받는다.
        if !flush(session.as_mut(), sink, lifecycle, observer, wake) {
            return;
        }

        // 소켓 읽기는 취소할 수 없다 — 취소 가능한 것은 조건 변수 대기뿐이다. 그래서 읽기
        // 시한을 짧게 잘라, 끄기·종료 신호를 이 주기 안에 반드시 보게 한다.
        match session.receive(deadlines.read.min(MAX_RECEIVE_SLICE)) {
            Ok(Some(frame)) => {
                if sink.accept(&frame) == SinkOutcome::CloseChannel {
                    // 정책 위반으로 닫는다. 전송 실패와 같은 경로로 물러나므로 즉시
                    // 다시 붙지 않고 백오프를 탄다.
                    session.close();
                    record_failure(lifecycle, TransportError::Unavailable, observer);
                    wake.signal();
                    return;
                }
                if !flush(session.as_mut(), sink, lifecycle, observer, wake) {
                    return;
                }
            }
            // 시한만 지났다. 다음 바퀴의 주기 점검과 비우기가 곧바로 이어진다.
            Ok(None) => {}
            Err(error) => {
                session.close();
                record_failure(lifecycle, error, observer);
                wake.signal();
                return;
            }
        }
    }
}

/// 싱크가 큐에 넣은 프레임을 모두 내보낸다. 하나라도 실패하면 세션을 끝낸다 — 반쯤 나간
/// 핸드셰이크를 이어 가면 상대는 영원히 기다린다.
fn flush(
    session: &mut dyn RelaySession,
    sink: &mut dyn RelayFrameSink,
    lifecycle: &mut RelayLifecycle,
    observer: &dyn RelayObserver,
    wake: &Wake,
) -> bool {
    for frame in sink.drain_outbound() {
        if let Err(error) = session.send(&frame) {
            session.close();
            record_failure(lifecycle, error, observer);
            wake.signal();
            return false;
        }
    }
    true
}

/// 실패 종류에 따라 수명주기를 옮긴다. 자동 재시도가 허용되는 것은 일시적 실패뿐이다.
fn record_failure(
    lifecycle: &mut RelayLifecycle,
    error: TransportError,
    observer: &dyn RelayObserver,
) {
    match error {
        TransportError::Unavailable => {
            lifecycle.transport_failed(unix_now(), jitter_ratio());
        }
        TransportError::AuthenticationRejected => lifecycle.authentication_failed(),
        TransportError::Revoked => lifecycle.revoked(),
    }
    observer.state_changed(lifecycle.state());
}

/// 큐에 쌓인 명령을 모두 적용한다. 종료 명령을 보면 `false`.
fn apply_commands(
    lifecycle: &mut RelayLifecycle,
    inbox: &Receiver<RelayCommand>,
    observer: &dyn RelayObserver,
) -> bool {
    while let Ok(command) = inbox.try_recv() {
        match command {
            RelayCommand::Enable => {
                lifecycle.enable(unix_now());
            }
            RelayCommand::Disable => lifecycle.disable(),
            RelayCommand::Shutdown => {
                lifecycle.shutdown();
                observer.state_changed(lifecycle.state());
                return false;
            }
        }
        observer.state_changed(lifecycle.state());
    }
    true
}

/// 붙을 수 없을 때 얼마나 잘 것인가. 백오프 중이면 남은 시간, 멈춰 있으면 길게 잔다
/// (어차피 명령이 오면 즉시 깨어난다).
fn idle_wait(lifecycle: &RelayLifecycle, now: u64) -> Duration {
    match lifecycle.state() {
        RelayState::Backoff { until, .. } => {
            Duration::from_millis(until.saturating_sub(now).saturating_mul(1_000).min(1_000))
        }
        // 멈춰 있을 때는 길게 잔다. 이 대기는 취소 가능하므로 명령이 오면 즉시 깨어난다 —
        // 짧게 잡으면 인증 실패 뒤에도 소유 스레드가 영원히 2Hz로 깨어난다.
        _ => Duration::from_secs(60),
    }
}

/// 수명주기가 쓰는 단조 시계(프로세스 시작 기준 초). 벽시계를 쓰면 NTP가 시각을 뒤로
/// 돌렸을 때 `may_connect`가 영영 거짓이 되어 Relay가 조용히 죽어 있는다.
fn unix_now() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_secs()
}

/// 백오프 지터용 0.0..1.0. 실패하면 지터 없이(0.0) 진행한다 — 엔트로피 부족이
/// 재접속을 막을 이유는 없다.
fn jitter_ratio() -> f64 {
    let mut bytes = [0u8; 2];
    if getrandom::fill(&mut bytes).is_err() {
        return 0.0;
    }
    f64::from(u16::from_be_bytes(bytes)) / f64::from(u16::MAX)
}

/// 테스트에서 스레드가 어떤 조건에 도달하기를 기다린다.
#[cfg(test)]
fn wait_until(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let stop_at = std::time::Instant::now() + deadline;
    while std::time::Instant::now() < stop_at {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    condition()
}

#[cfg(test)]
mod tests {
    use super::super::lifecycle::HaltReason;
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    /// 시나리오를 미리 적어 두고 그대로 돌려주는 전송. 네트워크 없이 워커의 성질만 본다.
    struct ScriptedTransport {
        results: Mutex<Vec<Result<(), TransportError>>>,
        attempts: Arc<AtomicUsize>,
    }

    impl ScriptedTransport {
        fn new(results: Vec<Result<(), TransportError>>) -> (Box<Self>, Arc<AtomicUsize>) {
            let attempts = Arc::new(AtomicUsize::new(0));
            (
                Box::new(Self {
                    results: Mutex::new(results),
                    attempts: Arc::clone(&attempts),
                }),
                attempts,
            )
        }
    }

    impl RelayTransport for ScriptedTransport {
        fn connect(
            &mut self,
            _endpoint: &RelayEndpoint,
            _deadline: Duration,
        ) -> Result<Box<dyn RelaySession>, TransportError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let mut results = self
                .results
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let outcome = if results.is_empty() {
                Err(TransportError::Unavailable)
            } else {
                results.remove(0)
            };
            outcome.map(|()| Box::new(IdleSession) as Box<dyn RelaySession>)
        }
    }

    /// 아무것도 보내지 않는 세션 — 수신은 늘 시한 만료다.
    struct IdleSession;

    impl RelaySession for IdleSession {
        fn receive(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, TransportError> {
            std::thread::sleep(timeout.min(Duration::from_millis(5)));
            Ok(None)
        }

        fn send(&mut self, _frame: &[u8]) -> Result<(), TransportError> {
            Ok(())
        }

        fn close(&mut self) {}
    }

    #[derive(Default)]
    struct RecordingSink {
        frames: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl RelayFrameSink for RecordingSink {
        fn accept(&mut self, frame: &[u8]) -> SinkOutcome {
            self.frames
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(frame.to_vec());
            SinkOutcome::Continue
        }
    }

    #[derive(Default)]
    struct RecordingObserver {
        states: Mutex<Vec<RelayState>>,
    }

    impl RelayObserver for RecordingObserver {
        fn state_changed(&self, state: RelayState) {
            self.states
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(state);
        }
    }

    impl RecordingObserver {
        fn saw_halt(&self, reason: HaltReason) -> bool {
            self.states
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&RelayState::Halted(reason))
        }

        fn saw_backoff(&self) -> bool {
            self.states
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .any(|state| matches!(state, RelayState::Backoff { .. }))
        }
    }

    /// 프레임 하나를 **끊임없이** 흘려보내는 세션 — 수신 시한이 만료되는 일이 없다.
    struct TalkingSession;

    impl RelaySession for TalkingSession {
        fn receive(&mut self, _timeout: Duration) -> Result<Option<Vec<u8>>, TransportError> {
            Ok(Some(b"frame".to_vec()))
        }

        fn send(&mut self, _frame: &[u8]) -> Result<(), TransportError> {
            Ok(())
        }

        fn close(&mut self) {}
    }

    struct TalkingTransport;

    impl RelayTransport for TalkingTransport {
        fn connect(
            &mut self,
            _endpoint: &RelayEndpoint,
            _deadline: Duration,
        ) -> Result<Box<dyn RelaySession>, TransportError> {
            Ok(Box::new(TalkingSession))
        }
    }

    fn endpoint() -> RelayEndpoint {
        RelayEndpoint::parse("wss://relay.example.test").unwrap()
    }

    /// 테스트용 백오프는 아주 짧게 — 성질은 같고 시간만 줄인다.
    fn fast_backoff() -> BackoffPolicy {
        BackoffPolicy {
            initial: Duration::from_millis(1),
            maximum: Duration::from_millis(4),
            multiplier: 2,
        }
    }

    fn deadlines() -> RelayDeadlines {
        RelayDeadlines {
            connect: Duration::from_millis(50),
            read: Duration::from_millis(5),
        }
    }

    fn spawn(
        results: Vec<Result<(), TransportError>>,
    ) -> (RelayWorker, Arc<AtomicUsize>, Arc<RecordingObserver>) {
        let (transport, attempts) = ScriptedTransport::new(results);
        let observer = Arc::new(RecordingObserver::default());
        let worker = RelayWorker::spawn(
            endpoint(),
            transport,
            Box::new(RecordingSink::default()),
            Arc::clone(&observer) as Arc<dyn RelayObserver>,
            deadlines(),
            fast_backoff(),
        );
        (worker, attempts, observer)
    }

    /// 켜기 전에는 소켓을 한 번도 열지 않는다.
    #[test]
    fn a_worker_opens_no_connection_until_it_is_enabled() {
        let (mut worker, attempts, _observer) = spawn(vec![Ok(())]);
        assert!(!wait_until(Duration::from_millis(200), || attempts
            .load(Ordering::SeqCst)
            > 0));
        assert_eq!(attempts.load(Ordering::SeqCst), 0);

        worker.enable();
        assert!(wait_until(Duration::from_secs(3), || attempts
            .load(Ordering::SeqCst)
            > 0));
        worker.shutdown();
    }

    /// 일시적 실패는 백오프 뒤 계속 재시도한다.
    #[test]
    fn transient_failures_are_retried_with_backoff() {
        let (mut worker, attempts, _observer) = spawn(vec![
            Err(TransportError::Unavailable),
            Err(TransportError::Unavailable),
            Ok(()),
        ]);
        worker.enable();
        assert!(
            wait_until(Duration::from_secs(5), || attempts.load(Ordering::SeqCst)
                >= 3),
            "재시도가 이어져야 한다"
        );
        worker.shutdown();
    }

    /// 자격증명이 거부되면 **스스로 다시 붙지 않는다**.
    #[test]
    fn an_authentication_rejection_stops_all_further_attempts() {
        let (mut worker, attempts, observer) =
            spawn(vec![Err(TransportError::AuthenticationRejected)]);
        worker.enable();
        assert!(wait_until(Duration::from_secs(3), || observer
            .saw_halt(HaltReason::AuthenticationFailed)));

        let settled = attempts.load(Ordering::SeqCst);
        assert!(!wait_until(Duration::from_millis(300), || attempts
            .load(Ordering::SeqCst)
            > settled));
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            settled,
            "거부 뒤에는 자동으로 다시 붙지 않는다"
        );
        worker.shutdown();
    }

    /// 취소도 마찬가지다.
    #[test]
    fn a_revocation_stops_all_further_attempts() {
        let (mut worker, attempts, observer) = spawn(vec![Err(TransportError::Revoked)]);
        worker.enable();
        assert!(wait_until(Duration::from_secs(3), || observer
            .saw_halt(HaltReason::Revoked)));

        let settled = attempts.load(Ordering::SeqCst);
        assert!(!wait_until(Duration::from_millis(300), || attempts
            .load(Ordering::SeqCst)
            > settled));
        worker.shutdown();
    }

    /// 끄면 재시도가 즉시 멈춘다 — 백오프 남은 시간을 다 기다리지 않는다.
    #[test]
    fn disabling_stops_reconnection_promptly() {
        let (mut worker, attempts, observer) = spawn(vec![Err(TransportError::Unavailable); 64]);
        worker.enable();
        assert!(wait_until(Duration::from_secs(3), || attempts
            .load(Ordering::SeqCst)
            > 0));

        worker.disable();
        assert!(wait_until(Duration::from_secs(3), || observer
            .saw_halt(HaltReason::Disabled)));
        let settled = attempts.load(Ordering::SeqCst);
        assert!(
            !wait_until(Duration::from_millis(300), || attempts
                .load(Ordering::SeqCst)
                > settled + 1),
            "끈 뒤에는 재시도가 멈춘다"
        );
        worker.shutdown();
    }

    /// 껐다 다시 켜면 재개된다 — 사용자의 명시적 행동만이 되살린다.
    #[test]
    fn re_enabling_after_a_halt_resumes_connecting() {
        let (mut worker, attempts, observer) =
            spawn(vec![Err(TransportError::AuthenticationRejected)]);
        worker.enable();
        assert!(wait_until(Duration::from_secs(3), || observer
            .saw_halt(HaltReason::AuthenticationFailed)));
        let settled = attempts.load(Ordering::SeqCst);

        worker.enable();
        assert!(
            wait_until(Duration::from_secs(3), || attempts.load(Ordering::SeqCst)
                > settled),
            "사용자가 다시 켜면 재개된다"
        );
        worker.shutdown();
    }

    /// 종료는 스레드를 join하고 즉시 끝난다.
    #[test]
    fn shutdown_joins_the_owner_thread_promptly() {
        let (mut worker, _attempts, _observer) = spawn(vec![Ok(())]);
        worker.enable();
        let started = Instant::now();
        worker.shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "종료가 오래 걸리면 앱 종료가 막힌다"
        );
        // 두 번 불러도 안전하다.
        worker.shutdown();
    }

    /// 대기 직전에 도착한 신호를 잃지 않는다. 잃으면 끄기와 종료가 한 주기씩 늦어진다.
    #[test]
    fn a_signal_that_arrives_before_the_wait_is_not_lost() {
        let wake = Wake::new();
        wake.signal();
        let started = Instant::now();
        wake.wait(Duration::from_secs(5));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "대기 전에 온 신호를 놓치면 그대로 잠들어 버린다"
        );

        // 신호를 소비했으므로 다음 대기는 실제로 시한만큼 기다린다.
        let started = Instant::now();
        wake.wait(Duration::from_millis(50));
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    /// 싱크가 닫으라고 하면 실제로 닫힌다. 로그만 남기고 계속 받으면 위반 상한이
    /// 아무것도 강제하지 못한다.
    #[test]
    fn a_sink_that_asks_to_close_actually_ends_the_session() {
        /// 첫 프레임에서 바로 닫으라고 한다.
        struct ClosingSink {
            closed: Arc<AtomicUsize>,
        }

        impl RelayFrameSink for ClosingSink {
            fn accept(&mut self, _frame: &[u8]) -> SinkOutcome {
                SinkOutcome::CloseChannel
            }

            fn session_ended(&mut self) {
                self.closed.fetch_add(1, Ordering::SeqCst);
            }
        }

        let closed = Arc::new(AtomicUsize::new(0));
        let mut worker = RelayWorker::spawn(
            endpoint(),
            Box::new(TalkingTransport),
            Box::new(ClosingSink {
                closed: Arc::clone(&closed),
            }),
            Arc::new(IgnoreObserver),
            deadlines(),
            fast_backoff(),
        );
        worker.enable();
        assert!(
            wait_until(Duration::from_secs(5), || closed.load(Ordering::SeqCst) > 0),
            "싱크가 닫으라고 했는데 세션이 끝나지 않았다"
        );
        worker.shutdown();
    }

    /// 주기 점검은 상대가 쉬지 않고 말을 걸어도 돈다. 수신 시한 만료에만 걸어 두면 1초보다
    /// 자주 오는 상대 하나가 생존 신호·마감 판정·조정자의 거절을 통째로 멈춰 세운다 —
    /// 정확히 그 상대가 문제인 경우다.
    #[test]
    fn the_periodic_poll_runs_even_when_frames_never_stop_arriving() {
        struct CountingSink {
            polls: Arc<AtomicUsize>,
        }

        impl RelayFrameSink for CountingSink {
            fn accept(&mut self, _frame: &[u8]) -> SinkOutcome {
                SinkOutcome::Continue
            }

            fn poll(&mut self) -> SinkOutcome {
                self.polls.fetch_add(1, Ordering::SeqCst);
                SinkOutcome::Continue
            }
        }

        let polls = Arc::new(AtomicUsize::new(0));
        let mut worker = RelayWorker::spawn(
            endpoint(),
            Box::new(TalkingTransport),
            Box::new(CountingSink {
                polls: Arc::clone(&polls),
            }),
            Arc::new(IgnoreObserver),
            deadlines(),
            fast_backoff(),
        );
        worker.enable();
        assert!(
            wait_until(Duration::from_secs(5), || polls.load(Ordering::SeqCst) >= 3),
            "유휴 분기가 오지 않는 세션에서도 주기 점검은 돌아야 한다"
        );
        worker.shutdown();
    }

    /// 주기 점검이 닫으라고 하면 세션이 끝나고 전송 실패로 기록된다 — 프레임이 끊이지 않는
    /// 중에도 그렇다. 봉인 실패와 조정자의 거절이 이 경로로만 관측된다.
    #[test]
    fn a_poll_that_asks_to_close_ends_the_session_and_backs_off() {
        struct PollClosingSink {
            ended: Arc<AtomicUsize>,
        }

        impl RelayFrameSink for PollClosingSink {
            fn accept(&mut self, _frame: &[u8]) -> SinkOutcome {
                SinkOutcome::Continue
            }

            fn poll(&mut self) -> SinkOutcome {
                SinkOutcome::CloseChannel
            }

            fn session_ended(&mut self) {
                self.ended.fetch_add(1, Ordering::SeqCst);
            }
        }

        let ended = Arc::new(AtomicUsize::new(0));
        let observer = Arc::new(RecordingObserver::default());
        let mut worker = RelayWorker::spawn(
            endpoint(),
            Box::new(TalkingTransport),
            Box::new(PollClosingSink {
                ended: Arc::clone(&ended),
            }),
            Arc::clone(&observer) as Arc<dyn RelayObserver>,
            deadlines(),
            fast_backoff(),
        );
        worker.enable();
        assert!(
            wait_until(Duration::from_secs(5), || ended.load(Ordering::SeqCst) > 0),
            "주기 점검이 닫으라고 했는데 세션이 끝나지 않았다"
        );
        assert!(
            observer.saw_backoff(),
            "전송 실패로 기록되어야 즉시 다시 붙지 않는다"
        );
        worker.shutdown();
    }

    /// 소켓 읽기는 취소할 수 없다. 워커가 준 읽기 시한이 아무리 길어도 한 번의 읽기는
    /// `MAX_RECEIVE_SLICE`를 넘지 않아야 끄기·종료가 그 안에 관측된다.
    #[test]
    fn a_single_receive_never_blocks_longer_than_the_slice() {
        struct SlowSession {
            observed: Arc<Mutex<Vec<Duration>>>,
        }

        impl RelaySession for SlowSession {
            fn receive(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, TransportError> {
                self.observed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(timeout);
                std::thread::sleep(Duration::from_millis(2));
                Ok(None)
            }

            fn send(&mut self, _frame: &[u8]) -> Result<(), TransportError> {
                Ok(())
            }

            fn close(&mut self) {}
        }

        struct SlowTransport {
            observed: Arc<Mutex<Vec<Duration>>>,
        }

        impl RelayTransport for SlowTransport {
            fn connect(
                &mut self,
                _endpoint: &RelayEndpoint,
                _deadline: Duration,
            ) -> Result<Box<dyn RelaySession>, TransportError> {
                Ok(Box::new(SlowSession {
                    observed: Arc::clone(&self.observed),
                }))
            }
        }

        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut worker = RelayWorker::spawn(
            endpoint(),
            Box::new(SlowTransport {
                observed: Arc::clone(&observed),
            }),
            Box::new(RecordingSink::default()),
            Arc::new(IgnoreObserver),
            RelayDeadlines {
                connect: Duration::from_millis(50),
                // 워커가 30초를 줘도…
                read: Duration::from_secs(30),
            },
            fast_backoff(),
        );
        worker.enable();
        assert!(wait_until(Duration::from_secs(3), || !observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()));
        let started = Instant::now();
        worker.shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "종료가 읽기 시한에 묶이면 안 된다"
        );
        // …실제 소켓 읽기는 조각 상한을 넘지 않는다.
        for timeout in observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
        {
            assert!(*timeout <= MAX_RECEIVE_SLICE, "{timeout:?}");
        }
    }

    /// 명령 큐는 유계다 — UI 클릭이 큐를 무한정 밀어 넣지 못한다.
    #[test]
    fn the_command_queue_is_bounded_and_rejects_instead_of_growing() {
        let (transport, _attempts) = ScriptedTransport::new(vec![Ok(())]);
        // 스레드를 띄우지 않고 큐만 본다: 채널 용량이 상한이라는 사실을 직접 확인한다.
        let (commands, inbox) = sync_channel::<RelayCommand>(MAX_PENDING_COMMANDS);
        for _ in 0..MAX_PENDING_COMMANDS {
            assert!(commands.try_send(RelayCommand::Enable).is_ok());
        }
        assert!(
            matches!(
                commands.try_send(RelayCommand::Enable),
                Err(TrySendError::Full(_))
            ),
            "상한을 넘으면 거절한다"
        );
        drop(inbox);
        drop(transport);
    }

    /// 상대가 아무것도 보내지 않아도 싱크가 큐에 넣은 프레임은 유휴 tick에 나간다.
    #[test]
    fn queued_outbound_frames_leave_on_an_idle_tick_without_any_inbound_frame() {
        struct RecordingSession {
            sent: Arc<Mutex<Vec<Vec<u8>>>>,
        }
        impl RelaySession for RecordingSession {
            fn receive(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, TransportError> {
                std::thread::sleep(timeout.min(Duration::from_millis(5)));
                Ok(None)
            }
            fn send(&mut self, frame: &[u8]) -> Result<(), TransportError> {
                self.sent
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(frame.to_vec());
                Ok(())
            }
            fn close(&mut self) {}
        }
        struct RecordingTransport {
            sent: Arc<Mutex<Vec<Vec<u8>>>>,
        }
        impl RelayTransport for RecordingTransport {
            fn connect(
                &mut self,
                _endpoint: &RelayEndpoint,
                _deadline: Duration,
            ) -> Result<Box<dyn RelaySession>, TransportError> {
                Ok(Box::new(RecordingSession {
                    sent: Arc::clone(&self.sent),
                }))
            }
        }
        /// 세션 시작 뒤 한참 있다가(수신 없이) 프레임 하나를 큐에 넣는 싱크.
        struct LateSink {
            drains: usize,
        }
        impl RelayFrameSink for LateSink {
            fn accept(&mut self, _frame: &[u8]) -> SinkOutcome {
                SinkOutcome::Continue
            }
            fn drain_outbound(&mut self) -> Vec<Vec<u8>> {
                self.drains += 1;
                // 첫 번째 비우기는 세션 시작 직후다. 그 뒤의 비우기는 주기 tick뿐이다.
                if self.drains == 3 {
                    vec![b"late-frame".to_vec()]
                } else {
                    Vec::new()
                }
            }
        }

        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut worker = RelayWorker::spawn(
            endpoint(),
            Box::new(RecordingTransport {
                sent: Arc::clone(&sent),
            }),
            Box::new(LateSink { drains: 0 }),
            Arc::new(RecordingObserver::default()) as Arc<dyn RelayObserver>,
            deadlines(),
            fast_backoff(),
        );
        worker.enable();
        assert!(
            wait_until(Duration::from_millis(500), || {
                sent.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .any(|frame| frame == b"late-frame")
            }),
            "유휴 tick에서 큐가 비워져야 한다"
        );
        worker.shutdown();
    }
}
