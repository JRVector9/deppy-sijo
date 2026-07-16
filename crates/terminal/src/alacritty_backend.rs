//! AlacrittyBackend (설계문서 4.1/4.3). tty/event_loop는 사용 금지(1.3) —
//! Term + vte parser + grid만 쓴다. PTY는 pty crate가 소유한다.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Row, Scroll};
use alacritty_terminal::term::cell::Cell as AlacrittyCell;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode, test::TermSize};
use alacritty_terminal::vte::ansi::{
    Color, CursorShape as VteCursorShape, NamedColor, Processor, Rgb,
};
use unicode_normalization::UnicodeNormalization;

use crate::backend::{
    TerminalBackend, TerminalCacheBudget, TerminalCacheClass, TerminalCacheEvent,
    TerminalCacheEventKind, TerminalCacheFootprint, TerminalExternalSurfaceHandle,
    TerminalRenderModel,
};
use crate::change_set::TerminalChangeSet;
use crate::viewport_snapshot::{
    CellAttrs, CursorShape, CursorSnapshot, TerminalCell, TerminalViewportSnapshot,
};

/// 셀의 base char에 alacritty가 붙인 zerowidth(조합) 문자를 NFC로 합성한다.
/// macOS 등은 파일명을 NFD(자소 분해)로 저장해, 한글은 초성만·악센트 라틴은 base만
/// 보이던 문제를 해결한다. zerowidth가 없으면(대부분의 셀) base를 그대로 반환해
/// 단일-char 셀 모델과 오버헤드를 유지한다. 합성이 단일 char로 안 되면(고아 조합/옛한글)
/// 기존과 동일하게 base만 반환한다.
fn composed_char(base: char, zerowidth: Option<&[char]>) -> char {
    match zerowidth {
        Some(zw) if !zw.is_empty() => {
            let mut s = String::with_capacity(4);
            s.push(base);
            s.extend(zw.iter());
            // 정확히 단일 char로 합성될 때만 사용한다. 다중 scalar로 남으면(쌓인 결합
            // 기호 등) 셀은 한 글자만 담으므로 부분 합성 대신 base로 폴백한다.
            let mut it = s.nfc();
            match (it.next(), it.next()) {
                (Some(c), None) => c,
                _ => base,
            }
        }
        _ => base,
    }
}

/// 터미널 질의 응답을 수집한다 (PtyWrite + OSC 색상 질의).
/// title/bell 이벤트는 PR-10/13에서 소비 예정 — 현재는 무시.
/// ColorRequest는 팔레트 참조가 필요해 feed()에서 해석한다.
type ColorFormatter = std::sync::Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>;

#[derive(Clone, Default)]
struct CollectingListener {
    pty_responses: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    color_requests: std::sync::Arc<std::sync::Mutex<Vec<(usize, ColorFormatter)>>>,
    /// OSC 0/2로 프로그램이 설정한 터미널 제목(현재 값). 세션 이름 동적 표시에 쓴다.
    title: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl EventListener for CollectingListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => self
                .pty_responses
                .lock()
                .expect("pty_responses lock")
                .extend_from_slice(text.as_bytes()),
            Event::ColorRequest(index, formatter) => self
                .color_requests
                .lock()
                .expect("color_requests lock")
                .push((index, formatter)),
            // OSC 0/2 제목 — 최신 값 보관, 리셋이면 비운다.
            Event::Title(t) => *self.title.lock().expect("title lock") = Some(t),
            Event::ResetTitle => *self.title.lock().expect("title lock") = None,
            _ => {}
        }
    }
}

pub struct AlacrittyBackend {
    term: Term<CollectingListener>,
    listener: CollectingListener,
    processor: Processor,
    scrollback_lines: usize,
    cache_class: TerminalCacheClass,
    active_scrollback_limit: usize,
}

impl AlacrittyBackend {
    pub fn new(cols: u16, rows: u16, scrollback_lines: usize) -> Self {
        let listener = CollectingListener::default();
        let cache_class = TerminalCacheClass::Visible;
        let active_scrollback_limit =
            effective_scrollback_limit(scrollback_lines, cols as usize, rows as usize, cache_class);
        Self {
            term: new_term(cols, rows, active_scrollback_limit, listener.clone()),
            listener,
            processor: Processor::new(),
            scrollback_lines,
            cache_class,
            active_scrollback_limit,
        }
    }

    fn apply_cache_class(&mut self, class: TerminalCacheClass) -> Option<TerminalCacheEvent> {
        let before = self.cache_footprint();
        let target = effective_scrollback_limit(
            self.scrollback_lines,
            self.term.columns(),
            self.term.screen_lines(),
            class,
        );
        self.cache_class = class;
        if target != self.active_scrollback_limit {
            self.term.set_options(Config {
                scrolling_history: target,
                ..Config::default()
            });
            self.active_scrollback_limit = target;
        }
        let after = self.cache_footprint();
        let limit_reduced = target < before.scrollback_limit_lines;
        let history_trimmed = after.history_lines < before.history_lines;
        (limit_reduced || history_trimmed).then_some(TerminalCacheEvent {
            kind: TerminalCacheEventKind::ScrollbackLimitApplied,
            class,
            budget: TerminalCacheBudget::for_class(class),
            before,
            after,
        })
    }
}

