//! CellGrid 렌더러 (설계문서 4.3 egui_cell_renderer).
//! egui 0.35 신 시그니처(&mut Ui) 기준 (설계문서 1.1). egui_term(1.7)은 참고만.

use std::sync::Arc;

use crate::viewport_snapshot::{CellRange, CursorShape, TerminalViewportSnapshot};

pub struct RenderOutput {
    pub response: egui::Response,
    /// 셀 하나의 화면 크기 — 호출측이 cols/rows 계산에 쓴다
    pub cell_size: egui::Vec2,
    /// 그리드 좌상단 화면 좌표 — 호출측이 포인터→셀 변환(선택 드래그)에 쓴다
    pub origin: egui::Pos2,
}

/// 세션/pane별 retained row layout cache. UI는 이 캐시를 소유만 하고 backend 타입을
/// 보지 않는다. selection/cursor/IME는 오버레이라 캐시 무효화 대상이 아니다.
#[derive(Default)]
pub struct TerminalRenderCache {
    cols: u16,
    rows: u16,
    scroll_offset: i32,
    is_alt_screen: bool,
    font_size_bits: u32,
    rows_cache: Vec<Option<RowRenderCache>>,
    rebuilt_rows_last_frame: usize,
}

impl TerminalRenderCache {
    pub fn clear(&mut self) {
        self.rows_cache.clear();
        self.cols = 0;
        self.rows = 0;
        self.scroll_offset = 0;
        self.is_alt_screen = false;
        self.font_size_bits = 0;
        self.rebuilt_rows_last_frame = 0;
    }

    pub fn rebuilt_rows_last_frame(&self) -> usize {
        self.rebuilt_rows_last_frame
    }

    fn prepare(&mut self, snapshot: &TerminalViewportSnapshot, font_size: f32) {
        self.rebuilt_rows_last_frame = 0;
        let font_size_bits = font_size.to_bits();
        let shape_changed = self.cols != snapshot.cols
            || self.rows != snapshot.rows
            || self.scroll_offset != snapshot.scroll_offset
            || self.is_alt_screen != snapshot.is_alt_screen
            || self.font_size_bits != font_size_bits
            || self.rows_cache.len() != snapshot.rows as usize;
        if shape_changed {
            self.cols = snapshot.cols;
            self.rows = snapshot.rows;
            self.scroll_offset = snapshot.scroll_offset;
            self.is_alt_screen = snapshot.is_alt_screen;
            self.font_size_bits = font_size_bits;
            self.rows_cache.clear();
            self.rows_cache.resize_with(snapshot.rows as usize, || None);
        }
    }
}

struct RowRenderCache {
    bg_runs: Vec<RowBgRun>,
    text_runs: Vec<RowTextRun>,
}

struct RowBgRun {
    start_col: usize,
    end_col: usize,
    color: egui::Color32,
}

struct RowTextRun {
    col: usize,
    galley: Arc<egui::Galley>,
    color: egui::Color32,
}

/// 주어진 폰트 크기의 셀 크기 (모노스페이스 'M' 폭 × 행 높이).
pub fn cell_size(ctx: &egui::Context, font_size: f32) -> egui::Vec2 {
    let font_id = egui::FontId::monospace(font_size);
    ctx.fonts_mut(|fonts| egui::vec2(fonts.glyph_width(&font_id, 'M'), fonts.row_height(&font_id)))
}

