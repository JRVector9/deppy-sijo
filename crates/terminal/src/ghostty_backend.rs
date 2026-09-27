//! LibGhosttyBackend (설계문서 §13 v1.x experimental — A/B 실측용).
//! libghostty-vt(Ghostty VT 엔진, FFI)를 TerminalBackend 뒤에 얹는다.
//! 모든 타입이 !Send/!Sync — Session은 runtime worker 스레드 안에서만 생성/사용되므로
//! 스레드 경계를 넘지 않는다 (in_process.rs Worker가 spawn 클로저 안에서 생성됨).
//!
//! 알려진 편차 (experimental — 실측 후 승격 시 해소):
//! - scrollback 상한 런타임 변경 API가 없어 set_cache_class는 class 기록만 한다
//!   (§14.3 hidden trim 미적용 — 메모리 A/B 시 이 차이를 감안할 것).
//! - feed의 dirty_rows는 출력이 있으면 전체 행으로 보수적 보고
//!   (per-row dirty는 RenderState 업데이트가 필요해 feed 핫패스에서 제외).
//! - OSC 색상 질의(]10;? 등) 자동 응답 없음 (DA1/DSR은 콜백으로 응답).

use std::cell::{Cell as StdCell, RefCell};
use std::rc::Rc;

use crate::alacritty_backend::composed_cell;
use libghostty_vt::render::{CellIterator, CursorVisualStyle, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::terminal::{
    ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Mode,
    PrimaryDeviceAttributes, ScrollViewport, SecondaryDeviceAttributes, TertiaryDeviceAttributes,
};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};

use crate::backend::{
    TerminalBackend, TerminalCacheBudget, TerminalCacheClass, TerminalCacheEvent,
    TerminalCacheFootprint, TerminalExternalSurfaceHandle, TerminalRenderModel,
};
use crate::change_set::TerminalChangeSet;
use crate::viewport_snapshot::{
    CellAttrs, CellGrapheme, CursorShape, CursorSnapshot, TerminalCell, TerminalViewportSnapshot,
};

// alacritty_backend와 동일한 기본색 (앱 테마 §theme.rs와 짝)
const DEFAULT_FG: [u8; 3] = [0xd8, 0xd8, 0xd8];
const DEFAULT_BG: [u8; 3] = [0x18, 0x18, 0x1c];

/// vt_write 콜백들이 채우는 공유 상태 (단일 스레드 — Rc/Cell로 충분).
#[derive(Default)]
struct SharedEffects {
    pty_responses: RefCell<Vec<u8>>,
    title_changed: StdCell<bool>,
    bell: StdCell<bool>,
}

pub struct GhosttyBackend {
    /// Box 필수 — on_* 콜백 등록이 Terminal 내부 vtable 주소를 C쪽 userdata로
    /// 저장하므로, 등록 후 Terminal이 이동하면 dangling pointer가 된다
    /// (crate 0.1 API 함정 — heap 고정 후 등록해야 안전).
    term: Box<Terminal<'static, 'static>>,
    // 렌더 상태/반복자는 재사용 핸들 — viewport_snapshot(&self)에서 갱신하므로 RefCell
    render: RefCell<RenderState<'static>>,
    row_iter: RefCell<RowIterator<'static>>,
    cell_iter: RefCell<CellIterator<'static>>,
    effects: Rc<SharedEffects>,
    scrollback_lines: usize,
    cache_class: TerminalCacheClass,
    cols: u16,
    rows: u16,
}

impl GhosttyBackend {
    pub fn new(cols: u16, rows: u16, scrollback_lines: usize) -> anyhow::Result<Self> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        // §14.3 visible cap과 동일한 상한. 런타임 축소가 없어 생성 시 한 번만 적용.
        let max_scrollback =
            scrollback_lines.min(TerminalCacheBudget::VISIBLE.max_scrollback_lines);
        // 콜백 등록 전에 Box로 heap에 고정한다 (struct 필드 주석 참조 — 이동 금지).
        let mut term = Box::new(
            Terminal::new(TerminalOptions {
                cols,
                rows,
                max_scrollback,
            })
            .map_err(|e| anyhow::anyhow!("ghostty Terminal 생성 실패: {e:?}"))?,
        );