fn new_term(
    cols: u16,
    rows: u16,
    scrollback_lines: usize,
    listener: CollectingListener,
) -> Term<CollectingListener> {
    let config = Config {
        scrolling_history: scrollback_lines,
        ..Config::default()
    };
    let mut term = Term::new(
        config,
        &TermSize::new(cols.max(1) as usize, rows.max(1) as usize),
        listener,
    );
    term.reset_damage();
    term
}

fn effective_scrollback_limit(
    requested: usize,
    cols: usize,
    rows: usize,
    class: TerminalCacheClass,
) -> usize {
    let budget = TerminalCacheBudget::for_class(class);
    requested
        .min(budget.max_scrollback_lines)
        .min(history_lines_for_byte_budget(cols, rows, budget.max_bytes))
}

fn history_lines_for_byte_budget(cols: usize, rows: usize, max_bytes: usize) -> usize {
    let bytes_per_line = estimated_bytes_per_line(cols.max(1));
    let total_lines = max_bytes / bytes_per_line;
    total_lines.saturating_sub(rows.max(1))
}

fn estimated_bytes_per_line(cols: usize) -> usize {
    std::mem::size_of::<Row<AlacrittyCell>>()
        .saturating_add(cols.saturating_mul(std::mem::size_of::<AlacrittyCell>()))
}

fn estimated_terminal_bytes(cols: usize, rows: usize, history_lines: usize) -> usize {
    rows.saturating_add(history_lines)
        .saturating_mul(estimated_bytes_per_line(cols.max(1)))
}

impl TerminalBackend for AlacrittyBackend {
    fn feed(&mut self, bytes: &[u8]) -> anyhow::Result<TerminalChangeSet> {
        // 위치만 비교하면 DECTCEM(?25l/h) 가시성이나 DECSCUSR shape 변경을
        // 놓친다 (codex 리뷰) — dirty 기반 repaint가 cursor-only 변화를 못 본다
        let cursor_before = (
            self.term.grid().cursor.point,
            self.term.mode().contains(TermMode::SHOW_CURSOR),
            self.term.cursor_style(),
        );
        let alt_screen_before = self.term.mode().contains(TermMode::ALT_SCREEN);
        self.processor.advance(&mut self.term, bytes);

        let screen_lines = self.term.screen_lines();
        let mut dirty_rows: Vec<u16> = match self.term.damage() {
            TermDamage::Full => (0..screen_lines as u16).collect(),
            TermDamage::Partial(lines) => lines
                .filter(|l| l.is_damaged())
                .map(|l| l.line as u16)
                .collect(),
        };
        if self.term.mode().contains(TermMode::ALT_SCREEN) != alt_screen_before {
            dirty_rows = (0..screen_lines as u16).collect();
        }
        self.term.reset_damage();

        let mut pty_responses = std::mem::take(
            &mut *self
                .listener
                .pty_responses
                .lock()
                .expect("pty_responses lock"),
        );
        // OSC 색상 질의(ESC]10;? / ESC]4;n;? 등) — 현재 팔레트(재정의 반영) 기준으로 응답
        let color_requests = std::mem::take(
            &mut *self
                .listener
                .color_requests
                .lock()
                .expect("color_requests lock"),
        );
        for (index, formatter) in color_requests {
            // 자식 프로세스 출력은 임의 데이터 — 범위 밖 인덱스는 무시 (panic 금지)
            if index >= alacritty_terminal::term::color::COUNT {
                continue;
            }
            let [r, g, b] = self.term.colors()[index]
                .map(rgb_to_arr)
                .unwrap_or_else(|| palette_default(index));
            pty_responses.extend_from_slice(formatter(Rgb { r, g, b }).as_bytes());
        }
        Ok(TerminalChangeSet {
            dirty_rows,
            cursor_changed: (
                self.term.grid().cursor.point,
                self.term.mode().contains(TermMode::SHOW_CURSOR),
                self.term.cursor_style(),
            ) != cursor_before,
            // title/bell 이벤트 소비는 PR-10/13에서
            title_changed: false,
            bell: false,
            pty_responses,
        })
    }

    fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.term
            .resize(TermSize::new(cols.max(1) as usize, rows.max(1) as usize));
        Ok(())
    }

    fn render_model(&self) -> TerminalRenderModel {
        TerminalRenderModel::CellGrid
    }

    fn viewport_snapshot(&self) -> Option<TerminalViewportSnapshot> {
        let content = self.term.renderable_content();
        let cols = self.term.columns();
        let rows = self.term.screen_lines();
        let display_offset = content.display_offset;
        let colors = content.colors;

        let mut cells = vec![TerminalCell::default(); cols * rows];
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + display_offset as i32;
            let col = indexed.point.column.0;
            if row < 0 || row as usize >= rows || col >= cols {
                continue;
            }
            let cell = &mut cells[row as usize * cols + col];
            let flags = indexed.flags;
            let (mut fg, mut bg) = (
                resolve_color(indexed.fg, colors, DEFAULT_FG),
                resolve_color(indexed.bg, colors, DEFAULT_BG),
            );
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            // SGR 텍스트 속성 (B-1, 2026-07-14): 이전엔 이 flag들을 읽지도 않고 버려
            // bold/italic/underline이 화면에 전혀 반영되지 않았다. INVERSE/HIDDEN은
            // 위에서 이미 fg/bg·문자에 반영했으므로 attrs에 담지 않는다.
            let mut attrs = CellAttrs::empty();
            attrs.set(CellAttrs::BOLD, flags.contains(Flags::BOLD));
            attrs.set(CellAttrs::ITALIC, flags.contains(Flags::ITALIC));
            // 밑줄 변형(이중/곡선/점선/파선)은 전부 단일 밑줄로 렌더한다 — egui가
            // 밑줄 스타일을 구분하지 않는다(구분이 필요해지면 attrs에 비트를 늘린다).
            attrs.set(
                CellAttrs::UNDERLINE,
                flags.intersects(
                    Flags::UNDERLINE
                        | Flags::DOUBLE_UNDERLINE
                        | Flags::UNDERCURL
                        | Flags::DOTTED_UNDERLINE
                        | Flags::DASHED_UNDERLINE,
                ),
            );
            attrs.set(CellAttrs::STRIKEOUT, flags.contains(Flags::STRIKEOUT));
            attrs.set(CellAttrs::DIM, flags.contains(Flags::DIM));
            *cell = TerminalCell {
                // SGR conceal(ESC[8m)은 공백으로 — 속성은 유지
                c: if flags.contains(Flags::HIDDEN) {
                    ' '
                } else {
                    composed_char(indexed.c, indexed.zerowidth())
                },
                fg,
                bg,
                wide: flags.contains(Flags::WIDE_CHAR),
                wide_spacer: flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
                attrs,
            };
        }

        // cursor.point는 grid 좌표 — 스크롤 중이면 viewport 밖일 수 있다
        let cursor_row = content.cursor.point.line.0 + display_offset as i32;
        let in_view = (0..rows as i32).contains(&cursor_row);
        let (shape, shape_visible) = map_cursor_shape(content.cursor.shape);
        let cursor = CursorSnapshot {
            col: content.cursor.point.column.0 as u16,
            row: cursor_row.max(0) as u16,
            shape,
            visible: shape_visible && in_view && self.term.mode().contains(TermMode::SHOW_CURSOR),
        };

        Some(TerminalViewportSnapshot {
            cols: cols as u16,
            rows: rows as u16,
            cursor,
            visible_cells: cells.into(),
            // backend 레벨에선 빈 값 — Session::take_snapshot이 누적 dirty rows로
            // 덮어쓰고(session.rs), renderer_egui가 행 캐시 무효화에 소비한다
            // (감사 2026-07-13: "소비자 없음" 서술은 stale이라 교정).
            dirty_ranges: Vec::new(),
            // OSC 0/2로 프로그램이 설정한 제목 — 세션 이름 동적 표시(없으면 폴더명 fallback).
            title: self.listener.title.lock().ok().and_then(|t| t.clone()),
            scroll_offset: display_offset as i32,
            is_alt_screen: self.term.mode().contains(TermMode::ALT_SCREEN),
        })
    }

    fn external_surface(&self) -> Option<TerminalExternalSurfaceHandle> {
        None
    }

    fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    fn reset(&mut self) {
        // alacritty 0.26에는 reset_state가 없다 — Term 재생성으로 초기화
        let cols = self.term.columns() as u16;
        let rows = self.term.screen_lines() as u16;
        self.active_scrollback_limit = effective_scrollback_limit(
            self.scrollback_lines,
            cols as usize,
            rows as usize,
            self.cache_class,
        );
        self.term = new_term(
            cols,
            rows,
            self.active_scrollback_limit,
            self.listener.clone(),
        );
        self.processor = Processor::new();
    }

    fn set_cache_class(&mut self, class: TerminalCacheClass) -> Option<TerminalCacheEvent> {
        self.apply_cache_class(class)
    }

    fn cache_class(&self) -> TerminalCacheClass {
        self.cache_class
    }

    fn cache_footprint(&self) -> TerminalCacheFootprint {
        let cols = self.term.columns();
        let rows = self.term.screen_lines();
        let history_lines = self.term.history_size();
        TerminalCacheFootprint {
            class: self.cache_class,
            scrollback_limit_lines: self.active_scrollback_limit,
            history_lines,
            screen_lines: rows,
            columns: cols,
            bytes_per_line: estimated_bytes_per_line(cols),
            estimated_bytes: estimated_terminal_bytes(cols, rows, history_lines),
        }
    }

    fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// grid 전체(history+화면)를 truecolor SGR ANSI로 덤프한다. 새 백엔드에
    /// 그대로 feed하면 스크롤백·색·wide char가 복원된다 (압축 아카이브 왕복용).
    /// wrapped 행은 개행 없이 이어붙여 복원 시 reflow가 자연스럽다.
    fn serialize_scrollback(&self) -> Option<Vec<u8>> {
        let grid = self.term.grid();
        let cols = self.term.columns();
        let rows = self.term.screen_lines();
        let history = self.term.history_size();
        let colors = self.term.colors();
        let mut out: Vec<u8> = Vec::with_capacity((history + rows) * cols);
        // 현재 SGR 상태 — 색이 바뀔 때만 시퀀스를 낸다
        let mut current: Option<([u8; 3], [u8; 3])> = None;
        let total = history as i32 + rows as i32;
        for (emitted, line_idx) in (-(history as i32)..rows as i32).enumerate() {
            let line = &grid[alacritty_terminal::index::Line(line_idx)];
            let wrapped = line[alacritty_terminal::index::Column(cols - 1)]
                .flags
                .contains(Flags::WRAPLINE);
            // trailing 기본 빈칸 trim (wrapped 행은 전체 폭 보존 — 이어붙는 내용)
            let mut end = cols;
            if !wrapped {
                while end > 0 {
                    let cell = &line[alacritty_terminal::index::Column(end - 1)];
                    let plain = cell.c == ' '
                        && cell.zerowidth().is_none()
                        && resolve_color(cell.bg, colors, DEFAULT_BG) == DEFAULT_BG
                        && !cell.flags.contains(Flags::INVERSE);
                    if plain {
                        end -= 1;
                    } else {
                        break;
                    }
                }
            }
            for col in 0..end {
                let cell = &line[alacritty_terminal::index::Column(col)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let (mut fg, mut bg) = (
                    resolve_color(cell.fg, colors, DEFAULT_FG),
                    resolve_color(cell.bg, colors, DEFAULT_BG),
                );
                if cell.flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if current != Some((fg, bg)) {
                    if (fg, bg) == (DEFAULT_FG, DEFAULT_BG) {
                        out.extend_from_slice(b"\x1b[0m");
                    } else {
                        out.extend_from_slice(
                            format!(
                                "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m",
                                fg[0], fg[1], fg[2], bg[0], bg[1], bg[2]
                            )
                            .as_bytes(),
                        );
                    }
                    current = Some((fg, bg));
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
                if let Some(zerowidth) = cell.zerowidth() {
                    for zw in zerowidth {
                        out.extend_from_slice(zw.encode_utf8(&mut buf).as_bytes());
                    }
                }
            }
            // wrapped면 개행 없이 이어붙임, 마지막 행 뒤에는 개행 없음(화면 밀림 방지)
            if !wrapped && (emitted as i32) < total - 1 {
                out.extend_from_slice(b"\r\n");
            }
        }
        out.extend_from_slice(b"\x1b[0m");
        Some(out)
    }

    /// scrollback+화면 전체에서 query를 부분 문자열로(대소문자 무시) 찾는다 (T3).
    /// 화면 최하단에서 위(과거)로 훑어 상한(max_matches)에 걸리면 최신 매치가 남게 한다.
    /// 매치 좌표는 line_from_bottom(최하단=0)과 grid 열 범위(wide spacer 포함)로 돌려준다.
    fn search_scrollback(
        &self,
        query: &str,
        max_matches: usize,
    ) -> crate::backend::ScrollbackSearchResult {
        use crate::backend::{
            ScrollbackMatch, ScrollbackSearchResult, fold_char, substring_matches,
        };

        let needle: Vec<char> = query.chars().map(fold_char).collect();
        let grid = self.term.grid();
        let cols = self.term.columns();
        let rows = self.term.screen_lines();
        let history = self.term.history_size();
        let total = history + rows;
        if needle.is_empty() || cols == 0 {
            return ScrollbackSearchResult {
                matches: Vec::new(),
                total_lines: total as u32,
                capped: false,
            };
        }

        let mut matches: Vec<ScrollbackMatch> = Vec::new();
        let mut capped = false;
        // 라인별 재사용 버퍼 (매 라인 할당 방지)
        let mut chars: Vec<char> = Vec::with_capacity(cols);
        let mut spans: Vec<(u16, u16)> = Vec::with_capacity(cols);
        // 화면 최하단(rows-1)에서 위(가장 오래된 history)로. cap이 걸려도 최신 매치가 남는다.
        'lines: for line_idx in (-(history as i32)..rows as i32).rev() {
            chars.clear();
            spans.clear();
            let line = &grid[alacritty_terminal::index::Line(line_idx)];
            for col in 0..cols {
                let cell = &line[alacritty_terminal::index::Column(col)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    // wide char의 자리 채움 셀 — 직전 char의 열 범위를 이 열까지 확장
                    if let Some(last) = spans.last_mut() {
                        last.1 = col as u16 + 1;
                    }
                    continue;
                }
                // conceal(SGR 8)은 화면과 동일하게 공백 취급 (보이지 않는 텍스트로 매치 금지)
                let c = if cell.flags.contains(Flags::HIDDEN) {
                    ' '
                } else {
                    composed_char(cell.c, cell.zerowidth())
                };
                chars.push(fold_char(c));
                spans.push((col as u16, col as u16 + 1));
            }
            let line_from_bottom = ((rows as i32 - 1) - line_idx) as u32;
            for (s, e) in substring_matches(&chars, &needle) {
                matches.push(ScrollbackMatch {
                    line_from_bottom,
                    col_start: spans[s].0,
                    col_end: spans[e - 1].1,
                });
                if matches.len() >= max_matches {
                    capped = true;
                    break 'lines;
                }
            }
        }

        ScrollbackSearchResult {
            matches,
            total_lines: total as u32,
            capped,
        }
    }

    fn screen_text(&self) -> String {
        // 셀 벡터/Arc 할당 없이 문자만 모은다 — snapshot이 아니다.
        // display_iter는 스크롤된 viewport를 반영하므로 쓰지 않는다 —
        // 사용자가 스크롤백을 보고 있어도 감지는 항상 live 화면 기준이어야 한다.
        let grid = self.term.grid();
        let cols = self.term.columns();
        let rows = self.term.screen_lines();
        let mut out = String::with_capacity(cols * rows);
        for row in 0..rows {
            if row > 0 {
                out.push('\n');
            }
            let line = &grid[alacritty_terminal::index::Line(row as i32)];
            for col in 0..cols {
                let cell = &line[alacritty_terminal::index::Column(col)];
                if !cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    // conceal(SGR 8)은 snapshot과 동일하게 공백 취급 —
                    // 화면에 보이지 않는 텍스트로 상태를 감지하면 안 된다
                    out.push(if cell.flags.contains(Flags::HIDDEN) {
                        ' '
                    } else {
                        composed_char(cell.c, cell.zerowidth())
                    });
                }
            }
        }
        out
    }
}

