//! AlacrittyBackend (설계문서 4.1/4.3). tty/event_loop는 사용 금지(1.3) —
//! Term + vte parser + grid만 쓴다. PTY는 pty crate가 소유한다.

use crate::visible_cells::VisibleCells;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Row, Scroll};
use alacritty_terminal::term::cell::Cell as AlacrittyCell;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode, test::TermSize};
use alacritty_terminal::vte::ansi::{
    Color, CursorShape as VteCursorShape, NamedColor, Processor, Rgb,
};
use std::cell::RefCell;
use std::sync::Arc;
use unicode_normalization::UnicodeNormalization;

use crate::backend::{
    TerminalBackend, TerminalCacheBudget, TerminalCacheClass, TerminalCacheEvent,
    TerminalCacheEventKind, TerminalCacheFootprint, TerminalExternalSurfaceHandle,
    TerminalRenderModel,
};
use crate::change_set::TerminalChangeSet;
use crate::viewport_snapshot::{
    CellAttrs, CellGrapheme, CursorShape, CursorSnapshot, TerminalCell, TerminalViewportSnapshot,
};

/// NFC single scalars stay inline; non-composable text preserves all source scalars.
pub(crate) fn composed_cell(base: char, zerowidth: Option<&[char]>) -> (char, Option<String>) {
    let Some(zw) = zerowidth.filter(|zw| !zw.is_empty()) else {
        return (base, None);
    };
    let mut nfc = std::iter::once(base).chain(zw.iter().copied()).nfc();
    if let (Some(c), None) = (nfc.next(), nfc.next()) {
        return (c, None);
    }
    let mut text = String::with_capacity(4 + zw.len() * 4);
    text.push(base);
    text.extend(zw.iter());
    (base, Some(text))
}

/// 터미널 질의 응답을 수집한다 (PtyWrite + OSC 색상 질의).
/// title/bell 이벤트는 PR-10/13에서 소비 예정 — 현재는 무시.
/// ColorRequest는 팔레트 참조가 필요해 feed()에서 해석한다.
type ColorFormatter = std::sync::Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>;

enum PtyResponseEvent {
    Bytes(String),
    ColorRequest(usize, ColorFormatter),
}

