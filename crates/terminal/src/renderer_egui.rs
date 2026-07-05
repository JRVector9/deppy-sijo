//! CellGrid 렌더러 (설계문서 4.3 egui_cell_renderer).
//! egui 0.35 신 시그니처(&mut Ui) 기준 (설계문서 1.1). egui_term(1.7)은 참고만.

use crate::viewport_snapshot::{CursorShape, TerminalViewportSnapshot};

pub struct RenderOutput {
    pub response: egui::Response,
    /// 셀 하나의 화면 크기 — 호출측이 cols/rows 계산에 쓴다
    pub cell_size: egui::Vec2,
    /// 그리드 좌상단 화면 좌표 — 호출측이 포인터→셀 변환(선택 드래그)에 쓴다
    pub origin: egui::Pos2,
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
    let painter = ui.painter_at(rect);
    let origin = rect.min;

    let cols = snapshot.cols as usize;
    let default_bg = egui::Color32::from_rgb(0x18, 0x18, 0x1c);
    let selection = selection.and_then(|(a, b)| normalize_selection_range(snapshot, a, b));
    painter.rect_filled(rect, 0.0, default_bg);

    for (i, term_cell) in snapshot.visible_cells.iter().enumerate() {
        if term_cell.wide_spacer {
            continue;
        }
        let (row, col) = (i / cols, i % cols);
        let pos = origin + egui::vec2(col as f32 * cell.x, row as f32 * cell.y);
        let width = if term_cell.wide { cell.x * 2.0 } else { cell.x };
        let selected = selection.is_some_and(|(a, b)| i >= a && i <= b);
        let bg = if selected {
            egui::Color32::from_rgb(0x2d, 0x4f, 0x77) // 선택 하이라이트
        } else {
            rgb(term_cell.bg)
        };
        if bg != default_bg {
            painter.rect_filled(
                egui::Rect::from_min_size(pos, egui::vec2(width, cell.y)),
                0.0,
                bg,
            );
        }
        if term_cell.c != ' ' {
            painter.text(
                pos + egui::vec2(width / 2.0, 0.0),
                egui::Align2::CENTER_TOP,
                term_cell.c,
                font_id.clone(),
                rgb(term_cell.fg),
            );
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
    use crate::viewport_snapshot::{CursorShape, CursorSnapshot, TerminalCell};

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
