//! PTY 격리 crate (설계문서 1.2 / 9장).
//! portable-pty 타입은 이 crate 밖으로 노출하지 않는다 — PtyBackend trait으로 감싼다.

mod input_queue;
mod process_identity;

use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use anyhow::Context;

pub use input_queue::{
    PtyInputEnqueueResult, PtyInputPressure, PtyInputQueuePolicy, PtyInputRejectReason,
};
pub use process_identity::{ProcessIdentity, ProcessIdentitySource};

/// 실행할 프로그램. portable-pty CommandBuilder를 노출하지 않기 위한 최소 스펙.
/// env 값에 secret 평문이 올 수 있다 — 절대 로그에 찍지 말 것 (Debug 미구현 이유).
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    /// 추가 환경변수 (상속 env 위에 덮어쓴다)
    pub env: Vec<(String, String)>,
}

/// 플랫폼 기본 셸 (설계문서 PR-04: macOS zsh / Windows PowerShell).
pub fn default_shell() -> CommandSpec {
    #[cfg(windows)]
    let program = "powershell.exe".to_owned();
    #[cfg(not(windows))]
    let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_owned());
    CommandSpec {
        program,
        args: Vec::new(),
        env: Vec::new(),
    }
}

pub trait PtyBackend {
    fn spawn(&self, cmd: &CommandSpec, cols: u16, rows: u16)
    -> anyhow::Result<Box<dyn PtySession>>;
}

pub trait PtySession: Send {
    /// dedicated reader thread가 채우는 출력 채널. 최초 1회만 Some.
    /// 채널 disconnect는 EOF(프로세스 종료 또는 PTY 닫힘)를 뜻한다.
    fn take_output(&mut self) -> Option<Receiver<Vec<u8>>>;
    fn process_identity(&self) -> ProcessIdentity;
    fn write_input(&mut self, bytes: &[u8]) -> anyhow::Result<PtyInputEnqueueResult>;
    fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()>;
    fn try_exit_code(&mut self) -> anyhow::Result<Option<u32>>;
    fn kill(&mut self) -> anyhow::Result<()>;
}

pub struct PortablePtyBackend;

struct PortablePtySession {
    // resize용으로만 유지. reader/writer는 이미 분리해서 보관한다.
    master: Box<dyn portable_pty::MasterPty + Send>,
    /// 입력은 writer 전용 스레드가 쓴다 — worker가 blocking write에 매달리지 않는다.
    /// (출력 폭주로 child의 stdout이 막힌 상태에서 worker가 대량 paste를
    /// 동기 write하면 reader(backpressure)와 맞물려 full-duplex deadlock — codex P1)
    input_tx: Option<SyncSender<Vec<u8>>>,
    input_queue: input_queue::PtyInputQueueState,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: Option<Receiver<Vec<u8>>>,
}

