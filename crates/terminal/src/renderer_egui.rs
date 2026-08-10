//! CellGrid 렌더러 (설계문서 4.3 egui_cell_renderer).
//! egui 0.35 신 시그니처(&mut Ui) 기준 (설계문서 1.1). egui_term(1.7)은 참고만.

use std::sync::Arc;

use egui::emath::GuiRounding as _;

use crate::viewport_snapshot::{CellAttrs, CellRange, CursorShape, TerminalViewportSnapshot};

pub struct RenderOutput {
    pub response: egui::Response,
    /// 셀 하나의 화면 크기 — 호출측이 cols/rows 계산에 쓴다
    pub cell_size: egui::Vec2,
    /// 그리드 좌상단 화면 좌표 — 호출측이 포인터→셀 변환(선택 드래그)에 쓴다
    pub origin: egui::Pos2,
    /// 이번 draw의 계측 카운터 — 렌더러 A/B 실측(B1)에서 호출측이 프레임 단위로 합산한다.
    /// 정수 증가뿐이라 게이트 없이 항상 집계한다(기존 rebuilt_rows_last_frame과 동일 정책).
    pub counters: RenderCounters,
}

/// draw 1회의 렌더 비용 카운터. `shapes`는 **우리가 발행한 painter 호출 수**로,
/// epaint가 내부에서 만드는 테셀레이션 삼각형 수가 아니다.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct RenderCounters {
    /// 이번 프레임에 갤리를 다시 shaping한 행 수 (dirty 또는 캐시 미스)
    pub rows_rebuilt: usize,
    /// 이번 프레임에 실제로 그린(캐시 소비) 행 수
    pub rows_painted: usize,
    /// 이번 프레임에 발행한 painter 호출 수 (rect_filled + galley + text + line_segment)
    pub shapes: usize,
    /// snapshot.dirty_ranges가 가리키는 행 수
    pub dirty_rows: usize,
}

impl std::ops::AddAssign for RenderCounters {
    fn add_assign(&mut self, rhs: Self) {
        self.rows_rebuilt += rhs.rows_rebuilt;
        self.rows_painted += rhs.rows_painted;
        self.shapes += rhs.shapes;
        self.dirty_rows += rhs.dirty_rows;
    }
}

/// 터미널 셀 그리드 좌우의 고정 내부 여백.
///
/// 열 수 계산과 실제 렌더링이 같은 값을 사용해야 마지막 열이 우측 여백을 침범하거나
/// 잘리지 않는다.
pub const HORIZONTAL_PADDING: f32 = 3.0;

/// backend snapshot이 기본 셀 배경에 쓰는 레거시 색. 새 AgentTerminal surface에서는
/// 이 색을 아래의 더 어두운 작업면 색으로 remap하고, 그 밖의 ANSI 배경색은 유지한다.
///
/// 이 값 자체는 **화면에 칠해지지 않는다** — `build_row_cache`가 이 색과 같은 셀 배경을
/// 건너뛰어(`if bg == default_bg { continue; }`) 아래 작업면이 그대로 비치게 한다.
/// 즉 backend가 보고하는 "기본 배경" 센티넬이므로, 두 backend의 `DEFAULT_BG`와 값이
/// 어긋나면 기본 셀이 통째로 칠해져 작업면 색이 묻힌다.
const SNAPSHOT_DEFAULT_BG: egui::Color32 = egui::Color32::from_rgb(0x18, 0x18, 0x1c);
/// 실제로 보이는 터미널 작업면. 화면에서 가장 넓은 면이라 앱 전체 인상을 좌우한다.
///
/// 2026-08-06: 앱 팔레트를 hsl 220도 축으로 옮기면서 여기도 맞췄다. 이전 #0f1117은
/// 225도라 축에서 살짝 벗어나 있었고, 무엇보다 L 7.5%로 사이드바 본문(8.6%)과 거의
/// 같아 터미널이 "깊은 작업면"으로 읽히지 않았다. L 5.5%로 낮춰 사이드바보다 확실히
/// 뒤로 물러나게 한다(사용자 확인, 목업 A/B 비교).
///
/// 호출부도 이 색이 필요하다 — 그리드 폭이 셀 단위로 떨어져 pane 우측에 최대 한 셀만큼
/// 남는데, 그 자리를 pane 본문 rect에 미리 칠해 두지 않으면 CentralPanel 배경(앱 크롬)이
/// 비쳐 밝은 여백 띠가 된다. pane 폭을 아는 쪽이 호출부라 여기서 공개한다.
pub const TERMINAL_SURFACE_BG: egui::Color32 = egui::Color32::from_rgb(0x0b, 0x0d, 0x11);

/// 전체 터미널 폭에서 좌우 내부 여백을 제외한 셀 그리드 가용 폭.
pub fn grid_width_for_available(available_width: f32) -> f32 {
    (available_width.max(0.0) - HORIZONTAL_PADDING * 2.0).max(0.0)
}

/// pane 높이에서 만들 수 있는 PTY 행 수. 실제 가용 높이를 전부 행 계산에 사용하고,
/// 남는 sub-cell 픽셀은 renderer가 터미널 배경으로 채운다.
pub fn grid_rows_for_available(available_height: f32, cell_height: f32) -> u16 {
    if !available_height.is_finite() || !cell_height.is_finite() || cell_height <= 0.0 {
        return 3;
    }
    ((available_height.max(0.0) / cell_height).floor() as u16).clamp(3, 200)
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
    counters: RenderCounters,
    /// 마지막으로 dirty를 소비한 스냅샷 세대. `snapshot.dirty_ranges`는 "그 스냅샷이
    /// 만들어질 때 바뀐 행"이라 **같은 스냅샷을 다시 그리면 같은 행이 계속 dirty로 보인다**
    /// — 새 출력이 없는 repaint(리소스 표시 갱신·애니메이션)마다 전 행을 재-shaping했다
    /// (2026-07-14 실측: idle에서 rows_rebuilt≈전체 행). 세대가 같으면 이미 소비한 것으로 본다.
    last_gen: Option<u64>,
}

