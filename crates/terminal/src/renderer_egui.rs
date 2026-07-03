//! CellGrid 렌더러 (설계문서 4.3 egui_cell_renderer).
//! egui 0.35 신 시그니처(&mut Ui) 기준 (설계문서 1.1). egui_term(1.7)은 참고만.

use crate::viewport_snapshot::{CursorShape, TerminalViewportSnapshot};

pub struct RenderOutput {
    pub response: egui::Response,
    /// 셀 하나의 화면 크기 — 호출측이 cols/rows 계산에 쓴다
    pub cell_size: egui::Vec2,
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
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter_at(rect);
    let origin = rect.min;

    let cols = snapshot.cols as usize;
    let default_bg = egui::Color32::from_rgb(0x18, 0x18, 0x1c);
    painter.rect_filled(rect, 0.0, default_bg);

    for (i, term_cell) in snapshot.visible_cells.iter().enumerate() {
        if term_cell.wide_spacer {
            continue;
        }
        let (row, col) = (i / cols, i % cols);
        let pos = origin + egui::vec2(col as f32 * cell.x, row as f32 * cell.y);
        let width = if term_cell.wide { cell.x * 2.0 } else { cell.x };
        let bg = rgb(term_cell.bg);
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
    }
}

fn rgb(c: [u8; 3]) -> egui::Color32 {
    egui::Color32::from_rgb(c[0], c[1], c[2])
}