        let effects = Rc::new(SharedEffects::default());
        term.on_pty_write({
            let effects = Rc::clone(&effects);
            move |_t, data| effects.pty_responses.borrow_mut().extend_from_slice(data)
        })
        .and_then(|t| {
            t.on_title_changed({
                let effects = Rc::clone(&effects);
                move |_t| effects.title_changed.set(true)
            })
        })
        .and_then(|t| {
            t.on_bell({
                let effects = Rc::clone(&effects);
                move |_t| effects.bell.set(true)
            })
        })
        .and_then(|t| {
            // DA 질의에 응답해야 TUI(vim/claude 등)가 기능 협상에서 멈추지 않는다.
            t.on_device_attributes(|_t| {
                Some(DeviceAttributes {
                    primary: PrimaryDeviceAttributes::new(
                        ConformanceLevel::VT220,
                        [DeviceAttributeFeature::ANSI_COLOR],
                    ),
                    secondary: SecondaryDeviceAttributes {
                        device_type: DeviceType::VT220,
                        firmware_version: 1,
                        rom_cartridge: 0,
                    },
                    tertiary: TertiaryDeviceAttributes::default(),
                })
            })
        })
        .map_err(|e| anyhow::anyhow!("ghostty 콜백 등록 실패: {e:?}"))?;

        Ok(Self {
            term,
            render: RefCell::new(
                RenderState::new().map_err(|e| anyhow::anyhow!("RenderState 실패: {e:?}"))?,
            ),
            row_iter: RefCell::new(
                RowIterator::new().map_err(|e| anyhow::anyhow!("RowIterator 실패: {e:?}"))?,
            ),
            cell_iter: RefCell::new(
                CellIterator::new().map_err(|e| anyhow::anyhow!("CellIterator 실패: {e:?}"))?,
            ),
            effects,
            scrollback_lines: max_scrollback,
            cache_class: TerminalCacheClass::Visible,
            cols,
            rows,
        })
    }

    fn cursor_state(&self) -> (u16, u16, bool) {
        (
            self.term.cursor_x().unwrap_or(0),
            self.term.cursor_y().unwrap_or(0),
            self.term.is_cursor_visible().unwrap_or(true),
        )
    }

    /// 과거로 스크롤된 줄 수 (alacritty display_offset과 동일 의미, bottom=0).
    fn scroll_offset(&self) -> i32 {
        self.term
            .scrollbar()
            .map(|sb| sb.total.saturating_sub(sb.offset + sb.len) as i32)
            .unwrap_or(0)
    }
}

fn compose_cluster(cluster: &[char]) -> (char, Option<String>) {
    match cluster.split_first() {
        Some((&base, rest)) => composed_cell(base, Some(rest)),
        None => (' ', None),
    }
}

fn map_cursor_shape(style: CursorVisualStyle) -> CursorShape {
    match style {
        CursorVisualStyle::Bar => CursorShape::Beam,
        CursorVisualStyle::Underline => CursorShape::Underline,
        CursorVisualStyle::Block | CursorVisualStyle::BlockHollow => CursorShape::Block,
        // non_exhaustive — 새 스타일은 Block으로 근사
        _ => CursorShape::Block,
    }
}

fn estimated_bytes_per_line(cols: usize) -> usize {
    // ghostty는 페이지 기반 + 스타일 dedup — 셀당 4B 근사 + 행 오버헤드
    cols.max(1) * 4 + 64
}

impl TerminalBackend for GhosttyBackend {
    fn feed(&mut self, bytes: &[u8]) -> anyhow::Result<TerminalChangeSet> {
        let cursor_before = self.cursor_state();
        self.term.vt_write(bytes);

        let pty_responses = std::mem::take(&mut *self.effects.pty_responses.borrow_mut());
        let title_changed = self.effects.title_changed.replace(false);
        let bell = self.effects.bell.replace(false);

        // per-row dirty는 RenderState 갱신이 필요해 feed 핫패스에서 뺐다 —
        // 출력이 있으면 전체 행 보수 보고 (렌더러 row 캐시가 실제 변경만 다시 그림).
        let dirty_rows: Vec<u16> = if bytes.is_empty() {
            Vec::new()
        } else {
            (0..self.rows).collect()
        };

        Ok(TerminalChangeSet {
            dirty_rows,
            cursor_changed: self.cursor_state() != cursor_before,
            title_changed,
            bell,
            pty_responses,
        })
    }

    fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        // 픽셀 크기는 size report/이미지 프로토콜용 — 대표 셀 크기로 근사
        self.term
            .resize(cols, rows, cols as u32 * 8, rows as u32 * 16)
            .map_err(|e| anyhow::anyhow!("ghostty resize 실패: {e:?}"))?;
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    fn grid_dimensions(&self) -> anyhow::Result<(u16, u16)> {
        // 렌더 갱신은 dirty 상태를 소비하므로 실제 terminal 메타데이터만 조회한다.
        let cols = self
            .term
            .cols()
            .map_err(|_| anyhow::anyhow!("ghostty 실제 열 조회 실패"))?;
        let rows = self
            .term
            .rows()
            .map_err(|_| anyhow::anyhow!("ghostty 실제 행 조회 실패"))?;
        Ok((cols, rows))
    }

    fn render_model(&self) -> TerminalRenderModel {
        TerminalRenderModel::CellGrid
    }

    fn viewport_metadata(&self) -> Option<crate::TerminalViewportMetadata> {
        Some(crate::TerminalViewportMetadata {
            scroll_offset: self.scroll_offset(),
            is_alt_screen: self
                .term
                .active_screen()
                .map(|screen| {
                    screen
                        != libghostty_vt::ffi::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_PRIMARY
                })
                .unwrap_or(false),
        })
    }

    fn viewport_snapshot(&self) -> Option<TerminalViewportSnapshot> {
        let mut render = self.render.borrow_mut();
        let snap = render.update(&self.term).ok()?;
        let cols = snap.cols().ok()?.max(1) as usize;
        let rows = snap.rows().ok()?.max(1) as usize;

        let mut cells = vec![TerminalCell::default(); cols * rows];
        let mut graphemes = Vec::new();
        let mut row_iter = self.row_iter.borrow_mut();
        let mut cell_iter = self.cell_iter.borrow_mut();
        let mut rows_iter = row_iter.update(&snap).ok()?;
        let mut grapheme_buf = vec!['\0'; 8];
        let mut row = 0usize;
        while let Some(row_it) = rows_iter.next() {
            if row >= rows {
                break;
            }
            let mut cells_iter = cell_iter.update(row_it).ok()?;
            let mut col = 0usize;
            while let Some(cell) = cells_iter.next() {
                if col >= cols {
                    break;
                }
                let out = &mut cells[row * cols + col];
                col += 1;

                let raw = cell.raw_cell().ok();
                let wide = raw.and_then(|r| r.wide().ok()).unwrap_or(CellWide::Narrow);
                if matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead) {
                    out.wide_spacer = true;
                    continue;
                }

                let style = cell.style().ok();
                let mut fg = cell
                    .fg_color()
                    .ok()
                    .flatten()
                    .map(|c| [c.r, c.g, c.b])
                    .unwrap_or(DEFAULT_FG);
                let mut bg = cell
                    .bg_color()
                    .ok()
                    .flatten()
                    .map(|c| [c.r, c.g, c.b])
                    .unwrap_or(DEFAULT_BG);
                if style.is_some_and(|s| s.inverse) {
                    std::mem::swap(&mut fg, &mut bg);
                }

                // SGR conceal(invisible)은 공백으로 — alacritty HIDDEN과 동일 정책
                let (c, text) = if style.is_some_and(|s| s.invisible) {
                    (' ', None)
                } else {
                    let len = cell.graphemes_len().unwrap_or(0);
                    grapheme_buf.resize(len, '\0');
                    if len == 0 {
                        (' ', None)
                    } else if cell.graphemes_buf(&mut grapheme_buf[..len]).is_ok() {
                        compose_cluster(&grapheme_buf[..len])
                    } else {
                        (' ', None)
                    }
                };
                if let Some(text) = text {
                    graphemes.push(CellGrapheme {
                        index: row * cols + col - 1,
                        text,
                    });
                }

                *out = TerminalCell {
                    c,
                    fg,
                    bg,
                    wide: matches!(wide, CellWide::Wide),
                    wide_spacer: false,
                    attrs: CellAttrs::empty(),
                };
            }
            row += 1;
        }

