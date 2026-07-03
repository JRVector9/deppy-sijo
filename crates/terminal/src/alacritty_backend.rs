//! AlacrittyBackend (설계문서 4.1/4.3). tty/event_loop는 사용 금지(1.3) —
//! Term + vte parser + grid만 쓴다. PTY는 pty crate가 소유한다.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode, test::TermSize};
use alacritty_terminal::vte::ansi::{
    Color, CursorShape as VteCursorShape, NamedColor, Processor, Rgb,
};

use crate::backend::{TerminalBackend, TerminalExternalSurfaceHandle, TerminalRenderModel};
use crate::change_set::TerminalChangeSet;
use crate::viewport_snapshot::{
    CursorShape, CursorSnapshot, TerminalCell, TerminalViewportSnapshot,
};

/// 터미널 질의 응답을 수집한다 (PtyWrite + OSC 색상 질의).
/// title/bell 이벤트는 PR-10/13에서 소비 예정 — 현재는 무시.
/// ColorRequest는 팔레트 참조가 필요해 feed()에서 해석한다.
type ColorFormatter = std::sync::Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>;

#[derive(Clone, Default)]
struct CollectingListener {
    pty_responses: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    color_requests: std::sync::Arc<std::sync::Mutex<Vec<(usize, ColorFormatter)>>>,
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
            _ => {}
        }
    }
}

pub struct AlacrittyBackend {
    term: Term<CollectingListener>,
    listener: CollectingListener,
    processor: Processor,
    scrollback_lines: usize,
}

impl AlacrittyBackend {
    pub fn new(cols: u16, rows: u16, scrollback_lines: usize) -> Self {
        let listener = CollectingListener::default();
        Self {
            term: new_term(cols, rows, scrollback_lines, listener.clone()),
            listener,
            processor: Processor::new(),
            scrollback_lines,
        }
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
    Term::new(
        config,
        &TermSize::new(cols.max(1) as usize, rows.max(1) as usize),
        listener,
    )
}

impl TerminalBackend for AlacrittyBackend {
    fn feed(&mut self, bytes: &[u8]) -> anyhow::Result<TerminalChangeSet> {
        let cursor_before = self.term.grid().cursor.point;
        self.processor.advance(&mut self.term, bytes);

        let screen_lines = self.term.screen_lines();
        let dirty_rows = match self.term.damage() {
            TermDamage::Full => (0..screen_lines as u16).collect(),
            TermDamage::Partial(lines) => lines
                .filter(|l| l.is_damaged())
                .map(|l| l.line as u16)
                .collect(),
        };
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
            cursor_changed: self.term.grid().cursor.point != cursor_before,
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
            *cell = TerminalCell {
                // SGR conceal(ESC[8m)은 공백으로 — 속성은 유지
                c: if flags.contains(Flags::HIDDEN) {
                    ' '
                } else {
                    indexed.c
                },
                fg,
                bg,
                wide: flags.contains(Flags::WIDE_CHAR),
                wide_spacer: flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
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
            dirty_ranges: Vec::new(), // PR-21 렌더 최적화에서 채움
            title: None,              // PR-10 tabs에서 listener 도입 시
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

    fn reset(&mut self) {
        // alacritty 0.26에는 reset_state가 없다 — Term 재생성으로 초기화
        let cols = self.term.columns() as u16;
        let rows = self.term.screen_lines() as u16;
        self.term = new_term(cols, rows, self.scrollback_lines, self.listener.clone());
        self.processor = Processor::new();
    }

    fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
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
}