/// snapshot을 그린다. preedit은 IME 조합 중 텍스트 — 커서 위치에 표시한다.
pub fn draw(
    ui: &mut egui::Ui,
    snapshot: &TerminalViewportSnapshot,
    font_size: f32,
    cache: &mut TerminalRenderCache,
    preedit: Option<&str>,
    // 선택 영역 (정규화된 선형 셀 인덱스, inclusive) — 셀 배경을 선택색으로 그린다
    selection: Option<(usize, usize)>,
) -> RenderOutput {
    let font_id = egui::FontId::monospace(font_size);
    let cell = cell_size(ui.ctx(), font_size);
    // hit-test/응답 rect는 pane 영역을 넘지 않게 clamp한다 — split/resize 직후
    // stale(더 큰) snapshot이 이웃 pane의 클릭/스크롤을 가로채는 것 방지 (codex 리뷰).
    // 넘치는 셀은 어차피 호출측 clip_rect로 잘린다.
    let avail = ui.available_size();
    let size = egui::vec2(
        (cell.x * snapshot.cols as f32).min(avail.x.max(0.0)),
        (cell.y * snapshot.rows as f32).min(avail.y.max(0.0)),
    );
    // click_and_drag: 클릭=포커스, 드래그=선택 (2026-07-05 복사 지원)
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    if response.has_focus() {
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(response.id, terminal_focus_lock_filter());
        });
    }
    let painter = ui.painter_at(rect);
    // 좌측 여백 — 텍스트가 pane 경계선에 딱 붙지 않게 살짝 띄운다(사용자 요청). 배경은
    // rect 전체를 채우므로 이 여백은 배경색 간격이 된다. paint/hit-test 모두 이 origin 기준.
    const LEFT_PAD: f32 = 2.0;
    let origin = rect.min + egui::vec2(LEFT_PAD, 0.0);

    let default_bg = egui::Color32::from_rgb(0x18, 0x18, 0x1c);
    let selection = selection.and_then(|(a, b)| normalize_selection_range(snapshot, a, b));
    painter.rect_filled(rect, 0.0, default_bg);

    cache.prepare(snapshot, font_size);
    for row in 0..snapshot.rows as usize {
        let needs_rebuild = row_is_dirty(snapshot, row)
            || cache
                .rows_cache
                .get(row)
                .and_then(|cached| cached.as_ref())
                .is_none();
        if needs_rebuild {
            let row_cache = build_row_cache(&painter, snapshot, row, &font_id, default_bg);
            if let Some(slot) = cache.rows_cache.get_mut(row) {
                *slot = Some(row_cache);
                cache.rebuilt_rows_last_frame += 1;
            }
        }

        if let Some(row_cache) = cache.rows_cache.get(row).and_then(|cached| cached.as_ref()) {
            let row_y = row as f32 * cell.y;
            for bg in &row_cache.bg_runs {
                let pos = origin + egui::vec2(bg.start_col as f32 * cell.x, row_y);
                let width = (bg.end_col - bg.start_col) as f32 * cell.x;
                painter.rect_filled(
                    egui::Rect::from_min_size(pos, egui::vec2(width, cell.y)),
                    0.0,
                    bg.color,
                );
            }
            paint_selection_row(&painter, snapshot, row, origin, cell, selection);
            for run in &row_cache.text_runs {
                let pos = origin + egui::vec2(run.col as f32 * cell.x, row_y);
                painter.galley(pos, Arc::clone(&run.galley), run.color);
            }
        }
    }

    // 커서 (스크롤 중이거나 hidden이면 snapshot.visible이 false)
    if snapshot.cursor.visible {
        let pos = origin
            + egui::vec2(
                snapshot.cursor.col as f32 * cell.x,
                snapshot.cursor.row as f32 * cell.y,
            );
        let cursor_color = egui::Color32::from_rgba_unmultiplied(0xd8, 0xd8, 0xd8, 0xa0);
        let cursor_rect = match snapshot.cursor.shape {
            CursorShape::Block => egui::Rect::from_min_size(pos, cell),
            CursorShape::Underline => egui::Rect::from_min_size(
                pos + egui::vec2(0.0, cell.y - 2.0),
                egui::vec2(cell.x, 2.0),
            ),
            CursorShape::Beam => egui::Rect::from_min_size(pos, egui::vec2(2.0, cell.y)),
        };
        painter.rect_filled(cursor_rect, 0.0, cursor_color);

        // IME는 터미널이 포커스를 가질 때만 — 다른 입력창의 조합/후보창을 뺏지 않는다
        if response.has_focus() {
            // 조합 중 텍스트를 커서 위치에 표시
            if let Some(preedit) = preedit.filter(|p| !p.is_empty()) {
                let galley_rect = painter.text(
                    pos,
                    egui::Align2::LEFT_TOP,
                    preedit,
                    font_id.clone(),
                    egui::Color32::BLACK,
                );
                painter.rect_filled(galley_rect, 0.0, egui::Color32::from_rgb(0xd8, 0xd8, 0xd8));
                painter.text(
                    pos,
                    egui::Align2::LEFT_TOP,
                    preedit,
                    font_id,
                    egui::Color32::BLACK,
                );
                painter.line_segment(
                    [galley_rect.left_bottom(), galley_rect.right_bottom()],
                    egui::Stroke::new(1.5, egui::Color32::BLACK),
                );
            }
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    rect,
                    cursor_rect: egui::Rect::from_min_size(pos, cell),
                    should_interrupt_composition: false,
                });
            });
        }
    }

    RenderOutput {
        response,
        cell_size: cell,
        origin,
    }
}