impl TerminalRenderCache {
    pub fn clear(&mut self) {
        self.rows_cache.clear();
        self.cols = 0;
        self.rows = 0;
        self.scroll_offset = 0;
        self.is_alt_screen = false;
        self.font_size_bits = 0;
        self.counters = RenderCounters::default();
        self.last_gen = None;
    }

    pub fn rebuilt_rows_last_frame(&self) -> usize {
        self.counters.rows_rebuilt
    }

    /// 이번 draw에서 `dirty_ranges`를 신뢰할 수 있는가 — 같은 세대(같은 스냅샷)를 다시
    /// 그리는 것이면 dirty는 이미 소비됐다(행 캐시가 최신). 위 `last_gen` 주석 참조.
    fn dirty_is_fresh(&self, generation: u64) -> bool {
        self.last_gen != Some(generation)
    }

    fn prepare(&mut self, snapshot: &TerminalViewportSnapshot, font_size: f32, generation: u64) {
        self.counters = RenderCounters::default();
        self.last_gen = Some(generation);
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

/// bold 셀에 쓸 모노 굵은 폰트 패밀리 이름 (B-1). 앱(fonts.rs)이 같은 이름으로 등록한다 —
/// 미등록이면 egui가 기본 Monospace로 폴백하므로 안전하다.
pub const MONO_BOLD_FAMILY: &str = "mono_bold";

/// 속성이 적용된 셀 텍스트 갤리를 만든다 (B-1). bold는 굵은 패밀리, italic은 egui가
/// 합성(기울임), underline/strikeout은 TextFormat의 선, dim은 색을 낮춘다.
fn layout_attr_text(
    painter: &egui::Painter,
    text: String,
    font_id: &egui::FontId,
    color: egui::Color32,
    attrs: CellAttrs,
) -> Arc<egui::Galley> {
    if attrs.is_empty() {
        return painter.layout_no_wrap(text, font_id.clone(), color);
    }
    let mut font = font_id.clone();
    if attrs.contains(CellAttrs::BOLD) {
        font.family = egui::FontFamily::Name(MONO_BOLD_FAMILY.into());
    }
    let color = if attrs.contains(CellAttrs::DIM) {
        dim_color(color)
    } else {
        color
    };
    let line = egui::Stroke::new(1.0, color);
    let format = egui::TextFormat {
        font_id: font,
        color,
        italics: attrs.contains(CellAttrs::ITALIC),
        underline: if attrs.contains(CellAttrs::UNDERLINE) {
            line
        } else {
            egui::Stroke::NONE
        },
        strikethrough: if attrs.contains(CellAttrs::STRIKEOUT) {
            line
        } else {
            egui::Stroke::NONE
        },
        ..Default::default()
    };
    let mut job = egui::text::LayoutJob::default();
    job.append(&text, 0.0, format);
    job.wrap.max_width = f32::INFINITY;
    painter.layout_job(job)
}

/// SGR 2(dim) — 밝기를 60%로 낮춘다(터미널 관례).
fn dim_color(color: egui::Color32) -> egui::Color32 {
    let f = |c: u8| (c as f32 * 0.6) as u8;
    egui::Color32::from_rgb(f(color.r()), f(color.g()), f(color.b()))
}

/// 셀 격자의 기하를 정하는 두 값 — 항상 함께 다닌다(설정에서 온 값을 그대로 싣는다).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CellMetrics {
    /// 모노스페이스 폰트 크기 (UI 배율로 역보정된 값)
    pub font_size: f32,
    /// 행 높이 배수 — 폰트가 내장한 행 높이(ascent+descent+line_gap)에 곱한다.
    /// 1.0이면 폰트 메트릭 그대로. config에서 0.8~2.0으로 clamp된다.
    pub line_height: f32,
}

/// 셀 하나의 화면 크기 (모노스페이스 'M' 폭 × 행 높이 × 행 높이 배수).
///
/// 셀이 커지면 cols/rows 계산(workspace.rs)이 자동으로 따라가 PTY resize까지 이어진다.
pub fn cell_size(ctx: &egui::Context, metrics: CellMetrics) -> egui::Vec2 {
    let font_id = egui::FontId::monospace(metrics.font_size);
    ctx.fonts_mut(|fonts| {
        egui::vec2(
            fonts.glyph_width(&font_id, 'M'),
            fonts.row_height(&font_id) * metrics.line_height,
        )
    })
}

/// snapshot을 그린다. preedit은 IME 조합 중 텍스트 — 커서 위치에 표시한다.
/// `ime_active`는 호출측이 결정한 논리적 터미널 키보드 소유 상태다. 활성 상태면
/// 같은 프레임에 egui 포커스를 확보한 뒤 공식 IME 소유권을 확인한다.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    ui: &mut egui::Ui,
    snapshot: &TerminalViewportSnapshot,
    metrics: CellMetrics,
    cache: &mut TerminalRenderCache,
    preedit: Option<&str>,
    ime_active: bool,
    // 선택 영역 (정규화된 선형 셀 인덱스, inclusive) — 셀 배경을 선택색으로 그린다
    selection: Option<(usize, usize)>,
    // 스냅샷 세대 — 호출측이 새 스냅샷을 받을 때마다 +1. 같은 세대를 다시 그리면
    // dirty_ranges는 이미 소비된 것이라 재-shaping하지 않는다 (2026-07-14 idle 낭비 수정).
    snapshot_gen: u64,
) -> RenderOutput {
    let font_id = egui::FontId::monospace(metrics.font_size);
    let cell = cell_size(ui.ctx(), metrics);
    // 셀 안에서 글자를 세로 중앙에 둔다 — 안 그러면 넓힌 행간이 전부 글자 아래로만 몰린다.
    // cell.y는 글자 높이 × line_height라 나누면 원래 글자 높이가 되고(config에서 0.8 하한으로
    // clamp되어 0으로 나눌 일이 없다), 그 차이의 절반이 위쪽 여백이다.
    let text_dy = (cell.y - cell.y / metrics.line_height) * 0.5;
    // hit-test/응답 rect는 pane 영역을 넘지 않게 clamp한다 — split/resize 직후
    // stale(더 큰) snapshot이 이웃 pane의 클릭/스크롤을 가로채는 것 방지 (codex 리뷰).
    // 넘치는 셀은 아래 content_rect로 잘리며 좌우 여백을 침범하지 않는다.
    let avail = ui.available_size();
    let render_height = if avail.y.is_finite() {
        avail.y.max(0.0)
    } else {
        cell.y * snapshot.rows as f32
    };
    // 그리드 폭은 셀 단위로 떨어지므로 pane 우측에 최대 한 셀만큼 남는다. 그 자리는
    // **호출부가** 작업면 색으로 미리 덮는다(pane 폭을 아는 쪽은 거기다) — 여기서
    // avail.x를 그대로 쓰면 무제한 ui에서 터미널이 화면 전체를 차지한다.
    let size = egui::vec2(
        ((cell.x * snapshot.cols as f32).min(grid_width_for_available(avail.x))
            + HORIZONTAL_PADDING * 2.0)
            .min(avail.x.max(0.0)),
        render_height,
    );
    // click_and_drag: 클릭=포커스, 드래그=선택 (2026-07-05 복사 지원)
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    // 조합이 없을 때만 공식 소유권을 즉시 되찾는다. egui의 request_focus는 현재 조합을
    // interrupt하므로, 진행 중 preedit 동안 비-TextEdit 포커스가 한 프레임 튀었다고
    // 호출하면 macOS가 자모를 강제 commit한다. 진행 중 조합은 아래 IME output을 유지한
    // 채 호출측이 논리적 입력 소유권으로 계속 소비하고, commit 뒤 다음 프레임에 복귀한다.
    let continues_preedit = ime_active && preedit.is_some_and(|preedit| !preedit.is_empty());
    if ime_active && !continues_preedit && !ui.memory(|memory| memory.owns_ime_events(response.id))
    {
        response.request_focus();
    }
    let owns_ime_events = ime_active && ui.memory(|memory| memory.owns_ime_events(response.id));
    let maintains_ime_composition = owns_ime_events || continues_preedit;
    if response.has_focus() {
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(response.id, terminal_focus_lock_filter());
        });
    }
    let background_painter = ui.painter_at(rect);
    let content_rect = terminal_content_rect(rect);
    let painter = background_painter.with_clip_rect(content_rect);
    let origin = content_rect.min;
    let default_bg = TERMINAL_SURFACE_BG;
    let selection = selection.and_then(|(a, b)| normalize_selection_range(snapshot, a, b));
    background_painter.rect_filled(rect, 0.0, default_bg);

    let dirty_fresh = cache.dirty_is_fresh(snapshot_gen);
    // line_height는 갤리 shaping에 영향을 주지 않는다(글자를 그리는 y 위치만 바뀐다) —
    // 캐시 무효화 기준은 font_size 그대로다.
    cache.prepare(snapshot, metrics.font_size, snapshot_gen);
    cache.counters.shapes += 1; // 위 배경 rect_filled
    for row in 0..snapshot.rows as usize {
        let dirty = dirty_fresh && row_is_dirty(snapshot, row);
        if dirty {
            cache.counters.dirty_rows += 1;
        }
        let needs_rebuild = dirty
            || cache
                .rows_cache
                .get(row)
                .and_then(|cached| cached.as_ref())
                .is_none();
        if needs_rebuild {
            let row_cache = build_row_cache(&painter, snapshot, row, &font_id, SNAPSHOT_DEFAULT_BG);
            if let Some(slot) = cache.rows_cache.get_mut(row) {
                *slot = Some(row_cache);
                cache.counters.rows_rebuilt += 1;
            }
        }

        if let Some(row_cache) = cache.rows_cache.get(row).and_then(|cached| cached.as_ref()) {
            let row_y = row as f32 * cell.y;
            for bg in &row_cache.bg_runs {
                let pos = origin + egui::vec2(bg.start_col as f32 * cell.x, row_y);
                let width = (bg.end_col - bg.start_col) as f32 * cell.x;
                // 픽셀 경계 스냅 — 소수 좌표 rect가 맞닿으면 feathering이 인접 rect
                // 사이에 배경이 비치는 이음새(셀 간 여백처럼 보임)를 만든다. min/max를
                // 각각 반올림하므로 같은 경계를 공유하는 이웃 rect는 틈도 겹침도 없다.
                let bg_rect = egui::Rect::from_min_size(pos, egui::vec2(width, cell.y))
                    .round_to_pixels(painter.pixels_per_point());
                painter.rect_filled(bg_rect, 0.0, bg.color);
            }
            let selection_shapes =
                paint_selection_row(&painter, snapshot, row, origin, cell, selection);
            for run in &row_cache.text_runs {
                let pos = origin + egui::vec2(run.col as f32 * cell.x, row_y + text_dy);
                painter.galley(pos, Arc::clone(&run.galley), run.color);
            }
            cache.counters.rows_painted += 1;
            cache.counters.shapes +=
                row_cache.bg_runs.len() + row_cache.text_runs.len() + selection_shapes;
        }
    }

    // 커서 좌표는 hidden이어도 유효하다 — IME/preedit 배치에 계속 쓴다.
    let cursor_pos = origin
        + egui::vec2(
            snapshot.cursor.col as f32 * cell.x,
            snapshot.cursor.row as f32 * cell.y,
        );
    // 커서 rect (스크롤 중이거나 hidden이면 snapshot.visible이 false)
    if snapshot.cursor.visible {
        // 커서 rect는 셀 전체를 채운다(배경과 동일) — text_dy는 글자에만 적용한다.
        let cursor_color = egui::Color32::from_rgba_unmultiplied(0xd8, 0xd8, 0xd8, 0xa0);
        let cursor_rect = match snapshot.cursor.shape {
            CursorShape::Block => egui::Rect::from_min_size(cursor_pos, cell),
            CursorShape::Underline => egui::Rect::from_min_size(
                cursor_pos + egui::vec2(0.0, cell.y - 2.0),
                egui::vec2(cell.x, 2.0),
            ),
            CursorShape::Beam => egui::Rect::from_min_size(cursor_pos, egui::vec2(2.0, cell.y)),
        };
        painter.rect_filled(cursor_rect, 0.0, cursor_color);
        cache.counters.shapes += 1;
    }

    // IME는 터미널이 egui의 공식 IME 소유자일 때만 — 다른 입력창의 조합/후보창을
    // 뺏지 않는다. 위에서 논리적 소유권과 egui 포커스를 같은 프레임에 동기화한다.
    // **커서 가시성과는 무관하게** 매 프레임 세팅해야 한다: egui-winit은
    // `allow_ime = ime.is_some()`이라(0.35 handle_platform_output), 한 프레임이라도
    // 비우면 set_ime_allowed(false)로 macOS가 진행 중인 한글 조합을 강제 커밋한다.
    // TUI(claude 등)는 리드로우마다 커서를 숨겼다 켜므로(?25l/?25h) 커서 가시성에
    // 묶으면 조합이 자모 단위로 끊긴다 (2026-07-14 사용자: "ㄹㅗ" 분리).
    if maintains_ime_composition {
        // 조합 중 텍스트를 커서 위치에 표시
        if let Some(preedit) = preedit.filter(|p| !p.is_empty()) {
            // 조합 텍스트도 글자이므로 셀 안 세로 중앙 정렬을 따른다.
            let text_pos = cursor_pos + egui::vec2(0.0, text_dy);
            let galley_rect = painter.text(
                text_pos,
                egui::Align2::LEFT_TOP,
                preedit,
                font_id.clone(),
                egui::Color32::BLACK,
            );
            painter.rect_filled(galley_rect, 0.0, egui::Color32::from_rgb(0xd8, 0xd8, 0xd8));
            painter.text(
                text_pos,
                egui::Align2::LEFT_TOP,
                preedit,
                font_id,
                egui::Color32::BLACK,
            );
            painter.line_segment(
                [galley_rect.left_bottom(), galley_rect.right_bottom()],
                egui::Stroke::new(1.5, egui::Color32::BLACK),
            );
            cache.counters.shapes += 4; // text ×2 + rect_filled + line_segment
        }
        ui.ctx().output_mut(|o| {
            o.ime = Some(egui::output::IMEOutput {
                rect,
                cursor_rect: egui::Rect::from_min_size(cursor_pos, cell),
                should_interrupt_composition: false,
            });
        });
    }

    RenderOutput {
        response,
        cell_size: cell,
        origin,
        counters: cache.counters,
    }
}

