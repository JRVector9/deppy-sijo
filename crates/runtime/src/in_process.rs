//! v0 구현체 (설계문서 2.3). worker thread가 PTY 세션과 terminal backend를
//! 소유한다. 정식 Session Runtime 분리는 PR-08.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pty::{CommandSpec, PortablePtyBackend, PtyBackend, PtySession};
use terminal::{AlacrittyBackend, TerminalBackend};

use crate::client::{RuntimeClient, RuntimeCommandSink, RuntimeEventReceiver, RuntimeEventStream};
use crate::command::{RuntimeCommand, SessionId};
use crate::event::RuntimeEvent;

/// 한 tick에 backend로 넘기는 PTY 출력 상한 (UI 프레임 독점 방지와 동일 취지)
const FEED_PER_TICK_CAP: usize = 256 * 1024;

pub struct InProcessRuntimeClient {
    command_tx: Sender<RuntimeCommand>,
    subscribers: Arc<Mutex<Vec<Sender<RuntimeEvent>>>>,
}

impl InProcessRuntimeClient {
    /// `output_batch_ms`: Viewport push 주기 (설계문서 10.1, config.performance 소비).
    /// 시작 시점에 고정 — 변경은 앱 재시작 필요.
    pub fn new(output_batch_ms: u64) -> Self {
        Self::with_shell(output_batch_ms, pty::default_shell())
    }

    /// 테스트용: 셸 대신 임의 명령을 spawn한다.
    pub fn with_shell(output_batch_ms: u64, shell: CommandSpec) -> Self {
        let (command_tx, command_rx) = channel();
        let subscribers: Arc<Mutex<Vec<Sender<RuntimeEvent>>>> = Arc::default();
        let worker_subscribers = Arc::clone(&subscribers);
        std::thread::Builder::new()
            .name("runtime-worker".into())
            .spawn(move || {
                Worker {
                    command_rx,
                    subscribers: worker_subscribers,
                    batch: Duration::from_millis(output_batch_ms.max(1)),
                    shell,
                    next_id: 1,
                    session: None,
                }
                .run();
            })
            .expect("runtime worker thread 생성");
        Self {
            command_tx,
            subscribers,
        }
    }
}

impl RuntimeCommandSink for InProcessRuntimeClient {
    fn send_command(&self, command: RuntimeCommand) -> anyhow::Result<()> {
        self.command_tx
            .send(command)
            .map_err(|_| anyhow::anyhow!("runtime worker가 종료됨"))
    }
}

impl RuntimeEventStream for InProcessRuntimeClient {
    fn subscribe(&self) -> RuntimeEventReceiver {
        let (tx, rx) = channel();
        self.subscribers.lock().expect("subscribers lock").push(tx);
        rx
    }
}

impl RuntimeClient for InProcessRuntimeClient {}

struct ActiveSession {
    id: SessionId,
    /// 종료 후에는 None — backend는 scrollback 열람을 위해 유지한다
    pty: Option<Box<dyn PtySession>>,
    output: Receiver<Vec<u8>>,
    backend: AlacrittyBackend,
    /// 마지막 push 이후 화면 변경 여부
    dirty: bool,
}