pub fn terminal_focus_lock_filter() -> egui::EventFilter {
    egui::EventFilter {
        tab: true,
        horizontal_arrows: true,
        vertical_arrows: true,
        escape: true,
    }
}

fn build_row_cache(
    painter: &egui::Painter,
    snapshot: &TerminalViewportSnapshot,
    row: usize,
    font_id: &egui::FontId,
    default_bg: egui::Color32,
) -> RowRenderCache {
    let cols = snapshot.cols as usize;
    let row_start = row * cols;
    let row_end = row_start + cols;
    let Some(cells) = snapshot.visible_cells.get(row_start..row_end) else {
        return RowRenderCache {
            bg_runs: Vec::new(),
            text_runs: Vec::new(),
        };
    };

    let mut bg_runs = Vec::new();
    for (col, term_cell) in cells.iter().enumerate() {
        if term_cell.wide_spacer {
            continue;
        }
        let bg = rgb(term_cell.bg);
        if bg == default_bg {
            continue;
        }
        let width_cols = if term_cell.wide { 2 } else { 1 };
        push_bg_run(&mut bg_runs, col, (col + width_cols).min(cols), bg);
    }

    let mut text_runs = Vec::new();
    let mut pending = PendingTextRun::default();
    for (col, term_cell) in cells.iter().enumerate() {
        if term_cell.wide_spacer || term_cell.c == ' ' {
            pending.flush(&mut text_runs, painter, font_id);
            continue;
        }

        let fg = rgb(term_cell.fg);
        if term_cell.wide {
            pending.flush(&mut text_runs, painter, font_id);
            let text = term_cell.c.to_string();
            text_runs.push(RowTextRun {
                col,
                galley: painter.layout_no_wrap(text, font_id.clone(), fg),
                color: fg,
            });
        } else {
            if pending.needs_flush(col, fg) {
                pending.flush(&mut text_runs, painter, font_id);
            }
            pending.push(col, term_cell.c, fg);
        }
    }
    pending.flush(&mut text_runs, painter, font_id);

    RowRenderCache { bg_runs, text_runs }
}

#[derive(Default)]
struct PendingTextRun {
    start_col: usize,
    next_col: usize,
    color: Option<egui::Color32>,
    text: String,
}

impl PendingTextRun {
    fn needs_flush(&self, col: usize, color: egui::Color32) -> bool {
        self.color.is_some() && (self.color != Some(color) || self.next_col != col)
    }

    fn push(&mut self, col: usize, ch: char, color: egui::Color32) {
        if self.color.is_none() {
            self.start_col = col;
            self.next_col = col;
            self.color = Some(color);
        }
        self.text.push(ch);
        self.next_col = col + 1;
    }