fn terminal_content_rect(rect: egui::Rect) -> egui::Rect {
    // 극단적으로 좁은 pane에서도 좌우가 교차하지 않게 같은 inset을 절반까지 줄인다.
    let inset = HORIZONTAL_PADDING.min(rect.width().max(0.0) * 0.5);
    egui::Rect::from_min_max(
        rect.min + egui::vec2(inset, 0.0),
        rect.max - egui::vec2(inset, 0.0),
    )
}

pub fn terminal_focus_lock_filter() -> egui::EventFilter {
    egui::EventFilter {
        tab: true,
        horizontal_arrows: true,
        vertical_arrows: true,
        escape: true,
    }
}

/// 표시용 글리프 치환 — 일부 기호는 어떤 텍스트/모노 폰트에도 없어(예: ⏺ U+23FA는 Menlo·SF
/// Mono·Apple Symbols·JetBrains Mono 전부 미보유) egui 흑백 이모지 폴백으로 작게 그려진다.
/// 육안상 같은 글리프로 바꿔 크기를 맞춘다 — **그리드 셀 원본은 불변**이라 복사/선택엔 영향 없다.
fn display_char(c: char) -> char {
    match c {
        '\u{23FA}' => '\u{25CF}', // ⏺ → ● (JetBrains Mono 보유, 동일한 채운 원)
        other => other,
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
        let attrs = term_cell.attrs;
        if term_cell.wide {
            pending.flush(&mut text_runs, painter, font_id);
            let text = display_char(term_cell.c).to_string();
            text_runs.push(RowTextRun {
                col,
                galley: layout_attr_text(painter, text, font_id, fg, attrs),
                color: fg,
            });
        } else {
            if pending.needs_flush(col, fg, attrs) {
                pending.flush(&mut text_runs, painter, font_id);
            }
            pending.push(col, display_char(term_cell.c), fg, attrs);
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
    /// run은 색뿐 아니라 **속성이 같을 때만** 이어진다 (B-1).
    attrs: CellAttrs,
    text: String,
}

impl PendingTextRun {
    fn needs_flush(&self, col: usize, color: egui::Color32, attrs: CellAttrs) -> bool {
        self.color.is_some()
            && (self.color != Some(color) || self.attrs != attrs || self.next_col != col)
    }

    fn push(&mut self, col: usize, ch: char, color: egui::Color32, attrs: CellAttrs) {
        if self.color.is_none() {
            self.start_col = col;
            self.next_col = col;
            self.color = Some(color);
            self.attrs = attrs;
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
        let attrs = std::mem::take(&mut self.attrs);
        text_runs.push(RowTextRun {
            col: self.start_col,
            galley: layout_attr_text(painter, text, font_id, color, attrs),
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

/// 선택 배경을 그리고 **발행한 rect 수**를 돌려준다 (shapes 카운터용).
fn paint_selection_row(
    painter: &egui::Painter,
    snapshot: &TerminalViewportSnapshot,
    row: usize,
    origin: egui::Pos2,
    cell_size: egui::Vec2,
    selection: Option<(usize, usize)>,
) -> usize {
    let Some((start, end)) = selection else {
        return 0;
    };
    let cols = snapshot.cols as usize;
    let row_start = row * cols;
    let row_end = row_start + cols;
    if cols == 0 || end < row_start || start >= row_end {
        return 0;
    }

    let selection_bg = egui::Color32::from_rgb(0x2d, 0x4f, 0x77);
    // 연속 선택 셀을 **run으로 병합**해 rect 하나로 그린다 — 배경색(push_bg_run)이 이미
    // 쓰는 관례. 셀마다 rect를 발행하던 이전 구현은 200×60 전체 선택에서 shape가 18배
    // (2.2ms, 프레임 예산 13%)로 폭증했다 (2026-07-14 실측). wide_spacer는 앞선 wide
    // 셀의 폭에 이미 포함되므로 run을 끊지 않고 건너뛴다(폭 계산은 col 진행으로 처리).
    let mut painted = 0;
    let mut run_start: Option<usize> = None; // run의 시작 col
    let mut run_end_col = 0usize; // run의 끝(배타) col
    let flush = |run_start: &mut Option<usize>, run_end_col: usize, painted: &mut usize| {
        if let Some(start_col) = run_start.take() {
            let pos = origin + egui::vec2(start_col as f32 * cell_size.x, row as f32 * cell_size.y);
            let width = (run_end_col - start_col) as f32 * cell_size.x;
            // 픽셀 경계 스냅 — 행마다 rect를 그리므로 소수 좌표면 위/아래 행 사이에
            // feathering 이음새(셀 간 여백처럼 보임)가 생긴다 (bg_runs와 동일 규약).
            let run_rect = egui::Rect::from_min_size(pos, egui::vec2(width, cell_size.y))
                .round_to_pixels(painter.pixels_per_point());
            painter.rect_filled(run_rect, 0.0, selection_bg);
            *painted += 1;
        }
    };
    for col in 0..cols {
        let index = row_start + col;
        let selected = index >= start
            && index <= end
            && snapshot
                .visible_cells
                .get(index)
                .is_some_and(|cell| !cell.wide_spacer || run_start.is_some());
        if selected {
            if run_start.is_none() {
                run_start = Some(col);
            }
            run_end_col = col + 1;
        } else {
            flush(&mut run_start, run_end_col, &mut painted);
        }
    }
    flush(&mut run_start, run_end_col, &mut painted);
    painted
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
    #[test]
    fn display_char는_없는_기호만_치환한다() {
        assert_eq!(display_char('\u{23FA}'), '\u{25CF}'); // ⏺ → ●
        assert_eq!(display_char('\u{25CF}'), '\u{25CF}'); // ● 그대로
        assert_eq!(display_char('A'), 'A');
        assert_eq!(display_char('가'), '가');
    }
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
                    attrs: Default::default(),
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
        draw_gen_for_test(cache, snapshot, next_gen())
    }

    /// 세대를 명시해 draw — 같은 세대 재draw(= 같은 스냅샷 repaint)를 재현한다.
    fn draw_gen_for_test(
        cache: &mut TerminalRenderCache,
        snapshot: &TerminalViewportSnapshot,
        generation: u64,
    ) -> usize {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_min_size(egui::vec2(500.0, 200.0));
            draw(
                ui,
                snapshot,
                m(13.0, 1.0),
                cache,
                None,
                false,
                None,
                generation,
            );
        });
        cache.rebuilt_rows_last_frame()
    }

    /// 테스트용 CellMetrics — 폰트 크기 + 행 높이 배수.
    fn m(font_size: f32, line_height: f32) -> CellMetrics {
        CellMetrics {
            font_size,
            line_height,
        }
    }

    /// 테스트용 단조 증가 세대 — 매 호출이 "새 스냅샷"을 뜻한다.
    fn next_gen() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static GEN: AtomicU64 = AtomicU64::new(1);
        GEN.fetch_add(1, Ordering::Relaxed)
    }

    /// 렌더 경로 실측(수동, `#[ignore]`) — 전용 GPU 렌더패스(Rio sugarloaf 방식) 도입을
    /// 재검토할 때 숫자를 다시 뽑는 도구다.
    ///
    /// **회귀 가드가 아니다.** 아무것도 assert하지 않고 stderr로 찍기만 한다 — 타이밍에
    /// 임계값을 걸면 느린 러너에서 바로 flaky가 되므로 의도적으로 그렇게 뒀다. 감시가
    /// 필요하면 이 벤치가 아니라 별도 수단을 써야 한다.
    ///
    /// 두 축을 잰다: (a) all-dirty = 매 프레임 전 행 재-shaping(무거운 출력),
    /// (b) cached = 같은 스냅샷 재draw(재-shaping 0인데도 egui가 갤리를 재-테셀레이션하는
    /// 순수 리페인트 비용 — 이게 sugarloaf가 없애는 부분).
    ///
    /// 실행: `cargo test -p terminal --release render_tessellation_bench -- --ignored --nocapture`
    /// (release 필수 — debug는 테셀레이션이 수십 배 느려 판정에 못 쓴다)
    ///
    /// 2026-08-10 실측(M-series, release, 13px): 최악 300x80 all-dirty가 0.59ms/frame으로
    /// 16.6ms 예산의 3.6%. cached 리페인트 0.25ms. 결론은 `docs/render-path-analysis.md`와
    /// `docs/render-resource-decision.md`에 있다 — egui 테셀레이션은 병목이 아니다.
    #[test]
    #[ignore = "렌더 테셀레이션 실측 — --ignored --nocapture로만"]
    fn render_tessellation_bench() {
        use std::time::Instant;

        // 현실적 색 변화(color_period 셀마다 fg 전환)로 run 병합을 실제 수준으로 낮춘다.
        fn bench_snap(cols: u16, rows: u16, color_period: usize) -> TerminalViewportSnapshot {
            let palette = [
                [0xd8u8, 0xd8, 0xd8],
                [0xe0, 0x6c, 0x75],
                [0x98, 0xc3, 0x79],
                [0x61, 0xaf, 0xef],
                [0xc6, 0x78, 0xdd],
            ];
            let cb: Vec<char> = "abcdefghijklmnopqrstuvwxyz0123456789 (){}[];:=+-*/<>"
                .chars()
                .collect();
            let mut cells = Vec::with_capacity(cols as usize * rows as usize);
            for idx in 0..cols as usize * rows as usize {
                cells.push(TerminalCell {
                    c: cb[idx % cb.len()],
                    fg: palette[(idx / color_period) % palette.len()],
                    bg: [0x18, 0x18, 0x1c],
                    wide: false,
                    wide_spacer: false,
                    attrs: Default::default(),
                });
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

        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(4000.0, 2400.0),
            )),
            ..Default::default()
        };
        let tessellate_ms = |ctx: &egui::Context, full: egui::FullOutput| -> (f64, usize) {
            let ppp = ctx.pixels_per_point();
            let t = Instant::now();
            let prims = ctx.tessellate(full.shapes, ppp);
            let ms = t.elapsed().as_secs_f64() * 1e3;
            let tris: usize = prims
                .iter()
                .map(|p| match &p.primitive {
                    egui::epaint::Primitive::Mesh(m) => m.indices.len() / 3,
                    _ => 0,
                })
                .sum();
            (ms, tris)
        };

        // 폰트 아틀라스 워밍업(첫 프레임 1회성 비용 제외).
        {
            let mut c = TerminalRenderCache::default();
            let s = bench_snap(80, 24, 8);
            let full = ctx.run_ui(raw.clone(), |ui| {
                draw(ui, &s, m(13.0, 1.0), &mut c, None, false, None, next_gen());
            });
            let _ = tessellate_ms(&ctx, full);
        }

        const ITERS: usize = 30;
        eprintln!("── render 경로 실측 (평균 {ITERS}프레임, font 13px) ──");
        for (cols, rows) in [(80u16, 24u16), (200, 50), (300, 80)] {
            let snapshot = bench_snap(cols, rows, 8);

            // (a) all-dirty: 매 프레임 새 캐시 → 전 행 재-shaping + 재-테셀레이션.
            let (mut d_ms, mut t_ms, mut tris, mut shapes) = (0.0, 0.0, 0usize, 0usize);
            for _ in 0..ITERS {
                let mut cache = TerminalRenderCache::default();
                let t = Instant::now();
                let full = ctx.run_ui(raw.clone(), |ui| {
                    draw(
                        ui,
                        &snapshot,
                        m(13.0, 1.0),
                        &mut cache,
                        None,
                        false,
                        None,
                        next_gen(),
                    );
                });
                d_ms += t.elapsed().as_secs_f64() * 1e3;
                shapes = cache.counters.shapes;
                let (tm, tr) = tessellate_ms(&ctx, full);
                t_ms += tm;
                tris = tr;
            }
            eprintln!(
                "[{cols:>3}x{rows:<2} all-dirty] paint-build {:.2}ms + tessellate {:.2}ms = {:.2}ms/frame  shapes={shapes} tris={tris}",
                d_ms / ITERS as f64,
                t_ms / ITERS as f64,
                (d_ms + t_ms) / ITERS as f64,
            );

            // (b) cached: 같은 세대 재draw → 재-shaping 0, 그래도 재-테셀레이션.
            let mut cache = TerminalRenderCache::default();
            let g = next_gen();
            let full = ctx.run_ui(raw.clone(), |ui| {
                draw(
                    ui,
                    &snapshot,
                    m(13.0, 1.0),
                    &mut cache,
                    None,
                    false,
                    None,
                    g,
                );
            });
            let _ = tessellate_ms(&ctx, full);
            let (mut cd_ms, mut ct_ms) = (0.0, 0.0);
            for _ in 0..ITERS {
                let t = Instant::now();
                let full = ctx.run_ui(raw.clone(), |ui| {
                    draw(
                        ui,
                        &snapshot,
                        m(13.0, 1.0),
                        &mut cache,
                        None,
                        false,
                        None,
                        g,
                    );
                });
                cd_ms += t.elapsed().as_secs_f64() * 1e3;
                let (tm, _) = tessellate_ms(&ctx, full);
                ct_ms += tm;
            }
            eprintln!(
                "[{cols:>3}x{rows:<2} cached   ] paint-build {:.2}ms + tessellate {:.2}ms = {:.2}ms/frame  (rows_rebuilt/frame={})",
                cd_ms / ITERS as f64,
                ct_ms / ITERS as f64,
                (cd_ms + ct_ms) / ITERS as f64,
                cache.rebuilt_rows_last_frame(),
            );
        }
    }

    #[test]
    fn terminal_좌우_내부여백은_각각_3픽셀이다() {
        assert_eq!(HORIZONTAL_PADDING, 3.0);
        assert_eq!(grid_width_for_available(100.0), 94.0);

        let snapshot = snap(4, 1, &["test"]);
        let ctx = egui::Context::default();
        let mut cache = TerminalRenderCache::default();
        let mut measured = None;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_min_size(egui::vec2(500.0, 200.0));
            let output = draw(
                ui,
                &snapshot,
                m(13.0, 1.0),
                &mut cache,
                None,
                false,
                None,
                next_gen(),
            );
            measured = Some((output.response.rect, output.origin, output.cell_size));
        });

        let (rect, origin, cell) = measured.expect("terminal should be rendered");
        let grid_right = origin.x + cell.x * snapshot.cols as f32;
        assert!((origin.x - rect.left() - 3.0).abs() < f32::EPSILON);
        assert!(
            (rect.right() - grid_right - 3.0).abs() < 0.01,
            "rect={rect:?}, origin={origin:?}, cell={cell:?}, grid_right={grid_right}"
        );
    }

    #[test]
    fn ime_영역은_같은_프레임에_공식_소유권을_확보한_뒤_통보된다() {
        // egui-winit은 o.ime가 비는 프레임마다 set_ime_allowed(false)를 호출해
        // macOS가 진행 중인 한글 조합을 강제 커밋한다. TUI는 리드로우마다 커서를
        // 숨기므로(?25l) 커서 가시성에 IME를 묶으면 자모가 분리된다 (2026-07-14).
        let ctx = egui::Context::default();
        let mut cache = TerminalRenderCache::default();
        let mut snapshot = snap(4, 1, &["test"]);
        snapshot.cursor.visible = false;
        let mut owns_ime_events = false;
        // 논리적 소유권이 넘어온 첫 프레임에도 request_focus 후 공식 소유자가 되어
        // IME 영역을 내보내야 한다.
        let full = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_min_size(egui::vec2(500.0, 200.0));
            let output = draw(
                ui,
                &snapshot,
                m(13.0, 1.0),
                &mut cache,
                None,
                true,
                None,
                next_gen(),
            );
            owns_ime_events = ui.memory(|memory| memory.owns_ime_events(output.response.id));
        });
        assert!(owns_ime_events, "터미널이 egui IME 소유자가 되어야 한다");
        assert!(
            full.platform_output.ime.is_some(),
            "소유권 전환 프레임에서 IME가 비면 조합이 끊긴다"
        );
    }

    #[test]
    fn ime_영역은_비활성_터미널이_출력하지_않는다() {
        let ctx = egui::Context::default();
        let mut cache = TerminalRenderCache::default();
        let snapshot = snap(4, 1, &["test"]);
        let full = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_min_size(egui::vec2(500.0, 200.0));
            draw(
                ui,
                &snapshot,
                m(13.0, 1.0),
                &mut cache,
                None,
                false,
                None,
                next_gen(),
            );
        });
        assert!(full.platform_output.ime.is_none());
    }

    #[test]
    fn 조합중_비textedit_포커스전이는_ime를_중단하지_않는다() {
        let ctx = egui::Context::default();
        let mut cache = TerminalRenderCache::default();
        let snapshot = snap(4, 1, &["test"]);

        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let transient = ui.button("transient focus");
            transient.request_focus();
        });
        let full = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_min_size(egui::vec2(500.0, 200.0));
            let _ = ui.button("transient focus");
            draw(
                ui,
                &snapshot,
                m(13.0, 1.0),
                &mut cache,
                Some("ㄱ"),
                true,
                None,
                next_gen(),
            );
        });

        let ime = full
            .platform_output
            .ime
            .expect("진행 중 조합은 IME allowance를 유지해야 한다");
        assert!(
            !ime.should_interrupt_composition,
            "조합 중 request_focus는 egui가 IME 강제 중단으로 바꾼다"
        );
    }

    #[test]
    fn 행높이_배수는_셀_높이만_키우고_폭은_그대로다() {
        let ctx = egui::Context::default();
        // fonts는 첫 프레임에 초기화된다 — run_ui 안에서 재야 한다.
        let mut measured = None;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            measured = Some((
                cell_size(ui.ctx(), m(13.0, 1.0)),
                cell_size(ui.ctx(), m(13.0, 1.5)),
                cell_size(ui.ctx(), m(13.0, 0.8)),
            ));
        });
        let (base, tall, tight) = measured.expect("cell size measured");

        // 폭은 배수와 무관 — 열 정렬이 깨지면 안 된다.
        assert_eq!(base.x, tall.x);
        assert_eq!(base.x, tight.x);
        // 높이는 정확히 배수만큼.
        assert!((tall.y - base.y * 1.5).abs() < 0.01, "tall={tall:?}");
        assert!((tight.y - base.y * 0.8).abs() < 0.01, "tight={tight:?}");

        // 셀이 높아지면 같은 pane 높이에 들어가는 행 수가 줄어든다 (PTY resize로 이어짐).
        let rows_base = grid_rows_for_available(400.0, base.y);
        let rows_tall = grid_rows_for_available(400.0, tall.y);
        assert!(
            rows_tall < rows_base,
            "rows_base={rows_base}, rows_tall={rows_tall}"
        );
    }

    #[test]
    fn terminal_높이는_완전한_행을_모두_사용한다() {
        assert_eq!(grid_rows_for_available(100.0, 20.0), 5);
        assert_eq!(grid_rows_for_available(119.9, 20.0), 5);
        assert_eq!(grid_rows_for_available(120.0, 20.0), 6);
    }

    #[test]
    fn terminal_행계산은_비정상_높이에서도_최솟값을_지킨다() {
        assert_eq!(grid_rows_for_available(1.0, 20.0), 3);
        assert_eq!(grid_rows_for_available(f32::NAN, 20.0), 3);
        assert_eq!(grid_rows_for_available(100.0, 0.0), 3);
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
    fn render_counters는_dirty_painted_shapes를_집계한다() {
        fn counters(
            cache: &mut TerminalRenderCache,
            snapshot: &TerminalViewportSnapshot,
        ) -> RenderCounters {
            let ctx = egui::Context::default();
            let mut out = RenderCounters::default();
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.set_min_size(egui::vec2(500.0, 200.0));
                out = draw(
                    ui,
                    snapshot,
                    m(13.0, 1.0),
                    cache,
                    None,
                    false,
                    None,
                    next_gen(),
                )
                .counters;
            });
            out
        }

        let mut cache = TerminalRenderCache::default();
        let first = snap(4, 3, &["aaaa", "bbbb", "cccc"]);
        let c = counters(&mut cache, &first);
        // 첫 프레임: dirty_ranges는 비었지만 캐시 미스로 3행 전부 재구성 + 3행 전부 페인트.
        assert_eq!(c.dirty_rows, 0);
        assert_eq!(c.rows_rebuilt, 3);
        assert_eq!(c.rows_painted, 3);
        // 배경 rect 1 + 행마다 text_run 1개 (기본 bg라 bg_run 없음)
        assert_eq!(c.shapes, 1 + 3);

        // 2프레임: 1행만 dirty → 재구성 1행, 페인트는 여전히 전체 행(shape 발행은 매 프레임).
        let mut second = snap(4, 3, &["aaaa", "bbxb", "cccc"]);
        second.dirty_ranges = vec![CellRange { start: 4, end: 8 }];
        let c = counters(&mut cache, &second);
        assert_eq!(c.dirty_rows, 1);
        assert_eq!(c.rows_rebuilt, 1);
        assert_eq!(c.rows_painted, 3);
        assert_eq!(c.shapes, 1 + 3);
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

    /// 2026-07-14 실측 회귀: 같은 스냅샷을 다시 그리면(새 출력 없는 repaint —
    /// 리소스 표시 갱신·애니메이션) dirty_ranges가 남아 있어 전 행을 재-shaping했다.
    /// 세대가 같으면 dirty는 이미 소비된 것으로 보고 재빌드하지 않아야 한다.
    #[test]
    fn 같은_스냅샷_재draw는_행을_재구성하지_않는다() {
        let mut snapshot = snap(6, 3, &["one", "two", "three"]);
        // 전 행 dirty인 스냅샷(대량 출력 직후 상태)
        snapshot.dirty_ranges = vec![CellRange {
            start: 0,
            end: 6 * 3,
        }];
        let mut cache = TerminalRenderCache::default();
        let generation = 7;
        // 첫 draw: 전 행 빌드(캐시 비어 있음)
        assert_eq!(draw_gen_for_test(&mut cache, &snapshot, generation), 3);
        // 같은 세대 재draw(= 같은 스냅샷 repaint): 재구성 0
        assert_eq!(
            draw_gen_for_test(&mut cache, &snapshot, generation),
            0,
            "같은 스냅샷 repaint가 전 행을 재-shaping했다 (idle 낭비 회귀)"
        );
        // 새 세대(새 스냅샷): dirty를 다시 신뢰해 재구성
        assert_eq!(draw_gen_for_test(&mut cache, &snapshot, generation + 1), 3);
    }

    /// 2026-07-14 실측 회귀: 선택 하이라이트가 셀마다 rect를 발행해 전체 선택 시
    /// shape가 폭증했다(200×60에서 2.2ms). 연속 셀은 run 하나로 병합해야 한다.
    #[test]
    fn 선택_하이라이트는_연속_셀을_run으로_병합한다() {
        let snapshot = snap(10, 2, &["0123456789", "abcdefghij"]);
        let ctx = egui::Context::default();
        let painter = ctx.layer_painter(egui::LayerId::background());
        let cell = egui::vec2(8.0, 16.0);
        let origin = egui::Pos2::ZERO;
        // 첫 행 전체(0..=9) 선택 → rect 1개(셀 10개가 아니라)
        let painted = paint_selection_row(&painter, &snapshot, 0, origin, cell, Some((0, 9)));
        assert_eq!(
            painted, 1,
            "연속 10셀이 rect 10개로 발행됐다 (run 병합 회귀)"
        );
        // 끊긴 선택(0..=2, 5..=7)은 행 안에서 run 2개 — 선택은 선형 범위라 여기선
        // 행 경계로 잘린 부분 선택만 확인한다(0..=2 = run 1개).
        let painted = paint_selection_row(&painter, &snapshot, 0, origin, cell, Some((0, 2)));
        assert_eq!(painted, 1);
        // 선택이 이 행에 없으면 0
        let painted = paint_selection_row(&painter, &snapshot, 1, origin, cell, Some((0, 2)));
        assert_eq!(painted, 0);
    }
}