struct Worker {
    command_rx: Receiver<RuntimeCommand>,
    subscribers: Arc<Mutex<Vec<Sender<RuntimeEvent>>>>,
    batch: Duration,
    shell: CommandSpec,
    next_id: u64,
    session: Option<ActiveSession>,
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
            self.pump_session();
        }
        // session drop → PtySession Drop이 프로세스 정리
    }

    fn emit(&self, event: RuntimeEvent) {
        // 죽은 구독자는 제거
        self.subscribers
            .lock()
            .expect("subscribers lock")
            .retain(|tx| tx.send(event.clone()).is_ok());
    }

    fn handle_command(&mut self, command: RuntimeCommand) {
        match command {
            RuntimeCommand::SpawnShell {
                cols,
                rows,
                scrollback_lines,
            } => self.spawn_shell(cols, rows, scrollback_lines),
            RuntimeCommand::WriteInput { session, bytes } => {
                if let Some(active) = self.session_mut(session)
                    && let Some(pty) = &mut active.pty
                    && let Err(e) = pty.write_input(&bytes)
                {
                    tracing::warn!("PTY 입력 실패: {e:#}");
                }
            }
            RuntimeCommand::Resize {
                session,
                cols,
                rows,
            } => {
                if let Some(active) = self.session_mut(session) {
                    let _ = active.backend.resize(cols, rows);
                    if let Some(pty) = &mut active.pty
                        && let Err(e) = pty.resize(cols, rows)
                    {
                        tracing::warn!("PTY resize 실패: {e:#}");
                    }
                    active.dirty = true;
                }
            }
            RuntimeCommand::Scroll { session, delta } => {
                if let Some(active) = self.session_mut(session) {
                    active.backend.scroll(delta);
                    active.dirty = true;
                }
            }
            RuntimeCommand::KillSession { session } => {
                if self.session.as_ref().is_some_and(|a| a.id == session) {
                    // Drop이 process group 정리를 보장한다
                    self.session = None;
                }
            }
        }
    }

    fn session_mut(&mut self, id: SessionId) -> Option<&mut ActiveSession> {
        self.session.as_mut().filter(|a| a.id == id)
    }

    fn spawn_shell(&mut self, cols: u16, rows: u16, scrollback_lines: usize) {
        // v0: 단일 세션 — 기존 세션은 교체
        self.session = None;
        match PortablePtyBackend.spawn(&self.shell, cols, rows) {
            Ok(mut pty) => {
                let output = pty.take_output().expect("새 세션의 output 채널");
                let id = SessionId(self.next_id);
                self.next_id += 1;
                self.session = Some(ActiveSession {
                    id,
                    pty: Some(pty),
                    output,
                    backend: AlacrittyBackend::new(cols, rows, scrollback_lines),
                    dirty: true,
                });
                self.emit(RuntimeEvent::ShellSpawned { session: id });
            }
            Err(e) => self.emit(RuntimeEvent::SpawnFailed {
                message: format!("{e:#}"),
            }),
        }
    }

    /// PTY 출력을 backend에 먹이고, 변경이 있으면 Viewport를 push한다.
    fn pump_session(&mut self) {
        let Some(active) = &mut self.session else {
            return;
        };
        let mut fed = 0usize;
        let mut eof = false;
        while active.pty.is_some() {
            if fed >= FEED_PER_TICK_CAP {
                break; // 나머지는 다음 tick에서
            }
            match active.output.try_recv() {
                Ok(chunk) => {
                    fed += chunk.len();
                    match active.backend.feed(&chunk) {
                        Ok(changes) => {
                            active.dirty = true;
                            // 터미널 질의(DA 등) 응답 회신
                            if !changes.pty_responses.is_empty()
                                && let Some(pty) = &mut active.pty
                                && let Err(e) = pty.write_input(&changes.pty_responses)
                            {
                                tracing::warn!("터미널 질의 응답 전송 실패: {e:#}");
                            }
                        }
                        Err(e) => tracing::warn!("terminal feed 실패: {e:#}"),
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    eof = true;
                    break;
                }
            }
        }
        if active.dirty
            && let Some(snapshot) = active.backend.viewport_snapshot()
        {
            let event = RuntimeEvent::Viewport {
                session: active.id,
                snapshot: Arc::new(snapshot),
                bracketed_paste: active.backend.bracketed_paste(),
            };
            active.dirty = false;
            self.emit(event);
        }
        // EOF: 프로세스만 정리하고 backend는 유지 — 종료 후 scrollback 열람 가능
        if eof && let Some(active) = &mut self.session {
            let exit_code = active
                .pty
                .take()
                .and_then(|mut pty| pty.try_exit_code().unwrap_or(None));
            let id = active.id;
            self.emit(RuntimeEvent::SessionExited {
                session: id,
                exit_code,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn spec(program: &str, args: &[&str]) -> CommandSpec {
        CommandSpec {
            program: program.into(),
            args: args.iter().map(|s| (*s).into()).collect(),
        }
    }

    /// 조건을 만족하는 이벤트가 올 때까지 수신 (timeout 시 panic).
    fn wait_for<T>(
        rx: &RuntimeEventReceiver,
        timeout: Duration,
        mut pick: impl FnMut(&RuntimeEvent) -> Option<T>,
    ) -> T {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(event) = rx.recv_timeout(Duration::from_millis(100))
                && let Some(value) = pick(&event)
            {
                return value;
            }
        }
        panic!("기다리던 이벤트가 오지 않음");
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
        let client = InProcessRuntimeClient::with_shell(5, spec("/bin/echo", &["hi-runtime"]));
        let rx = client.subscribe();
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let session = wait_for(&rx, Duration::from_secs(5), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        // 출력이 Viewport로 push된다
        wait_for(&rx, Duration::from_secs(5), |e| match e {
            RuntimeEvent::Viewport { snapshot, .. }
                if snapshot_text(snapshot, 0).contains("hi-runtime") =>
            {
                Some(())
            }
            _ => None,
        });
        // echo 종료 → SessionExited
        let (exited, code) = wait_for(&rx, Duration::from_secs(5), |e| match e {
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
        let client = InProcessRuntimeClient::with_shell(5, spec("/bin/cat", &[]));
        let rx = client.subscribe();
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let session = wait_for(&rx, Duration::from_secs(5), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        client
            .send_command(RuntimeCommand::WriteInput {
                session,
                bytes: b"ping\r".to_vec(),
            })
            .unwrap();
        wait_for(&rx, Duration::from_secs(5), |e| match e {
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
        let new_session = wait_for(&rx, Duration::from_secs(5), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        assert_ne!(new_session, session);
    }

    #[test]
    #[cfg(unix)]
    fn 종료_후에도_scrollback_열람_가능() {
        let client = InProcessRuntimeClient::with_shell(5, spec("/bin/echo", &["done"]));
        let rx = client.subscribe();
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let session = wait_for(&rx, Duration::from_secs(5), |e| match e {
            RuntimeEvent::ShellSpawned { session } => Some(*session),
            _ => None,
        });
        wait_for(&rx, Duration::from_secs(5), |e| match e {
            RuntimeEvent::SessionExited { .. } => Some(()),
            _ => None,
        });
        // backend가 유지되어 Scroll에 Viewport로 응답해야 한다
        client
            .send_command(RuntimeCommand::Scroll { session, delta: 1 })
            .unwrap();
        wait_for(&rx, Duration::from_secs(5), |e| match e {
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
    fn 다중_구독자() {
        let client = InProcessRuntimeClient::with_shell(5, spec("/bin/echo", &["multi"]));
        let rx1 = client.subscribe();
        let rx2 = client.subscribe();
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        for rx in [&rx1, &rx2] {
            wait_for(rx, Duration::from_secs(5), |e| match e {
                RuntimeEvent::ShellSpawned { .. } => Some(()),
                _ => None,
            });
        }
    }
}