    fn flush(
        &mut self,
        text_runs: &mut Vec<RowTextRun>,
        painter: &egui::Painter,
        font_id: &egui::FontId,
    ) {
        let Some(color) = self.color.take() else {
            return;
        };
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        text_runs.push(RowTextRun {
            col: self.start_col,
            galley: painter.layout_no_wrap(text, font_id.clone(), color),
            color,
        });
    }
}

fn push_bg_run(runs: &mut Vec<RowBgRun>, start_col: usize, end_col: usize, color: egui::Color32) {
    if start_col >= end_col {
        return;
    }
    if let Some(last) = runs.last_mut()
        && last.end_col == start_col
        && last.color == color
    {
        last.end_col = end_col;
        return;
    }
    runs.push(RowBgRun {
        start_col,
        end_col,
        color,
    });
}

fn row_is_dirty(snapshot: &TerminalViewportSnapshot, row: usize) -> bool {
    let cols = snapshot.cols as usize;
    if cols == 0 || row >= snapshot.rows as usize {
        return false;
    }
    let row_start = row * cols;
    let row_end = row_start + cols;
    snapshot
        .dirty_ranges
        .iter()
        .any(|range| range_intersects_row(range, row_start, row_end))
}

fn range_intersects_row(range: &CellRange, row_start: usize, row_end: usize) -> bool {
    range.start < row_end && range.end > row_start && range.start < range.end
}

fn paint_selection_row(
    painter: &egui::Painter,
    snapshot: &TerminalViewportSnapshot,
    row: usize,
    origin: egui::Pos2,
    cell_size: egui::Vec2,
    selection: Option<(usize, usize)>,
) {
    let Some((start, end)) = selection else {
        return;
    };
    let cols = snapshot.cols as usize;
    let row_start = row * cols;
    let row_end = row_start + cols;
    if cols == 0 || end < row_start || start >= row_end {
        return;
    }

    let selection_bg = egui::Color32::from_rgb(0x2d, 0x4f, 0x77);
    for col in 0..cols {
        let index = row_start + col;
        if index < start || index > end {
            continue;
        }
        let Some(term_cell) = snapshot.visible_cells.get(index) else {
            continue;
        };
        if term_cell.wide_spacer {
            continue;
        }
        let width = if term_cell.wide {
            cell_size.x * 2.0
        } else {
            cell_size.x
        };
        let pos = origin + egui::vec2(col as f32 * cell_size.x, row as f32 * cell_size.y);
        painter.rect_filled(
            egui::Rect::from_min_size(pos, egui::vec2(width, cell_size.y)),
            0.0,
            selection_bg,
        );
    }
}

/// 선택 범위(선형 인덱스, inclusive)의 텍스트를 추출한다 — 행마다 trailing 공백
/// 제거 + 개행, wide_spacer는 건너뛴다 (복사용).
pub fn selection_text(snapshot: &TerminalViewportSnapshot, start: usize, end: usize) -> String {
    let cols = snapshot.cols as usize;
    let Some((start, end)) = normalize_selection_range(snapshot, start, end) else {
        return String::new();
    };
    let mut out = String::new();
    let mut line = String::new();
    let mut current_row = start / cols;
    for i in start..=end {
        let row = i / cols;
        if row != current_row {
            out.push_str(line.trim_end());
            out.push('\n');
            line.clear();
            current_row = row;
        }
        let cell = &snapshot.visible_cells[i];
        if !cell.wide_spacer {
            line.push(cell.c);
        }
    }
    out.push_str(line.trim_end());
    out
}

fn normalize_selection_range(
    snapshot: &TerminalViewportSnapshot,
    start: usize,
    end: usize,
) -> Option<(usize, usize)> {
    let cols = snapshot.cols as usize;
    let len = snapshot.visible_cells.len();
    if cols == 0 || len == 0 {
        return None;
    }
    let end = end.min(len.saturating_sub(1));
    if start > end {
        return None;
    }

    let start = normalize_selection_endpoint(snapshot, start)?;
    let end = normalize_selection_endpoint(snapshot, end)?;
    Some(if start <= end {
        (start, end)
    } else {
        (end, start)
    })
}