fn map_cursor_shape(shape: VteCursorShape) -> (CursorShape, bool) {
    match shape {
        VteCursorShape::Block | VteCursorShape::HollowBlock => (CursorShape::Block, true),
        VteCursorShape::Underline => (CursorShape::Underline, true),
        VteCursorShape::Beam => (CursorShape::Beam, true),
        VteCursorShape::Hidden => (CursorShape::Block, false),
    }
}

impl Default for TerminalCell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            wide: false,
            wide_spacer: false,
            attrs: CellAttrs::empty(),
        }
    }
}

const DEFAULT_FG: [u8; 3] = [0xd8, 0xd8, 0xd8];
const DEFAULT_BG: [u8; 3] = [0x18, 0x18, 0x1c];

/// 표준 16색 (xterm 기준).
#[rustfmt::skip]
const ANSI16: [[u8; 3]; 16] = [
    [0x18, 0x18, 0x1c], [0xcc, 0x57, 0x4f], [0x6a, 0xb0, 0x4e], [0xc5, 0xa3, 0x3d],
    [0x4f, 0x83, 0xcc], [0xa9, 0x6b, 0xc4], [0x3f, 0xa8, 0xa8], [0xd8, 0xd8, 0xd8],
    [0x5c, 0x5c, 0x64], [0xe6, 0x71, 0x69], [0x84, 0xd0, 0x68], [0xdf, 0xbd, 0x57],
    [0x69, 0x9d, 0xe6], [0xc3, 0x85, 0xde], [0x59, 0xc2, 0xc2], [0xf2, 0xf2, 0xf2],
];

