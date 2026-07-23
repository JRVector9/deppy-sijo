use std::io::Read;
use std::sync::mpsc::TryRecvError;

use deppy_core::SessionId;
use pty::{
    CommandSpec, PortablePtyBackend, ProcessIdentity, PtyBackend, PtyInputEnqueueResult,
    PtyOutputReceiver, PtyOutputWake, PtySession,
};
use terminal::{
    CellRange, TerminalBackend, TerminalCacheClass, TerminalCacheEvent, TerminalCacheFootprint,
    TerminalViewportSnapshot,
};

use crate::lifecycle::SessionLifecycle;
use crate::prompt_marks::PromptMarks;

/// 한 pump에 backend로 넘기는 PTY 출력 상한 (호출 스레드 독점 방지)
const FEED_PER_PUMP_CAP: usize = 256 * 1024;

/// 마지막 명령 출력 추출 상한 (셸 통합 2단계). 초과 시 앞(오래된)쪽을 버리고 뒤를
/// 남긴다 — 최근 출력이 판단에 더 중요하다.
const LAST_OUTPUT_MAX_BYTES: usize = 64 * 1024;
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
    /// Number of PTY output bytes consumed during this pump.
    pub output_bytes: usize,
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
    output: PtyOutputReceiver,
    /// 백엔드 선택은 terminal::new_default_backend (기본 alacritty,
    /// ghostty-backend feature + DEPPY_TERM_BACKEND=ghostty면 libghostty — A/B 실측용).
    backend: Box<dyn TerminalBackend>,
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
    /// OSC 133 프롬프트 마크 (셸 통합 1단계) — pump의 출력 스트림에서 스캔한다.
    prompt_marks: PromptMarks,
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
        Self::spawn_with_spec_inner(id, kind, spec, cols, rows, scrollback_lines, None)
    }

    /// 런타임 worker용 spawn. PTY 출력 reader가 새 chunk를 받은 즉시 `output_wake`를
    /// 호출해 고정 batch timeout을 기다리지 않고 terminal parser를 pump한다.
    pub fn spawn_with_spec_and_output_wake(
        id: SessionId,
        kind: SessionKind,
        spec: &CommandSpec,
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
        output_wake: PtyOutputWake,
    ) -> anyhow::Result<Self> {
        Self::spawn_with_spec_inner(
            id,
            kind,
            spec,
            cols,
            rows,
            scrollback_lines,
            Some(output_wake),
        )
    }

    fn spawn_with_spec_inner(
        id: SessionId,
        kind: SessionKind,
        spec: &CommandSpec,
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
        output_wake: Option<PtyOutputWake>,
    ) -> anyhow::Result<Self> {
        let mut pty = match output_wake {
            Some(wake) => PortablePtyBackend.spawn_with_output_wake(spec, cols, rows, wake)?,
            None => PortablePtyBackend.spawn(spec, cols, rows)?,
        };
        let process_identity = pty.process_identity();
        let output = pty.take_output().expect("새 세션의 output 채널");
        Ok(Self {
            id,
            kind,
            pty: Some(pty),
            process_identity,
            output,
            backend: terminal::new_default_backend(cols, rows, scrollback_lines),
            lifecycle: SessionLifecycle::Running,
            dirty: true,
            pending_full_dirty: true,
            pending_dirty_rows: Vec::new(),
            child_dead: false,
            exit_code: None,
            exit_wait_ticks: 0,
            cache_class: TerminalCacheClass::Visible,
            prompt_marks: PromptMarks::default(),
        })
    }

    /// 압축 아카이브에서 복원한 열람 전용 세션 (§14.3 확장 — exited 백엔드 복원).
    /// 프로세스 없음(pty None) — 백엔드에 아카이브 ANSI를 재주입해 스크롤백을 되살린다.
    /// dump는 [`Read`]로 64KB 청크 스트리밍 feed — 전체(≤32MB)를 통째로 메모리에
    /// 올리지 않는다 (복원 시 순간 메모리 스파이크 방지, 2026-07-16). 읽기/feed 오류는
    /// 경고 후 부분 복원으로 진행한다 — 완결성 판정은 호출측(ArchiveStream::finish) 소관.
    pub fn restore_archived(
        id: SessionId,
        kind: SessionKind,
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
        exit_code: Option<u32>,
        ansi_dump: &mut impl Read,
    ) -> Self {
        let output = PtyOutputReceiver::disconnected();
        let mut backend = terminal::new_default_backend(cols, rows, scrollback_lines);
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match ansi_dump.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if let Err(e) = backend.feed(&buffer[..read]) {
                        tracing::warn!("아카이브 복원 feed 실패: {e:#}");
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!("아카이브 복원 읽기 실패: {e:#}");
                    break;
                }
            }
        }
        // 복원 즉시 exited budget 적용 (visible 한도로 부풀지 않게)
        backend.set_cache_class(TerminalCacheClass::Exited);
        Self {
            id,
            kind,
            pty: None,
            process_identity: pty::ProcessIdentity::unavailable(),
            output,
            backend,
            lifecycle: SessionLifecycle::Exited { exit_code },
            dirty: true,
            pending_full_dirty: true,
            pending_dirty_rows: Vec::new(),
            child_dead: true,
            exit_code,
            exit_wait_ticks: 0,
            cache_class: TerminalCacheClass::Exited,
            prompt_marks: PromptMarks::default(),
        }
    }

    /// scrollback+화면을 ANSI로 직렬화 (압축 아카이브용 — 미지원 백엔드는 None).
    pub fn serialize_scrollback(&self) -> Option<Vec<u8>> {
        self.backend.serialize_scrollback()
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

    /// 폭주 세션 동결(SIGSTOP)/재개(SIGCONT) — PtySession에 위임 (로드맵 B3).
    /// PTY가 이미 정리된(종료된) 세션은 false.
    pub fn freeze(&self) -> bool {
        self.pty.as_ref().is_some_and(|pty| pty.freeze().is_ok())
    }

    pub fn resume(&self) -> bool {
        self.pty.as_ref().is_some_and(|pty| pty.resume().is_ok())
    }

    /// PTY 출력을 terminal backend에 반영한다. 출력 wake 또는 fallback tick마다 호출.
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
                    // OSC 133 프롬프트 마크 스캔 (셸 통합 1단계) — 로그/감지와 같은
                    // raw output stream 기반이다 (설계문서 4.1).
                    self.prompt_marks.scan(&chunk);
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
                                } else if let Some(pty) = &mut self.pty {
                                    match pty.write_input(&changes.pty_responses) {
                                        Ok(result) if result.is_accepted() => {}
                                        Ok(result) => tracing::warn!(
                                            "터미널 질의 응답 전송 pressure: {result:?}"
                                        ),
                                        Err(e) => {
                                            tracing::warn!("터미널 질의 응답 전송 실패: {e:#}");
                                        }
                                    }
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
            output_bytes: fed,
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

    /// scrollback+화면 전체 텍스트 검색 (T3) — backend에 위임한다.
    pub fn search_scrollback(
        &self,
        query: &str,
        max_matches: usize,
    ) -> terminal::ScrollbackSearchResult {
        self.backend.search_scrollback(query, max_matches)
    }

    /// 이전 실행의 redacted ANSI 로그를 같은 terminal parser에 다시 통과시켜
    /// scrollback과 셀 색상을 복원한다. 로그는 chunk 단위로 읽으므로 큰 세션도
    /// 파일 전체를 메모리에 올리지 않는다. 재생 중 생기는 터미널 질의 응답은 과거
    /// 출력에 대한 것이므로 새 PTY에 보내지 않는다.
    pub fn replay_ansi(&mut self, reader: &mut impl Read) -> anyhow::Result<u64> {
        const REPLAY_CHUNK: usize = 64 * 1024;
        let mut buffer = [0u8; REPLAY_CHUNK];
        let mut replayed = 0u64;
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            if replayed == 0 {
                // tail replay는 이전 파일의 SGR state를 알 수 없다. 새 parser가 ground인
                // 상태에서 색/속성만 reset하고, 이후 로그의 ANSI가 정확히 다시 적용되게 한다.
                self.backend.feed(b"\x1b[0m")?;
            }
            self.backend.feed(&buffer[..read])?;
            replayed = replayed.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        }
        if replayed > 0 {
            self.mark_full_dirty();
        }
        Ok(replayed)
    }

    /// 영속 ANSI 화면 뒤에 fresh PTY를 붙이기 전 terminal mode/cursor 경계를 만든다.
    /// 이전 agent가 alternate screen·mouse tracking·좁은 scroll region을 남긴 채 앱이
    /// 종료됐어도 새 셸 출력은 main screen의 새 줄에서 시작해야 한다. 이 바이트는
    /// 복원용 parser에만 적용되고 append-only 세션 로그에는 기록되지 않는다.
    pub fn finish_ansi_replay(&mut self) -> anyhow::Result<()> {
        self.backend.feed(
            b"\x1b[?1049l\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[r\x1b[?6l\x1b[?7h\x1b[4l\x1b[0m\x1b[?25h\r\n",
        )?;
        self.mark_full_dirty();
        Ok(())
    }

    /// 입력 큐가 비었는가 — backpressure 해소 판정(2026-07-09). PTY가 이미 닫혔으면
    /// 더 쌓일 것도 없으니 idle로 본다.
    pub fn input_queue_idle(&self) -> bool {
        self.pty
            .as_ref()
            .map(|pty| pty.input_queue_idle())
            .unwrap_or(true)
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> Option<PtyInputEnqueueResult> {
        if let Some(pty) = &mut self.pty {
            match pty.write_input(bytes) {
                Ok(result) => return Some(result),
                Err(e) => tracing::warn!("PTY 입력 실패: {e:#}"),
            }
        }
        None
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

    /// 스크롤백에서 맨 아래(라이브 화면)로 복귀 (pane 메뉴/단축키).
    pub fn scroll_to_bottom(&mut self) {
        self.backend.scroll_to_bottom();
        self.mark_full_dirty();
    }

    /// OSC 133 프롬프트 마크로 점프 (−1=이전/과거, +1=다음/최신 — 단축키 ⌘⇧↑/↓).
    /// 델타 수식은 T3 검색의 스크롤 수식과 동치 (prompt_marks.rs 참조).
    pub fn scroll_to_prompt(&mut self, direction: i8) {
        let footprint = self.backend.cache_footprint();
        // 현재 스크롤 오프셋은 snapshot으로만 읽는다 — 키 입력 빈도라 비용 무시 가능.
        let offset = self
            .backend
            .viewport_snapshot()
            .map_or(0, |snapshot| snapshot.scroll_offset);
        if let Some(delta) = self.prompt_marks.jump_delta(
            direction,
            offset,
            footprint.screen_lines,
            footprint.history_lines,
        ) {
            self.backend.scroll(delta);
            self.mark_full_dirty();
        }
    }

    /// OSC 133 C~D 마크 범위의 마지막 명령 출력 텍스트 (셸 통합 2단계 — pane 메뉴
    /// 「마지막 출력 복사/에이전트로」). 반환 `(text, truncated)`. 마크가 없거나 범위가
    /// 비면 빈 text — 판정(알림)은 UI 몫. 최신 라인부터 위로 모아 64KB 상한이나 트림
    /// 경계에 닿으면 멈춘다 — 뒤(최근)쪽이 남는다. 텍스트 읽기는 T3 검색과 같은 backend
    /// grid 경로([`terminal::TerminalBackend::logical_line_back_from_cursor`], 논리 라인
    /// = soft wrap 병합 — LF 카운터 좌표와 일치). 개행 없이 끝난 출력은 D 라인의 선두
    /// `last_line_chars`만 남겨 뒤에 그려진 프롬프트를 배제한다 (codex P2).
    pub fn extract_last_output(&self) -> (String, bool) {
        let Some(range) = self.prompt_marks.last_output_back_range() else {
            return (String::new(), false);
        };
        let mut pieces: Vec<String> = Vec::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        for back in range.end_back..=range.start_back {
            // 트림된 라인/미지원 백엔드는 None — 모은 최근 라인까지만.
            let Some(mut text) = self
                .backend
                .logical_line_back_from_cursor(usize::try_from(back).unwrap_or(usize::MAX))
            else {
                break;
            };
            // 개행 없는 마지막 출력 라인 — D 이후 같은 논리 라인에 그려진 프롬프트·
            // EOL 마커를 잘라낸다 (char 카운트 근사 — prompt_marks 참조).
            if back == range.end_back
                && let Some(chars) = range.last_line_chars
            {
                text = text.chars().take(chars).collect();
            }
            if bytes + text.len() + 1 > LAST_OUTPUT_MAX_BYTES {
                truncated = true;
                if pieces.is_empty() {
                    // 최신 라인 하나가 이미 상한 초과(개행 없는 거대 출력) —
                    // 뒤(최근)쪽 상한만큼만 남긴다.
                    let mut cut = text.len() - LAST_OUTPUT_MAX_BYTES;
                    while !text.is_char_boundary(cut) {
                        cut += 1;
                    }
                    pieces.push(text.split_off(cut));
                }
                break;
            }
            bytes += text.len() + 1;
            pieces.push(text);
        }
        // 최신→과거로 모았으니 뒤집어 개행으로 잇는다.
        pieces.reverse();
        (pieces.join("\n"), truncated)
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

    /// budget class 적용 — trim이 일어났으면 이벤트를 반환한다. 소비는 반환값으로만
    /// 한다(worker의 trace_terminal_cache_event) — 내부 누적 Vec을 두면 드레인 없이
    /// 세션 수명 내내 자란다 (2026-07-16 리뷰에서 제거).
    pub fn set_cache_class(&mut self, class: TerminalCacheClass) -> Option<TerminalCacheEvent> {
        self.cache_class = class;
        self.backend.set_cache_class(class)
    }

    pub fn cache_class(&self) -> TerminalCacheClass {
        self.cache_class
    }

    pub fn cache_footprint(&self) -> TerminalCacheFootprint {
        self.backend.cache_footprint()
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

    #[test]
    fn 아카이브_복원_세션은_스크롤백을_보존한다() {
        // live 백엔드에 출력 → 직렬화 → 복원 세션이 화면·스크롤백·상태를 되살린다
        let mut backend = terminal::new_default_backend(40, 5, 100);
        for i in 0..12 {
            backend.feed(format!("line{i}\r\n").as_bytes()).unwrap();
        }
        let dump = backend
            .serialize_scrollback()
            .expect("alacritty는 직렬화 지원");

        let mut restored = Session::restore_archived(
            SessionId(9),
            SessionKind::Shell,
            40,
            5,
            100,
            Some(0),
            &mut dump.as_slice(),
        );
        assert!(!restored.lifecycle().is_running());
        assert_eq!(restored.cache_footprint().class, TerminalCacheClass::Exited);
        // 화면 마지막 줄 + 스크롤백 히스토리 보존
        assert!(restored.screen_text().contains("line11"));
        assert!(restored.cache_footprint().history_lines > 0);
        // 복원 직후 dirty — 첫 snapshot이 바로 나온다 ("연결 중" 공백 방지)
        assert!(restored.take_snapshot().is_some());
        // 스크롤로 과거 내용 열람 가능
        restored.scroll(1000);
        let top = restored.take_snapshot().unwrap();
        let cols = top.cols as usize;
        let first_row: String = top.visible_cells[..cols]
            .iter()
            .filter(|c| !c.wide_spacer)
            .map(|c| c.c)
            .collect();
        assert!(first_row.trim_end().starts_with("line0"), "{first_row}");
    }

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
            cwd: None,
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
            cwd: None,
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
            cwd: None,
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
    fn ansi_replay는_글자와_truecolor를_복원한다() {
        let spec = CommandSpec {
            program: "/bin/cat".into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
        };
        let mut session =
            Session::spawn_with_spec(SessionId(20), SessionKind::Shell, &spec, 80, 24, 100)
                .unwrap();
        let ansi = b"\x1b[38;2;12;34;56m\x1b[48;2;78;90;123mPERSIST-COLOR\x1b[0m\r\n";

        assert_eq!(
            session
                .replay_ansi(&mut std::io::Cursor::new(ansi))
                .unwrap(),
            ansi.len() as u64
        );
        let snapshot = session.take_snapshot().unwrap();
        let cell = snapshot
            .visible_cells
            .iter()
            .find(|cell| cell.c == 'P')
            .expect("replayed text");
        assert_eq!(cell.fg, [12, 34, 56]);
        assert_eq!(cell.bg, [78, 90, 123]);
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
    fn ansi_replay_경계는_alt_screen을_끝내고_fresh_출력을_새줄에_둔다() {
        let spec = CommandSpec {
            program: "/bin/cat".into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
        };
        let mut session =
            Session::spawn_with_spec(SessionId(21), SessionKind::Shell, &spec, 40, 6, 100).unwrap();

        session
            .replay_ansi(&mut std::io::Cursor::new(
                b"OLD-HISTORY\x1b[?1049hALT-SCREEN",
            ))
            .unwrap();
        assert!(session.take_snapshot().unwrap().is_alt_screen);

        session.finish_ansi_replay().unwrap();
        session
            .replay_ansi(&mut std::io::Cursor::new(b"FRESH-PROMPT"))
            .unwrap();
        let snapshot = session.take_snapshot().unwrap();
        assert!(!snapshot.is_alt_screen);
        let text = session.screen_text();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("OLD-HISTORY"), "{text:?}");
        assert!(lines[1].starts_with("FRESH-PROMPT"), "{text:?}");
    }

    #[test]
    #[cfg(unix)]
    fn snapshot_dirty_ranges는_변경된_행만_전달하고_소거한다() {
        let spec = CommandSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 0.1; printf x; sleep 1".into()],
            env: Vec::new(),
            cwd: None,
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
    fn scroll_to_bottom은_라이브_화면으로_복귀한다() {
        let spec = CommandSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "i=0; while [ $i -lt 60 ]; do echo line-$i; i=$((i+1)); done; sleep 30".into(),
            ],
            env: Vec::new(),
            cwd: None,
        };
        let mut session =
            Session::spawn_with_spec(SessionId(11), SessionKind::Shell, &spec, 80, 5, 1000)
                .unwrap();
        wait(Duration::from_secs(5), || {
            session.pump(|_| {});
            (session.cache_footprint().history_lines > 20).then_some(())
        });
        session.scroll(10);
        let scrolled = session.take_snapshot().expect("snapshot");
        assert!(scrolled.scroll_offset > 0, "{}", scrolled.scroll_offset);
        session.scroll_to_bottom();
        let bottom = session.take_snapshot().expect("snapshot");
        assert_eq!(bottom.scroll_offset, 0);
    }

    #[test]
    #[cfg(unix)]
    fn scroll은_전체_dirty_range로_무효화한다() {
        let spec = CommandSpec {
            program: "/bin/sleep".into(),
            args: vec!["1".into()],
            env: Vec::new(),
            cwd: None,
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

    /// 셸 통합 1단계: OSC 133;A 마크 2개를 심고 ⌘⇧↑/↓ 점프가 T3 검색과 같은
    /// 수식으로 스크롤백을 오간다.
    #[test]
    #[cfg(unix)]
    fn 프롬프트_마크로_점프하고_복귀한다() {
        let spec = CommandSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                concat!(
                    "printf '\\033]133;A\\007prompt-1\\n'; ",
                    "i=0; while [ $i -lt 40 ]; do echo fill-$i; i=$((i+1)); done; ",
                    "printf '\\033]133;A\\007prompt-2\\n'; sleep 30"
                )
                .into(),
            ],
            env: Vec::new(),
            cwd: None,
        };
        let mut session =
            Session::spawn_with_spec(SessionId(30), SessionKind::Shell, &spec, 80, 5, 1000)
                .unwrap();
        wait(Duration::from_secs(5), || {
            session.pump(|_| {});
            session.screen_text().contains("prompt-2").then_some(())
        });

        // 이전 프롬프트(prompt-1, 라인 0)로 — history 상단 클램프까지 스크롤된다.
        session.scroll_to_prompt(-1);
        let jumped = session.take_snapshot().unwrap();
        assert!(jumped.scroll_offset > 0, "{}", jumped.scroll_offset);
        assert!(
            row_text(&jumped, 0).contains("prompt-1"),
            "{:?}",
            row_text(&jumped, 0)
        );

        // 다음 프롬프트(prompt-2, 최하단 근처)로 — 맨 아래 복귀.
        session.scroll_to_prompt(1);
        let back = session.take_snapshot().unwrap();
        assert_eq!(back.scroll_offset, 0);

        // 다음 마크(prompt-2)는 이미 현재 위치(델타 0) — 스크롤 유지.
        session.scroll_to_prompt(1);
        assert_eq!(session.take_snapshot().unwrap().scroll_offset, 0);
    }

    /// 셸 통합 2단계: C~D 마크 범위의 출력 추출. 화면(24행)이 다 안 찬 프레시 셸도
    /// 커서 기준이라 정확하다.
    #[test]
    #[cfg(unix)]
    fn 마지막_출력을_추출한다() {
        let spec = CommandSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                concat!(
                    "printf '\\033]133;A\\007$ cmd\\n\\033]133;C\\007'; ",
                    "echo out-1; echo out-2; ",
                    "printf '\\033]133;D;0\\007\\033]133;A\\007ready\\n'; sleep 30"
                )
                .into(),
            ],
            env: Vec::new(),
            cwd: None,
        };
        let mut session =
            Session::spawn_with_spec(SessionId(31), SessionKind::Shell, &spec, 80, 24, 1000)
                .unwrap();
        wait(Duration::from_secs(5), || {
            session.pump(|_| {});
            session.screen_text().contains("ready").then_some(())
        });
        let (text, truncated) = session.extract_last_output();
        assert_eq!(text, "out-1\nout-2");
        assert!(!truncated);
    }

    /// codex P2 회귀: `printf foo`처럼 개행 없이 끝난 출력 — D가 그 출력과 같은 라인에
    /// 찍혀도 그 라인이 추출에 포함되고, D 이후에 그려지는 EOL 마커(zsh PROMPT_SP의
    /// `%`+공백 autowrap)와 다음 프롬프트는 잘려 나간다.
    #[test]
    #[cfg(unix)]
    fn 개행_없이_끝난_마지막_출력도_추출된다() {
        let spec = CommandSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                // "foo"(개행 없음) → D+A → zsh PROMPT_SP 모사: '%'+공백 autowrap → 프롬프트
                concat!(
                    "printf '\\033]133;A\\007$ cmd\\n\\033]133;C\\007'; ",
                    "printf foo; ",
                    "printf '\\033]133;D;0\\007\\033]133;A\\007'; ",
                    "printf '%%%76sPS1>' ''; sleep 30"
                )
                .into(),
            ],
            env: Vec::new(),
            cwd: None,
        };
        let mut session =
            Session::spawn_with_spec(SessionId(33), SessionKind::Shell, &spec, 80, 24, 1000)
                .unwrap();
        wait(Duration::from_secs(5), || {
            session.pump(|_| {});
            session.screen_text().contains("PS1>").then_some(())
        });
        let (text, truncated) = session.extract_last_output();
        assert_eq!(text, "foo");
        assert!(!truncated);
    }

    /// 64KB 상한 — 초과분은 앞(오래된)쪽부터 버려지고 truncated가 선다.
    #[test]
    #[cfg(unix)]
    fn 마지막_출력_추출은_상한_초과_시_뒤쪽을_남긴다() {
        let spec = CommandSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                concat!(
                    "printf '\\033]133;C\\007'; ",
                    "i=0; while [ $i -lt 1000 ]; do printf 'line-%04d-%060d\\n' $i 7; ",
                    "i=$((i+1)); done; ",
                    "printf '\\033]133;D;0\\007\\033]133;A\\007ready\\n'; sleep 30"
                )
                .into(),
            ],
            env: Vec::new(),
            cwd: None,
        };
        // 라인 70자 × 1000 = 70KB > 64KB. cols 120이라 wrap 없음, scrollback은 충분히.
        let mut session =
            Session::spawn_with_spec(SessionId(32), SessionKind::Shell, &spec, 120, 24, 2000)
                .unwrap();
        wait(Duration::from_secs(10), || {
            session.pump(|_| {});
            session.screen_text().contains("ready").then_some(())
        });
        let (text, truncated) = session.extract_last_output();
        assert!(truncated);
        assert!(text.len() <= LAST_OUTPUT_MAX_BYTES + 128, "{}", text.len());
        // 뒤(최근)쪽이 남는다 — 마지막 라인은 있고 첫 라인은 잘렸다.
        assert!(text.ends_with(&format!("line-0999-{:060}", 7)), "잘림 방향");
        assert!(!text.contains("line-0000-"));
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
            cwd: None,
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
    }
}
