//! v0 구현체 (설계문서 2.3). worker thread가 세션들을 소유한다.
//! 세션 로직(PTY+terminal+lifecycle)은 session crate 소관 (PR-08).

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pty::CommandSpec;
use secret::SecretStore;
use session::Session;

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
    pub fn new(output_batch_ms: u64, secret_store: Arc<dyn SecretStore>) -> Self {
        Self::with_shell(output_batch_ms, secret_store, pty::default_shell())
    }

    /// 테스트용: 셸 대신 임의 명령을 spawn한다.
    pub fn with_shell(
        output_batch_ms: u64,
        secret_store: Arc<dyn SecretStore>,
        shell: CommandSpec,
    ) -> Self {
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
                    secret_store,
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
                    &self.shell,
                    cols,
                    rows,
                    scrollback_lines,
                ) {
                    Ok(new_session) => {
                        self.sessions.insert(id, new_session);
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
            } => {
                // secret은 여기(spawn 직전)에서만 resolve된다 — PR-09 완료 기준.
                // 실패 시 아무것도 spawn하지 않는다 (부분 주입 금지).
                let mut env = env_plain;
                let mut resolve_failed = None;
                for (key, credential_id) in env_secrets {
                    match self.secret_store.get_secret(&credential_id) {
                        Ok(value) => env.push((key, value.expose().to_owned())),
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
                match Session::spawn_with_spec(
                    id,
                    session::SessionKind::Agent,
                    &spec,
                    cols,
                    rows,
                    scrollback_lines,
                ) {
                    Ok(new_session) => {
                        self.sessions.insert(id, new_session);
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
            RuntimeCommand::KillSession { session } => {
                // Session drop → PtySession Drop이 process group 정리를 보장한다
                self.sessions.remove(&session);
            }
        }
    }

    /// 모든 세션의 PTY 출력을 반영하고, 변경된 세션의 Viewport를 push한다.
    fn pump_sessions(&mut self) {
        let mut events = Vec::new();
        for active in self.sessions.values_mut() {
            let result = active.pump();
            if result.dirty
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
        for event in events {
            self.emit(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn test_store() -> Arc<dyn SecretStore> {
        Arc::new(secret::KeyringSecretStore)
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
        let client =
            InProcessRuntimeClient::with_shell(5, test_store(), spec("/bin/echo", &["hi-runtime"]));
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(5), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        // 출력이 Viewport로 push된다
        probe.wait_for(Duration::from_secs(5), |e| match e {
            RuntimeEvent::Viewport { snapshot, .. }
                if snapshot_text(snapshot, 0).contains("hi-runtime") =>
            {
                Some(())
            }
            _ => None,
        });
        // echo 종료 → SessionExited
        let (exited, code) = probe.wait_for(Duration::from_secs(5), |e| match e {
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
        let client = InProcessRuntimeClient::with_shell(5, test_store(), spec("/bin/cat", &[]));
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(5), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        client
            .send_command(RuntimeCommand::WriteInput {
                session,
                bytes: b"ping\r".to_vec(),
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(5), |e| match e {
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
        let new_session = probe.wait_for(Duration::from_secs(5), |e| match e {
            RuntimeEvent::ShellSpawned { session: s } if *s != session => Some(*s),
            _ => None,
        });
        assert_ne!(new_session, session);
    }

    #[test]
    #[cfg(unix)]
    fn 종료_후에도_scrollback_열람_가능() {
        let client =
            InProcessRuntimeClient::with_shell(5, test_store(), spec("/bin/echo", &["done"]));
        let mut probe = Probe::new(client.subscribe());
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let session = probe.wait_for(Duration::from_secs(5), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        probe.wait_for(Duration::from_secs(5), |e| match e {
            RuntimeEvent::SessionExited { .. } => Some(()),
            _ => None,
        });
        // backend가 유지되어 Scroll에 Viewport로 응답해야 한다
        // (종료 전 Viewport와 구분하기 위해 관측 버퍼를 비운다)
        probe.seen.clear();
        client
            .send_command(RuntimeCommand::Scroll { session, delta: 1 })
            .unwrap();
        probe.wait_for(Duration::from_secs(5), |e| match e {
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
        let client = InProcessRuntimeClient::with_shell(5, test_store(), spec("/bin/cat", &[]));
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
            let id = probe.wait_for(Duration::from_secs(5), move |e| match e {
                RuntimeEvent::ShellSpawned { session } if !known.contains(session) => {
                    Some(*session)
                }
                _ => None,
            });
            ids.push(id);
        }
        // 각 세션에 서로 다른 입력 → 각자의 Viewport에만 반영 (독립성)
        for (i, id) in ids.iter().enumerate() {
            client
                .send_command(RuntimeCommand::WriteInput {
                    session: *id,
                    bytes: format!("mark-{i}\r").into_bytes(),
                })
                .unwrap();
        }
        for (i, id) in ids.iter().enumerate() {
            let expect = format!("mark-{i}");
            let id = *id;
            probe.wait_for(Duration::from_secs(5), move |e| match e {
                RuntimeEvent::Viewport {
                    session, snapshot, ..
                } if *session == id && snapshot_text(snapshot, 0).contains(&expect) => Some(()),
                _ => None,
            });
        }
    }

    #[test]
    fn spawn_실패_이벤트() {
        let client = InProcessRuntimeClient::with_shell(
            5,
            test_store(),
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
        probe.wait_for(Duration::from_secs(5), |e| match e {
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

        let client = InProcessRuntimeClient::with_shell(5, store, pty::default_shell());
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
            })
            .unwrap();
        probe.wait_for(Duration::from_secs(5), |e| match e {
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
        let client = InProcessRuntimeClient::with_shell(5, test_store(), pty::default_shell());
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
            })
            .unwrap();
        // resolve 실패 → SpawnFailed, 메시지에 secret 값 없음 (credential id만)
        let message = probe.wait_for(Duration::from_secs(5), |e| match e {
            RuntimeEvent::SpawnFailed { message, .. } => Some(message.clone()),
            _ => None,
        });
        assert!(message.contains("cred-없음"));
    }

    #[test]
    #[cfg(unix)]
    fn 다중_구독자() {
        let client =
            InProcessRuntimeClient::with_shell(5, test_store(), spec("/bin/echo", &["multi"]));
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
            probe.wait_for(Duration::from_secs(5), |e| match e {
                RuntimeEvent::ShellSpawned { .. } => Some(()),
                _ => None,
            });
        }
    }
}