fn resolve_color(
    color: Color,
    palette: &alacritty_terminal::term::color::Colors,
    default: [u8; 3],
) -> [u8; 3] {
    match color {
        Color::Spec(rgb) => [rgb.r, rgb.g, rgb.b],
        Color::Indexed(i) => palette[i as usize]
            .map(rgb_to_arr)
            .unwrap_or_else(|| indexed_default(i)),
        Color::Named(named) => palette[named as usize]
            .map(rgb_to_arr)
            .unwrap_or_else(|| named_default(named, default)),
    }
}

fn rgb_to_arr(rgb: Rgb) -> [u8; 3] {
    [rgb.r, rgb.g, rgb.b]
}

/// 팔레트 전체 인덱스(0..269)의 기본값 — OSC 색상 질의 응답용.
fn palette_default(index: usize) -> [u8; 3] {
    match index {
        0..=255 => indexed_default(index as u8),
        i if i == NamedColor::Foreground as usize || i == NamedColor::Cursor as usize => DEFAULT_FG,
        i if i == NamedColor::Background as usize => DEFAULT_BG,
        _ => DEFAULT_FG,
    }
}

/// OSC로 팔레트가 재정의되지 않았을 때의 xterm 256색 기본값.
fn indexed_default(i: u8) -> [u8; 3] {
    match i {
        0..=15 => ANSI16[i as usize],
        16..=231 => {
            let i = i - 16;
            let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            [step(i / 36), step(i / 6 % 6), step(i % 6)]
        }
        232..=255 => {
            let v = 8 + (i - 232) * 10;
            [v, v, v]
        }
    }
}

