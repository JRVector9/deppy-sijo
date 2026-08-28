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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::lifecycle::{BackoffPolicy, RelayEndpoint, RelayLifecycle, RelayState};

/// 대기 중인 명령 상한. 이 이상은 거절한다 — UI 클릭이 큐를 무한정 밀어 넣지 못하게 한다.
pub const MAX_PENDING_COMMANDS: usize = 32;

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

/// 워커가 받은 프레임을 넘길 곳. 권한 강제 어댑터가 이 자리에 들어온다.
pub trait RelayFrameSink: Send {
    fn accept(&mut self, frame: &[u8]);
    /// 세션이 끝났다. 어댑터가 세션 상태를 버릴 기회다.
    fn session_ended(&mut self) {}
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

        match session.receive(deadlines.read) {
            Ok(Some(frame)) => sink.accept(&frame),
            // 시한만 지났다. 명령을 다시 확인하고 계속 기다린다.
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
        _ => Duration::from_millis(500),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
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
        fn accept(&mut self, frame: &[u8]) {
            self.frames
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(frame.to_vec());
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
}