/// kill 후 reap을 폴링으로 — kill이 실패해도(권한/플랫폼 문제) 무한 wait에
/// 매달리지 않는다 (codex P1: portable-pty 0.9 Windows kill 리스크).
/// 제한 시간 내에 reap하지 못하면 leak을 감수하고 로그만 남긴다.
fn kill_and_reap_bounded(child: &mut Box<dyn portable_pty::Child + Send + Sync>) {
    let _ = child.kill();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return, // reap 완료
            Ok(None) => {}
            Err(_) => return, // 조회 불가 — 더 기다려도 알 수 없다
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!("PTY child가 kill 후에도 종료되지 않음 — reap 포기 (leak 감수)");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn pty_size(cols: u16, rows: u16) -> portable_pty::PtySize {
    portable_pty::PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

impl PtyBackend for PortablePtyBackend {
    fn spawn(
        &self,
        cmd: &CommandSpec,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<Box<dyn PtySession>> {
        let pair = portable_pty::native_pty_system()
            .openpty(pty_size(cols, rows))
            .context("PTY 생성 실패")?;
        let mut builder = portable_pty::CommandBuilder::new(&cmd.program);
        builder.args(&cmd.args);
        for (key, value) in &cmd.env {
            builder.env(key, value);
        }
        let child = pair
            .slave
            .spawn_command(builder)
            .with_context(|| format!("셸 실행 실패: {}", cmd.program))?;
        // 설계문서 1.2 리스크 3: slave가 master보다 오래 살면 handle 파괴가
        // 비결정적 — spawn 직후 즉시 drop한다.
        drop(pair.slave);

        // 여기부터 실패하면 child가 orphan으로 남는다 — 실패 경로에서 정리 (codex P2)
        let mut child = child;
        let mut reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(e) => {
                kill_and_reap_bounded(&mut child);
                return Err(e).context("PTY reader 생성 실패");
            }
        };
        let mut writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(e) => {
                kill_and_reap_bounded(&mut child);
                return Err(e).context("PTY writer 생성 실패");
            }
        };

        // bounded 채널: 소비가 느리면 reader thread가 send에서 블록 → PTY 버퍼가
        // 차고 child가 write에서 멈추는 표준 backpressure. 무한 메모리 증가 방지.
        let (tx, rx) = sync_channel(64);
        let reader_thread =
            std::thread::Builder::new()
                .name("pty-reader".into())
                .spawn(move || {
                    let mut buf = [0u8; 8192];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break, // EOF → 채널 drop으로 종료 전파
                            Ok(n) => {
                                if tx.send(buf[..n].to_vec()).is_err() {
                                    break; // 수신측이 사라짐
                                }
                            }
                        }
                    }
                });
        if let Err(e) = reader_thread {
            kill_and_reap_bounded(&mut child);
            return Err(e).context("PTY reader thread 생성 실패");
        }

        // 입력 전용 writer thread — write_input은 try_send만 하고 즉시 리턴.
        // byte/message budget으로 무한 누적을 막되 writer thread가 blocking write를
        // 소유하므로 runtime worker와 reader backpressure가 맞물린 deadlock은 피한다.
        let input_policy = PtyInputQueuePolicy::default();
        let input_queue = input_queue::PtyInputQueueState::new(input_policy);
        let writer_queue = input_queue.clone();
        let (input_tx, input_rx) = sync_channel::<Vec<u8>>(input_policy.max_messages.max(1));
        let writer_thread =
            std::thread::Builder::new()
                .name("pty-writer".into())
                .spawn(move || {
                    for bytes in input_rx {
                        let len = bytes.len();
                        if writer
                            .write_all(&bytes)
                            .and_then(|()| writer.flush())
                            .is_err()
                        {
                            // PTY가 닫힘 — 세션 종료 경로가 곧 정리한다
                            tracing::debug!("PTY 입력 쓰기 실패 — writer 종료");
                            writer_queue.complete(len);
                            break;
                        }
                        writer_queue.complete(len);
                    }
                    writer_queue.close();
                });
        if let Err(e) = writer_thread {
            kill_and_reap_bounded(&mut child);
            return Err(e).context("PTY writer thread 생성 실패");
        }

        Ok(Box::new(PortablePtySession {
            master: pair.master,
            input_tx: Some(input_tx),
            input_queue,
            child,
            output: Some(rx),
        }))
    }
}

impl Drop for PortablePtySession {
    /// 세션을 버릴 때 PTY에 붙은 프로세스를 정리한다.
    /// reader thread는 프로세스 종료(EOF) 또는 수신측 drop 후 send 실패로 끝난다.
    fn drop(&mut self) {
        // 터미널 종료 규약: foreground process group에 SIGHUP —
        // 셸이 kill되어도 살아남는 grandchild job까지 정리 대상에 포함
        // (SIGHUP을 무시하는 프로세스는 nohup과 동일하게 살아남는다 — 표준 동작)
        #[cfg(unix)]
        if let Some(pgid) = self.master.process_group_leader() {
            unsafe { libc::killpg(pgid, libc::SIGHUP) };
        }
        // kill 실패해도 무한 wait에 매달리지 않는다 (bounded reap — codex P1)
        kill_and_reap_bounded(&mut self.child);
        // teardown 계약: input_tx drop → writer thread 종료(채널 닫힘),
        // output Receiver drop(필드) → send 블록된 reader thread가 Err로 풀림,
        // reader의 read 블록은 child 종료의 EOF로 풀린다. thread join은 하지
        // 않는다 — SIGHUP을 무시한 grandchild가 slave를 쥐고 있으면 read가
        // 안 끝날 수 있어, join이 오히려 Drop을 영구 블록시킨다 (detach가 안전).
        drop(self.input_tx.take());
    }
}

impl PtySession for PortablePtySession {
    fn take_output(&mut self) -> Option<Receiver<Vec<u8>>> {
        self.output.take()
    }

    fn process_identity(&self) -> ProcessIdentity {
        let pid = self.child.process_id();
        #[cfg(unix)]
        let process_group = self
            .master
            .process_group_leader()
            .and_then(|pgid| u32::try_from(pgid).ok());
        #[cfg(not(unix))]
        let process_group = None;
        let source = if pid.is_some() || process_group.is_some() {
            ProcessIdentitySource::PortablePty
        } else {
            ProcessIdentitySource::Unavailable
        };
        ProcessIdentity {
            pid,
            process_group,
            source,
        }
    }

    fn write_input(&mut self, bytes: &[u8]) -> anyhow::Result<PtyInputEnqueueResult> {
        // writer thread로 위임 — worker가 blocking write에 매달리지 않는다.
        // 실제 write 에러는 비동기(writer thread 로그)로 넘어간다. 여기서는 queue
        // pressure/closed 상태를 명시적으로 반환한다.
        let Some(tx) = self.input_tx.as_ref() else {
            let policy = self.input_queue.policy();
            return Ok(PtyInputEnqueueResult::Rejected {
                pressure: PtyInputPressure {
                    attempted_bytes: bytes.len(),
                    queued_bytes: 0,
                    queued_messages: 0,
                    max_bytes: policy.max_bytes,
                    max_messages: policy.max_messages,
                    reason: PtyInputRejectReason::SessionClosed,
                },
            });
        };
        input_queue::enqueue_input(tx, &self.input_queue, bytes)
    }

    fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()> {
        // 0 크기는 PTY/터미널 계층에서 의미가 없다 — 경계에서 clamp (codex P3)
        self.master
            .resize(pty_size(cols.max(1), rows.max(1)))
            .context("PTY resize 실패")
    }

    fn try_exit_code(&mut self) -> anyhow::Result<Option<u32>> {
        Ok(self
            .child
            .try_wait()
            .context("exit status 조회 실패")?
            .map(|status| status.exit_code()))
    }

    fn kill(&mut self) -> anyhow::Result<()> {
        // Drop과 같은 규약: process group에 SIGHUP까지 — grandchild job 포함
        // (reap은 try_exit_code/Drop 경로가 담당. codex P2)
        #[cfg(unix)]
        if let Some(pgid) = self.master.process_group_leader() {
            unsafe { libc::killpg(pgid, libc::SIGHUP) };
        }
        self.child.kill().context("프로세스 kill 실패")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn spawn(program: &str, args: &[&str]) -> Box<dyn PtySession> {
        PortablePtyBackend
            .spawn(
                &CommandSpec {
                    program: program.into(),
                    args: args.iter().map(|s| (*s).into()).collect(),
                    env: Vec::new(),
                },
                80,
                24,
            )
            .unwrap()
    }

    #[test]
    fn process_identity_exposes_redacted_pid_metadata() {
        let session = spawn("/bin/sleep", &["1"]);
        let identity = session.process_identity();
        assert!(identity.pid.is_some());
        #[cfg(unix)]
        assert!(identity.process_group.is_some());
        assert_eq!(identity.source, ProcessIdentitySource::PortablePty);
        let debug = format!("{identity:?}");
        assert!(!debug.contains("/bin/sleep"));
        assert!(!debug.contains("SHELL="));
    }

    /// 채널이 닫힐 때까지 출력을 모은다 (timeout 포함).
    fn collect_output(rx: &Receiver<Vec<u8>>, timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => out.extend(chunk),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        out
    }

    fn wait_exit(session: &mut Box<dyn PtySession>, timeout: Duration) -> Option<u32> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(code) = session.try_exit_code().unwrap() {
                return Some(code);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    #[test]
    fn 출력과_종료코드() {
        let mut session = spawn("/bin/echo", &["hello-pty"]);
        let rx = session.take_output().unwrap();
        assert!(session.take_output().is_none()); // 최초 1회만
        let out = collect_output(&rx, Duration::from_secs(5));
        assert!(String::from_utf8_lossy(&out).contains("hello-pty"));
        assert_eq!(wait_exit(&mut session, Duration::from_secs(5)), Some(0));
    }

    #[test]
    fn 입력_echo_roundtrip() {
        // cat은 입력을 그대로 되돌린다 → 입력 경로 검증
        let mut session = spawn("/bin/cat", &[]);
        let rx = session.take_output().unwrap();
        session.write_input(b"ping\r").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut out = Vec::new();
        while Instant::now() < deadline {
            if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
                out.extend(chunk);
            }
            if String::from_utf8_lossy(&out).contains("ping") {
                break;
            }
        }
        assert!(String::from_utf8_lossy(&out).contains("ping"));
        session.kill().unwrap();
    }

    /// codex P1 회귀: 출력 폭주(수신 미소비)로 backpressure가 걸린 상태에서
    /// 대량 입력이 worker를 블록하면 full-duplex deadlock이었다 —
    /// write_input은 이제 writer thread 위임이라 즉시 리턴해야 한다.
    #[test]
    fn 출력_폭주중_대량_입력이_블록되지_않는다() {
        let mut session = spawn("/bin/cat", &[]);
        let _rx = session.take_output().unwrap(); // 붙잡되 소비하지 않음
        let big = vec![b'x'; 1024 * 1024];
        let start = Instant::now();
        session.write_input(&big).unwrap(); // cat echo → 채널/PTY 버퍼 포화 유도
        session.write_input(&big).unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "write_input이 블록됨 (deadlock 재발)"
        );
    }

    #[test]
    fn ctrl_c로_종료() {
        let mut session = spawn("/bin/cat", &[]);
        let _rx = session.take_output().unwrap();
        session.write_input(b"\x03").unwrap(); // PTY line discipline이 SIGINT로 변환
        assert!(wait_exit(&mut session, Duration::from_secs(5)).is_some());
    }

    #[test]
    fn kill로_종료() {
        let mut session = spawn("/bin/cat", &[]);
        let _rx = session.take_output().unwrap();
        session.kill().unwrap();
        assert!(wait_exit(&mut session, Duration::from_secs(5)).is_some());
    }
}
