//! PTY 격리 crate (설계문서 1.2 / 9장).
//! portable-pty 타입은 이 crate 밖으로 노출하지 않는다 — PtyBackend trait으로 감싼다.

use std::io::Read;
use std::sync::mpsc::{Receiver, sync_channel};

use anyhow::Context;

/// 실행할 프로그램. portable-pty CommandBuilder를 노출하지 않기 위한 최소 스펙.
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
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
    fn write_input(&mut self, bytes: &[u8]) -> anyhow::Result<()>;
    fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()>;
    fn try_exit_code(&mut self) -> anyhow::Result<Option<u32>>;
    fn kill(&mut self) -> anyhow::Result<()>;
}

pub struct PortablePtyBackend;

struct PortablePtySession {
    // resize용으로만 유지. reader/writer는 이미 분리해서 보관한다.
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: Option<Receiver<Vec<u8>>>,
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
        let child = pair
            .slave
            .spawn_command(builder)
            .with_context(|| format!("셸 실행 실패: {}", cmd.program))?;
        // 설계문서 1.2 리스크 3: slave가 master보다 오래 살면 handle 파괴가
        // 비결정적 — spawn 직후 즉시 drop한다.
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("PTY reader 생성 실패")?;
        let writer = pair.master.take_writer().context("PTY writer 생성 실패")?;

        // bounded 채널: 소비가 느리면 reader thread가 send에서 블록 → PTY 버퍼가
        // 차고 child가 write에서 멈추는 표준 backpressure. 무한 메모리 증가 방지.
        let (tx, rx) = sync_channel(64);
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
            })
            .context("PTY reader thread 생성 실패")?;

        Ok(Box::new(PortablePtySession {
            master: pair.master,
            writer,
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
        let _ = self.child.kill();
        let _ = self.child.wait(); // zombie 방지 reap
    }
}

impl PtySession for PortablePtySession {
    fn take_output(&mut self) -> Option<Receiver<Vec<u8>>> {
        self.output.take()
    }

    fn write_input(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.writer.write_all(bytes).context("PTY 입력 실패")?;
        self.writer.flush().context("PTY flush 실패")
    }

    fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.master
            .resize(pty_size(cols, rows))
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
                },
                80,
                24,
            )
            .unwrap()
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