#[derive(Clone, Default)]
struct CollectingListener {
    /// 응답 종류별 버퍼를 따로 두면 `OSC 11` 뒤의 `CSI 6n`처럼 한 feed에 들어온
    /// 질의의 응답 순서가 뒤집힌다. 자식이 요청한 순서 그대로 한 큐에 보관한다.
    pty_response_events: std::sync::Arc<std::sync::Mutex<Vec<PtyResponseEvent>>>,
    /// OSC 0/2로 프로그램이 설정한 터미널 제목(현재 값). 세션 이름 동적 표시에 쓴다.
    title: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl EventListener for CollectingListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => self
                .pty_response_events
                .lock()
                .expect("pty response events lock")
                .push(PtyResponseEvent::Bytes(text)),
            Event::ColorRequest(index, formatter) => self
                .pty_response_events
                .lock()
                .expect("pty response events lock")
                .push(PtyResponseEvent::ColorRequest(index, formatter)),
            // OSC 0/2 제목 — 최신 값 보관, 리셋이면 비운다.
            Event::Title(t) => *self.title.lock().expect("title lock") = Some(t),
            Event::ResetTitle => *self.title.lock().expect("title lock") = None,
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ViewportKey {
    cols: usize,
    rows: usize,
    display_offset: usize,
    alt: bool,
}

#[derive(Default)]
struct ViewportCache {
    key: Option<ViewportKey>,
    cells: Option<VisibleCells>,
    graphemes: Option<Arc<[CellGrapheme]>>,
    dirty: Vec<bool>,
    scratch: Option<Row<AlacrittyCell>>,
    grapheme_bytes: usize,
    scratch_bytes: usize,
}
impl ViewportCache {
    fn heap_bytes(&self) -> usize {
        self.cells.as_ref().map_or(0, VisibleCells::heap_bytes)
            + self.grapheme_bytes
            + self.scratch_bytes
            + self.dirty.capacity() * std::mem::size_of::<bool>()
    }
}

#[inline]
fn snapshot_cell_style(
    cell: &AlacrittyCell,
    colors: &alacritty_terminal::term::color::Colors,
    c: char,
) -> TerminalCell {
    let flags = cell.flags;
    let (mut fg, mut bg) = (
        resolve_color(cell.fg, colors, DEFAULT_FG),
        resolve_color(cell.bg, colors, DEFAULT_BG),
    );
    if flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    let mut attrs = CellAttrs::empty();
    attrs.set(CellAttrs::BOLD, flags.contains(Flags::BOLD));
    attrs.set(CellAttrs::ITALIC, flags.contains(Flags::ITALIC));
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
    TerminalCell::new(
        c,
        fg,
        bg,
        flags.contains(Flags::WIDE_CHAR),
        flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
        attrs,
    )
}

#[inline]
fn snapshot_cell(
    cell: &AlacrittyCell,
    colors: &alacritty_terminal::term::color::Colors,
) -> (TerminalCell, Option<String>) {
    let flags = cell.flags;
    let (c, text) = if flags
        .intersects(Flags::HIDDEN | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
    {
        (
            if flags.contains(Flags::HIDDEN) {
                ' '
            } else {
                cell.c
            },
            None,
        )
    } else {
        composed_cell(cell.c, cell.zerowidth())
    };
    (snapshot_cell_style(cell, colors, c), text)
}

/// Compare the projected row without creating transient grapheme Strings.
#[inline]
fn snapshot_cell_matches(
    cell: &AlacrittyCell,
    colors: &alacritty_terminal::term::color::Colors,
    previous: &TerminalCell,
    previous_text: Option<&str>,
) -> bool {
    let flags = cell.flags;
    let (c, text_matches) = if flags
        .intersects(Flags::HIDDEN | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
    {
        (
            if flags.contains(Flags::HIDDEN) {
                ' '
            } else {
                cell.c
            },
            previous_text.is_none(),
        )
    } else if let Some(zw) = cell.zerowidth().filter(|zw| !zw.is_empty()) {
        if let Some(text) = previous_text {
            // A stored multi-scalar grapheme preserves source scalars, not normalized text.
            // Equality with that source also proves the existing composition policy is unchanged.
            (
                cell.c,
                std::iter::once(cell.c)
                    .chain(zw.iter().copied())
                    .eq(text.chars()),
            )
        } else {
            let mut nfc = std::iter::once(cell.c).chain(zw.iter().copied()).nfc();
            match (nfc.next(), nfc.next()) {
                (Some(c), None) => (c, true),
                _ => (cell.c, false),
            }
        }
    } else {
        (cell.c, previous_text.is_none())
    };
    text_matches && snapshot_cell_style(cell, colors, c) == *previous
}

pub struct AlacrittyBackend {
    term: Term<CollectingListener>,
    listener: CollectingListener,
    processor: Processor,
    scrollback_lines: usize,
    cache_class: TerminalCacheClass,
    active_scrollback_limit: usize,
    /// 메모리 압박 하 강제 트림이 씌운 스크롤백 상한(줄). Some이면 apply_cache_class가
    /// 클래스 예산을 이 값으로 클램프해, resize/가시성 전이의 클래스 재적용이 트림을
    /// 되돌리지 못하게 한다(리뷰 A-H1의 flip-flop 방지). 압박 해소 시 runtime이
    /// clear_pressure_trim으로 해제해 스크롤백을 회복한다.
    pressure_trim_floor: Option<usize>,
    viewport_cache: RefCell<ViewportCache>,
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
            pressure_trim_floor: None,
            viewport_cache: RefCell::new(ViewportCache::default()),
        }
    }

    #[cfg(test)]
    fn viewport_snapshot_uncached(&self) -> Option<TerminalViewportSnapshot> {
        let cols = self.term.columns();
        let rows = self.term.screen_lines();
        let colors = self.term.colors();

        // 커서/스크롤 오프셋은 renderable_content 기준(vi 모드 반영)으로 뽑고 borrow를
        // 즉시 놓는다. 셀은 아래에서 grid.read_line으로 직접 순회한다.
        let (display_offset, cursor_point, cursor_shape) = {
            let content = self.term.renderable_content();
            (
                content.display_offset,
                content.cursor.point,
                content.cursor.shape,
            )
        };

        // deppy-sijo(D): display_iter 대신 read_line으로 직접 순회한다 — 스크롤이 압축
        // 영역까지 가면 read_line이 scratch로 행을 복원해 준다(비압축이면 원시 행 그대로).
        // display_iter와 동일 매핑: 화면 row r ↔ grid line (r - display_offset).
        let grid = self.term.grid();
        let mut scratch = Row::<AlacrittyCell>::new(cols);
        let mut cells = vec![TerminalCell::default(); cols * rows];
        let mut graphemes = Vec::new();
        for screen_row in 0..rows {
            let grid_line =
                alacritty_terminal::index::Line(screen_row as i32 - display_offset as i32);
            let line = grid.read_line(grid_line, &mut scratch);
            for col in 0..cols {
                let cell = &line[alacritty_terminal::index::Column(col)];
                let flags = cell.flags;
                let (mut fg, mut bg) = (
                    resolve_color(cell.fg, colors, DEFAULT_FG),
                    resolve_color(cell.bg, colors, DEFAULT_BG),
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
                let index = screen_row * cols + col;
                let c = if flags.contains(Flags::HIDDEN) {
                    ' '
                } else if flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    cell.c
                } else if let Some(zw) = cell.zerowidth().filter(|zw| !zw.is_empty()) {
                    let (c, text) = composed_cell(cell.c, Some(zw));
                    if let Some(text) = text {
                        graphemes.push(CellGrapheme { index, text });
                    }
                    c
                } else {
                    cell.c
                };
                cells[index] = TerminalCell::new(
                    c,
                    fg,
                    bg,
                    flags.contains(Flags::WIDE_CHAR),
                    flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
                    attrs,
                );
            }
        }

        // cursor.point는 grid 좌표 — 스크롤 중이면 viewport 밖일 수 있다
        let cursor_row = cursor_point.line.0 + display_offset as i32;
        let in_view = (0..rows as i32).contains(&cursor_row);
        let (shape, shape_visible) = map_cursor_shape(cursor_shape);
        let cursor = CursorSnapshot {
            col: cursor_point.column.0 as u16,
            row: cursor_row.max(0) as u16,
            shape,
            visible: shape_visible && in_view && self.term.mode().contains(TermMode::SHOW_CURSOR),
        };

        Some(TerminalViewportSnapshot {
            cols: cols as u16,
            rows: rows as u16,
            cursor,
            visible_cells: cells.into(),
            graphemes: crate::viewport_snapshot::share_cell_graphemes(graphemes),
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

    fn apply_cache_class(&mut self, class: TerminalCacheClass) -> Option<TerminalCacheEvent> {
        let before = self.cache_footprint();
        let budget_target = effective_scrollback_limit(
            self.scrollback_lines,
            self.term.columns(),
            self.term.screen_lines(),
            class,
        );
        // 압박 트림이 씌운 상한이 있으면 클래스 예산을 그 이하로 클램프한다 — 이래야
        // resize/전이의 클래스 재적용이 트림을 되돌리지 않는다(A-H1 flip-flop 방지).
        let target = budget_target.min(self.pressure_trim_floor.unwrap_or(usize::MAX));
        self.cache_class = class;
        if class != TerminalCacheClass::Visible {
            *self.viewport_cache.get_mut() = ViewportCache::default();
        }
        if target != self.active_scrollback_limit {
            self.term.set_options(Config {
                scrolling_history: target,
                preserve_scrollback_on_clear: true,
                ..Config::default()
            });
            self.active_scrollback_limit = target;
        }
        // deppy-sijo(D): 보이지 않는 세션은 스크롤 반응성이 필요 없으므로 HOT 창까지
        // 포함해 히스토리 전체를 압축한다(feed 트리거의 HOT=256 비압축분도 회수).
        // 다시 Visible이 되면 이후 feed가 최근 창을 그대로 두고, 그 전 스크롤은 read_line
        // 이 복원한다. Visible은 feed의 HOT 창 유지 정책(compress_history(HOT))을 따른다.
        if matches!(
            class,
            TerminalCacheClass::Hidden | TerminalCacheClass::Exited
        ) {
            self.term.grid_mut().compress_history(0);
            self.term.inactive_grid_mut().compress_history(0);
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
        preserve_scrollback_on_clear: true,
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

/// 바이트 예산이 담을 수 있는 **히스토리 줄 수**를 압축 인지로 역산한다.
///
/// 예산 배분: 화면 rows + HOT 히스토리 창은 **비압축 전액**(`bytes_per_line`)으로 두고,
/// 그보다 오래된(식은) 히스토리는 라인당 `ceil(bytes_per_line / SCROLLBACK_COMPRESSION_DIVISOR)`
/// 만 든다고 본다. feed()가 언제나 최근 HOT 창을 비압축으로 유지(compress_history(HOT))
/// 하므로 어떤 캐시 클래스에서도 비압축 상한은 HOT 줄이다 — 이 함수가 가정하는 배분과
/// 실제 grid 배치가 일치한다.
///
/// 보수성: `div_ceil`로 라인당 압축 비용을 올려 담을 줄 수를 **작게** 잡는다. 반환값이
/// 곧 히스토리 캡이고, `effective_scrollback_limit`이 `max_scrollback_lines`로 다시
/// 클램프하므로 절대 상한은 유지된다. 이 값을 그대로 채웠을 때의 보수적 추정 바이트는
/// 정의상 `max_bytes`를 넘지 않는다(테스트 `최악_압축_추정은_예산_이내` 참고).
fn history_lines_for_byte_budget(cols: usize, rows: usize, max_bytes: usize) -> usize {
    let bytes_per_line = estimated_bytes_per_line(cols.max(1));
    let rows = rows.max(1);
    let screen_bytes = rows.saturating_mul(bytes_per_line);
    // 화면 rows조차 예산을 넘으면 히스토리를 담을 여유가 없다.
    let Some(mut remaining) = max_bytes.checked_sub(screen_bytes) else {
        return 0;
    };
    // (1) HOT 히스토리 창 — feed가 비압축으로 유지하므로 전액으로 채운다.
    let hot_uncompressed = (remaining / bytes_per_line).min(HOT_SCROLLBACK_LINES);
    remaining -= hot_uncompressed * bytes_per_line;
    if hot_uncompressed < HOT_SCROLLBACK_LINES {
        // 예산이 HOT 창조차 다 못 채운다 — 압축 히스토리를 위한 여지 없음.
        return hot_uncompressed;
    }
    // (2) 남은 예산은 식은(압축된) 히스토리를 보수적 비율로 커버한다.
    let compressed_bytes_per_line = bytes_per_line
        .div_ceil(SCROLLBACK_COMPRESSION_DIVISOR)
        .max(1);
    let cold_lines = remaining / compressed_bytes_per_line;
    hot_uncompressed.saturating_add(cold_lines)
}

fn estimated_bytes_per_line(cols: usize) -> usize {
    std::mem::size_of::<Row<AlacrittyCell>>()
        .saturating_add(cols.saturating_mul(std::mem::size_of::<AlacrittyCell>()))
}

/// 백엔드가 실제로 점유하는 스크롤백 바이트 추정. 부풀린 전량-비압축 추정 대신
/// (화면 rows + **비압축** 히스토리 행)은 셀 배열 전액으로, 압축된 히스토리는 grid의
/// **실제** 압축 곁가지 바이트(`compressed_heap_bytes`)로 계산한다. 이래야 전역 128MB
/// 강제가 부풀린 추정이 아니라 진짜(작은) 메모리를 본다.
///
/// `uncompressed_history_lines`는 호출자(`cache_footprint`)가 클래스별 압축 정책으로
/// 산정한다 — 압축된 행을 raw로 이중 계상하지 않도록.
fn estimated_terminal_bytes(
    cols: usize,
    rows: usize,
    uncompressed_history_lines: usize,
    compressed_heap_bytes: usize,
) -> usize {
    rows.saturating_add(uncompressed_history_lines)
        .saturating_mul(estimated_bytes_per_line(cols.max(1)))
        .saturating_add(compressed_heap_bytes)
}

/// grid의 `idx` 행 마지막 열이 WRAPLINE(soft wrap)인지 판정한다. 압축된 히스토리 행은
/// read_line이 scratch로 복원해 주므로 스크롤백 깊은 곳도 안전하게 읽는다.
fn line_soft_wrapped(
    grid: &alacritty_terminal::grid::Grid<AlacrittyCell>,
    idx: i32,
    cols: usize,
    scratch: &mut Row<AlacrittyCell>,
) -> bool {
    grid.read_line(alacritty_terminal::index::Line(idx), scratch)
        [alacritty_terminal::index::Column(cols - 1)]
    .flags
    .contains(Flags::WRAPLINE)
}

/// deppy-sijo(D) 스크롤백 라인 압축: 이 줄 수만큼의 **최근** 스크롤아웃 히스토리는
/// 비압축(셀 배열)으로 남긴다. 되돌려-스크롤이 이 범위 안이면 decode 없이 즉시 보이고,
/// 그보다 더 과거로 스크롤할 때만 렌더가 행을 복원한다. 값이 클수록 스크롤 반응은 좋지만
/// RSS 절감은 줄어든다 — 몇 화면 분량으로 잡는다.
const HOT_SCROLLBACK_LINES: usize = 256;

/// 압축 인지 예산이 **식은(HOT 밖) 히스토리 줄**에 가정하는 보수적 압축비.
///
/// 예산 역산(`history_lines_for_byte_budget`)은 화면 rows + HOT 창은 비압축 전액으로,
/// 그보다 오래된 히스토리는 라인당 `bytes_per_line / 이 값`만큼만 든다고 보고 담을 줄
/// 수를 늘린다. 실측(`compression_rss_probe`)의 현실 로그류 압축비는 ~30배지만, 색이
/// 자주 바뀌어 run-length가 잘 안 되는 콘텐츠는 그보다 훨씬 덜 압축된다.
///
/// 안전성 한계(정직히): 압축 행의 `AttrRun`(12B) + 텍스트(1B/셀)라, **셀마다 색이 다른**
/// 병리적 콘텐츠는 라인당 ~13B/셀 → `Cell`(24B) 대비 겨우 **~1.86배**로만 압축된다. 즉
/// 어떤 divisor>1도 그런 콘텐츠로 캡을 가득 채우면 per-session 바이트 예산을 넘는다.
/// **2**로 잡아(4에서 하향) 그 최악을 완화한다 — 200열 Visible 기준 캡을 이전 ~3,448줄
/// 대비 ~6,600줄로 늘리면서(현실 콘텐츠엔 순기능), 병리적 최악은 예산의 ~1.66배(divisor 4)
/// 에서 ~1.1배로 줄인다. 현실 로그류(~30배)에선 여유가 넘친다.
///
/// 최종 방어선: (a) 절대 상한 `TerminalCacheBudget::max_scrollback_lines`(줄 수 캡), (b)
/// `cache_footprint`가 **실제** 압축 곁가지 바이트를 정확히 보고. 단, 전역 128MB 강제는
/// 현재 **exited 세션만** 아카이브해 회수하므로(`exited_to_archive_for_budget`), live
/// visible/hidden 세션은 이 강제로 회수되지 않는다 — 병리적 live 세션을 실제로 bound하려면
/// live 세션 scrollback trim 경로가 필요하다(runaway 보호 로드맵 후속 과제).
const SCROLLBACK_COMPRESSION_DIVISOR: usize = 2;

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

        // deppy-sijo(D): 새로 스크롤아웃된 히스토리를 압축해 RSS를 낮춘다. 압축은
        // 화면 밖 히스토리(Line<0, hot 범위 밖)만 건드리므로 damage/렌더 좌표에 영향이
        // 없다. 정상 상태에선 갓 식은 몇 행만 처리한다(내부에서 압축 frontier에서 중단).
        self.term.grid_mut().compress_history(HOT_SCROLLBACK_LINES);

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
        let cache = self.viewport_cache.get_mut();
        for row in &dirty_rows {
            if let Some(dirty) = cache.dirty.get_mut(*row as usize) {
                *dirty = true;
            }
        }

        let response_events = std::mem::take(
            &mut *self
                .listener
                .pty_response_events
                .lock()
                .expect("pty response events lock"),
        );
        let mut pty_responses = Vec::new();
        for event in response_events {
            match event {
                PtyResponseEvent::Bytes(text) => {
                    pty_responses.extend_from_slice(text.as_bytes());
                }
                // OSC 색상 질의(ESC]10;? / ESC]4;n;? 등) — 현재 팔레트(재정의
                // 반영) 기준으로 응답하되 다른 질의와의 원래 순서를 보존한다.
                PtyResponseEvent::ColorRequest(index, formatter) => {
                    // 자식 프로세스 출력은 임의 데이터 — 범위 밖 인덱스는 무시 (panic 금지)
                    if index >= alacritty_terminal::term::color::COUNT {
                        continue;
                    }
                    let [r, g, b] = self.term.colors()[index]
                        .map(rgb_to_arr)
                        .unwrap_or_else(|| palette_default(index));
                    pty_responses.extend_from_slice(formatter(Rgb { r, g, b }).as_bytes());
                }
            }
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
        *self.viewport_cache.get_mut() = ViewportCache::default();
        Ok(())
    }

    fn grid_dimensions(&self) -> anyhow::Result<(u16, u16)> {
        Ok((
            self.term.columns().try_into()?,
            self.term.screen_lines().try_into()?,
        ))
    }

    fn render_model(&self) -> TerminalRenderModel {
        TerminalRenderModel::CellGrid
    }

    fn viewport_metadata(&self) -> Option<crate::TerminalViewportMetadata> {
        Some(crate::TerminalViewportMetadata {
            scroll_offset: self.term.grid().display_offset().min(i32::MAX as usize) as i32,
            is_alt_screen: self.term.mode().contains(TermMode::ALT_SCREEN),
        })
    }

    fn viewport_snapshot(&self) -> Option<TerminalViewportSnapshot> {
        let cols = self.term.columns();
        let rows = self.term.screen_lines();
        let colors = self.term.colors();
        let (display_offset, cursor_point, cursor_shape) = {
            let content = self.term.renderable_content();
            (
                content.display_offset,
                content.cursor.point,
                content.cursor.shape,
            )
        };
        let key = ViewportKey {
            cols,
            rows,
            display_offset,
            alt: self.term.mode().contains(TermMode::ALT_SCREEN),
        };
        let mut cache = self.viewport_cache.borrow_mut();
        if cache.key != Some(key) {
            *cache = ViewportCache::default();
            cache.key = Some(key);
            cache.dirty = vec![true; rows];
            cache.scratch = Some(Row::new(cols));
        }
        let previous = cache.cells.clone();
        let previous_graphemes = cache.graphemes.clone();
        let old_graphemes = previous_graphemes.as_deref().unwrap_or(&[]);
        let mut replacement_rows = previous.is_none().then(|| Vec::with_capacity(rows));
        let mut grapheme_updates: Vec<(usize, Vec<CellGrapheme>)> = Vec::new();
        let grid = self.term.grid();
        let mut read_rows = false;
        for row in 0..rows {
            if !cache.dirty[row] {
                continue;
            }
            read_rows = true;
            let grid_line = alacritty_terminal::index::Line(row as i32 - display_offset as i32);
            let line = grid.read_line(grid_line, cache.scratch.as_mut().unwrap());
            let start = row * cols;
            let old_text_start = old_graphemes.partition_point(|entry| entry.index < start);
            let old_text_end = old_graphemes.partition_point(|entry| entry.index < start + cols);
            let old_text = &old_graphemes[old_text_start..old_text_end];
            let unchanged = previous.as_ref().is_some_and(|previous| {
                (0..cols).all(|col| {
                    let old_text = old_text
                        .binary_search_by_key(&(start + col), |entry| entry.index)
                        .ok()
                        .map(|index| old_text[index].text.as_str());
                    snapshot_cell_matches(
                        &line[alacritty_terminal::index::Column(col)],
                        colors,
                        previous.get(start + col).unwrap(),
                        old_text,
                    )
                })
            });
            if !unchanged {
                let mut cells = Arc::<[TerminalCell]>::new_uninit_slice(cols);
                let slots = Arc::get_mut(&mut cells).unwrap();
                let mut text = Vec::new();
                for col in 0..cols {
                    let (cell, grapheme) =
                        snapshot_cell(&line[alacritty_terminal::index::Column(col)], colors);
                    slots[col].write(cell);
                    if let Some(text_value) = grapheme {
                        text.push(CellGrapheme {
                            index: start + col,
                            text: text_value,
                        });
                    }
                }
                // SAFETY: the loop writes exactly once to every slot in 0..cols before
                // publication. TerminalCell is Copy and owns no resources, so a panic
                // before this point safely drops only the uninitialized Arc allocation.
                let cells = unsafe { cells.assume_init() };
                let replacement = replacement_rows.get_or_insert_with(|| {
                    previous.as_ref().unwrap().shared_rows().unwrap().to_vec()
                });
                if replacement.len() <= row {
                    replacement.push(cells);
                } else {
                    replacement[row] = cells;
                }
                if text != old_text {
                    grapheme_updates.push((row, text));
                }
            }
        }
        if let Some(replacement) = replacement_rows {
            cache.cells = Some(VisibleCells::from_rows(cols, replacement));
        }
        if !grapheme_updates.is_empty() {
            let mut entries: Vec<_> = old_graphemes
                .iter()
                .filter(|entry| {
                    !grapheme_updates
                        .iter()
                        .any(|(row, _)| entry.index / cols == *row)
                })
                .cloned()
                .collect();
            for (_, text) in grapheme_updates {
                entries.extend(text);
            }
            entries.sort_unstable_by_key(|entry| entry.index);
            cache.graphemes = Some(crate::share_cell_graphemes(entries));
            cache.grapheme_bytes = cache.graphemes.as_ref().map_or(0, |entries| {
                if entries.is_empty() {
                    0
                } else {
                    2 * std::mem::size_of::<usize>()
                        + std::mem::size_of_val(&**entries)
                        + entries
                            .iter()
                            .map(|entry| entry.text.capacity())
                            .sum::<usize>()
                }
            });
        } else if cache.graphemes.is_none() {
            cache.graphemes = Some(crate::share_cell_graphemes(Vec::new()));
        }
        if read_rows {
            let scratch = cache.scratch.as_ref().unwrap();
            cache.scratch_bytes = scratch.capacity() * std::mem::size_of::<AlacrittyCell>()
                + scratch
                    .into_iter()
                    .map(AlacrittyCell::extra_heap_bytes)
                    .sum::<usize>();
        }
        cache.dirty.fill(false);
        let cursor_row = cursor_point.line.0 + display_offset as i32;
        let (shape, shape_visible) = map_cursor_shape(cursor_shape);
        let snapshot = TerminalViewportSnapshot {
            cols: cols as u16,
            rows: rows as u16,
            cursor: CursorSnapshot {
                col: cursor_point.column.0 as u16,
                row: cursor_row.max(0) as u16,
                shape,
                visible: shape_visible
                    && (0..rows as i32).contains(&cursor_row)
                    && self.term.mode().contains(TermMode::SHOW_CURSOR),
            },
            visible_cells: cache.cells.as_ref().unwrap().clone(),
            graphemes: cache.graphemes.as_ref().unwrap().clone(),
            dirty_ranges: Vec::new(),
            title: self
                .listener
                .title
                .lock()
                .ok()
                .and_then(|title| title.clone()),
            scroll_offset: display_offset as i32,
            is_alt_screen: key.alt,
        };
        // Hidden/Exited readers may request snapshots without a later class transition.
        // The immutable reader result survives, but no render cache stays in the backend.
        if self.cache_class != TerminalCacheClass::Visible {
            *cache = ViewportCache::default();
        }
        Some(snapshot)
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
        *self.viewport_cache.get_mut() = ViewportCache::default();
    }

    fn set_cache_class(&mut self, class: TerminalCacheClass) -> Option<TerminalCacheEvent> {
        self.apply_cache_class(class)
    }

    fn set_scrollback_limit(&mut self, requested: usize) -> crate::ScrollbackApplyResult {
        let bounded = requested.min(crate::policy::SCROLLBACK_LINES_MAX);
        let before = self.term.grid().history_size() + self.term.inactive_grid().history_size();
        if self.scrollback_lines != bounded {
            self.scrollback_lines = bounded;
            self.apply_cache_class(self.cache_class);
        }
        let after = self.term.grid().history_size() + self.term.inactive_grid().history_size();
        crate::ScrollbackApplyResult::Applied {
            requested,
            effective: self.active_scrollback_limit,
            trimmed: before.saturating_sub(after),
        }
    }

    fn trim_scrollback(&mut self, max_lines: usize) -> Option<TerminalCacheEvent> {
        let target = self.active_scrollback_limit.min(max_lines);
        if target >= self.active_scrollback_limit {
            return None; // 이미 그 이하 — 더 줄일 것 없음
        }
        let before = self.cache_footprint();
        // 스크롤백 상한을 낮춰 가장 오래된 히스토리를 드롭한다.
        self.term.set_options(Config {
            scrolling_history: target,
            preserve_scrollback_on_clear: true,
            ..Config::default()
        });
        self.active_scrollback_limit = target;
        // 트림 상한을 영속화한다 — 이후 클래스 재적용(resize/전이)이 이 값을 존중해
        // 스크롤백을 도로 늘리지 못한다(A-H1). 이미 더 낮은 floor가 있으면 유지.
        self.pressure_trim_floor = Some(target.min(self.pressure_trim_floor.unwrap_or(usize::MAX)));
        // 남은 히스토리를 HOT 창까지 전부 압축해 최대한 회수한다(압박 하 최후 수단).
        // 두 그리드 모두 압축한다 — alt-screen 세션은 스크롤백이 inactive(primary)에
        // 있어 active(alt)만 압축하면 회수가 불완전하다(리뷰 L3). 스크롤백 없는 쪽은 no-op.
        self.term.grid_mut().compress_history(0);
        self.term.inactive_grid_mut().compress_history(0);
        let after = self.cache_footprint();
        (after.estimated_bytes < before.estimated_bytes
            || after.history_lines < before.history_lines)
            .then_some(TerminalCacheEvent {
                kind: TerminalCacheEventKind::ScrollbackLimitApplied,
                class: self.cache_class,
                budget: TerminalCacheBudget::for_class(self.cache_class),
                before,
                after,
            })
    }

    fn clear_pressure_trim(&mut self) -> bool {
        if self.pressure_trim_floor.is_none() {
            return false;
        }
        self.pressure_trim_floor = None;
        // floor 해제 후 현재 클래스를 재적용해 스크롤백 상한을 예산 전액으로 회복한다
        // (상한만 올리므로 즉시 메모리가 늘진 않고, 이후 feed로 채워진다).
        self.apply_cache_class(self.cache_class);
        true
    }

    fn cache_class(&self) -> TerminalCacheClass {
        self.cache_class
    }

    fn cache_footprint(&self) -> TerminalCacheFootprint {
        let cols = self.term.columns();
        let rows = self.term.screen_lines();
        // 활성 + 비활성(alt↔primary) 두 그리드의 스크롤백을 모두 합산한다. alt-screen
        // (vim/less/htop) 활성 시 실제 스크롤백은 inactive(primary)에 있어, 활성 그리드만
        // 보면 히스토리를 ~0으로 오판해 예산·트림 사각지대가 된다(리뷰 A-M2). 한쪽은 늘
        // 스크롤백이 없어(alt=0) 합산해도 실제 총량과 같다.
        let active = self.term.grid();
        let inactive = self.term.inactive_grid();
        let history_lines = active.history_size() + inactive.history_size();
        // 압축된(HOT 밖) 히스토리는 grid의 실제 곁가지 힙 바이트로 계상한다.
        let compressed_heap_bytes =
            active.compressed_heap_bytes() + inactive.compressed_heap_bytes();
        // 비압축(raw) 히스토리 행 수를 **실제 grid 상태**로 산정한다: 전체 히스토리에서
        // 실제 압축된 슬롯 수를 뺀다. 클래스 모델(예전: Hidden=0)은 배경 feed를 받는
        // hidden 세션이 HOT 창을 raw로 남기는 것을 놓쳐 ~HOT*bpl 과소계상했다(리뷰 B-M2).
        // 실측 기반이라 클래스와 무관하게 정확하고 이중계상도 없다(압축 행은 raw로 안 셈).
        let compressed_rows = active.compressed_row_count() + inactive.compressed_row_count();
        let uncompressed_history = history_lines.saturating_sub(compressed_rows);
        TerminalCacheFootprint {
            class: self.cache_class,
            scrollback_limit_lines: self.active_scrollback_limit,
            history_lines,
            screen_lines: rows,
            columns: cols,
            bytes_per_line: estimated_bytes_per_line(cols),
            estimated_bytes: estimated_terminal_bytes(
                cols,
                rows,
                uncompressed_history,
                compressed_heap_bytes,
            )
            .saturating_add(self.viewport_cache.borrow().heap_bytes()),
        }
    }

    fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    fn application_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// grid 전체(history+화면)를 truecolor SGR ANSI로 덤프한다. 새 백엔드에
    /// 그대로 feed하면 스크롤백·색·wide char가 복원된다 (압축 아카이브 왕복용).
    /// wrapped 행은 개행 없이 이어붙여 복원 시 reflow가 자연스럽다.
    fn serialize_scrollback(&self) -> Option<Vec<u8>> {
        // 복원/압축 archive의32MiB상한을직렬화할때부터지킨다.
        serialize_scrollback_bounded(self, 32 * 1024 * 1024).ok()
    }

    fn serialize_scrollback_bounded(
        &self,
        max_bytes: usize,
    ) -> Result<Vec<u8>, crate::ScrollbackSerializeError> {
        serialize_scrollback_bounded(self, max_bytes)
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
        // deppy-sijo(D): 압축된 히스토리 행은 read_line이 scratch로 복원해 준다.
        let mut scratch = Row::<AlacrittyCell>::new(cols);
        // 화면 최하단(rows-1)에서 위(가장 오래된 history)로. cap이 걸려도 최신 매치가 남는다.
        'lines: for line_idx in (-(history as i32)..rows as i32).rev() {
            chars.clear();
            spans.clear();
            let line = grid.read_line(alacritty_terminal::index::Line(line_idx), &mut scratch);
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
                let (c, text) = if cell.flags.contains(Flags::HIDDEN) {
                    (' ', None)
                } else {
                    composed_cell(cell.c, cell.zerowidth())
                };
                if let Some(text) = text {
                    for scalar in text.chars() {
                        chars.push(fold_char(scalar));
                        spans.push((col as u16, col as u16 + 1));
                    }
                } else {
                    chars.push(fold_char(c));
                    spans.push((col as u16, col as u16 + 1));
                }
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

    /// 커서 기준 `back` **논리 라인** 위의 텍스트 (셸 통합 2단계 — 마지막 출력 추출).
    /// soft wrap(WRAPLINE) 행들을 한 논리 라인으로 병합해 세션의 LF 카운터 좌표와
    /// 일치시킨다 — 긴 출력 wrap은 물론, zsh PROMPT_SP가 개행 없는 출력 뒤에 만드는
    /// wrap 행(EOL 마커+프롬프트)도 LF 없이 생기므로 시각 행 인덱스는 어긋난다.
    /// 셀 읽기는 search_scrollback과 동일한 규칙, wrapped 판정은 serialize_scrollback과
    /// 동일하게 마지막 열의 WRAPLINE flag.
    fn logical_line_back_from_cursor(&self, back: usize) -> Option<String> {
        let cols = self.term.columns();
        let rows = self.term.screen_lines() as i32;
        let history = self.term.history_size();
        if cols == 0 {
            return None;
        }
        let grid = self.term.grid();
        let oldest = -(history as i32);
        // deppy-sijo(D): 압축된 히스토리 행은 read_line이 scratch로 복원해 준다.
        let mut scratch = Row::<AlacrittyCell>::new(cols);
        // 커서가 속한 논리 라인의 첫 행 — 위 행이 soft wrap이면 계속 위로.
        // cursor.point.line은 화면 좌표(0=최상단) — display_offset(스크롤)과 무관하다.
        let mut start = grid.cursor.point.line.0;
        while start > oldest && line_soft_wrapped(grid, start - 1, cols, &mut scratch) {
            start -= 1;
        }
        // back 논리 라인 위로 — 각 단계는 이전 논리 라인의 첫 행까지 걷는다.
        for _ in 0..back {
            if start <= oldest {
                return None; // 스크롤백 밖으로 트림된 라인
            }
            start -= 1;
            while start > oldest && line_soft_wrapped(grid, start - 1, cols, &mut scratch) {
                start -= 1;
            }
        }
        // start부터 wrap run을 이어붙인다. 중간(wrapped) 행은 전체 폭(내용이 이어짐),
        // 마지막 행 뒤에서만 trailing 공백을 trim.
        let mut out = String::with_capacity(cols);
        let mut idx = start;
        loop {
            let line = grid.read_line(alacritty_terminal::index::Line(idx), &mut scratch);
            let wrapped = line[alacritty_terminal::index::Column(cols - 1)]
                .flags
                .contains(Flags::WRAPLINE);
            for col in 0..cols {
                let cell = &line[alacritty_terminal::index::Column(col)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                // conceal(SGR 8)은 화면과 동일하게 공백 취급 (search_scrollback 관례)
                if cell.flags.contains(Flags::HIDDEN) {
                    out.push(' ');
                } else {
                    out.push(cell.c);
                    if let Some(zerowidth) = cell.zerowidth() {
                        out.extend(zerowidth.iter().copied());
                    }
                }
            }
            idx += 1;
            if !wrapped || idx >= rows {
                break;
            }
        }
        out.truncate(out.trim_end().len());
        Some(out)
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
                    let (c, text) = if cell.flags.contains(Flags::HIDDEN) {
                        (' ', None)
                    } else {
                        composed_cell(cell.c, cell.zerowidth())
                    };
                    if let Some(text) = text {
                        out.push_str(&text);
                    } else {
                        out.push(c);
                    }
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
        Self::new(
            ' ',
            DEFAULT_FG,
            DEFAULT_BG,
            false,
            false,
            CellAttrs::empty(),
        )
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

// 큰 history를 ANSI로 펼치는 중에도 출력 상한을 넘는 임시 할당을 만들지 않는다.
struct BoundedAnsiDump {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedAnsiDump {
    fn append(&mut self, bytes: &[u8]) -> Result<(), crate::ScrollbackSerializeError> {
        let next = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or(crate::ScrollbackSerializeError::LimitExceeded)?;
        if next > self.limit {
            return Err(crate::ScrollbackSerializeError::LimitExceeded);
        }
        if next > self.bytes.capacity() {
            // 기하급수 확장으로 복사 비용은 선형으로 유지하되 상한은 넘지 않는다.
            let capacity = self
                .bytes
                .capacity()
                .max(32 * 1024)
                .saturating_mul(2)
                .min(self.limit)
                .max(next);
            self.bytes
                .try_reserve_exact(capacity - self.bytes.len())
                .map_err(|_| crate::ScrollbackSerializeError::Unavailable)?;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}

fn serialize_scrollback_bounded(
    backend: &AlacrittyBackend,
    max_bytes: usize,
) -> Result<Vec<u8>, crate::ScrollbackSerializeError> {
    let grid = backend.term.grid();
    let cols = backend.term.columns();
    let rows = backend.term.screen_lines();
    let history = backend.term.history_size();
    let colors = backend.term.colors();
    let mut out = BoundedAnsiDump {
        bytes: Vec::new(),
        limit: max_bytes,
    };
    // 현재 SGR 상태 — 색이 바뀔 때만 시퀀스를 낸다
    let mut current: Option<([u8; 3], [u8; 3])> = None;
    let total = history as i32 + rows as i32;
    // deppy-sijo(D): 압축된 히스토리 행은 read_line이 scratch로 복원해 준다.
    let mut scratch = Row::<AlacrittyCell>::new(cols);
    for (emitted, line_idx) in (-(history as i32)..rows as i32).enumerate() {
        let line = grid.read_line(alacritty_terminal::index::Line(line_idx), &mut scratch);
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
                    out.append(b"\x1b[0m")?;
                } else {
                    out.append(
                        format!(
                            "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m",
                            fg[0], fg[1], fg[2], bg[0], bg[1], bg[2]
                        )
                        .as_bytes(),
                    )?;
                }
                current = Some((fg, bg));
            }
            let mut buf = [0u8; 4];
            out.append(cell.c.encode_utf8(&mut buf).as_bytes())?;
            if let Some(zerowidth) = cell.zerowidth() {
                for zw in zerowidth {
                    out.append(zw.encode_utf8(&mut buf).as_bytes())?;
                }
            }
        }
        // wrapped면 개행 없이 이어붙임, 마지막 행 뒤에는 개행 없음(화면 밀림 방지)
        if !wrapped && (emitted as i32) < total - 1 {
            out.append(b"\r\n")?;
        }
    }
    out.append(b"\x1b[0m")?;
    Ok(out.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::TerminalBackend;

    /// 스크롤백 메모리 산정 근거 — Cell/Row 크기와 라인당 바이트를 출력한다(수동).
    /// `cargo test -p terminal size_probe -- --ignored --nocapture`
    #[test]
    #[ignore = "size probe — run manually with --nocapture"]
    fn size_probe() {
        eprintln!(
            "SIZE-PROBE Cell={}B Flags={}B Row<Cell>(header)={}B  200열/line={}B  10000줄={:.1}MB",
            std::mem::size_of::<AlacrittyCell>(),
            std::mem::size_of::<Flags>(),
            std::mem::size_of::<Row<AlacrittyCell>>(),
            200 * std::mem::size_of::<AlacrittyCell>(),
            (10_000.0 * 200.0 * std::mem::size_of::<AlacrittyCell>() as f64) / (1024.0 * 1024.0),
        );
    }

    /// 실제 스크롤백 RSS 실측(수동) — 20,000줄을 먹인 뒤 Visible에서의 실제 phys_footprint,
    /// 그리고 Hidden으로 트리밍했을 때의 감소를 잰다.
    /// `cargo test -p terminal --release rss_probe -- --ignored --nocapture`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "RSS probe — run manually with --nocapture"]
    fn rss_probe() {
        use crate::backend::TerminalCacheClass;
        fn footprint_mb() -> f64 {
            let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::uninit();
            let rc = unsafe {
                libc::proc_pid_rusage(
                    std::process::id() as libc::c_int,
                    libc::RUSAGE_INFO_V4,
                    info.as_mut_ptr().cast(),
                )
            };
            if rc != 0 {
                return 0.0;
            }
            let info = unsafe { info.assume_init() };
            info.ri_phys_footprint as f64 / (1024.0 * 1024.0)
        }
        let base = footprint_mb();
        // Visible 클래스(10,000줄 / 16MB 예산), 200열. 20,000줄을 먹여 캡을 넘긴다.
        let mut backend = AlacrittyBackend::new(200, 40, 10_000);
        backend.set_cache_class(TerminalCacheClass::Visible);
        let line: Vec<u8> = std::iter::repeat_n(b'x', 200)
            .chain([b'\r', b'\n'])
            .collect();
        for _ in 0..20_000 {
            let _ = backend.feed(&line);
        }
        let visible = footprint_mb();
        // Hidden으로 전이 → 트리밍
        backend.set_cache_class(TerminalCacheClass::Hidden);
        // alacritty가 shrink를 반영하도록 소량 추가 feed(트림은 set_options 시점에 적용).
        let _ = backend.feed(&line);
        let hidden = footprint_mb();
        let fp = backend.cache_footprint();
        eprintln!(
            "RSS-PROBE base={:.1}MB  visible(20k줄 입력)={:.1}MB (Δ{:.1}MB)  hidden트림후={:.1}MB (Δ{:.1}MB)  history_lines={} limit={}",
            base,
            visible,
            visible - base,
            hidden,
            hidden - base,
            fp.history_lines,
            fp.scrollback_limit_lines,
        );
    }

    /// 옵션 D 스크롤백 압축의 실제 RSS 절감 실측(수동). 현실적 로그류 2만 줄을 먹인 뒤:
    /// (1) 결정론적 압축비 = 원시 셀배열 추정 바이트 ÷ 압축 곁가지 힙 바이트,
    /// (2) phys_footprint(압축 상태) vs inflate_all(전부 복원) 델타 = 실제 RSS 차이.
    /// 실행: `cargo test -p terminal compression_rss_probe -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn compression_rss_probe() {
        fn footprint_mb() -> f64 {
            let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::uninit();
            let rc = unsafe {
                libc::proc_pid_rusage(
                    std::process::id() as libc::c_int,
                    libc::RUSAGE_INFO_V4,
                    info.as_mut_ptr().cast(),
                )
            };
            if rc != 0 {
                return 0.0;
            }
            unsafe { info.assume_init() }.ri_phys_footprint as f64 / (1024.0 * 1024.0)
        }

        let base = footprint_mb();
        let mut backend = AlacrittyBackend::new(200, 40, 10_000);
        // 줄마다 다른 내용 + 가끔 SGR 색 — 합성 반복 문자보다 현실적인 압축률을 본다.
        for i in 0..20_000u32 {
            let line = format!(
                "\x1b[32m2026-07-24T12:00:{:02}\x1b[0m INFO worker[{}] req={} status=200 \
                 path=/api/v1/items/{} latency={}ms bytes={}\r\n",
                i % 60,
                i % 8,
                i,
                (i * 7) % 100_000,
                i % 500,
                (i * 131) % 65_536,
            );
            let _ = backend.feed(line.as_bytes());
        }

        // Visible 상태(feed 트리거: HOT 창 비압축).
        let visible_fp = footprint_mb();
        let history = backend.term.history_size();
        let cols = backend.term.columns();
        let visible_heap = backend.term.grid().compressed_heap_bytes();

        // Hidden 전환: HOT 창까지 전체 압축(안 보이는 세션의 최대 RSS 회수).
        backend.set_cache_class(crate::backend::TerminalCacheClass::Hidden);
        let hidden_fp = footprint_mb();
        let hidden_hist = backend.term.history_size();
        let hidden_heap = backend.term.grid().compressed_heap_bytes();
        let hidden_raw_estimate = hidden_hist * estimated_bytes_per_line(cols);

        // 비압축 기준선: 전부 복원.
        backend.term.grid_mut().inflate_all();
        let inflated_fp = footprint_mb();

        // 신뢰 지표는 결정론적 live-heap 회계(곁가지 vs 원시추정)다. phys_footprint는
        // 프로세스 전역이라 테스트의 format! churn·할당자 free-list 잔류에 오염되므로
        // 참고용으로만 본다(해제된 셀 배열을 macOS malloc이 OS에 즉시 반환하지 않는다).
        eprintln!(
            "COMPRESS-RSS (결정론적 live-heap)\n\
             visible: history={}줄, HOT 밖 압축 곁가지={:.2}MB\n\
             hidden : history={}줄 전체압축, 곁가지={:.3}MB vs 원시추정={:.2}MB → 압축비={:.1}x\n\
             [참고] phys_footprint base={:.1} visible={:.1} hidden={:.1} inflated={:.1}MB (노이즈 큼)",
            history,
            visible_heap as f64 / 1e6,
            hidden_hist,
            hidden_heap as f64 / 1e6,
            hidden_raw_estimate as f64 / 1e6,
            hidden_raw_estimate as f64 / hidden_heap.max(1) as f64,
            base,
            visible_fp,
            hidden_fp,
            inflated_fp,
        );
    }

    fn feed(backend: &mut AlacrittyBackend, bytes: &[u8]) -> TerminalChangeSet {
        backend.feed(bytes).unwrap()
    }

    #[test]
    fn native_zero_width_format_extras_remain_valid_sparse_text() {
        for text in [
            "a\u{200b}",
            "a\u{202e}",
            "a\u{200e}",
            "a\u{200d}",
            "a\u{034f}",
            "가ᇹ",
            "a\u{301}\u{308}",
        ] {
            let mut backend = AlacrittyBackend::new(20, 3, 10);
            feed(&mut backend, text.as_bytes());
            let snapshot = backend.viewport_snapshot().unwrap();
            assert_eq!(snapshot.cell_grapheme(0), Some(text));
            assert!(
                crate::validate_cell_graphemes(&snapshot.visible_cells, &snapshot.graphemes)
                    .is_ok(),
                "{text:?}"
            );
            assert_eq!(crate::renderer_egui::selection_text(&snapshot, 0, 19), text);
        }
    }

    #[test]
    fn ascii_snapshots_share_empty_grapheme_storage() {
        let backend = AlacrittyBackend::new(20, 3, 10);
        let first = backend.viewport_snapshot().unwrap();
        let second = backend.viewport_snapshot().unwrap();
        assert!(std::sync::Arc::ptr_eq(&first.graphemes, &second.graphemes));
    }

    #[test]
    fn grapheme_snapshot_search_and_screen_text_preserve_clusters() {
        for text in ["가ᇹ", "a\u{0301}\u{0308}"] {
            let mut backend = AlacrittyBackend::new(20, 3, 10);
            feed(&mut backend, text.as_bytes());
            let snapshot = backend.viewport_snapshot().unwrap();
            assert_eq!(snapshot.cell_grapheme(0), Some(text));
            assert!(backend.screen_text().starts_with(text));
            let found = backend.search_scrollback(text, 10);
            assert_eq!(found.matches.len(), 1, "{text}");
            assert_eq!(found.matches[0].col_start, 0);
        }
        let mut ascii = AlacrittyBackend::new(20, 3, 10);
        feed(&mut ascii, b"ASCII");
        assert!(ascii.viewport_snapshot().unwrap().graphemes.is_empty());
        let mut hidden = AlacrittyBackend::new(20, 3, 10);
        feed(&mut hidden, "\x1b[8ma\u{0301}\u{0308}".as_bytes());
        assert!(hidden.viewport_snapshot().unwrap().graphemes.is_empty());
    }

    #[test]
    fn grapheme_selection_preserves_non_composable_scalars() {
        for text in ["가ᇹ", "a\u{0301}\u{0308}", "한", "ASCII"] {
            let mut backend = AlacrittyBackend::new(20, 3, 10);
            feed(&mut backend, text.as_bytes());
            let snapshot = backend.viewport_snapshot().unwrap();
            assert_eq!(crate::renderer_egui::selection_text(&snapshot, 0, 19), text);
        }
    }

    #[test]
    fn snapshot_metadata_and_cursor_only_reuse_all_row_payloads() {
        let mut backend = AlacrittyBackend::new(8, 3, 20);
        backend.feed("가ᇹ\r\nabc".as_bytes()).unwrap();
        let first = backend.viewport_snapshot().unwrap();
        let unchanged = backend.viewport_snapshot().unwrap();
        assert!(
            std::ptr::eq(&first.visible_cells[0], &unchanged.visible_cells[0]),
            "metadata-only row payload must be shared"
        );
        assert!(std::sync::Arc::ptr_eq(
            &first.graphemes,
            &unchanged.graphemes
        ));
        backend.feed(b"\x1b[3;8H").unwrap();
        let cursor_only = backend.viewport_snapshot().unwrap();
        for row in 0..3 {
            assert!(
                std::ptr::eq(
                    &first.visible_cells[row * 8],
                    &cursor_only.visible_cells[row * 8]
                ),
                "cursor damage must not replace unchanged row {row}"
            );
        }
        assert_eq!(cursor_only.cursor.row, 2);
        assert_eq!(cursor_only.cursor.col, 7);
    }

    #[test]
    fn snapshot_dirty1_keeps_other_rows_and_sparse_sidecar_shared() {
        let mut backend = AlacrittyBackend::new(8, 3, 20);
        backend.feed("가ᇹ\r\nabc".as_bytes()).unwrap();
        let first = backend.viewport_snapshot().unwrap();
        backend.feed(b"\x1b[2;1HX").unwrap();
        let next = backend.viewport_snapshot().unwrap();
        for row in [0, 2] {
            assert!(
                std::ptr::eq(&first.visible_cells[row * 8], &next.visible_cells[row * 8]),
                "unchanged row {row} must keep its allocation"
            );
        }
        assert!(!std::ptr::eq(
            &first.visible_cells[8],
            &next.visible_cells[8]
        ));
        assert_eq!(
            first.visible_cells[8].c, 'a',
            "old snapshots remain immutable"
        );
        assert_eq!(next.visible_cells[8].c, 'X');
        assert!(
            std::sync::Arc::ptr_eq(&first.graphemes, &next.graphemes),
            "ASCII dirtiness must not clone sparse strings"
        );
    }

    #[test]
    fn snapshot_resident_cache_is_budgeted_and_hidden_reads_release_it() {
        let mut backend = AlacrittyBackend::new(8, 3, 20);
        let base = backend.cache_footprint().estimated_bytes;
        let visible = backend.viewport_snapshot().unwrap();
        assert!(
            backend.cache_footprint().estimated_bytes
                >= base + 24 * std::mem::size_of::<TerminalCell>(),
            "owned snapshot cache must be in the backend budget"
        );
        backend.set_cache_class(TerminalCacheClass::Hidden);
        let hidden_base = backend.cache_footprint().estimated_bytes;
        assert_eq!(hidden_base, base);
        let hidden = backend.viewport_snapshot().unwrap();
        assert_eq!(
            backend.cache_footprint().estimated_bytes,
            hidden_base,
            "explicit hidden reads must not leave a resident cache"
        );
        assert_eq!(visible.visible_cells, hidden.visible_cells);
        backend.set_cache_class(TerminalCacheClass::Visible);
        let restored = backend.viewport_snapshot().unwrap();
        assert_eq!(restored.visible_cells, visible.visible_cells);
        assert!(backend.cache_footprint().estimated_bytes > base);
        backend.set_cache_class(TerminalCacheClass::Exited);
        let exited_base = backend.cache_footprint().estimated_bytes;
        backend.viewport_snapshot().unwrap();
        assert_eq!(backend.cache_footprint().estimated_bytes, exited_base);
    }

    #[test]
    fn snapshot_shared_rows_match_uncached_full_oracle_across_resyncs() {
        fn check(backend: &AlacrittyBackend) {
            let actual = backend.viewport_snapshot().unwrap();
            let full = backend.viewport_snapshot_uncached().unwrap();
            assert_eq!(actual, full);
        }
        let mut backend = AlacrittyBackend::new(10, 4, 30);
        check(&backend);
        for output in [
            "abc한가ᇹa\u{0301}\u{0308}\r\nlast",
            "\x1b[2;2H\x1b[31;4;3mX\x1b[0m",
            "\x1b]4;1;#00FF00\x07",
            "\x1b[?1049hALT\r\n가ᇹ",
            "\x1b[?1049l",
            "\x1b[1;1Ha\u{0300}\u{0308}",
        ] {
            backend.feed(output.as_bytes()).unwrap();
            check(&backend);
        }
        backend.feed("old line\r\n".repeat(100).as_bytes()).unwrap();
        check(&backend);
        backend.scroll(9);
        check(&backend);
        backend.scroll(3);
        check(&backend);
        backend.scroll_to_bottom();
        check(&backend);
        backend.resize(12, 5).unwrap();
        check(&backend);
        backend.resize(4, 2).unwrap();
        check(&backend);
        backend.reset();
        check(&backend);
    }

    #[test]
    fn snapshot_flat_materialization_is_accounted_without_scanning_rows() {
        let backend = AlacrittyBackend::new(8, 3, 20);
        let snapshot = backend.viewport_snapshot().unwrap();
        let before = backend.cache_footprint().estimated_bytes;
        assert_eq!(snapshot.visible_cells[..].len(), 24);
        assert_eq!(
            backend.cache_footprint().estimated_bytes - before,
            24 * std::mem::size_of::<TerminalCell>() + 2 * std::mem::size_of::<usize>()
        );
        let after = backend.cache_footprint().estimated_bytes;
        for _ in 0..5 {
            assert_eq!(backend.cache_footprint().estimated_bytes, after);
        }
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
            .filter(|c| !c.wide_spacer())
            .map(|c| c.c)
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    #[test]
    fn live_scrollback_직렬화는_출력상한_직전부터_할당을_제한한다() {
        let mut backend = AlacrittyBackend::new(20, 5, 100);
        backend.feed(b"\x1b[31mhello\r\nworld").unwrap();
        let expected = backend.serialize_scrollback().unwrap();
        assert_eq!(
            serialize_scrollback_bounded(&backend, expected.len()).unwrap(),
            expected
        );
        assert_eq!(
            serialize_scrollback_bounded(&backend, expected.len() - 1),
            Err(crate::ScrollbackSerializeError::LimitExceeded)
        );
        assert_eq!(
            serialize_scrollback_bounded(&backend, 0),
            Err(crate::ScrollbackSerializeError::LimitExceeded)
        );
    }

    #[test]
    fn live_scrollback_사용자_십만줄은_고정_바이트상한으로_줄이지_않는다() {
        for class in [TerminalCacheClass::Visible, TerminalCacheClass::Hidden] {
            assert_eq!(
                effective_scrollback_limit(100_000, 500, 100, class),
                100_000
            );
        }
    }

    #[test]
    fn hidden_visible_scrollback_cap() {
        let mut b = AlacrittyBackend::new(20, 5, 5000);
        for i in 0..2000 {
            feed(&mut b, format!("line{i}\r\n").as_bytes());
        }
        let before = b.serialize_scrollback().unwrap();
        let history = b.cache_footprint().history_lines;
        assert!(history > 1000);
        b.set_visible(false);
        assert_eq!(b.cache_footprint().history_lines, history);
        assert_eq!(b.serialize_scrollback().unwrap(), before);
        b.set_visible(true);
        assert_eq!(b.serialize_scrollback().unwrap(), before);
    }

    #[test]
    fn live_scrollback_축소후_증가는_삭제한_기록을_복원하지_않는다() {
        let mut b = AlacrittyBackend::new(20, 5, 5000);
        for i in 0..2000 {
            feed(&mut b, format!("line{i}\r\n").as_bytes());
        }
        let before = b.cache_footprint().history_lines;
        assert_eq!(
            b.set_scrollback_limit(100),
            crate::ScrollbackApplyResult::Applied {
                requested: 100,
                effective: 100,
                trimmed: before - 100,
            }
        );
        let shrunk = b.serialize_scrollback().unwrap();
        assert_eq!(
            b.set_scrollback_limit(5000),
            crate::ScrollbackApplyResult::Applied {
                requested: 5000,
                effective: 5000,
                trimmed: 0,
            }
        );
        assert_eq!(b.cache_footprint().history_lines, 100);
        assert_eq!(b.serialize_scrollback().unwrap(), shrunk);
        for i in 0..200 {
            feed(&mut b, format!("new{i}\r\n").as_bytes());
        }
        assert!(b.cache_footprint().history_lines > 100);
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
    fn hidden_byte_budget은_가시성만으로_이력을_삭제하지_않는다() {
        let mut b = AlacrittyBackend::new(240, 5, 10_000);
        for i in 0..2500 {
            feed(&mut b, format!("line-{i}\r\n").as_bytes());
        }
        let before = b.cache_footprint();
        assert!(before.history_lines > 1000);
        assert!(b.set_cache_class(TerminalCacheClass::Hidden).is_none());
        let after = b.cache_footprint();
        assert_eq!(after.history_lines, before.history_lines);
        assert_eq!(after.scrollback_limit_lines, before.scrollback_limit_lines);
        assert!(after.estimated_bytes <= before.estimated_bytes);
    }

    // ── 압축 인지 스크롤백 예산 ──────────────────────────────────────────────

    /// 대표 열 수/클래스에서 압축 인지 예산이 이전(전량-비압축) 캡보다 더 많은
    /// 히스토리 줄을 담는다. 절대 상한(max_scrollback_lines)은 넘지 않는다.
    #[test]
    fn 압축_인지_예산은_비압축_추정보다_많은_줄을_담는다() {
        let rows = 24usize;
        for (class, cols) in [
            (TerminalCacheClass::Visible, 80usize),
            (TerminalCacheClass::Visible, 200usize),
            (TerminalCacheClass::Hidden, 200usize),
        ] {
            let budget = TerminalCacheBudget::for_class(class);
            let bpl = estimated_bytes_per_line(cols);
            // 이전 공식: (max_bytes / bpl - rows), 그 뒤 line cap으로 클램프.
            let old_cap = (budget.max_bytes / bpl)
                .saturating_sub(rows)
                .min(budget.max_scrollback_lines);
            let new_cap = effective_scrollback_limit(usize::MAX, cols, rows, class);
            assert!(
                new_cap >= old_cap,
                "{class:?} cols={cols}: 새 캡 {new_cap} 이 이전 {old_cap} 보다 작으면 안 됨",
            );
            assert!(
                new_cap <= budget.max_scrollback_lines,
                "{class:?} cols={cols}: 새 캡 {new_cap} 이 라인 상한 {} 를 넘음",
                budget.max_scrollback_lines,
            );
        }
    }

    /// 안전성: 역산한 캡을 최악(비압축에 가까운, 보수적 divisor 압축만 되는) 콘텐츠로
    /// 가득 채워도 그 추정 바이트가 예산을 넘지 않는다 — 예산 초과 불가 증명.
    #[test]
    fn 최악_압축_추정은_예산_이내() {
        let rows = 24usize;
        for (class, cols) in [
            (TerminalCacheClass::Visible, 80usize),
            (TerminalCacheClass::Visible, 200usize),
            (TerminalCacheClass::Hidden, 200usize),
            (TerminalCacheClass::Exited, 200usize),
        ] {
            let budget = TerminalCacheBudget::for_class(class);
            let cap = effective_scrollback_limit(usize::MAX, cols, rows, class);
            let bpl = estimated_bytes_per_line(cols);
            // footprint 회계와 동일 구조: 화면+HOT은 전액, 나머지는 ceil(bpl/divisor).
            let hot = cap.min(HOT_SCROLLBACK_LINES);
            let cold = cap.saturating_sub(HOT_SCROLLBACK_LINES);
            let compressed_bpl = bpl.div_ceil(SCROLLBACK_COMPRESSION_DIVISOR).max(1);
            let worst = (rows + hot) * bpl + cold * compressed_bpl;
            assert!(
                worst <= budget.max_bytes,
                "{class:?} cols={cols}: 최악추정 {worst} > 예산 {}",
                budget.max_bytes,
            );
        }
    }

    /// HOT 창을 넘게 feed하면 cache_footprint가 실제 압축을 반영해 전량-비압축
    /// 추정보다 작게(그러나 비압축 baseline 이상으로) 보고한다.
    #[test]
    fn cache_footprint는_압축을_반영한다() {
        let cols = 200usize;
        let rows = 40usize;
        let mut b = AlacrittyBackend::new(cols as u16, rows as u16, 10_000);
        // HOT(256)을 크게 넘기는 현실적 로그류(줄마다 내용 + 가끔 SGR 색).
        for i in 0..4_000u32 {
            let line = format!(
                "\x1b[32m2026-07-24T12:00:{:02}\x1b[0m INFO worker[{}] req={} status=200 \
                 path=/api/v1/items/{} latency={}ms\r\n",
                i % 60,
                i % 8,
                i,
                (i * 7) % 100_000,
                i % 500,
            );
            feed(&mut b, line.as_bytes());
        }
        let fp = b.cache_footprint();
        assert!(
            fp.history_lines > HOT_SCROLLBACK_LINES,
            "압축 대상 히스토리가 부족: {}",
            fp.history_lines,
        );
        assert!(
            b.term.grid().compressed_heap_bytes() > 0,
            "HOT 밖 히스토리가 압축되지 않았다",
        );
        // 전량-비압축 추정보다 작아야 압축이 반영된 것.
        let all_uncompressed = (rows + fp.history_lines) * estimated_bytes_per_line(cols);
        assert!(
            fp.estimated_bytes < all_uncompressed,
            "footprint {} 이 비압축 추정 {} 보다 작아야 함",
            fp.estimated_bytes,
            all_uncompressed,
        );
        // 비압축 baseline(화면 + HOT 히스토리) 이상은 되어야 한다(과소보고 아님).
        let baseline =
            (rows + fp.history_lines.min(HOT_SCROLLBACK_LINES)) * estimated_bytes_per_line(cols);
        assert!(
            fp.estimated_bytes >= baseline,
            "footprint {} 이 baseline {} 보다 작으면 과소보고",
            fp.estimated_bytes,
            baseline,
        );
    }

    #[test]
    fn agent_redraw_다음입력후_이전출력을_보존한다() {
        for redraw in [b"\x1b[H\x1b[2J\x1b[3J".as_slice(), b"\x1b[3J\x1b[H\x1b[2J"] {
            let mut backend = AlacrittyBackend::new(40, 4, 100);
            feed(
                &mut backend,
                "이전 답변\r\n출력 2\r\n출력 3\r\n출력 4\r\n입력 대기".as_bytes(),
            );
            assert_eq!(backend.search_scrollback("이전 답변", 10).matches.len(), 1);

            // 다음 입력 뒤 CLI가 화면과 스크롤백을 지우고 다시 그리는 상황이다.
            // PTY 읽기 경계에서 제어 문자가 나뉘어 도착해도 같은 동작이어야 한다.
            for byte in redraw {
                feed(&mut backend, std::slice::from_ref(byte));
            }
            feed(&mut backend, "다음 작업 실행 중".as_bytes());

            assert_eq!(row_text(&backend, 0), "다음 작업 실행 중");
            assert_eq!(
                backend.search_scrollback("이전 답변", 10).matches.len(),
                1,
                "다음 입력 후 화면을 다시 그리면서 이전 답변까지 삭제됐다"
            );
            assert_eq!(backend.search_scrollback("입력 대기", 10).matches.len(), 1);
        }
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
        assert!(first.wide());
        assert!(cell_at(&backend, 0, 1).wide_spacer());
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
    fn osc_색상과_커서_질의_응답은_요청_순서를_지킨다() {
        let mut backend = AlacrittyBackend::new(80, 24, 100);
        // termenv(gh가 터미널 테마 감지에 사용)는 OSC 11 다음 DSR을 보내고,
        // 같은 순서로 응답을 읽는다. 순서가 뒤집히면 OSC 응답이 셸 입력에 남는다.
        let changes = feed(&mut backend, b"\x1b]11;?\x1b\\\x1b[6n");
        assert!(
            changes.pty_responses.starts_with(b"\x1b]11;rgb:"),
            "OSC 11보다 다른 응답이 먼저 나옴: {:?}",
            String::from_utf8_lossy(&changes.pty_responses)
        );
        let osc_end = changes
            .pty_responses
            .windows(2)
            .position(|window| window == b"\x1b\\")
            .expect("OSC response terminator");
        assert!(
            changes.pty_responses[osc_end + 2..].starts_with(b"\x1b["),
            "OSC 11 뒤에 DSR 응답이 없음: {:?}",
            String::from_utf8_lossy(&changes.pty_responses)
        );
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
    fn application_cursor_mode_is_read_from_live_backend() {
        let mut backend = AlacrittyBackend::new(80, 24, 100);
        assert!(!backend.application_cursor());
        feed(&mut backend, b"\x1b[?1h");
        assert!(backend.application_cursor());
        feed(&mut backend, b"\x1b[?1l");
        assert!(!backend.application_cursor());
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
        assert!(cell_at(&b, 4, 0).wide());
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

    /// 셸 통합 2단계: 마지막 출력 추출은 grid 최하단이 아니라 **커서** 기준이다 —
    /// 화면이 아직 안 찬 프레시 셸의 첫 명령에서도 정확해야 한다.
    #[test]
    fn logical_line_back은_커서_기준으로_위_라인을_읽는다() {
        let mut b = AlacrittyBackend::new(10, 5, 100);
        feed(&mut b, b"one\r\ntwo\r\nthree");
        assert_eq!(b.logical_line_back_from_cursor(0), Some("three".to_owned()));
        assert_eq!(b.logical_line_back_from_cursor(2), Some("one".to_owned()));
        // history가 없으니 커서 위 3번째 라인은 범위 밖 — None.
        assert_eq!(b.logical_line_back_from_cursor(3), None);
    }

    /// soft wrap(WRAPLINE) 행들은 한 논리 라인으로 병합된다 — LF 카운터 좌표와 일치.
    #[test]
    fn logical_line_back은_wrap_행을_병합한다() {
        let mut b = AlacrittyBackend::new(4, 5, 100);
        feed(&mut b, b"abcdef\r\ng");
        // "abcdef"는 4열에서 "abcd"/"ef" 두 시각 행이지만 논리 라인은 하나.
        assert_eq!(b.logical_line_back_from_cursor(0), Some("g".to_owned()));
        assert_eq!(
            b.logical_line_back_from_cursor(1),
            Some("abcdef".to_owned())
        );
        assert_eq!(b.logical_line_back_from_cursor(2), None);
    }

    /// zsh PROMPT_SP 형태: 개행 없는 출력 뒤 EOL 마커+공백이 autowrap을 만들고 다음
    /// 시각 행에 프롬프트가 그려진다 — LF가 없으므로 전부 한 논리 라인이어야 커서
    /// 기준 back 좌표가 어긋나지 않는다.
    #[test]
    fn logical_line_back은_prompt_sp_wrap도_한_라인으로_본다() {
        let mut b = AlacrittyBackend::new(10, 5, 100);
        // "foo" + "%" + 공백 6개(autowrap 유발) → 다음 행 "$ " — LF 없음.
        feed(&mut b, b"foo%      $ ");
        let line = b.logical_line_back_from_cursor(0).unwrap();
        assert!(line.starts_with("foo%"), "{line:?}");
        assert!(line.ends_with('$'), "{line:?}");
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
        assert!(cells[0].attrs().contains(CellAttrs::BOLD), "bold 미반영");
        assert_eq!(cells[1].c, 'I');
        assert!(
            cells[1].attrs().contains(CellAttrs::ITALIC),
            "italic 미반영"
        );
        assert_eq!(cells[2].c, 'U');
        assert!(
            cells[2].attrs().contains(CellAttrs::UNDERLINE),
            "underline 미반영"
        );
        assert_eq!(cells[3].c, 'S');
        assert!(
            cells[3].attrs().contains(CellAttrs::STRIKEOUT),
            "strikeout 미반영"
        );
        assert_eq!(cells[4].c, 'D');
        assert!(cells[4].attrs().contains(CellAttrs::DIM), "dim 미반영");
        // 속성 없는 셀은 비어 있다
        assert!(snap.visible_cells[10].attrs().is_empty());
    }

    // ── deppy-sijo 옵션 D: 스크롤백 라인 압축 통합 ────────────────────────────

    /// HOT_SCROLLBACK_LINES(256)를 넘는 히스토리를 만들어 압축을 실제로 유발한다.
    fn backend_with_compressed_history() -> AlacrittyBackend {
        let mut a = AlacrittyBackend::new(40, 5, 1000);
        for i in 0..400 {
            feed(&mut a, format!("line{i}\r\n").as_bytes());
        }
        assert!(
            a.term.grid().compressed_heap_bytes() > 0,
            "HOT 밖 히스토리가 압축되지 않았다"
        );
        a
    }

    #[test]
    fn streaming_resize_뒤_검색_직렬화_출력이_압축_상태에서_동작한다() {
        let mut a = backend_with_compressed_history();
        a.resize(20, 8).unwrap();
        assert_eq!(
            a.term.grid().compressed_row_count(),
            a.term.grid().history_size()
        );
        assert!(!a.search_scrollback("line30", 1000).matches.is_empty());
        a.scroll(30);
        let compressed_snapshot = a.viewport_snapshot().unwrap();
        let compressed_archive = a.serialize_scrollback().unwrap();
        a.term.grid_mut().inflate_all();
        assert_eq!(compressed_archive, a.serialize_scrollback().unwrap());
        assert_eq!(
            compressed_snapshot.visible_cells,
            a.viewport_snapshot().unwrap().visible_cells
        );
        feed(&mut a, "리사이즈 뒤 새 출력\r\n".as_bytes());
        a.resize(60, 4).unwrap();
        assert!(!a.search_scrollback("새 출력", 1000).matches.is_empty());
    }

    /// 압축된 히스토리를 직렬화한 결과가, 전부 복원(inflate)한 뒤 직렬화한 것과 동일.
    #[test]
    fn deppy_압축_serialize는_inflate와_동일() {
        let mut a = backend_with_compressed_history();
        let compressed = a.serialize_scrollback().unwrap();

        a.term.grid_mut().inflate_all();
        assert_eq!(a.term.grid().compressed_heap_bytes(), 0);
        let inflated = a.serialize_scrollback().unwrap();

        assert_eq!(compressed, inflated, "압축/비압축 직렬화 불일치");
    }

    /// 압축 영역을 가로지르는 검색 결과가 복원본과 동일.
    #[test]
    fn deppy_압축_search는_inflate와_동일() {
        let mut a = backend_with_compressed_history();
        // "line3"은 line3, line30~39, line300~399 등 압축·비압축 영역에 두루 매치.
        let compressed = a.search_scrollback("line3", 10_000);
        a.term.grid_mut().inflate_all();
        let inflated = a.search_scrollback("line3", 10_000);

        assert_eq!(compressed, inflated, "압축/비압축 검색 결과 불일치");
        assert!(!compressed.matches.is_empty(), "매치가 하나도 없다");
    }

    /// 압축 영역까지 깊이 스크롤한 뷰포트 렌더가 복원본과 동일(픽셀=셀 단위).
    #[test]
    fn deppy_압축영역_스크롤_렌더가_inflate와_동일() {
        let mut a = backend_with_compressed_history();
        a.scroll(350); // display_offset > HOT(256) → 뷰포트가 압축 영역에 들어감
        let compressed_snap = a.viewport_snapshot().unwrap();
        assert!(
            (0..5).any(|r| row_text(&a, r).starts_with("line")),
            "압축 영역 히스토리 텍스트가 렌더되지 않았다"
        );

        a.term.grid_mut().inflate_all();
        let inflated_snap = a.viewport_snapshot().unwrap();
        assert_eq!(
            compressed_snap.visible_cells, inflated_snap.visible_cells,
            "압축/비압축 뷰포트 셀 불일치"
        );
    }

    /// (PR-2) trim_scrollback은 스크롤백을 클래스 예산 아래로 줄이고 메모리를 회수하며,
    /// 이미 그 이하면 no-op이다.
    #[test]
    fn deppy_trim_scrollback는_스크롤백을_줄이고_회수한다() {
        let mut a = AlacrittyBackend::new(200, 40, 10_000);
        for i in 0..2000 {
            feed(&mut a, format!("line{i}\r\n").as_bytes());
        }
        let before = a.cache_footprint();
        assert!(before.history_lines > 1000, "사전 조건: 충분한 히스토리");

        // 200줄로 강제 트림.
        let event = a.trim_scrollback(200);
        assert!(event.is_some(), "트림 이벤트가 나와야 함");
        let after = a.cache_footprint();
        assert!(
            after.history_lines <= 200 + a.term.screen_lines(),
            "히스토리가 안 줄음: {}",
            after.history_lines
        );
        assert!(
            after.estimated_bytes < before.estimated_bytes,
            "메모리 회수 안 됨: {} -> {}",
            before.estimated_bytes,
            after.estimated_bytes
        );

        // 이미 그 이하: 재트림·상향은 no-op.
        assert!(a.trim_scrollback(200).is_none(), "재트림이 이벤트를 냄");
        assert!(
            a.trim_scrollback(10_000).is_none(),
            "상향 요청은 no-op이어야"
        );
    }

    /// (A-H1) 트림 상한은 클래스 재적용(resize/전이)에 영속하고, clear_pressure_trim으로
    /// 회복된다. 이게 없으면 재적용이 트림을 되돌려 flip-flop이 난다.
    #[test]
    fn deppy_트림은_클래스_재적용에_영속하고_clear로_회복된다() {
        let mut a = AlacrittyBackend::new(200, 40, 10_000);
        for i in 0..2000 {
            feed(&mut a, format!("line{i}\r\n").as_bytes());
        }
        a.trim_scrollback(200);
        let floor = 200 + a.term.screen_lines();

        // 같은 클래스 재적용(resize/전이가 하는 것) — 트림을 되돌리면 안 된다.
        a.set_cache_class(TerminalCacheClass::Visible);
        for i in 2000..2600 {
            feed(&mut a, format!("line{i}\r\n").as_bytes());
        }
        assert!(
            a.cache_footprint().history_lines <= floor,
            "클래스 재적용이 트림을 되돌림: {}",
            a.cache_footprint().history_lines
        );

        // clear_pressure_trim → 상한 회복 → 재feed로 200 넘게 자란다.
        assert!(a.clear_pressure_trim(), "트림 상태인데 clear가 false");
        for i in 2600..4000 {
            feed(&mut a, format!("line{i}\r\n").as_bytes());
        }
        assert!(
            a.cache_footprint().history_lines > floor,
            "clear 후에도 스크롤백이 회복 안 됨: {}",
            a.cache_footprint().history_lines
        );
        assert!(!a.clear_pressure_trim(), "이미 회복인데 clear가 true");
    }

    /// (B-M2) 배경 feed를 계속 받는 hidden 세션은 feed의 compress_history(HOT)로 최근
    /// HOT 창을 raw로 남긴다 — footprint가 이를 0으로 과소계상하지 않고 실측으로 계상한다.
    #[test]
    fn deppy_활성_hidden_footprint는_hot_raw창을_계상한다() {
        let mut a = AlacrittyBackend::new(40, 5, 1000);
        for i in 0..400 {
            feed(&mut a, format!("line{i}\r\n").as_bytes());
        }
        a.set_cache_class(TerminalCacheClass::Hidden); // 전체 압축(compress_history(0))
        // 배경 feed 지속 — feed의 compress_history(HOT)가 최근 HOT행을 raw로 남긴다.
        for i in 400..700 {
            feed(&mut a, format!("line{i}\r\n").as_bytes());
        }

        let history = a.term.history_size();
        let raw_history = history.saturating_sub(a.term.grid().compressed_row_count());
        // 활성 hidden은 HOT 창만큼(±) raw 행을 갖는다 — 0이 아니다(예전 Hidden=0 과소계상).
        assert!(
            raw_history > 0,
            "활성 hidden이 raw HOT 창을 안 남김: raw={raw_history}"
        );
        assert!(
            raw_history <= HOT_SCROLLBACK_LINES + 5,
            "raw가 HOT 창보다 과함: {raw_history}"
        );

        // footprint가 그 raw 창을 계상한다(0으로 과소계상하던 예전과 대비).
        let fp = a.cache_footprint();
        let bpl = estimated_bytes_per_line(a.term.columns());
        let floor =
            a.term.screen_lines() * bpl + a.term.grid().compressed_heap_bytes() + raw_history * bpl;
        assert!(
            fp.estimated_bytes >= floor,
            "footprint가 raw HOT 창을 미계상: est={} floor={floor}",
            fp.estimated_bytes
        );
    }

    /// Hidden 전환 시 HOT 창까지 포함해 히스토리 전체가 압축되고(RSS 최대 회수),
    /// 읽기는 여전히 비압축과 동일하다.
    #[test]
    fn deppy_hidden_전환은_hot_창까지_압축() {
        use crate::backend::TerminalCacheClass;
        let mut a = backend_with_compressed_history(); // 400줄, Visible(HOT=256 밖만 압축)
        let visible_compressed = a.term.grid().compressed_heap_bytes();
        let visible_dump = a.serialize_scrollback().unwrap();

        a.set_cache_class(TerminalCacheClass::Hidden);
        let hidden_compressed = a.term.grid().compressed_heap_bytes();
        assert!(
            hidden_compressed > visible_compressed,
            "hidden이 HOT 창을 압축하지 않음 (visible={visible_compressed} hidden={hidden_compressed})"
        );

        // Hidden 예산(1000줄)이 400줄을 트림하지 않으므로 내용은 불변.
        let hidden_dump = a.serialize_scrollback().unwrap();
        assert_eq!(
            visible_dump, hidden_dump,
            "hidden 전환이 스크롤백 내용을 바꿈"
        );

        // 전부 복원 후 재직렬화와도 동일(압축/비압축 등가).
        a.term.grid_mut().inflate_all();
        assert_eq!(hidden_dump, a.serialize_scrollback().unwrap());
    }

    /// 압축 세션이 계속 출력을 받아 스크롤백이 가득 차고 가장 오래된 압축 슬롯이
    /// 재활용돼도(패닉 없이) 최근 내용이 온전한지 — end-to-end 재활용 검증.
    #[test]
    fn deppy_압축_가득참_재활용_후_최근줄_온전() {
        // 작은 스크롤백으로 빨리 가득 채운다.
        let mut a = AlacrittyBackend::new(40, 5, 300);
        for i in 0..600 {
            feed(&mut a, format!("row{i}\r\n").as_bytes());
        }
        // 스크롤백 상한(300) 도달 + 재활용 정상상태에서도 압축은 축적된다.
        assert!(a.term.grid().compressed_heap_bytes() > 0);
        // 마지막 "row599\r\n"의 개행으로 맨 아래(행4)는 빈 줄, row599는 행3.
        assert_eq!(row_text(&a, 3), "row599");
        // 직렬화가 패닉 없이 되고 최근 줄을 담는다(재활용이 최근 내용을 안 깨뜨림).
        let dump = String::from_utf8_lossy(&a.serialize_scrollback().unwrap()).into_owned();
        assert!(dump.contains("row599"), "최근 줄이 직렬화에 없음");
    }
}