        let cursor_visible = snap.cursor_visible().unwrap_or(false);
        let cursor_viewport = snap.cursor_viewport().ok().flatten();
        let shape = snap
            .cursor_visual_style()
            .map(map_cursor_shape)
            .unwrap_or(CursorShape::Block);
        let cursor = match cursor_viewport {
            Some(cur) => CursorSnapshot {
                col: cur.x,
                row: cur.y,
                shape,
                visible: cursor_visible,
            },
            // 스크롤로 viewport 밖 — alacritty와 동일하게 숨김 처리
            None => CursorSnapshot {
                col: 0,
                row: 0,
                shape,
                visible: false,
            },
        };

        let title = self
            .term
            .title()
            .ok()
            .filter(|t| !t.is_empty())
            .map(str::to_owned);
        let is_alt_screen = self
            .term
            .active_screen()
            .map(|s| s != libghostty_vt::ffi::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_PRIMARY)
            .unwrap_or(false);

        Some(TerminalViewportSnapshot {
            cols: cols as u16,
            rows: rows as u16,
            cursor,
            visible_cells: cells.into(),
            graphemes: graphemes.into(),
            // alacritty와 동일 — dirty_ranges는 Session.take_dirty_ranges가 채운다
            dirty_ranges: Vec::new(),
            title,
            scroll_offset: self.scroll_offset(),
            is_alt_screen,
        })
    }

    fn external_surface(&self) -> Option<TerminalExternalSurfaceHandle> {
        None
    }

    fn scroll(&mut self, delta: i32) {
        // 우리 계약: 양수 = 과거로. ghostty Delta는 음수 = 위(과거).
        self.term
            .scroll_viewport(ScrollViewport::Delta(-(delta as isize)));
    }

    fn reset(&mut self) {
        self.term.reset();
    }

    fn set_cache_class(&mut self, class: TerminalCacheClass) -> Option<TerminalCacheEvent> {
        // libghostty-vt 0.2에는 scrollback 상한 런타임 변경 API가 없다 —
        // class만 기록하고 trim 이벤트는 내지 않는다 (§14.3 편차, 파일 헤더 참조).
        self.cache_class = class;
        None
    }

    fn cache_class(&self) -> TerminalCacheClass {
        self.cache_class
    }

    fn cache_footprint(&self) -> TerminalCacheFootprint {
        let cols = self.cols as usize;
        let rows = self.rows as usize;
        let history_lines = self.term.scrollback_rows().unwrap_or(0);
        let bytes_per_line = estimated_bytes_per_line(cols);
        TerminalCacheFootprint {
            class: self.cache_class,
            scrollback_limit_lines: self.scrollback_lines,
            history_lines,
            screen_lines: rows,
            columns: cols,
            bytes_per_line,
            estimated_bytes: (rows + history_lines) * bytes_per_line,
        }
    }

    fn bracketed_paste(&self) -> bool {
        self.term.mode(Mode::BRACKETED_PASTE).unwrap_or(false)
    }

    fn screen_text(&self) -> String {
        // RenderState는 viewport 기준 — 사용자가 스크롤백을 보는 중이면 활성 화면과
        // 어긋난다. 상태감지는 스크롤 여부와 무관해야 하므로 bottom이 아니면
        // 일시적으로 bottom 스냅샷을 만들 수 없어 마지막 관측 화면을 그대로 쓴다
        // (스크롤 중 상태 전이는 다음 tick에 따라잡음 — 감지 지연 허용).
        let mut render = self.render.borrow_mut();
        let Ok(snap) = render.update(&self.term) else {
            return String::new();
        };
        let cols = snap.cols().unwrap_or(self.cols).max(1) as usize;
        let mut out = String::with_capacity(cols * self.rows as usize);
        let mut row_iter = self.row_iter.borrow_mut();
        let mut cell_iter = self.cell_iter.borrow_mut();
        let Ok(mut rows_iter) = row_iter.update(&snap) else {
            return String::new();
        };
        let mut grapheme_buf = vec!['\0'; 8];
        let mut first = true;
        while let Some(row_it) = rows_iter.next() {
            if !first {
                out.push('\n');
            }
            first = false;
            let Ok(mut cells_iter) = cell_iter.update(row_it) else {
                continue;
            };
            while let Some(cell) = cells_iter.next() {
                let wide = cell
                    .raw_cell()
                    .ok()
                    .and_then(|r| r.wide().ok())
                    .unwrap_or(CellWide::Narrow);
                if matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead) {
                    continue;
                }
                if cell.style().is_ok_and(|s| s.invisible) {
                    out.push(' ');
                    continue;
                }
                let len = cell.graphemes_len().unwrap_or(0);
                grapheme_buf.resize(len, '\0');
                if len == 0 {
                    out.push(' ');
                } else if cell.graphemes_buf(&mut grapheme_buf[..len]).is_ok() {
                    let (c, text) = compose_cluster(&grapheme_buf[..len]);
                    if let Some(text) = text {
                        out.push_str(&text);
                    } else {
                        out.push(c);
                    }
                } else {
                    out.push(' ');
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(cols: u16, rows: u16, scrollback: usize) -> GhosttyBackend {
        GhosttyBackend::new(cols, rows, scrollback).unwrap()
    }

    fn feed(b: &mut GhosttyBackend, bytes: &[u8]) -> TerminalChangeSet {
        b.feed(bytes).unwrap()
    }

    fn row_text(b: &GhosttyBackend, row: usize) -> String {
        let snap = b.viewport_snapshot().unwrap();
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
        let mut b = backend(80, 24, 100);
        let changes = feed(&mut b, b"hello");
        assert_eq!(row_text(&b, 0), "hello");
        assert!(changes.dirty_rows.contains(&0));
        assert!(changes.cursor_changed);
    }

    #[test]
    fn 한글_wide_char_2셀() {
        let mut b = backend(80, 24, 100);
        feed(&mut b, "가나".as_bytes());
        let snap = b.viewport_snapshot().unwrap();
        let first = snap.visible_cells[0];
        assert_eq!(first.c, '가');
        assert!(first.wide);
        assert!(snap.visible_cells[1].wide_spacer);
        assert_eq!(row_text(&b, 0), "가나");
    }

    #[test]
    fn nfd_한글_자소분해_입력을_음절로_합성() {
        let mut b = backend(80, 24, 100);
        let nfd = "\u{1112}\u{1161}\u{11AB}\u{1100}\u{1173}\u{11AF}";
        feed(&mut b, nfd.as_bytes());
        assert_eq!(row_text(&b, 0), "한글");
        assert_eq!(b.screen_text().lines().next().unwrap().trim_end(), "한글");
    }

    #[test]
    fn 터미널_질의_응답_수집() {
        let mut b = backend(80, 24, 100);
        let changes = feed(&mut b, b"\x1b[c");
        assert!(!changes.pty_responses.is_empty());
        assert!(changes.pty_responses.starts_with(b"\x1b["));
        assert!(feed(&mut b, b"x").pty_responses.is_empty());
    }

    #[test]
    fn bracketed_paste_모드() {
        let mut b = backend(80, 24, 100);
        assert!(!b.bracketed_paste());
        feed(&mut b, b"\x1b[?2004h");
        assert!(b.bracketed_paste());
        feed(&mut b, b"\x1b[?2004l");
        assert!(!b.bracketed_paste());
    }

    #[test]
    fn 스크롤백과_offset() {
        let mut b = backend(80, 4, 100);
        for i in 0..10 {
            feed(&mut b, format!("line{i}\r\n").as_bytes());
        }
        b.scroll(3);
        assert_eq!(b.viewport_snapshot().unwrap().scroll_offset, 3);
        b.scroll(-100);
        assert_eq!(b.viewport_snapshot().unwrap().scroll_offset, 0);
    }

    #[test]
    fn alt_screen_감지() {
        let mut b = backend(80, 24, 100);
        assert!(!b.viewport_snapshot().unwrap().is_alt_screen);
        feed(&mut b, b"\x1b[?1049h");
        assert!(b.viewport_snapshot().unwrap().is_alt_screen);
    }

    #[test]
    fn 제목_변경() {
        let mut b = backend(80, 24, 100);
        let changes = feed(&mut b, b"\x1b]2;My Title\x1b\\");
        assert!(changes.title_changed);
        assert_eq!(
            b.viewport_snapshot().unwrap().title.as_deref(),
            Some("My Title")
        );
    }

    #[test]
    fn reset은_화면을_비운다() {
        let mut b = backend(80, 24, 100);
        feed(&mut b, b"hello");
        b.reset();
        assert_eq!(row_text(&b, 0), "");
    }

    #[test]
    fn 커서_위치() {
        let mut b = backend(80, 24, 100);
        feed(&mut b, b"ab");
        let snap = b.viewport_snapshot().unwrap();
        assert_eq!((snap.cursor.col, snap.cursor.row), (2, 0));
        assert!(snap.cursor.visible);
    }
}
