use std::sync::mpsc::{Receiver, TryRecvError};

use deppy_core::SessionId;
use pty::{CommandSpec, PortablePtyBackend, PtyBackend, PtySession};
use terminal::{AlacrittyBackend, TerminalBackend, TerminalViewportSnapshot};

use crate::lifecycle::SessionLifecycle;

/// 한 pump에 backend로 넘기는 PTY 출력 상한 (호출 스레드 독점 방지)
const FEED_PER_PUMP_CAP: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Shell,
    Agent,
}

/// pump() 결과 — 호출측(runtime worker)이 이벤트 발행 여부를 결정한다.
#[derive(Debug, PartialEq)]
pub struct PumpResult {
    /// 화면이 바뀌어 snapshot 재생성이 의미 있는가
    pub dirty: bool,
    /// 이번 pump에서 Running → Exited로 전이했는가
    pub just_exited: bool,
}

/// 실행 중(또는 종료 후 scrollback 열람 중)인 세션 하나.
pub struct Session {
    id: SessionId,
    kind: SessionKind,
    /// Exited 후 None — backend는 scrollback 열람을 위해 유지
    pty: Option<Box<dyn PtySession>>,
    output: Receiver<Vec<u8>>,
    backend: AlacrittyBackend,
    lifecycle: SessionLifecycle,
    dirty: bool,
}

impl Session {
    /// resolve가 끝난 CommandSpec으로 spawn한다 (secret 참조 없음 — lib.rs 참조).
    pub fn spawn_with_spec(
        id: SessionId,
        kind: SessionKind,
        spec: &CommandSpec,
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
    ) -> anyhow::Result<Self> {
        let mut pty = PortablePtyBackend.spawn(spec, cols, rows)?;
        let output = pty.take_output().expect("새 세션의 output 채널");
        Ok(Self {
            id,
            kind,
            pty: Some(pty),
            output,
            backend: AlacrittyBackend::new(cols, rows, scrollback_lines),
            lifecycle: SessionLifecycle::Running,
            dirty: true,
        })
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn kind(&self) -> SessionKind {
        self.kind
    }

    pub fn lifecycle(&self) -> SessionLifecycle {
        self.lifecycle
    }

    /// PTY 출력을 terminal backend에 반영한다. batch tick마다 호출.
    pub fn pump(&mut self) -> PumpResult {
        let mut fed = 0usize;
        let mut eof = false;
        while self.pty.is_some() {
            if fed >= FEED_PER_PUMP_CAP {
                break; // 나머지는 다음 tick에서
            }
            match self.output.try_recv() {
                Ok(chunk) => {
                    fed += chunk.len();
                    match self.backend.feed(&chunk) {
                        Ok(changes) => {
                            self.dirty = true;
                            // 터미널 질의(DA 등) 응답 회신
                            if !changes.pty_responses.is_empty()
                                && let Some(pty) = &mut self.pty
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
        let mut just_exited = false;
        if eof && let Some(mut pty) = self.pty.take() {
            // EOF: 프로세스만 정리, backend(scrollback)는 유지
            self.lifecycle = SessionLifecycle::Exited {
                exit_code: pty.try_exit_code().unwrap_or(None),
            };
            just_exited = true;
        }
        PumpResult {
            dirty: self.dirty,
            just_exited,
        }
    }

    /// snapshot을 만들고 dirty를 지운다. 호출 시점은 호출측이 결정 —
    /// hidden pane에 대해 호출하지 않는 것이 14.4 규칙.
    pub fn take_snapshot(&mut self) -> Option<TerminalViewportSnapshot> {
        self.dirty = false;
        self.backend.viewport_snapshot()
    }

    pub fn bracketed_paste(&self) -> bool {
        self.backend.bracketed_paste()
    }

    pub fn write_input(&mut self, bytes: &[u8]) {
        if let Some(pty) = &mut self.pty
            && let Err(e) = pty.write_input(bytes)
        {
            tracing::warn!("PTY 입력 실패: {e:#}");
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let _ = self.backend.resize(cols, rows);
        if let Some(pty) = &mut self.pty
            && let Err(e) = pty.resize(cols, rows)
        {
            tracing::warn!("PTY resize 실패: {e:#}");
        }
        self.dirty = true;
    }

    pub fn scroll(&mut self, delta: i32) {
        self.backend.scroll(delta);
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait<T>(timeout: Duration, mut poll: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(value) = poll() {
                return value;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timeout");
    }

    fn row_text(snapshot: &TerminalViewportSnapshot, row: usize) -> String {
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
    fn lifecycle_running에서_exited로() {
        let spec = CommandSpec {
            program: "/bin/echo".into(),
            args: vec!["세션".into()],
            env: Vec::new(),
        };
        let mut session =
            Session::spawn_with_spec(SessionId(1), SessionKind::Shell, &spec, 80, 24, 100).unwrap();
        assert!(session.lifecycle().is_running());

        let result = wait(Duration::from_secs(5), || {
            let result = session.pump();
            result.just_exited.then_some(result)
        });
        assert!(result.just_exited);
        assert_eq!(
            session.lifecycle(),
            SessionLifecycle::Exited { exit_code: Some(0) }
        );
        // 종료 후에도 scrollback 열람 가능
        let snapshot = session.take_snapshot().unwrap();
        assert!(row_text(&snapshot, 0).contains("세션"));
    }

    #[test]
    #[cfg(unix)]
    fn 입력_출력_roundtrip과_snapshot_dirty() {
        let spec = CommandSpec {
            program: "/bin/cat".into(),
            args: Vec::new(),
            env: Vec::new(),
        };
        let mut session =
            Session::spawn_with_spec(SessionId(2), SessionKind::Shell, &spec, 80, 24, 100).unwrap();
        session.write_input(b"ping\r");
        wait(Duration::from_secs(5), || {
            session.pump();
            let snapshot = session.take_snapshot().unwrap();
            row_text(&snapshot, 0).contains("ping").then_some(())
        });
        // snapshot 후 dirty가 지워진다
        assert!(!session.pump().dirty);
    }
}