fn named_default(named: NamedColor, default: [u8; 3]) -> [u8; 3] {
    match named {
        NamedColor::Foreground | NamedColor::Cursor => DEFAULT_FG,
        NamedColor::Background => DEFAULT_BG,
        _ => {
            let idx = named as usize;
            if idx < 16 {
                ANSI16[idx]
            } else {
                // Dim* 등 나머지 변형은 기본색으로 근사 (팔레트 미정의 시에만 도달)
                default
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::TerminalBackend;

    fn feed(backend: &mut AlacrittyBackend, bytes: &[u8]) -> TerminalChangeSet {
        backend.feed(bytes).unwrap()
    }

    fn cell_at(backend: &AlacrittyBackend, row: usize, col: usize) -> TerminalCell {
        let snap = backend.viewport_snapshot().unwrap();
        snap.visible_cells[row * snap.cols as usize + col]
    }

    fn row_text(backend: &AlacrittyBackend, row: usize) -> String {
        let snap = backend.viewport_snapshot().unwrap();
        let cols = snap.cols as usize;
        snap.visible_cells[row * cols..(row + 1) * cols]
            .iter()
            .filter(|c| !c.wide_spacer)
            .map(|c| c.c)
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    #[test]
    fn hidden_visible_scrollback_cap() {
        // 5000 scrollback으로 생성 후 ~2000줄 출력 → history 축적
        let mut b = AlacrittyBackend::new(20, 5, 5000);
        for i in 0..2000 {
            feed(&mut b, format!("line{i}\r\n").as_bytes());
        }
        // 과거로 크게 스크롤하면 history 크기만큼만 (scroll_offset = display_offset)
        b.scroll(10_000);
        let visible_offset = b.viewport_snapshot().unwrap().scroll_offset;
        assert!(
            visible_offset > 1000,
            "visible은 1000 넘게 스크롤 가능: {visible_offset}"
        );

        // hidden 전환 → scrollback 1,000 cap (§14.3)
        b.set_visible(false);
        b.scroll(10_000);
        let hidden_offset = b.viewport_snapshot().unwrap().scroll_offset;
        assert!(
            hidden_offset <= 1000,
            "hidden은 1000 이하로 제한: {hidden_offset}"
        );

        // visible 복귀 → cap 해제(잘린 내용은 복구 안 됨). 새 출력으로 다시 늘어난다
        b.set_visible(true);
        for i in 0..2000 {
            feed(&mut b, format!("new{i}\r\n").as_bytes());
        }
        b.scroll(10_000);
        let regrown = b.viewport_snapshot().unwrap().scroll_offset;
        assert!(regrown > 1000, "visible 복귀 후 다시 1000 넘게: {regrown}");
    }

    #[test]
    fn visible_scrollback은_설계_cap을_넘지_않는다() {
        let b = AlacrittyBackend::new(20, 5, 100_000);
        let footprint = b.cache_footprint();
        assert_eq!(footprint.class, TerminalCacheClass::Visible);
        assert_eq!(
            footprint.scrollback_limit_lines,
            TerminalCacheBudget::VISIBLE.max_scrollback_lines
        );
    }

    #[test]
    fn hidden_byte_budget이_scrollback을_trim한다() {
        let cols = 240;
        let rows = 5;
        let mut b = AlacrittyBackend::new(cols as u16, rows as u16, 10_000);
        let hidden_limit =
            effective_scrollback_limit(10_000, cols, rows, TerminalCacheClass::Hidden);
        assert!(
            hidden_limit < TerminalCacheBudget::HIDDEN.max_scrollback_lines,
            "넓은 terminal에서는 2MB byte cap이 1,000 line cap보다 먼저 적용돼야 함"
        );

        for i in 0..hidden_limit + 400 {
            feed(&mut b, format!("line-{i}\r\n").as_bytes());
        }
        let before = b.cache_footprint();
        assert!(
            before.history_lines > hidden_limit,
            "테스트가 trim 대상 history를 충분히 만들지 못함: {before:?}"
        );

        let event = b
            .set_cache_class(TerminalCacheClass::Hidden)
            .expect("hidden 전환은 cache trim event를 남겨야 함");
        assert_eq!(event.class, TerminalCacheClass::Hidden);
        assert!(event.dropped_history_lines() > 0);
        assert!(event.freed_estimated_bytes() > 0);
        assert_eq!(event.after.scrollback_limit_lines, hidden_limit);
        assert!(event.after.estimated_bytes <= TerminalCacheBudget::HIDDEN.max_bytes);
    }

    #[test]
    fn 일반_텍스트_feed() {
        let mut backend = AlacrittyBackend::new(80, 24, 100);
        let changes = feed(&mut backend, b"hello");
        assert_eq!(row_text(&backend, 0), "hello");
        assert!(changes.dirty_rows.contains(&0));
        assert!(changes.cursor_changed);
    }

    #[test]
    fn ansi_색상() {
        let mut backend = AlacrittyBackend::new(80, 24, 100);
        feed(&mut backend, b"\x1b[31mred\x1b[0m plain");
        assert_eq!(cell_at(&backend, 0, 0).fg, ANSI16[1]); // red
        assert_eq!(cell_at(&backend, 0, 4).fg, DEFAULT_FG); // reset 후
    }

    #[test]
    fn 한글_wide_char_2셀() {
        let mut backend = AlacrittyBackend::new(80, 24, 100);
        feed(&mut backend, "가나".as_bytes());
        let first = cell_at(&backend, 0, 0);
        assert_eq!(first.c, '가');
        assert!(first.wide);
        assert!(cell_at(&backend, 0, 1).wide_spacer);
        assert_eq!(cell_at(&backend, 0, 2).c, '나');
        assert_eq!(row_text(&backend, 0), "가나");
    }

    #[test]
    fn nfd_한글_자소분해_입력을_음절로_합성() {
        // macOS는 파일명을 NFD로 저장 — "스크린샷"이 초성만 보이던 문제.
        // 한(ㅎ U+1112 + ㅏ U+1161 + ㄴ U+11AB), 글(ㄱ U+1100 + ㅡ U+1173 + ㄹ U+11AF)
        let mut backend = AlacrittyBackend::new(80, 24, 100);
        let nfd = "\u{1112}\u{1161}\u{11AB}\u{1100}\u{1173}\u{11AF}";
        feed(&mut backend, nfd.as_bytes());
        // 조합 자모를 버리지 않고 NFC 음절로 합성돼야 한다
        assert_eq!(cell_at(&backend, 0, 0).c, '한');
        assert_eq!(cell_at(&backend, 0, 2).c, '글');
        assert_eq!(row_text(&backend, 0), "한글");
        // screen_text(상태감지 regex 경로)도 동일하게 합성
        assert_eq!(
            backend.screen_text().lines().next().unwrap().trim_end(),
            "한글"
        );
    }

    #[test]
    fn resize_반영() {
        let mut backend = AlacrittyBackend::new(80, 24, 100);
        backend.resize(120, 40).unwrap();
        let snap = backend.viewport_snapshot().unwrap();
        assert_eq!((snap.cols, snap.rows), (120, 40));
    }

    #[test]
    fn 커서_위치() {
        let mut backend = AlacrittyBackend::new(80, 24, 100);
        feed(&mut backend, b"ab");
        let snap = backend.viewport_snapshot().unwrap();
        assert_eq!((snap.cursor.col, snap.cursor.row), (2, 0));
        assert!(snap.cursor.visible);
    }

    #[test]
    fn 스크롤백과_offset() {
        let mut backend = AlacrittyBackend::new(80, 4, 100);
        for i in 0..10 {
            feed(&mut backend, format!("line{i}\r\n").as_bytes());
        }
        backend.scroll(3);
        let snap = backend.viewport_snapshot().unwrap();
        assert_eq!(snap.scroll_offset, 3);
        backend.scroll(-100);
        assert_eq!(backend.viewport_snapshot().unwrap().scroll_offset, 0);
    }

    #[test]
    fn 터미널_질의_응답_수집() {
        let mut backend = AlacrittyBackend::new(80, 24, 100);
        // DA1 질의 (ESC[c) → 응답이 pty_responses로 나와야 한다
        let changes = feed(&mut backend, b"\x1b[c");
        assert!(!changes.pty_responses.is_empty());
        assert!(changes.pty_responses.starts_with(b"\x1b["));
        // 다음 feed에서는 비어 있어야 한다 (drain 확인)
        assert!(feed(&mut backend, b"x").pty_responses.is_empty());
    }

    #[test]
    fn osc_색상_질의_응답() {
        let mut backend = AlacrittyBackend::new(80, 24, 100);
        // OSC 10 (기본 전경색) 질의 → OSC 응답이 나와야 한다
        let changes = feed(&mut backend, b"\x1b]10;?\x07");
        let text = String::from_utf8_lossy(&changes.pty_responses).into_owned();
        assert!(text.contains("]10;"), "응답 없음: {text:?}");
        assert!(text.contains("rgb:"), "rgb 형식 아님: {text:?}");
    }

    #[test]
    fn 범위_밖_색상_질의는_panic하지_않는다() {
        let mut backend = AlacrittyBackend::new(80, 24, 100);
        // 자식 출력은 임의 데이터 — 잘못된 인덱스 질의에도 feed가 정상 반환해야 한다
        feed(&mut backend, b"\x1b]4;999;?\x07");
        assert_eq!(row_text(&backend, 0), "");
    }

    #[test]
    fn conceal은_공백_처리() {
        let mut backend = AlacrittyBackend::new(80, 24, 100);
        feed(&mut backend, b"\x1b[8msecret\x1b[0m");
        assert_eq!(row_text(&backend, 0), "");
    }

    #[test]
    fn bracketed_paste_모드() {
        let mut backend = AlacrittyBackend::new(80, 24, 100);
        assert!(!backend.bracketed_paste());
        feed(&mut backend, b"\x1b[?2004h");
        assert!(backend.bracketed_paste());
        feed(&mut backend, b"\x1b[?2004l");
        assert!(!backend.bracketed_paste());
    }

    #[test]
    fn alt_screen_감지() {
        let mut backend = AlacrittyBackend::new(80, 24, 100);
        assert!(!backend.viewport_snapshot().unwrap().is_alt_screen);
        feed(&mut backend, b"\x1b[?1049h");
        assert!(backend.viewport_snapshot().unwrap().is_alt_screen);
    }

    #[test]
    fn screen_text_경량_조회() {
        let mut backend = AlacrittyBackend::new(40, 5, 100);
        feed(&mut backend, "줄1 WAITING\r\n줄2".as_bytes());
        let text = backend.screen_text();
        assert!(text.contains("줄1 WAITING"));
        assert!(text.contains("줄2"));
    }

    #[test]
    fn scrollback_직렬화_왕복() {
        let mut a = AlacrittyBackend::new(40, 5, 100);
        for i in 0..20 {
            feed(&mut a, format!("line{i}\r\n").as_bytes());
        }
        feed(&mut a, "가나 \x1b[31mred\x1b[0m end".as_bytes());

        let dump = a.serialize_scrollback().unwrap();
        let mut b = AlacrittyBackend::new(40, 5, 100);
        b.feed(&dump).unwrap();

        // 화면 셀(문자·색·wide) 완전 일치 + history 줄 수 보존
        let snap_a = a.viewport_snapshot().unwrap();
        let snap_b = b.viewport_snapshot().unwrap();
        assert_eq!(snap_a.visible_cells, snap_b.visible_cells);
        assert_eq!(
            a.cache_footprint().history_lines,
            b.cache_footprint().history_lines
        );
        // 마지막 행: 한글 wide + 빨간 SGR 복원
        assert_eq!(cell_at(&b, 4, 0).c, '가');
        assert!(cell_at(&b, 4, 0).wide);
        assert_eq!(cell_at(&b, 4, 5).fg, ANSI16[1]);
        // 스크롤백 최상단까지 복원
        b.scroll(1000);
        assert_eq!(row_text(&b, 0), "line0");
    }

    #[test]
    fn reset은_화면을_비운다() {
        let mut backend = AlacrittyBackend::new(80, 24, 100);
        feed(&mut backend, b"hello");
        backend.reset();
        assert_eq!(row_text(&backend, 0), "");
    }

    #[test]
    fn 색상_256_기본값() {
        // 231 = cube 최대(백색), 255 = 가장 밝은 회색
        assert_eq!(indexed_default(231), [255, 255, 255]);
        assert_eq!(indexed_default(255), [238, 238, 238]);
        assert_eq!(indexed_default(16), [0, 0, 0]);
    }

    #[test]
    fn search_scrollback는_화면과_history를_모두_찾는다() {
        let mut b = AlacrittyBackend::new(40, 5, 1000);
        // 100줄 출력 → 대부분 history로 밀린다
        for i in 0..100 {
            feed(&mut b, format!("needle line {i}\r\n").as_bytes());
        }
        let result = b.search_scrollback("needle", 1000);
        // 화면 + history 전부에서 매치되어야 한다 (화면만이면 5줄 미만)
        assert!(
            result.matches.len() > 50,
            "history까지 검색: {}",
            result.matches.len()
        );
        // line_from_bottom 오름차순(최하단 우선)
        let bottoms: Vec<u32> = result.matches.iter().map(|m| m.line_from_bottom).collect();
        assert!(bottoms.windows(2).all(|w| w[0] <= w[1]), "최하단 우선 정렬");
    }

    #[test]
    fn search_scrollback는_대소문자를_무시한다() {
        let mut b = AlacrittyBackend::new(40, 5, 100);
        feed(&mut b, b"Hello WORLD hello\r\n");
        let result = b.search_scrollback("hello", 1000);
        assert_eq!(result.matches.len(), 2);
        // 최하단(출력 라인)의 두 매치 — 열 범위 확인 (0..5, 12..17)
        let cols: Vec<(u16, u16)> = result
            .matches
            .iter()
            .map(|m| (m.col_start, m.col_end))
            .collect();
        assert!(cols.contains(&(0, 5)));
        assert!(cols.contains(&(12, 17)));
    }

    #[test]
    fn search_scrollback_상한은_최신_매치를_남긴다() {
        let mut b = AlacrittyBackend::new(40, 5, 1000);
        for i in 0..50 {
            feed(&mut b, format!("hit {i}\r\n").as_bytes());
        }
        let result = b.search_scrollback("hit", 10);
        assert!(result.capped);
        assert_eq!(result.matches.len(), 10);
        // 최신 10개 = line_from_bottom 0..10 근방 (화면 최하단 우선)
        assert!(result.matches.iter().all(|m| m.line_from_bottom < 12));
    }

    /// B-1 회귀: SGR 속성(bold/italic/underline/strikeout/dim)이 스냅샷에 실려야 한다.
    /// 이전엔 backend가 이 flag들을 읽지도 않고 버려 화면에 전혀 반영되지 않았다.
    #[test]
    fn sgr_텍스트_속성이_스냅샷_셀에_실린다() {
        let mut backend = AlacrittyBackend::new(40, 4, 100);
        // 굵게 B / 기울임 I / 밑줄 U / 취소선 S / 흐리게 D — 각각 리셋(0m) 후 다음 속성
        backend
            .feed(b"\x1b[1mB\x1b[0m\x1b[3mI\x1b[0m\x1b[4mU\x1b[0m\x1b[9mS\x1b[0m\x1b[2mD\x1b[0m")
            .unwrap();
        let snap = backend.viewport_snapshot().expect("snapshot");
        let cells = &snap.visible_cells[..5];
        assert_eq!(cells[0].c, 'B');
        assert!(cells[0].attrs.contains(CellAttrs::BOLD), "bold 미반영");
        assert_eq!(cells[1].c, 'I');
        assert!(cells[1].attrs.contains(CellAttrs::ITALIC), "italic 미반영");
        assert_eq!(cells[2].c, 'U');
        assert!(
            cells[2].attrs.contains(CellAttrs::UNDERLINE),
            "underline 미반영"
        );
        assert_eq!(cells[3].c, 'S');
        assert!(
            cells[3].attrs.contains(CellAttrs::STRIKEOUT),
            "strikeout 미반영"
        );
        assert_eq!(cells[4].c, 'D');
        assert!(cells[4].attrs.contains(CellAttrs::DIM), "dim 미반영");
        // 속성 없는 셀은 비어 있다
        assert!(snap.visible_cells[10].attrs.is_empty());
    }
}