fn normalize_selection_endpoint(
    snapshot: &TerminalViewportSnapshot,
    index: usize,
) -> Option<usize> {
    if index >= snapshot.visible_cells.len() {
        return None;
    }
    if !snapshot.visible_cells[index].wide_spacer {
        return Some(index);
    }
    Some(owning_wide_cell(snapshot, index).unwrap_or(index))
}

fn owning_wide_cell(snapshot: &TerminalViewportSnapshot, spacer: usize) -> Option<usize> {
    let cols = snapshot.cols as usize;
    let cells = &snapshot.visible_cells;
    if cols == 0 || spacer >= cells.len() || !cells[spacer].wide_spacer {
        return None;
    }

    let col = spacer % cols;
    if col > 0 && cells.get(spacer - 1).is_some_and(|cell| cell.wide) {
        return Some(spacer - 1);
    }
    None
}

fn rgb(c: [u8; 3]) -> egui::Color32 {
    egui::Color32::from_rgb(c[0], c[1], c[2])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AlacrittyBackend;
    use crate::backend::TerminalBackend;
    use crate::viewport_snapshot::{CellRange, CursorShape, CursorSnapshot, TerminalCell};

    fn snap(cols: u16, rows: u16, text: &[&str]) -> TerminalViewportSnapshot {
        let mut cells = Vec::new();
        for r in 0..rows as usize {
            let line: Vec<char> = text.get(r).unwrap_or(&"").chars().collect();
            for c in 0..cols as usize {
                cells.push(TerminalCell {
                    c: *line.get(c).unwrap_or(&' '),
                    fg: [0xd8; 3],
                    bg: [0x18, 0x18, 0x1c],
                    wide: false,
                    wide_spacer: false,
                });
            }
        }
        TerminalViewportSnapshot {
            cols,
            rows,
            cursor: CursorSnapshot {
                col: 0,
                row: 0,
                shape: CursorShape::Block,
                visible: false,
            },
            visible_cells: cells.into(),
            dirty_ranges: Vec::new(),
            title: None,
            scroll_offset: 0,
            is_alt_screen: false,
        }
    }

    fn backend_snap(text: &str) -> TerminalViewportSnapshot {
        let mut backend = AlacrittyBackend::new(80, 4, 100);
        backend.feed(text.as_bytes()).unwrap();
        backend.viewport_snapshot().unwrap()
    }

    fn full_row_selection_text(snapshot: &TerminalViewportSnapshot) -> String {
        selection_text(snapshot, 0, snapshot.cols as usize - 1)
    }

    fn first_wide_spacer(snapshot: &TerminalViewportSnapshot) -> usize {
        snapshot
            .visible_cells
            .iter()
            .position(|cell| cell.wide_spacer)
            .expect("fixture should contain a wide spacer")
    }

    fn draw_for_test(
        cache: &mut TerminalRenderCache,
        snapshot: &TerminalViewportSnapshot,
    ) -> usize {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_min_size(egui::vec2(500.0, 200.0));
            draw(ui, snapshot, 13.0, cache, None, None);
        });
        cache.rebuilt_rows_last_frame()
    }

    #[test]
    fn render_cache는_dirty_row만_재구성한다() {
        let mut cache = TerminalRenderCache::default();
        let first = snap(4, 3, &["aaaa", "bbbb", "cccc"]);
        assert_eq!(draw_for_test(&mut cache, &first), 3);

        let mut second = snap(4, 3, &["aaaa", "bbxb", "cccc"]);
        second.dirty_ranges = vec![CellRange { start: 4, end: 8 }];
        assert_eq!(draw_for_test(&mut cache, &second), 1);

        let mut cursor_only = second.clone();
        cursor_only.cursor.col = 2;
        cursor_only.dirty_ranges.clear();
        assert_eq!(draw_for_test(&mut cache, &cursor_only), 0);
    }

    #[test]
    fn row_dirty는_cell_range_intersection을_사용한다() {
        let mut s = snap(5, 3, &["aaaaa", "bbbbb", "ccccc"]);
        s.dirty_ranges = vec![CellRange { start: 6, end: 7 }];
        assert!(!row_is_dirty(&s, 0));
        assert!(row_is_dirty(&s, 1));
        assert!(!row_is_dirty(&s, 2));
    }

    #[test]
    fn terminal_focus_lock_filter는_tui_navigation_keys를_ui_focus에서_잠근다() {
        let filter = terminal_focus_lock_filter();
        assert!(filter.tab);
        assert!(filter.horizontal_arrows);
        assert!(filter.vertical_arrows);
        assert!(filter.escape);
    }

    #[test]
    fn selection_text_행별_trailing_공백_제거와_개행() {
        let s = snap(8, 3, &["hello", "world ok", "tail"]);
        // 1행 전체 + 2행 전체 (인덱스 0..=15)
        assert_eq!(selection_text(&s, 0, 15), "hello\nworld ok");
        // 행 중간 → 다음 행 중간
        assert_eq!(selection_text(&s, 2, 9), "llo\nwo");
        // 범위 초과는 clamp, start>end는 빈 문자열
        assert_eq!(selection_text(&s, 16, 999), "tail");
        assert_eq!(selection_text(&s, 5, 2), "");
    }

    #[test]
    fn required_fixture_selection_copy_full_rows() {
        let fixtures = [
            "src/main.rs",
            "プロジェクト/設定ファイル.rs",
            "项目/配置文件.rs",
            "專案/設定檔.rs",
            "프로젝트/설정파일.rs",
            "project/🚀-deploy/config.json",
        ];

        for fixture in fixtures {
            let snapshot = backend_snap(fixture);
            assert_eq!(full_row_selection_text(&snapshot), fixture, "{fixture}");
        }

        let ascii = backend_snap("src/main.rs");
        assert_eq!(selection_text(&ascii, 4, 7), "main");
    }

    #[test]
    fn cjk_wide_spacer_selection_endpoints_include_owner() {
        let fixtures = [
            "プロジェクト/設定ファイル.rs",
            "项目/配置文件.rs",
            "專案/設定檔.rs",
            "프로젝트/설정파일.rs",
        ];

        for fixture in fixtures {
            let snapshot = backend_snap(fixture);
            let spacer = first_wide_spacer(&snapshot);
            let owner = owning_wide_cell(&snapshot, spacer).expect("wide spacer owner");
            let owner_text = snapshot.visible_cells[owner].c.to_string();

            assert_eq!(
                selection_text(&snapshot, spacer, snapshot.cols as usize - 1),
                fixture,
                "start on spacer should copy full fixture: {fixture}"
            );
            assert_eq!(
                selection_text(&snapshot, spacer, spacer),
                owner_text,
                "single spacer selection should copy owning char: {fixture}"
            );
            assert_eq!(
                selection_text(&snapshot, owner, spacer),
                owner_text,
                "end on spacer should copy owning char: {fixture}"
            );
        }
    }

    #[test]
    fn emoji_path_fixture_selection_copy_preserves_rocket() {
        let snapshot = backend_snap("project/🚀-deploy/config.json");
        assert_eq!(
            full_row_selection_text(&snapshot),
            "project/🚀-deploy/config.json"
        );

        let spacer = snapshot
            .visible_cells
            .iter()
            .enumerate()
            .find_map(|(index, cell)| {
                let owner = owning_wide_cell(&snapshot, index)?;
                (cell.wide_spacer && snapshot.visible_cells[owner].c == '🚀').then_some(index)
            })
            .expect("rocket fixture should contain a wide spacer");
        assert_eq!(selection_text(&snapshot, spacer, spacer), "🚀");
    }
}
