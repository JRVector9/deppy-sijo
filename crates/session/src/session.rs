use std::sync::mpsc::{Receiver, TryRecvError};

use deppy_core::SessionId;
use pty::{CommandSpec, PortablePtyBackend, ProcessIdentity, PtyBackend, PtySession};
use terminal::{
    AlacrittyBackend, CellRange, TerminalBackend, TerminalCacheClass, TerminalCacheEvent,
    TerminalCacheFootprint, TerminalViewportSnapshot,
};

use crate::lifecycle::SessionLifecycle;

/// 한 pump에 backend로 넘기는 PTY 출력 상한 (호출 스레드 독점 방지)
const FEED_PER_PUMP_CAP: usize = 256 * 1024;
/// EOF 후 exit code를 못 받은 채 전이를 유예할 최대 pump tick 수.
/// EOF 직후엔 wait이 아직 안 끝난 race가 흔하다 — tick 간격으로 재시도하되
/// (worker를 sleep으로 막지 않는다), 이 상한을 넘으면 exit_code=None으로 마감한다.
const EXIT_WAIT_TICK_CAP: u32 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Shell,
    Agent,
}

/// pump() 결과 — 호출측(runtime worker)이 이벤트 발행 여부를 결정한다.
#[derive(Debug, PartialEq)]
pub struct PumpResult {
    /// 화면이 바뀌어 snapshot 재생성이 의미 있는가 (누적 — snapshot 시 소거)
    pub dirty: bool,
    /// 이번 pump에서 새 출력이 있었는가 (비누적 — status 화면 스캔 게이트용)
    pub produced_output: bool,
    /// 이번 pump에서 Running → Exited로 전이했는가
    pub just_exited: bool,
}

/// 실행 중(또는 종료 후 scrollback 열람 중)인 세션 하나.
pub struct Session {
    id: SessionId,
    kind: SessionKind,
    /// Exited 후 None — backend는 scrollback 열람을 위해 유지
    pty: Option<Box<dyn PtySession>>,
    process_identity: ProcessIdentity,
    output: Receiver<Vec<u8>>,
    backend: AlacrittyBackend,
    lifecycle: SessionLifecycle,
    dirty: bool,
    pending_full_dirty: bool,
    pending_dirty_rows: Vec<u16>,
    /// child가 reap됐는가 (try_exit_code가 Some을 준 tick에 관찰 — 재호출 시 코드 유실)
    child_dead: bool,
    /// 관찰된 exit code (child_dead일 때 유효)
    exit_code: Option<u32>,
    /// "종료 중"(eof 또는 child_dead) 상태로 보낸 pump tick 수 — 논블로킹 grace
    exit_wait_ticks: u32,
    /// cache budget manager가 적용한 현재 class. visible이 최우선이고, hidden/exited는
    /// 줄/byte budget으로 scrollback을 줄인다.
    cache_class: TerminalCacheClass,
    cache_events: Vec<TerminalCacheEvent>,
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
        let process_identity = pty.process_identity();
        let output = pty.take_output().expect("새 세션의 output 채널");
        Ok(Self {
            id,
            kind,
            pty: Some(pty),
            process_identity,
            output,
            backend: AlacrittyBackend::new(cols, rows, scrollback_lines),
            lifecycle: SessionLifecycle::Running,
            dirty: true,
            pending_full_dirty: true,
            pending_dirty_rows: Vec::new(),
            child_dead: false,
            exit_code: None,
            exit_wait_ticks: 0,
            cache_class: TerminalCacheClass::Visible,
            cache_events: Vec::new(),
        })
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn kind(&self) -> SessionKind {
        self.kind
    }

    pub fn process_identity(&self) -> ProcessIdentity {
        self.process_identity
    }

    pub fn lifecycle(&self) -> SessionLifecycle {
        self.lifecycle
    }

    /// PTY 출력을 terminal backend에 반영한다. batch tick마다 호출.
    /// `on_output`은 raw chunk마다 불린다 — 로그/status detector는
    /// backend 내부가 아니라 이 output stream 기반이다 (설계문서 4.1).
    pub fn pump(&mut self, mut on_output: impl FnMut(&[u8])) -> PumpResult {
        let mut fed = 0usize;
        let mut eof = false;
        // 터미널 질의 응답의 tick당 상한 — 악성 출력이 DA/OSC 질의를 폭주시켜도
        // PTY write 큐가 무한히 쌓이지 않는다 (codex 리뷰). 초과분은 버린다:
        // 질의 응답은 최신 상태 재질의로 복구 가능한 종류다.
        const RESPONSE_PER_PUMP_CAP: usize = 64 * 1024;
        let mut responded = 0usize;
        while self.pty.is_some() {
            if fed >= FEED_PER_PUMP_CAP {
                break; // 나머지는 다음 tick에서
            }
            match self.output.try_recv() {
                Ok(chunk) => {
                    fed += chunk.len();
                    on_output(&chunk);
                    match self.backend.feed(&chunk) {
                        Ok(changes) => {
                            if !changes.dirty_rows.is_empty()
                                || changes.cursor_changed
                                || changes.title_changed
                                || changes.bell
                            {
                                self.dirty = true;
                                self.mark_dirty_rows(&changes.dirty_rows);
                            }
                            // 터미널 질의(DA 등) 응답 회신
                            if !changes.pty_responses.is_empty() {
                                responded += changes.pty_responses.len();
                                if responded > RESPONSE_PER_PUMP_CAP {
                                    tracing::warn!("터미널 질의 응답 폭주 — 이번 tick 초과분 버림");
                                } else if let Some(pty) = &mut self.pty
                                    && let Err(e) = pty.write_input(&changes.pty_responses)
                                {
                                    tracing::warn!("터미널 질의 응답 전송 실패: {e:#}");
                                }
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
        // child 종료 관찰 — try_exit_code는 reap하므로 tick당 한 번, 코드를 저장한다.
        // EOF와 무관하게 검사한다: descendant/background job이 PTY slave를 쥐고 있으면
        // reader가 EOF를 영원히 못 받으므로, child 종료 자체를 세션 종료로 본다 (codex P1).
        if !self.child_dead
            && let Some(pty) = &mut self.pty
            && let Ok(Some(code)) = pty.try_exit_code()
        {
            self.child_dead = true;
            self.exit_code = Some(code);
        }

        // 마감 판정 — worker를 sleep으로 막지 않고 tick 단위로 결정한다 (codex P3).
        //  - EOF(채널 Disconnected)는 "모든 출력 전달 완료"의 authoritative 신호다:
        //    이번 pump의 while 루프가 남은 출력을 이미 다 소비한 뒤라 tail 유실이 없다.
        //  - 그래서 정상 경로는 `eof && child_dead`에서 즉시 마감한다.
        //  - EOF인데 코드가 아직(짧은 race)이거나, 코드는 봤는데 EOF가 안 오는
        //    (descendant가 slave 보유) 경우엔 grace tick 안에서 매 tick 계속 draining
        //    하다가 상한 초과 시 마감한다. pty는 마감 순간까지 유지해 draining을 막지 않는다.
        let mut just_exited = false;
        if eof || self.child_dead {
            self.exit_wait_ticks += 1;
            let ready = eof && self.child_dead;
            if self.pty.is_some() && (ready || self.exit_wait_ticks >= EXIT_WAIT_TICK_CAP) {
                self.pty = None; // 프로세스만 정리, backend(scrollback)는 유지
                self.lifecycle = SessionLifecycle::Exited {
                    exit_code: self.exit_code,
                };
                just_exited = true;
            }
        }
        PumpResult {
            dirty: self.dirty,
            produced_output: fed > 0,
            just_exited,
        }
    }

    /// snapshot을 만들고 (성공 시에만) dirty를 지운다. 호출 시점은 호출측이 결정 —
    /// hidden pane에 대해 호출하지 않는 것이 14.4 규칙.
    /// None(외부 surface 백엔드 등)일 때 dirty를 지우면 변경이 영구 미발행된다.
    pub fn take_snapshot(&mut self) -> Option<TerminalViewportSnapshot> {
        let mut snapshot = self.backend.viewport_snapshot()?;
        snapshot.dirty_ranges = self.take_dirty_ranges(snapshot.cols, snapshot.rows);
        self.dirty = false;
        Some(snapshot)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.backend.bracketed_paste()
    }

    /// status detector용 경량 화면 텍스트 (snapshot 미생성 — PR-12 hidden 규칙).
    pub fn screen_text(&self) -> String {
        self.backend.screen_text()
    }

    pub fn write_input(&mut self, bytes: &[u8]) {
        if let Some(pty) = &mut self.pty
            && let Err(e) = pty.write_input(bytes)
        {
            tracing::warn!("PTY 입력 실패: {e:#}");
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Option<TerminalCacheEvent> {
        let _ = self.backend.resize(cols, rows);
        if let Some(pty) = &mut self.pty
            && let Err(e) = pty.resize(cols, rows)
        {
            tracing::warn!("PTY resize 실패: {e:#}");
        }
        self.mark_full_dirty();
        self.set_cache_class(self.cache_class)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.backend.scroll(delta);
        self.mark_full_dirty();
    }

    /// 가시성에 따라 scrollback 상한 조정 (§14.3). 전이 시에만 호출할 것.
    pub fn set_visible(&mut self, visible: bool) -> Option<TerminalCacheEvent> {
        let class = if visible {
            TerminalCacheClass::Visible
        } else {
            TerminalCacheClass::Hidden
        };
        self.set_cache_class(class)
    }

    pub fn set_cache_class(&mut self, class: TerminalCacheClass) -> Option<TerminalCacheEvent> {
        self.cache_class = class;
        let event = self.backend.set_cache_class(class);
        if let Some(event) = event {
            self.cache_events.push(event);
        }
        event
    }

    pub fn cache_class(&self) -> TerminalCacheClass {
        self.cache_class
    }

    pub fn cache_footprint(&self) -> TerminalCacheFootprint {
        self.backend.cache_footprint()
    }

    pub fn take_cache_events(&mut self) -> Vec<TerminalCacheEvent> {
        std::mem::take(&mut self.cache_events)
    }

    fn mark_dirty_rows(&mut self, rows: &[u16]) {
        self.pending_dirty_rows.extend_from_slice(rows);
    }

    fn mark_full_dirty(&mut self) {
        self.dirty = true;
        self.pending_full_dirty = true;
        self.pending_dirty_rows.clear();
    }

    fn take_dirty_ranges(&mut self, cols: u16, rows: u16) -> Vec<CellRange> {
        let ranges = if self.pending_full_dirty {
            full_dirty_ranges(cols, rows)
        } else {
            dirty_rows_to_ranges(&mut self.pending_dirty_rows, cols, rows)
        };
        self.pending_full_dirty = false;
        self.pending_dirty_rows.clear();
        ranges
    }
}

fn full_dirty_ranges(cols: u16, rows: u16) -> Vec<CellRange> {
    let len = cols as usize * rows as usize;
    (len > 0)
        .then_some(CellRange { start: 0, end: len })
        .into_iter()
        .collect()
}

fn dirty_rows_to_ranges(dirty_rows: &mut Vec<u16>, cols: u16, rows: u16) -> Vec<CellRange> {
    if cols == 0 || rows == 0 || dirty_rows.is_empty() {
        return Vec::new();
    }
    dirty_rows.sort_unstable();
    dirty_rows.dedup();

    let mut ranges = Vec::new();
    let mut start_row: Option<u16> = None;
    let mut last_row = 0u16;
    for row in dirty_rows.iter().copied().filter(|row| *row < rows) {
        match start_row {
            None => {
                start_row = Some(row);
                last_row = row;
            }
            Some(start) if row == last_row.saturating_add(1) => {
                last_row = row;
                start_row = Some(start);
            }
            Some(start) => {
                ranges.push(CellRange {
                    start: start as usize * cols as usize,
                    end: (last_row as usize + 1) * cols as usize,
                });
                start_row = Some(row);
                last_row = row;
            }
        }
    }
    if let Some(start) = start_row {
        ranges.push(CellRange {
            start: start as usize * cols as usize,
            end: (last_row as usize + 1) * cols as usize,
        });
    }
    ranges
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
    fn dirty_rows를_cell_range로_coalesce한다() {
        let mut rows = vec![4, 2, 3, 9, 2, 99];
        assert_eq!(
            dirty_rows_to_ranges(&mut rows, 10, 12),
            vec![
                CellRange { start: 20, end: 50 },
                CellRange {
                    start: 90,
                    end: 100
                },
            ]
        );
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
            let result = session.pump(|_| {});
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

    /// codex P1 회귀: 출력하고 곧바로 종료하는 명령의 tail 출력이 유실되면 안 된다.
    /// (child 종료를 EOF보다 먼저 감지하고 pty를 즉시 drop하면 버퍼 출력을 잃었다)
    #[test]
    #[cfg(unix)]
    fn 빠른_종료_명령의_출력이_유실되지_않는다() {
        // 여러 줄을 출력하고 즉시 종료
        let spec = CommandSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "for i in 1 2 3 4 5; do echo line-$i; done".into(),
            ],
            env: Vec::new(),
        };
        let mut session =
            Session::spawn_with_spec(SessionId(3), SessionKind::Shell, &spec, 80, 24, 100).unwrap();
        let mut collected = Vec::new();
        wait(Duration::from_secs(5), || {
            let result = session.pump(|chunk| collected.extend_from_slice(chunk));
            result.just_exited.then_some(())
        });
        // 종료 이벤트가 온 시점까지 on_output으로 모든 라인이 전달돼야 한다
        let text = String::from_utf8_lossy(&collected);
        for i in 1..=5 {
            assert!(
                text.contains(&format!("line-{i}")),
                "line-{i} 유실: {text:?}"
            );
        }
        assert_eq!(
            session.lifecycle(),
            SessionLifecycle::Exited { exit_code: Some(0) }
        );
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
            session.pump(|_| {});
            let snapshot = session.take_snapshot().unwrap();
            row_text(&snapshot, 0).contains("ping").then_some(())
        });
        // snapshot 후 dirty가 지워진다
        assert!(!session.pump(|_| {}).dirty);
    }

    #[test]
    #[cfg(unix)]
    fn snapshot_dirty_ranges는_변경된_행만_전달하고_소거한다() {
        let spec = CommandSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 0.1; printf x; sleep 1".into()],
            env: Vec::new(),
        };
        let mut session =
            Session::spawn_with_spec(SessionId(5), SessionKind::Shell, &spec, 80, 24, 100).unwrap();

        // 초기 full-dirty baseline은 먼저 소비한다.
        let initial = session.take_snapshot().unwrap();
        assert_eq!(
            initial.dirty_ranges,
            vec![CellRange {
                start: 0,
                end: 80 * 24,
            }]
        );

        let snapshot = wait(Duration::from_secs(5), || {
            let result = session.pump(|_| {});
            if !result.dirty {
                return None;
            }
            let snapshot = session.take_snapshot().unwrap();
            row_text(&snapshot, 0).contains('x').then_some(snapshot)
        });
        assert_eq!(snapshot.dirty_ranges, vec![CellRange { start: 0, end: 80 }]);

        assert!(!session.pump(|_| {}).dirty);
    }

    #[test]
    #[cfg(unix)]
    fn scroll은_전체_dirty_range로_무효화한다() {
        let spec = CommandSpec {
            program: "/bin/sleep".into(),
            args: vec!["1".into()],
            env: Vec::new(),
        };
        let mut session =
            Session::spawn_with_spec(SessionId(6), SessionKind::Shell, &spec, 80, 24, 100).unwrap();
        let _ = session.take_snapshot();

        session.scroll(1);
        let snapshot = session.take_snapshot().unwrap();
        assert_eq!(
            snapshot.dirty_ranges,
            vec![CellRange {
                start: 0,
                end: 80 * 24,
            }]
        );
    }

    #[test]
    #[cfg(unix)]
    fn cache_class_전이와_trim_event_추적() {
        let spec = CommandSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "i=0; while [ $i -lt 700 ]; do echo line-$i; i=$((i+1)); done; sleep 30".into(),
            ],
            env: Vec::new(),
        };
        let mut session =
            Session::spawn_with_spec(SessionId(4), SessionKind::Shell, &spec, 240, 5, 10_000)
                .unwrap();

        wait(Duration::from_secs(5), || {
            session.pump(|_| {});
            (session.cache_footprint().history_lines > 400).then_some(())
        });

        let event = session
            .set_visible(false)
            .expect("hidden 전환은 scrollback trim event를 남겨야 함");
        assert_eq!(session.cache_class(), TerminalCacheClass::Hidden);
        assert_eq!(session.cache_footprint().class, TerminalCacheClass::Hidden);
        assert!(event.dropped_history_lines() > 0);
        assert!(event.freed_estimated_bytes() > 0);

        let events = session.take_cache_events();
        assert_eq!(events, vec![event]);
    }
}
