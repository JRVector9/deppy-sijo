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

/// 단일·분할 pane의 실제 가용 폭으로 열 수를 계산한다. 남는 폭은 한 셀 미만이다.
pub fn grid_cols_for_available(available_width: f32, cell_width: f32) -> u16 {
    if !available_width.is_finite() || !cell_width.is_finite() || cell_width <= 0.0 {
        return 10;
    }
    ((grid_width_for_available(available_width) / cell_width).floor() as u16).clamp(10, 500)
}

/// 보존된 `cols`를 pane 폭 안에 담기 위한 균일 축소 배율(0 < scale ≤ 1).
///
/// 리사이즈로 pane이 좁아져도 **기존 출력의 줄바꿈 형태를 그대로 유지**하려면 PTY를
/// reflow시키지 않고 보존된 `snapshot.cols`를 그대로 그려야 한다. 그 폭이 pane보다 넓으면
/// 지금까지는 우측 열이 clip으로 잘렸다 — 대신 그리드 전체를 이 배율로 균일 축소해
/// 한 화면에 담는다. 폰트 크기는 건드리지 않으므로 행 갤리 캐시와 글리프 아틀라스가
/// 그대로 재사용된다(`TerminalRenderCache`는 `font_size`로 무효화된다).
///
/// **1.0을 돌려주는 경우**: 축소가 필요 없거나(그리드가 이미 들어감), 배율을 정의할 수
/// 없는 입력이다. 후자는 폭·셀 폭·`cols`가 0/음수/비유한이거나, 계산된 배율이 0으로
/// 언더플로한 경우다. 0 배율은 역변환이 불가능해 clip 계산이 깨지므로, 그때는 1.0으로
/// 두어 **기존 clip 경로**를 그대로 태운다.
///
/// 하한 clamp는 **의도적으로 없다** — 임의의 최소 배율에서 멈추면 그 아래에서 다시
/// 우측 열이 잘려 "한 화면에 전부 보인다"는 요구가 깨진다.
///
/// Resize 적용 대기 중인 넓은 snapshot도 임시로 화면 안에 담는다. 최종 PTY 크기는
/// 호출측에서 pane의 실제 가용 크기로 정한다.
pub fn fit_width_scale(available_width: f32, cell_width: f32, cols: u16) -> f32 {
    if !available_width.is_finite() || !cell_width.is_finite() || cell_width <= 0.0 || cols == 0 {
        return 1.0;
    }
    let grid_width = grid_width_for_available(available_width);
    let needed = cell_width * cols as f32;
    if grid_width <= 0.0 || !needed.is_finite() || needed <= 0.0 {
        return 1.0;
    }
    let scale = grid_width / needed;
    // NaN·0·언더플로는 역변환 불가 → 1.0(기존 clip 경로). 1 이상이면 축소하지 않는다.
    if scale > 0.0 && scale < 1.0 {
        scale
    } else {
        1.0
    }
}

/// 축소 그리드의 좌표 변환 — `origin`을 고정점으로 하는 균일 스케일.
/// `p ↦ origin + scale·(p − origin)` 이므로 translation은 `origin·(1 − scale)`이다.
/// 고정점이 `content_rect.min`이라 좌측 내부 여백과 첫 열의 화면 위치가 그대로 유지된다.
fn grid_fit_transform(origin: egui::Pos2, scale: f32) -> egui::emath::TSTransform {
    egui::emath::TSTransform::new(origin.to_vec2() * (1.0 - scale), scale)
}

/// 이 painter가 쓰는 PaintList의 다음 shape 인덱스 — 나중에 변환할 구간의 경계다.
fn next_shape_idx(painter: &egui::Painter) -> egui::layers::ShapeIdx {
    painter
        .ctx()
        .graphics_mut(|graphics| graphics.entry(painter.layer_id()).next_idx())
}

/// 이미 발행한 shape 구간을 제자리 변환하되, `skip`에 적힌 인덱스는 건너뛴다
/// (`PaintList::transform_range`).
///
/// 전용 레이어를 만들거나 부모/이웃 shape를 건드리지 않는다 — 구간 밖(터미널 배경 rect,
/// 이웃 위젯)은 화면 좌표 그대로 남는다. `transform_range`는 각 shape의 `clip_rect`도
/// 함께 변환하므로, 구간에 넣을 shape는 **역변환된 논리 clip**으로 발행해야 한다.
///
/// `skip`은 축소 갤리를 **이미 화면 좌표로** 그린 텍스트 shape다(`RowTextRun::scaled_galley`).
/// 여기서 다시 변환하면 두 번 축소된다. 오직 그것만 빼며 preedit 같은 나머지 텍스트는
/// 구간 안에 남는다 — 모든 Text를 빼면 조합 중 글자가 축소되지 않아 어긋난다.
/// `skip`은 발행 순서대로 오름차순이라 사이 구간만 차례로 변환하면 되고, 그래픽 락은
/// 한 번만 잡는다.
fn transform_grid_shapes(
    painter: &egui::Painter,
    start: egui::layers::ShapeIdx,
    end: egui::layers::ShapeIdx,
    skip: &[egui::layers::ShapeIdx],
    transform: egui::emath::TSTransform,
) {
    painter.ctx().graphics_mut(|graphics| {
        let list = graphics.entry(painter.layer_id());
        let mut range_start = start;
        for skipped in skip {
            if range_start.0 < skipped.0 {
                list.transform_range(range_start, *skipped, transform);
            }
            range_start = egui::layers::ShapeIdx(skipped.0 + 1);
        }
        if range_start.0 < end.0 {
            list.transform_range(range_start, end, transform);
        }
    });
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
    /// egui의 빈 갤리 캐시를 글꼴 저장소 세대 표식으로 쓴다. 배율·폰트·atlas가
    /// 바뀌면 새 Arc가 반환되므로 오래된 글리프 UV/크기를 재사용하지 않는다.
    font_cache_key: Option<Arc<egui::Galley>>,
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
        self.font_cache_key = None;
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

    fn prepare(
        &mut self,
        snapshot: &TerminalViewportSnapshot,
        font_size: f32,
        generation: u64,
        font_cache_key: Arc<egui::Galley>,
    ) {
        self.counters = RenderCounters::default();
        self.last_gen = Some(generation);
        let font_size_bits = font_size.to_bits();
        let shape_changed = self.cols != snapshot.cols
            || self.rows != snapshot.rows
            || self.scroll_offset != snapshot.scroll_offset
            || self.is_alt_screen != snapshot.is_alt_screen
            || self.font_size_bits != font_size_bits
            || self
                .font_cache_key
                .as_ref()
                .is_none_or(|key| !Arc::ptr_eq(key, &font_cache_key))
            || self.rows_cache.len() != snapshot.rows as usize;
        if shape_changed {
            self.cols = snapshot.cols;
            self.rows = snapshot.rows;
            self.scroll_offset = snapshot.scroll_offset;
            self.is_alt_screen = snapshot.is_alt_screen;
            self.font_size_bits = font_size_bits;
            self.font_cache_key = Some(font_cache_key);
            self.rows_cache.clear();
            self.rows_cache.resize_with(snapshot.rows as usize, || None);
        }
    }
}

struct RowRenderCache {
    bg_runs: Vec<RowBgRun>,
    text_runs: Vec<RowTextRun>,
    /// 밑줄·취소선은 갤리(TextFormat)가 아니라 셀 격자 위에 직접 긋는다(2026-08-21).
    /// 갤리에 맡기면 공백과 wide 문자마다 run이 끊겨 선이 토막나 보인다.
    underline_runs: Vec<RowBgRun>,
    strikeout_runs: Vec<RowBgRun>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RowBgRun {
    start_col: usize,
    end_col: usize,
    color: egui::Color32,
}

struct RowTextRun {
    col: usize,
    galley: Arc<egui::Galley>,
    color: egui::Color32,
    /// 이 run의 **축소 갤리 하나**와 그것을 만든 배율(`f32::to_bits`).
    ///
    /// `TextShape::transform`은 `Arc::make_mut`으로 갤리를 깊은 복제한다
    /// (epaint 0.35 `shapes/text_shape.rs:110-168`). 구간 변환에 그대로 맡기면 축소된
    /// pane이 **idle일 때도 프레임마다 전 행을 복제**한다. 배율이 같으면 여기 보관한
    /// 것을 Arc 클론으로 재사용한다.
    ///
    /// 보관은 run당 최대 1개다 — 배율이 바뀌면 교체되고, 축소가 없는 프레임(scale == 1)
    /// 에서는 해제한다. 그래서 배율을 계속 바꿔도 무한히 쌓이지 않는다.
    scaled: Option<(u32, Arc<egui::Galley>)>,
}

impl RowTextRun {
    /// `scale` 배율의 축소 갤리 — 같은 배율이면 Arc 클론만 돌려준다(복제 없음).
    ///
    /// 원본 `self.galley`는 건드리지 않는다: `TextShape`에 **클론한 Arc**를 넣고
    /// 변환하므로 `make_mut`이 사본을 만들고 원본 갤리와 행 캐시는 그대로다.
    fn scaled_galley(&mut self, scale: f32) -> Arc<egui::Galley> {
        let scale_bits = scale.to_bits();
        if let Some((cached_bits, cached)) = &self.scaled
            && *cached_bits == scale_bits
        {
            return Arc::clone(cached);
        }
        // 원점에서 크기만 줄인다 — 화면 위치는 그릴 때 transform.mul_pos로 준다.
        // (원점 스케일 + 그때의 이동 = 구간 변환 전체를 적용한 것과 같은 기하다.)
        let mut shape =
            egui::epaint::TextShape::new(egui::Pos2::ZERO, Arc::clone(&self.galley), self.color);
        shape.transform(egui::emath::TSTransform::from_scaling(scale));
        let scaled = shape.galley;
        self.scaled = Some((scale_bits, Arc::clone(&scaled)));
        scaled
    }

    /// 축소 캐시를 버린다 — 비축소 프레임에서 쓰지 않는 사본을 들고 있지 않는다.
    fn clear_scaled(&mut self) {
        self.scaled = None;
    }
}

/// bold 셀에 쓸 모노 굵은 폰트 패밀리 이름 (B-1). 앱(fonts.rs)이 같은 이름으로 등록한다.
///
/// **미등록이면 egui는 폴백하지 않고 패닉한다** — `FontFamily::Name`이 어떤 폰트에도
/// 묶여 있지 않으면 epaint가 `panic!("FontFamily::{{family:?}} is not bound to any fonts")`로
/// 죽는다(egui 0.35 실측, 2026-08-21 리뷰). 예전 주석은 "기본 Monospace로 폴백하므로
/// 안전하다"고 적혀 있었으나 사실이 아니었다. 그래서 렌더러가 직접 등록 여부를 확인하고
/// 미등록이면 Monospace로 내려간다(`mono_bold_family_ready`).
pub const MONO_BOLD_FAMILY: &str = "mono_bold";

/// bold 패밀리가 실제로 등록돼 있는지 — 미등록 상태로 그리면 epaint가 패닉하므로,
/// 프레임마다 한 번 확인해 bold run의 폴백 여부를 정한다(2026-08-21).
/// 폰트 설정을 런타임에 바꿀 수 있어 한 번 캐시하지 않고 프레임마다 본다.
fn mono_bold_family_ready(ctx: &egui::Context) -> bool {
    ctx.fonts(|fonts| {
        fonts.families().iter().any(|family| {
            matches!(family, egui::FontFamily::Name(name) if name.as_ref() == MONO_BOLD_FAMILY)
        })
    })
}

/// 속성이 적용된 셀 텍스트 갤리를 만든다 (B-1). bold는 굵은 패밀리, italic은 egui가
/// 합성(기울임), underline/strikeout은 TextFormat의 선, dim은 색을 낮춘다.
fn layout_attr_text(
    painter: &egui::Painter,
    text: String,
    font_id: &egui::FontId,
    color: egui::Color32,
    attrs: CellAttrs,
    bold_family_ready: bool,
) -> Arc<egui::Galley> {
    if attrs.is_empty() {
        return layout_spaced(painter, text, font_id.clone(), color, false);
    }
    let mut font = font_id.clone();
    // 미등록 패밀리를 지정하면 epaint가 패닉한다 — 등록됐을 때만 바꾼다.
    if attrs.contains(CellAttrs::BOLD) && bold_family_ready {
        font.family = egui::FontFamily::Name(MONO_BOLD_FAMILY.into());
    }
    let color = if attrs.contains(CellAttrs::DIM) {
        dim_color(color)
    } else {
        color
    };
    // 밑줄은 여기서 긋지 않는다 — 셀 격자 위에 직접 그어야 공백/wide 문자에서
    // 끊기지 않는다(underline_runs).
    layout_spaced(
        painter,
        text,
        font,
        color,
        attrs.contains(CellAttrs::ITALIC),
    )
}

/// 자간을 반영한 갤리를 만든다 — 셀 폭과 같은 값을 써야 격자와 어긋나지 않는다.
fn layout_spaced(
    painter: &egui::Painter,
    text: String,
    font: egui::FontId,
    color: egui::Color32,
    italics: bool,
) -> Arc<egui::Galley> {
    let extra = extra_letter_spacing(font.size);
    let format = egui::TextFormat {
        font_id: font,
        extra_letter_spacing: extra,
        color,
        italics,
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
            fonts.glyph_width(&font_id, 'M') + extra_letter_spacing(metrics.font_size),
            fonts.row_height(&font_id) * metrics.line_height,
        )
    })
}

/// 글자 사이에 더하는 여백. 셀 폭(`cell_size`)과 갤리 레이아웃에 **같은 값**이 들어가야
/// run 안에서 글자가 자기 셀에서 밀리지 않는다.
fn extra_letter_spacing(font_size: f32) -> f32 {
    (font_size * TERMINAL_LETTER_SPACING_RATIO).round()
}

/// 폰트 크기 대비 자간 비율.
///
/// **0이어야 한다 — 모노 격자에 가로 자간을 더하면 한글·CJK가 반드시 깨진다**
/// (2026-09-03 사용자 신고: grok 에이전트 한글이 글자마다 벌어짐).
///
/// 자간 `s`는 글자 **뒤에** 붙는 여백이라 셀 폭은 `M + s`가 되는데, wide 글자의 상자는
/// 2칸이라 `2M + 2s`인 반면 글리프 advance는 `2M + s`에 그친다. 그래서 한글끼리의
/// 간격만 `2s`가 되어 라틴(`s`)의 **정확히 두 배**로 벌어진다. D2Coding 13.5pt 실측:
/// 라틴 6.75+1=7.75(셀 폭과 일치, 간격 1px), 한글 13.5+1=14.5 in 15.5(간격 2px).
///
/// 글리프를 가로로만 늘리지 않는 한 이 배수는 없앨 수 없고, 그래서 실제 터미널들도
/// 이 값을 0으로 둔다 — cmux(manaflow-ai/cmux)가 쓰는 xterm.js v6도 `letterSpacing`
/// 기본값이 0이고 cmux는 이 옵션을 아예 설정하지 않는다.
///
/// 글자가 답답하면 **세로 여백(`line_height`)이나 폰트 크기**로 조절한다 — 둘 다 격자의
/// 1:2 관계를 깨지 않는다.
const TERMINAL_LETTER_SPACING_RATIO: f32 = 0.0;

/// 밑줄·취소선 두께와, 밑줄을 글자 블록 바닥에서 끌어올리는 양.
const UNDERLINE_THICKNESS: f32 = 1.0;
const UNDERLINE_LIFT: f32 = 2.0;
/// 취소선을 글자 블록 높이의 어디에 둘지(위에서부터의 비율).
const STRIKEOUT_HEIGHT_RATIO: f32 = 0.55;

/// 셀 격자 위에 가로선 런을 긋는다 — 밑줄과 취소선이 공유한다(2026-08-21).
/// 갤리에 맡기지 않는 이유는 `RowRenderCache::underline_runs` 주석 참고.
fn paint_cell_lines(
    painter: &egui::Painter,
    runs: &[RowBgRun],
    origin_x: f32,
    cell_width: f32,
    y: f32,
) {
    for run in runs {
        let rect = egui::Rect::from_min_max(
            egui::pos2(origin_x + run.start_col as f32 * cell_width, y),
            egui::pos2(
                origin_x + run.end_col as f32 * cell_width,
                y + UNDERLINE_THICKNESS,
            ),
        )
        .round_to_pixels(painter.pixels_per_point());
        painter.rect_filled(rect, 0.0, run.color);
    }
}

/// Character indices are the egui IME contract, not UTF-8 bytes or UTF-16 units.
#[derive(Clone, Copy, Debug)]
pub struct PreeditView<'a> {
    pub text: &'a str,
    pub active_range_chars: Option<&'a std::ops::Range<usize>>,
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
    draw_with_preedit(
        ui,
        snapshot,
        metrics,
        cache,
        preedit.map(|text| PreeditView {
            text,
            active_range_chars: None,
        }),
        ime_active,
        selection,
        snapshot_gen,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn draw_with_preedit(
    ui: &mut egui::Ui,
    snapshot: &TerminalViewportSnapshot,
    metrics: CellMetrics,
    cache: &mut TerminalRenderCache,
    preedit: Option<PreeditView<'_>>,
    ime_active: bool,
    selection: Option<(usize, usize)>,
    snapshot_gen: u64,
) -> RenderOutput {
    let font_id = egui::FontId::monospace(metrics.font_size);
    let cell = cell_size(ui.ctx(), metrics);
    let bold_family_ready = mono_bold_family_ready(ui.ctx());
    // 셀 안에서 글자를 세로 중앙에 둔다 — 안 그러면 넓힌 행간이 전부 글자 아래로만 몰린다.
    // cell.y는 글자 높이 × line_height라 나누면 원래 글자 높이가 되고(config에서 0.8 하한으로
    // clamp되어 0으로 나눌 일이 없다), 그 차이의 절반이 위쪽 여백이다.
    let text_dy = (cell.y - cell.y / metrics.line_height) * 0.5;
    // 글자 블록 높이(행간 배수를 뺀 순수 글자 높이) — 밑줄을 그 바로 아래에 둔다.
    let text_height = cell.y / metrics.line_height;
    // hit-test/응답 rect는 pane 영역을 넘지 않게 clamp한다 — split/resize 직후
    // stale(더 큰) snapshot이 이웃 pane의 클릭/스크롤을 가로채는 것 방지 (codex 리뷰).
    // 넘치는 셀은 아래 content_rect로 잘리며 좌우 여백을 침범하지 않는다.
    let avail = ui.available_size();
    let render_height = if avail.y.is_finite() {
        avail.y.max(0.0)
    } else {
        cell.y * snapshot.rows as f32
    };
    // 보존된 cols가 pane보다 넓으면 그리드만 균일 축소해 한 화면에 담는다(우측 열 잘림
    // 방지). font_size·cell·font_id는 **원래 값 그대로** 두고 마지막에 shape 구간만
    // 변환하므로 행 갤리 캐시와 글리프 아틀라스가 재사용된다.
    let scale = fit_width_scale(avail.x, cell.x, snapshot.cols);
    // Resize 적용 전 snapshot이 좁더라도 배경과 입력 영역은 pane 전체를 사용한다.
    let render_width = if avail.x.is_finite() {
        avail.x.max(0.0)
    } else {
        cell.x * snapshot.cols as f32 + HORIZONTAL_PADDING * 2.0
    };
    let size = egui::vec2(render_width, render_height);
    // click_and_drag: 클릭=포커스, 드래그=선택 (2026-07-05 복사 지원)
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    // 조합이 없을 때만 공식 소유권을 즉시 되찾는다. egui의 request_focus는 현재 조합을
    // interrupt하므로, 진행 중 preedit 동안 비-TextEdit 포커스가 한 프레임 튀었다고
    // 호출하면 macOS가 자모를 강제 commit한다. 진행 중 조합은 아래 IME output을 유지한
    // 채 호출측이 논리적 입력 소유권으로 계속 소비하고, commit 뒤 다음 프레임에 복귀한다.
    // 호출측 `preedit`은 draw 뒤에 이벤트를 읽어 채우므로 조합이 **시작되는 프레임**에는
    // 아직 비어 있다. 그 한 프레임의 공백만 보고 request_focus를 부르면 egui가
    // `Memory::interrupt_ime`를 켜고, egui-winit이 그걸 `set_ime_allowed(false)/(true)`로
    // 바꾼다. winit macOS는 그때 marked_text를 비우고 `ImeState::Disabled`로 래치하는데
    // `(true)`는 상태를 되돌리지 않아, 아직 조합을 들고 있는 macOS IM의 다음 커밋이
    // `Ime::Commit` 없이 원시 키로 새어 나간다. 이번 프레임 입력도 함께 본다.
    let continues_preedit = ime_active
        && (preedit.is_some_and(|preedit| !preedit.text.is_empty())
            || frame_has_active_preedit(ui.ctx()));
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
    // 두 painter는 **같은 레이어**를 쓴다(PaintList가 하나라 발행 순서가 그대로 유지된다).
    // screen_painter: 화면 좌표 clip — 축소 갤리를 이미 줄여서 그리므로 변환 대상이 아니다.
    // painter: 아래에서 논리(역변환) clip으로 바뀐다 — 나머지 그리드가 쓴다.
    let screen_painter = background_painter.with_clip_rect(content_rect);
    let mut painter = screen_painter.clone();
    let origin = content_rect.min;
    // 그리드는 **축소 전 좌표**(원래 cell·origin)로 그린 뒤 이 변환으로 한 번에 줄인다.
    // scale == 1이면 항등이라 아래 경로가 전부 기존 동작 그대로다.
    let transform = grid_fit_transform(origin, scale);
    if scale < 1.0 {
        // transform_range는 shape의 clip_rect도 함께 변환한다. 화면에서 원하는 clip은
        // 위 `content_rect ∩ 부모 clip`이므로, 변환 전에는 그 **역상**을 들고 있어야 한다.
        // with_clip_rect는 부모 clip과 다시 교집합을 내 역상(축소의 역이라 더 넓은 rect)을
        // 도로 깎아버리므로, 교집합 없이 세팅하는 set_clip_rect를 쓴다.
        painter.set_clip_rect(transform.inverse().mul_rect(painter.clip_rect()));
    }
    let default_bg = TERMINAL_SURFACE_BG;
    let selection = selection.and_then(|(a, b)| normalize_selection_range(snapshot, a, b));
    // 터미널 배경은 pane 전체를 덮는 화면 좌표 rect다 — **변환 구간 밖**이어야 축소해도
    // 우측/하단에 작업면이 아닌 앱 크롬이 비치지 않는다.
    background_painter.rect_filled(rect, 0.0, default_bg);
    // 여기부터 발행하는 shape(배경 런·글자·선택·밑줄·커서·preedit)만 변환 대상이다.
    // 축소할 때만 경계를 기록한다 — 비축소 경로에 그래픽 락을 추가하지 않는다.
    let grid_shapes_start = (scale < 1.0).then(|| next_shape_idx(&painter));
    // 이미 화면 좌표로 그린 축소 갤리들의 인덱스(발행 순서 오름차순). 축소할 때만
    // 채우므로 비축소 경로에서는 할당이 일어나지 않는다(`Vec::new`는 힙을 잡지 않는다).
    let mut scaled_text_shapes: Vec<egui::layers::ShapeIdx> = Vec::new();

    let dirty_fresh = cache.dirty_is_fresh(snapshot_gen);
    // line_height는 갤리 shaping에 영향을 주지 않는다(글자를 그리는 y 위치만 바뀐다) —
    // 캐시 무효화 기준은 font_size 그대로다.
    // 빈 job은 문자열/섹션 버퍼를 할당하지 않고, egui가 같은 배율에서 같은 Arc를
    // 재사용한다. 글자 atlas 재생성도 감지하되 정상 idle 프레임은 행을 다시 만들지 않는다.
    let font_cache_key = painter.layout_job(egui::text::LayoutJob::default());
    cache.prepare(snapshot, metrics.font_size, snapshot_gen, font_cache_key);
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
            let row_cache = build_row_cache(
                &painter,
                snapshot,
                row,
                &font_id,
                SNAPSHOT_DEFAULT_BG,
                bold_family_ready,
                cell.x,
            );
            if let Some(slot) = cache.rows_cache.get_mut(row) {
                *slot = Some(row_cache);
                cache.counters.rows_rebuilt += 1;
            }
        }

        // 행의 축소 갤리 캐시를 갱신해야 하므로 가변 참조로 받는다(원본 갤리는 불변).
        if let Some(row_cache) = cache
            .rows_cache
            .get_mut(row)
            .and_then(|cached| cached.as_mut())
        {
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
            for run in &mut row_cache.text_runs {
                let pos = origin + egui::vec2(run.col as f32 * cell.x, row_y + text_dy);
                if scale < 1.0 {
                    // 축소 갤리는 **화면 좌표로 직접** 그리고 아래 구간 변환에서 뺀다 —
                    // 매 프레임 galley를 깊은 복제하지 않기 위해서다. 위치는 논리 좌표를
                    // 변환한 값이라 나머지 그리드와 정확히 같은 화면 좌표에 놓인다.
                    let scaled = run.scaled_galley(scale);
                    // 빈 갤리는 painter.galley와 마찬가지로 아무것도 발행하지 않는다 —
                    // 발행하지 않은 것을 skip 목록에 넣으면 인덱스가 밀린다.
                    if !scaled.is_empty() {
                        let idx = screen_painter.add(egui::Shape::Text(
                            egui::epaint::TextShape::new(transform.mul_pos(pos), scaled, run.color),
                        ));
                        scaled_text_shapes.push(idx);
                    }
                } else {
                    // 비축소 프레임에서는 원본 갤리를 그대로 쓰고 축소 사본을 버린다.
                    run.clear_scaled();
                    painter.galley(pos, Arc::clone(&run.galley), run.color);
                }
            }
            // 밑줄·취소선은 셀 경계까지 이어 긋는다 — 글자 아래/한가운데.
            let text_top = origin.y + row_y + text_dy;
            paint_cell_lines(
                &painter,
                &row_cache.underline_runs,
                origin.x,
                cell.x,
                text_top + text_height - UNDERLINE_LIFT,
            );
            paint_cell_lines(
                &painter,
                &row_cache.strikeout_runs,
                origin.x,
                cell.x,
                text_top + text_height * STRIKEOUT_HEIGHT_RATIO,
            );
            cache.counters.rows_painted += 1;
            cache.counters.shapes += row_cache.bg_runs.len()
                + row_cache.text_runs.len()
                + row_cache.underline_runs.len()
                + row_cache.strikeout_runs.len()
                + selection_shapes;
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
        let mut ime_cursor = egui::Rect::from_min_size(cursor_pos, cell);
        if let Some(preedit) = preedit.filter(|preedit| !preedit.text.is_empty()) {
            let galley =
                painter.layout_no_wrap(preedit.text.to_owned(), font_id, egui::Color32::BLACK);
            let chars = preedit.text.chars().count();
            let active = preedit.active_range_chars.map(|range| {
                let start = range.start.min(chars);
                start..range.end.min(chars).max(start)
            });
            let caret = galley.pos_from_cursor(egui::text::CCursor::new(
                active.as_ref().map_or(0, |range| range.end),
            ));
            let clip = painter.clip_rect();
            let mut text_pos = cursor_pos + egui::vec2(0.0, text_dy);
            // Shift short compositions left at the pane edge. For a longer
            // composition, scroll its visual window so the internal caret fits.
            if galley.rect.width() <= clip.width() {
                text_pos.x = text_pos
                    .x
                    .min(clip.right() - galley.rect.width())
                    .max(clip.left());
            } else {
                text_pos.x = text_pos
                    .x
                    .min(clip.right() - caret.left() - cell.x.min(clip.width()));
            }
            let background = galley.rect.translate(text_pos.to_vec2()).intersect(clip);
            painter.rect_filled(background, 0.0, egui::Color32::from_rgb(0xd8, 0xd8, 0xd8));
            if let Some(range) = &active {
                if !range.is_empty() {
                    let start = galley.pos_from_cursor(egui::text::CCursor::new(range.start));
                    let selected = egui::Rect::from_min_max(
                        egui::pos2(start.left(), start.top()),
                        egui::pos2(caret.left(), caret.bottom()),
                    )
                    .translate(text_pos.to_vec2())
                    .intersect(clip);
                    painter.rect_filled(selected, 0.0, egui::Color32::from_rgb(0x9a, 0xbc, 0xe8));
                    cache.counters.shapes += 1;
                }
                let caret_pos = text_pos + caret.min.to_vec2();
                painter.line_segment(
                    [caret_pos, caret_pos + egui::vec2(0.0, caret.height())],
                    egui::Stroke::new(1.0, egui::Color32::BLACK),
                );
                cache.counters.shapes += 1;
            }
            let caret_x = (text_pos.x + caret.left()).clamp(
                clip.left(),
                (clip.right() - cell.x.min(clip.width())).max(clip.left()),
            );
            ime_cursor = egui::Rect::from_min_size(
                egui::pos2(caret_x, cursor_pos.y),
                egui::vec2(cell.x.min(clip.width()), cell.y),
            )
            .intersect(clip);
            painter.galley(text_pos, galley, egui::Color32::BLACK);
            painter.line_segment(
                [background.left_bottom(), background.right_bottom()],
                egui::Stroke::new(1.5, egui::Color32::BLACK),
            );
            cache.counters.shapes += 3; // One galley, background, composition underline.
        }
        ui.ctx().output_mut(|o| {
            o.ime = Some(egui::output::IMEOutput {
                purpose: egui::IMEPurpose::Terminal,
                // 터미널 영역은 이미 화면 좌표다(축소 대상이 아니다).
                rect,
                // 커서 rect는 축소 전 좌표로 계산했으므로 후보창이 뜰 화면 좌표로 옮긴다 —
                // shape가 아니라 output이라 transform_range가 닿지 않는다. scale == 1이면
                // 항등이라 기존 값과 같다.
                cursor_rect: transform.mul_rect(ime_cursor),
                should_interrupt_composition: false,
            });
        });
    }

    // 그리드 shape 구간만 균일 축소한다. scale == 1이면 변환 자체를 건너뛰어 기존
    // geometry(와 비용)를 그대로 둔다. 이미 축소해서 화면 좌표로 그린 글자 shape는
    // 구간에서 빼고, preedit을 포함한 나머지는 그대로 변환한다.
    if let Some(grid_shapes_start) = grid_shapes_start {
        let grid_shapes_end = next_shape_idx(&painter);
        transform_grid_shapes(
            &painter,
            grid_shapes_start,
            grid_shapes_end,
            &scaled_text_shapes,
            transform,
        );
    }

    RenderOutput {
        response,
        // 호출측의 포인터→셀 변환·drop marker가 쓰는 값이라 **화면에 보이는** 셀 크기다.
        cell_size: cell * scale,
        // 변환의 고정점이라 축소해도 그대로다.
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

/// 이번 프레임 raw 입력이 **진행 중인** IME 조합을 나타내는지.
///
/// 비어 있지 않은 `Preedit`만 조합으로 센다. 조합이 끝나며 오는 `Preedit("")`
/// (winit의 `unmarkText`와 커밋 경로가 낸다)와 `Commit`만 남은 프레임은 조합 중이
/// 아니므로 포커스 복구를 막지 않아야 한다 — 막으면 터미널이 egui 포커스를 영영
/// 되찾지 못한다. 호출측이 `ime_active`로 이미 TextEdit·팝업 소유 프레임을 걸러내므로
/// 이 판정이 다른 입력창의 조합을 가로채지 않는다.
pub fn frame_has_active_preedit(ctx: &egui::Context) -> bool {
    ctx.input(|input| {
        input.raw.events.iter().any(|event| {
            matches!(
                event,
                egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) if !text.is_empty()
            )
        })
    })
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

// Only scripts with terminal two-cell scalar semantics share a wide run.
// Emoji and symbols may use a different fallback face; keep them independent.
fn is_cjk_scalar(c: char) -> bool {
    matches!(c as u32, 0x1100..=0x11ff | 0x2e80..=0xa4cf | 0xac00..=0xd7af
        | 0xf900..=0xfaff | 0xfe10..=0xfe6f | 0xff00..=0xff60 | 0x20000..=0x323af)
}

fn build_row_cache(
    painter: &egui::Painter,
    snapshot: &TerminalViewportSnapshot,
    row: usize,
    font_id: &egui::FontId,
    default_bg: egui::Color32,
    bold_family_ready: bool,
    cell_width: f32,
) -> RowRenderCache {
    let cols = snapshot.cols as usize;
    let row_start = row * cols;
    let row_end = row_start + cols;
    let Some(cells) = snapshot.visible_cells.get(row_start..row_end) else {
        return RowRenderCache {
            bg_runs: Vec::new(),
            text_runs: Vec::new(),
            underline_runs: Vec::new(),
            strikeout_runs: Vec::new(),
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

    let (underline_runs, strikeout_runs) = build_line_runs(cells, cols);

    let mut text_runs = Vec::new();
    let mut pending = PendingTextRun::default();
    for (col, term_cell) in cells.iter().enumerate() {
        // A trailing spacer belongs to the preceding wide glyph and must not
        // interrupt its run. A leading wrap filler has no owner on this row.
        if term_cell.wide_spacer && col > 0 && cells[col - 1].wide {
            continue;
        }
        if term_cell.wide_spacer || term_cell.c == ' ' {
            pending.flush(
                &mut text_runs,
                painter,
                font_id,
                bold_family_ready,
                cell_width,
            );
            continue;
        }

        let fg = rgb(term_cell.fg);
        let attrs = term_cell.attrs;
        let width_cols = if term_cell.wide { 2 } else { 1 };
        // Width-class boundaries keep the fitting contract uniform. epaint
        // handles fallback fonts inside a galley; each scalar still receives
        // the terminal's exact one/two-cell advance below.
        let independent = term_cell.wide && !is_cjk_scalar(term_cell.c);
        if independent || pending.needs_flush(col, fg, attrs, width_cols) {
            pending.flush(
                &mut text_runs,
                painter,
                font_id,
                bold_family_ready,
                cell_width,
            );
        }
        pending.push(col, display_char(term_cell.c), fg, attrs, width_cols);
        if independent {
            pending.flush(
                &mut text_runs,
                painter,
                font_id,
                bold_family_ready,
                cell_width,
            );
        }
    }

    pending.flush(
        &mut text_runs,
        painter,
        font_id,
        bold_family_ready,
        cell_width,
    );

    RowRenderCache {
        bg_runs,
        text_runs,
        underline_runs,
        strikeout_runs,
    }
}

#[derive(Default)]
struct PendingTextRun {
    start_col: usize,
    next_col: usize,
    color: Option<egui::Color32>,
    /// run은 색뿐 아니라 **속성이 같을 때만** 이어진다 (B-1).
    attrs: CellAttrs,
    text: String,
    width_cols: usize,
}

impl PendingTextRun {
    fn needs_flush(
        &self,
        col: usize,
        color: egui::Color32,
        attrs: CellAttrs,
        width_cols: usize,
    ) -> bool {
        self.color.is_some()
            && (self.color != Some(color)
                || self.attrs != attrs
                || self.next_col != col
                || self.width_cols != width_cols)
    }

    fn push(
        &mut self,
        col: usize,
        ch: char,
        color: egui::Color32,
        attrs: CellAttrs,
        width_cols: usize,
    ) {
        if self.color.is_none() {
            self.start_col = col;
            self.next_col = col;
            self.color = Some(color);
            self.attrs = attrs;
            self.width_cols = width_cols;
        }
        self.text.push(ch);
        self.next_col = col + width_cols;
    }

    fn flush(
        &mut self,
        text_runs: &mut Vec<RowTextRun>,
        painter: &egui::Painter,
        font_id: &egui::FontId,
        bold_family_ready: bool,
        cell_width: f32,
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
            galley: fit_galley_to_cells(
                layout_attr_text(painter, text, font_id, color, attrs, bold_family_ready),
                cell_width * self.width_cols as f32,
            ),
            color,
            scaled: None,
        });
    }
}

/// 폰트의 advance 대신 backend가 정한 셀 폭으로 배치한다. D2Coding의 ① 같은
/// 기호는 1셀 문자지만 폰트에서는 2셀 폭이라 뒤의 한글이나 다음 run을 침범한다.
/// 폭이 맞는 run은 원본 Arc를 그대로 쓰고, 보정은 행 캐시 생성 때 한 번만 한다.
/// 이 갤리는 painter 전용이며 선택·커서·복사는 원래 snapshot의 셀 좌표를 사용한다.
fn fit_galley_to_cells(mut galley: Arc<egui::Galley>, glyph_cell_width: f32) -> Arc<egui::Galley> {
    if !glyph_cell_width.is_finite()
        || glyph_cell_width <= 0.0
        || !galley.rows.iter().any(|row| {
            row.glyphs
                .iter()
                .any(|glyph| (glyph.advance_width - glyph_cell_width).abs() > 0.01)
        })
    {
        return galley;
    }

    let fitted = Arc::make_mut(&mut galley);
    fitted.mesh_bounds = egui::Rect::NOTHING;
    fitted.rect.max.x = fitted.rect.min.x;
    for placed in &mut fitted.rows {
        let row = Arc::make_mut(&mut placed.row);
        for (index, glyph) in row.glyphs.iter_mut().enumerate() {
            let old_pos = glyph.pos;
            let new_x = index as f32 * glyph_cell_width;
            if !glyph.uv_rect.is_nothing() {
                // epaint는 글리프 하나에 네 꼭짓점을 둔다. UV와 색은 유지한다.
                let start = glyph.first_vertex as usize;
                if let Some(vertices) = row.visuals.mesh.vertices.get_mut(start..start + 4) {
                    let mut left = 0.0_f32;
                    let mut right = glyph.advance_width;
                    for vertex in vertices.iter() {
                        left = left.min(vertex.pos.x - old_pos.x);
                        right = right.max(vertex.pos.x - old_pos.x);
                    }
                    // 넘치는 글자만 종횡비를 유지해 줄인다. 정상 글자는 위치만 맞춘다.
                    let scale = if glyph.advance_width > glyph_cell_width + 0.01 {
                        (glyph_cell_width / (right - left)).min(1.0)
                    } else {
                        1.0
                    };
                    let inset = if scale < 1.0 { -left * scale } else { 0.0 };
                    let center_y = old_pos.y - glyph.font_ascent * 0.5;
                    for vertex in vertices {
                        vertex.pos.x = new_x + inset + (vertex.pos.x - old_pos.x) * scale;
                        vertex.pos.y = center_y + (vertex.pos.y - center_y) * scale;
                    }
                    glyph.pos.y = center_y + (old_pos.y - center_y) * scale;
                    glyph.font_ascent *= scale;
                    glyph.font_height *= scale;
                    glyph.font_face_ascent *= scale;
                    glyph.font_face_height *= scale;
                }
            }
            glyph.pos.x = new_x;
            glyph.advance_width = glyph_cell_width;
        }
        row.size.x = row.glyphs.len() as f32 * glyph_cell_width;
        row.visuals.mesh_bounds = row.visuals.mesh.calc_bounds();
        fitted.mesh_bounds |= row.visuals.mesh_bounds.translate(placed.pos.to_vec2());
        fitted.rect.max.x = fitted.rect.max.x.max(placed.pos.x + row.size.x);
    }
    galley
}

/// 밑줄·취소선 런을 만든다(2026-08-21). 공백 셀도 SGR 속성을 물고 있으므로 함께 이어
/// 붙인다 — 그래야 「A. 사이드네비 상태」처럼 단어 사이에서 선이 끊기지 않는다.
///
/// `wide_spacer` 칸은 `bg_runs`와 똑같이 건너뛴다. 두 종류가 다 걸린다:
/// 소유자가 같은 행에 있는 **뒷칸**은 소유자가 이미 2칸을 덮었으니 다시 밀어넣으면
/// 겹치는 run이 생기고, 행 끝 **필러**(`LEADING_WIDE_CHAR_SPACER`)는 소유자가 다음 줄에
/// 있어 이 행엔 그릴 글자가 없는데도 pen 속성을 물고 있어 **빈 칸 아래 유령 밑줄**이
/// 그려진다. 선택 하이라이트가 2026-08-18에 같은 자리에서 같은 실수를 했다
/// (`selection_covers_cell` 주석 참고).
fn build_line_runs(cells: &[crate::TerminalCell], cols: usize) -> (Vec<RowBgRun>, Vec<RowBgRun>) {
    let mut underline_runs: Vec<RowBgRun> = Vec::new();
    let mut strikeout_runs: Vec<RowBgRun> = Vec::new();
    for (col, term_cell) in cells.iter().enumerate() {
        if term_cell.wide_spacer {
            continue;
        }
        let attrs = term_cell.attrs;
        if !attrs.contains(CellAttrs::UNDERLINE) && !attrs.contains(CellAttrs::STRIKEOUT) {
            continue;
        }
        let color = if attrs.contains(CellAttrs::DIM) {
            dim_color(rgb(term_cell.fg))
        } else {
            rgb(term_cell.fg)
        };
        let width_cols = if term_cell.wide { 2 } else { 1 };
        let end_col = (col + width_cols).min(cols);
        if attrs.contains(CellAttrs::UNDERLINE) {
            push_bg_run(&mut underline_runs, col, end_col, color);
        }
        if attrs.contains(CellAttrs::STRIKEOUT) {
            push_bg_run(&mut strikeout_runs, col, end_col, color);
        }
    }
    (underline_runs, strikeout_runs)
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
/// 이 셀이 선택 **강조**(칠하기) 대상인가. `previous_selected`는 같은 행의 직전 열이
/// 선택됐는지다.
///
/// wide 글자(한글·CJK)는 2칸을 쓰고, 뒷칸은 `wide_spacer`다. 끝점은
/// [`normalize_selection_endpoint`]가 자리 채움을 **앞 글자로 되돌리므로**, 자리 채움을
/// `end`로 판정하면 행 끝 wide 글자가 절반만 칠해진다 — 복사는 정상인데 선택이 끝까지
/// 안 된 것처럼 보인다(2026-08-17 사용자 보고). 자리 채움은 앞 칸이 선택됐는지로만
/// 판정해 글자 하나가 반쪽으로 칠해지는 일이 없게 한다.
fn selection_covers_cell(
    snapshot: &TerminalViewportSnapshot,
    index: usize,
    start: usize,
    end: usize,
    previous_selected: bool,
) -> bool {
    match snapshot.visible_cells.get(index) {
        // 행 끝 필러(`LEADING_WIDE_CHAR_SPACER`)는 앞 글자의 뒷칸이 **아니다** — 소유자가
        // 다음 줄에 있으므로 `end` 상한을 그대로 적용한다. 구분하지 않으면 CJK로 wrap되는
        // 행에서 강조가 한 칸 더 칠해진다(2026-08-18 리뷰 실측).
        Some(cell) if cell.wide_spacer => {
            if snapshot.is_trailing_wide_spacer(index) {
                previous_selected
            } else {
                index >= start && index <= end
            }
        }
        Some(_) => index >= start && index <= end,
        None => false,
    }
}

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
        let selected = selection_covers_cell(snapshot, index, start, end, run_start.is_some());
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

    /// 밑줄 런 테스트용 셀 — 문자/속성/wide 지정.
    fn line_cell(c: char, bits: u8, wide: bool, wide_spacer: bool) -> crate::TerminalCell {
        crate::TerminalCell {
            c,
            fg: [0xd8; 3],
            bg: [0x18, 0x18, 0x1c],
            wide,
            wide_spacer,
            attrs: CellAttrs(bits),
        }
    }

    #[test]
    fn bold_패밀리가_미등록이면_기본_모노로_내려간다() {
        // egui는 미등록 FontFamily::Name을 만나면 폴백하지 않고 패닉한다(0.35 실측).
        // 등록 여부를 확인해 내려가지 않으면 bold 셀을 그리는 순간 렌더가 죽는다.
        let ctx = egui::Context::default();
        let mut checked = false;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            let ready = mono_bold_family_ready(ui.ctx());
            assert!(
                !ready,
                "기본 Context에는 mono_bold가 등록돼 있지 않다 — 이 전제가 깨지면 테스트가 무의미하다"
            );
            // 폴백이 없으면 이 호출이 패닉한다.
            let galley = layout_attr_text(
                ui.painter(),
                "A".to_owned(),
                &egui::FontId::monospace(12.0),
                egui::Color32::WHITE,
                CellAttrs(CellAttrs::BOLD),
                ready,
            );
            assert_eq!(
                galley.job.sections[0].format.font_id.family,
                egui::FontFamily::Monospace,
                "미등록이면 Monospace로 내려가야 한다"
            );
            checked = true;
        }).drop_without_applying_deltas();
        assert!(checked, "프레임이 돌지 않으면 검증이 비어 있다");
    }

    #[test]
    fn 밑줄은_공백을_건너뛰지_않고_한_런으로_이어진다() {
        // 원래 결함: 갤리에 밑줄을 맡기면 공백마다 run이 끊겨 밑줄이 토막나 보였다.
        let u = CellAttrs::UNDERLINE;
        let cells = vec![
            line_cell('A', u, false, false),
            line_cell(' ', u, false, false),
            line_cell('B', u, false, false),
        ];

        let (underline, strikeout) = build_line_runs(&cells, 3);

        assert_eq!(underline.len(), 1, "공백에서 끊기면 안 된다: {underline:?}");
        assert_eq!(underline[0].start_col, 0);
        assert_eq!(underline[0].end_col, 3);
        assert!(strikeout.is_empty());
    }

    #[test]
    fn 밑줄은_wide_문자의_뒷칸을_중복으로_담지_않는다() {
        // wide 소유자가 이미 2칸을 덮으므로 뒷칸(wide_spacer)까지 밀어넣으면 겹치는
        // run이 생긴다 — bg_runs는 이 칸을 건너뛴다. 같은 규칙을 지켜야 한다.
        let u = CellAttrs::UNDERLINE;
        let cells = vec![
            line_cell('가', u, true, false),
            line_cell(' ', u, false, true),
            line_cell('나', u, true, false),
            line_cell(' ', u, false, true),
        ];

        let (underline, _) = build_line_runs(&cells, 4);

        assert_eq!(
            underline,
            vec![RowBgRun {
                start_col: 0,
                end_col: 4,
                color: egui::Color32::from_rgb(0xd8, 0xd8, 0xd8),
            }],
            "wide 두 글자는 겹침 없이 한 런이어야 한다"
        );
    }

    #[test]
    fn 밑줄은_행끝_필러에_유령선을_긋지_않는다() {
        // 행 끝 필러(LEADING_WIDE_CHAR_SPACER)는 소유자가 다음 줄에 있어 이 행엔 그릴
        // 글자가 없는데도 pen 속성을 물고 있다 — 빈 칸 아래 밑줄이 그려지면 안 된다.
        let u = CellAttrs::UNDERLINE;
        let cells = vec![
            line_cell('A', u, false, false),
            line_cell(' ', u, false, true),
        ];

        let (underline, _) = build_line_runs(&cells, 2);

        assert_eq!(
            underline,
            vec![RowBgRun {
                start_col: 0,
                end_col: 1,
                color: egui::Color32::from_rgb(0xd8, 0xd8, 0xd8),
            }],
            "필러 칸까지 선이 넘어가면 안 된다"
        );
    }

    #[test]
    fn 취소선은_밑줄과_독립적으로_런을_만든다() {
        let cells = vec![
            line_cell('A', CellAttrs::UNDERLINE, false, false),
            line_cell('B', CellAttrs::STRIKEOUT, false, false),
        ];

        let (underline, strikeout) = build_line_runs(&cells, 2);

        assert_eq!(underline.len(), 1);
        assert_eq!((underline[0].start_col, underline[0].end_col), (0, 1));
        assert_eq!(strikeout.len(), 1);
        assert_eq!((strikeout[0].start_col, strikeout[0].end_col), (1, 2));
    }

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
        ctx: &egui::Context,
        cache: &mut TerminalRenderCache,
        snapshot: &TerminalViewportSnapshot,
    ) -> usize {
        draw_gen_for_test(ctx, cache, snapshot, next_gen())
    }

    /// 세대를 명시해 draw — 같은 세대 재draw(= 같은 스냅샷 repaint)를 재현한다.
    fn draw_gen_for_test(
        ctx: &egui::Context,
        cache: &mut TerminalRenderCache,
        snapshot: &TerminalViewportSnapshot,
        generation: u64,
    ) -> usize {
        ctx.run_ui(egui::RawInput::default(), |ui| {
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
        })
        .drop_without_applying_deltas();
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
            let mut full = ctx.run_ui(raw.clone(), |ui| {
                draw(ui, &s, m(13.0, 1.0), &mut c, None, false, None, next_gen());
            });
            full.textures_delta.clear();
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
                let mut full = ctx.run_ui(raw.clone(), |ui| {
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
                full.textures_delta.clear();
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
            let mut full = ctx.run_ui(raw.clone(), |ui| {
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
            full.textures_delta.clear();
            let _ = tessellate_ms(&ctx, full);
            let (mut cd_ms, mut ct_ms) = (0.0, 0.0);
            for _ in 0..ITERS {
                let t = Instant::now();
                let mut full = ctx.run_ui(raw.clone(), |ui| {
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
                full.textures_delta.clear();
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
    fn cjk_runs_keep_emoji_fallback_in_an_independent_run() {
        let snapshot = backend_snap("한글🚀가나");
        let ctx = egui::Context::default();
        let mut cache = TerminalRenderCache::default();
        draw_for_test(&ctx, &mut cache, &snapshot);
        let row = cache.rows_cache[0].as_ref().unwrap();
        assert_eq!(
            row.text_runs
                .iter()
                .map(|run| run.galley.text())
                .collect::<Vec<_>>(),
            vec!["한글", "🚀", "가나"]
        );
    }

    #[test]
    fn cjk_runs_batch_owned_spacers_and_preserve_two_cell_advances() {
        let snapshot = backend_snap("한글가나다");
        let ctx = egui::Context::default();
        let mut cache = TerminalRenderCache::default();
        draw_for_test(&ctx, &mut cache, &snapshot);
        let row = cache.rows_cache[0].as_ref().unwrap();
        assert_eq!(
            row.text_runs.len(),
            1,
            "same-style CJK should share a galley"
        );
        let galley = &row.text_runs[0].galley;
        let glyphs = &galley.rows[0].glyphs;
        let width = glyphs[0].advance_width;
        for (index, glyph) in glyphs.iter().enumerate() {
            assert!((glyph.pos.x - index as f32 * width).abs() < 0.01);
        }
        assert_eq!(selection_text(&snapshot, 0, 9), "한글가나다");
    }

    #[test]
    fn cjk_runs_stop_at_ascii_style_and_unowned_spacer_boundaries() {
        let snapshot = backend_snap("한글A가나\x1b[31m다라");
        let ctx = egui::Context::default();
        let mut cache = TerminalRenderCache::default();
        draw_for_test(&ctx, &mut cache, &snapshot);
        let row = cache.rows_cache[0].as_ref().unwrap();
        assert_eq!(
            row.text_runs.iter().map(|run| run.col).collect::<Vec<_>>(),
            vec![0, 4, 5, 9]
        );
        assert_eq!(
            row.text_runs
                .iter()
                .map(|run| run.galley.text())
                .collect::<Vec<_>>(),
            vec!["한글", "A", "가나", "다라"]
        );
    }

    #[test]
    fn terminal_좌우_내부여백은_각각_3픽셀이다() {
        assert_eq!(HORIZONTAL_PADDING, 3.0);
        assert_eq!(grid_width_for_available(100.0), 94.0);

        let snapshot = snap(4, 1, &["test"]);
        let ctx = egui::Context::default();
        let mut cache = TerminalRenderCache::default();
        let mut measured = None;
        ctx.run_ui(egui::RawInput::default(), |ui| {
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
        })
        .drop_without_applying_deltas();

        let (rect, origin, cell) = measured.expect("terminal should be rendered");
        let content = terminal_content_rect(rect);
        assert!((origin.x - rect.left() - 3.0).abs() < f32::EPSILON);
        assert!((rect.right() - content.right() - 3.0).abs() < 0.01);
        assert!(origin.x + cell.x * snapshot.cols as f32 <= content.right());
    }

    #[test]
    fn ime_preedit_is_laid_out_once_and_fits_at_right_edge() {
        let mut snapshot = snap(8, 1, &[""]);
        snapshot.cursor.col = 7;
        let ctx = egui::Context::default();
        let mut cache = TerminalRenderCache::default();
        let drawn = draw_in_pane(&ctx, &mut cache, &snapshot, 80.0, next_gen(), None, None);
        let text_shapes = drawn
            .shapes
            .iter()
            .filter(|shape| {
                matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "한")
            })
            .count();
        assert_eq!(text_shapes, 1, "preedit must issue one text shape");
        let background = preedit_box(&drawn.shapes);
        assert!(background.right() <= drawn.rect.right() - HORIZONTAL_PADDING + 0.01);
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
        let mut full = ctx.run_ui(egui::RawInput::default(), |ui| {
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
        full.textures_delta.clear();
        assert!(owns_ime_events, "터미널이 egui IME 소유자가 되어야 한다");
        assert_eq!(
            full.platform_output.ime.expect("터미널 IME 출력").purpose,
            egui::IMEPurpose::Terminal,
            "터미널 입력은 일반 문서 입력과 구분해야 한다"
        );
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
        let mut full = ctx.run_ui(egui::RawInput::default(), |ui| {
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
        full.textures_delta.clear();
        assert!(full.platform_output.ime.is_none());
    }

    #[test]
    fn 조합중_비textedit_포커스전이는_ime를_중단하지_않는다() {
        let ctx = egui::Context::default();
        let mut cache = TerminalRenderCache::default();
        let snapshot = snap(4, 1, &["test"]);

        ctx.run_ui(egui::RawInput::default(), |ui| {
            let transient = ui.button("transient focus");
            transient.request_focus();
        })
        .drop_without_applying_deltas();
        let mut full = ctx.run_ui(egui::RawInput::default(), |ui| {
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
        full.textures_delta.clear();

        let ime = full
            .platform_output
            .ime
            .expect("진행 중 조합은 IME allowance를 유지해야 한다");
        assert!(
            !ime.should_interrupt_composition,
            "조합 중 request_focus는 egui가 IME 강제 중단으로 바꾼다"
        );
    }

    /// 조합이 **이번 프레임에 막 시작된** 경우, 호출측 `preedit`은 아직 비어 있다
    /// (UI는 draw 뒤에 이벤트를 읽어 다음 프레임에야 채운다). 그 한 프레임의 공백을
    /// 근거로 `request_focus`를 부르면 egui가 `Memory::interrupt_ime`를 켜고,
    /// egui-winit이 `set_ime_allowed(false)/(true)`로 바꾼다. winit macOS는 그때
    /// marked_text를 비우고 `ImeState::Disabled`를 걸어두는데 `set_ime_allowed(true)`는
    /// 상태를 되돌리지 않는다. macOS IM은 계속 조합 중이므로 다음 `insertText:`가
    /// `hasMarkedText() == false`를 만나 `Ime::Commit`을 못 내고, 원시
    /// `NSEvent.characters`(한글 입력 소스에서는 자모)가 그대로 키 입력으로 나간다 —
    /// 빠르게 칠 때 "ㄱㅏ"로 갈라지는 경로다.
    #[test]
    fn 이번_프레임에_시작된_조합은_포커스_복구가_중단시키지_않는다() {
        let ctx = egui::Context::default();
        let mut cache = TerminalRenderCache::default();
        let snapshot = snap(4, 1, &["test"]);

        // 다른 위젯이 포커스를 쥔 상태 — 터미널은 논리적 키보드 소유자지만 egui의
        // 공식 소유자가 아니라 draw가 포커스를 되찾으려 한다.
        ctx.run_ui(egui::RawInput::default(), |ui| {
            let transient = ui.button("transient focus");
            transient.request_focus();
        })
        .drop_without_applying_deltas();

        let input = egui::RawInput {
            events: vec![egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "ㄱ".into(),
                active_range_chars: None,
            })],
            ..Default::default()
        };
        let mut full = ctx.run_ui(input, |ui| {
            ui.set_min_size(egui::vec2(500.0, 200.0));
            let _ = ui.button("transient focus");
            draw(
                ui,
                &snapshot,
                m(13.0, 1.0),
                &mut cache,
                // 호출측 preedit은 한 프레임 늦으므로 아직 비어 있다.
                None,
                true,
                None,
                next_gen(),
            );
        });
        full.textures_delta.clear();

        let ime = full
            .platform_output
            .ime
            .expect("키보드 소유 터미널은 IME allowance를 유지해야 한다");
        assert!(
            !ime.should_interrupt_composition,
            "이번 프레임에 시작된 조합을 포커스 복구가 강제 중단시켰다"
        );
    }

    /// 조합이 **끝나는** 프레임(`Preedit("")` + `Commit`)까지 조합 중으로 세면 터미널이
    /// egui 포커스를 영영 되찾지 못한다. 그 프레임에는 포커스 복구가 그대로 일어나야 한다.
    #[test]
    fn 조합이_끝난_프레임은_포커스_복구를_막지_않는다() {
        let ctx = egui::Context::default();
        let mut cache = TerminalRenderCache::default();
        let snapshot = snap(4, 1, &["test"]);

        ctx.run_ui(egui::RawInput::default(), |ui| {
            let transient = ui.button("transient focus");
            transient.request_focus();
        })
        .drop_without_applying_deltas();

        let input = egui::RawInput {
            events: vec![
                egui::Event::Ime(egui::ImeEvent::Preedit {
                    text: String::new(),
                    active_range_chars: None,
                }),
                egui::Event::Ime(egui::ImeEvent::Commit("가".into())),
            ],
            ..Default::default()
        };
        let mut owns_ime_events = false;
        let mut full = ctx.run_ui(input, |ui| {
            ui.set_min_size(egui::vec2(500.0, 200.0));
            let _ = ui.button("transient focus");
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
        full.textures_delta.clear();

        assert!(
            owns_ime_events,
            "조합이 끝난 프레임에서는 터미널이 egui IME 소유권을 되찾아야 한다"
        );
        assert!(
            full.platform_output.ime.is_some(),
            "소유권을 되찾은 프레임은 IME 영역을 내보내야 한다"
        );
    }

    /// 비활성 터미널(다른 TextEdit이 포커스를 쥔 프레임 등)은 이번 프레임에 조합
    /// 이벤트가 있어도 IME를 가져가지 않는다 — `ime_active`가 유일한 관문이다.
    #[test]
    fn 비활성_터미널은_조합_이벤트가_있어도_ime를_가져가지_않는다() {
        let ctx = egui::Context::default();
        let mut cache = TerminalRenderCache::default();
        let snapshot = snap(4, 1, &["test"]);

        let input = egui::RawInput {
            events: vec![egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "ㄱ".into(),
                active_range_chars: None,
            })],
            ..Default::default()
        };
        let mut full = ctx.run_ui(input, |ui| {
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
        full.textures_delta.clear();
        assert!(full.platform_output.ime.is_none());
    }

    #[test]
    fn 행높이_배수는_셀_높이만_키우고_폭은_그대로다() {
        let ctx = egui::Context::default();
        // fonts는 첫 프레임에 초기화된다 — run_ui 안에서 재야 한다.
        let mut measured = None;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            measured = Some((
                cell_size(ui.ctx(), m(13.0, 1.0)),
                cell_size(ui.ctx(), m(13.0, 1.5)),
                cell_size(ui.ctx(), m(13.0, 0.8)),
            ));
        })
        .drop_without_applying_deltas();
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
        let ctx = egui::Context::default();
        let mut cache = TerminalRenderCache::default();
        let first = snap(4, 3, &["aaaa", "bbbb", "cccc"]);
        assert_eq!(draw_for_test(&ctx, &mut cache, &first), 3);

        let mut second = snap(4, 3, &["aaaa", "bbxb", "cccc"]);
        second.dirty_ranges = vec![CellRange { start: 4, end: 8 }];
        assert_eq!(draw_for_test(&ctx, &mut cache, &second), 1);

        let mut cursor_only = second.clone();
        cursor_only.cursor.col = 2;
        cursor_only.dirty_ranges.clear();
        assert_eq!(draw_for_test(&ctx, &mut cache, &cursor_only), 0);
    }

    #[test]
    fn render_counters는_dirty_painted_shapes를_집계한다() {
        fn counters(
            ctx: &egui::Context,
            cache: &mut TerminalRenderCache,
            snapshot: &TerminalViewportSnapshot,
        ) -> RenderCounters {
            let mut out = RenderCounters::default();
            ctx.run_ui(egui::RawInput::default(), |ui| {
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
            })
            .drop_without_applying_deltas();
            out
        }

        let ctx = egui::Context::default();
        let mut cache = TerminalRenderCache::default();
        let first = snap(4, 3, &["aaaa", "bbbb", "cccc"]);
        let c = counters(&ctx, &mut cache, &first);
        // 첫 프레임: dirty_ranges는 비었지만 캐시 미스로 3행 전부 재구성 + 3행 전부 페인트.
        assert_eq!(c.dirty_rows, 0);
        assert_eq!(c.rows_rebuilt, 3);
        assert_eq!(c.rows_painted, 3);
        // 배경 rect 1 + 행마다 text_run 1개 (기본 bg라 bg_run 없음)
        assert_eq!(c.shapes, 1 + 3);

        // 2프레임: 1행만 dirty → 재구성 1행, 페인트는 여전히 전체 행(shape 발행은 매 프레임).
        let mut second = snap(4, 3, &["aaaa", "bbxb", "cccc"]);
        second.dirty_ranges = vec![CellRange { start: 4, end: 8 }];
        let c = counters(&ctx, &mut cache, &second);
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

    /// 2칸 글자가 행 끝에 안 들어가 다음 줄로 밀리면 그 행 마지막 칸에 **필러**가 남는다
    /// (alacritty `LEADING_WIDE_CHAR_SPACER`). 이건 앞 글자의 뒷칸이 아니므로 `end` 상한을
    /// 지켜야 한다 — 구분하지 않으면 강조가 한 칸 더 칠해진다(2026-08-18 리뷰 실측).
    #[test]
    fn 행_끝_wrap_필러는_end를_넘어_칠하지_않는다() {
        // cols=6에 "abcde"(5칸) 뒤 "가"(2칸) → row0 마지막 칸이 필러, "가"는 row1 0열.
        let mut backend = crate::alacritty_backend::AlacrittyBackend::new(6, 3, 100);
        backend.feed("abcde가".as_bytes()).unwrap();
        let snapshot = backend.viewport_snapshot().unwrap();

        let filler = 5; // row0의 마지막 칸
        assert!(
            snapshot.visible_cells[filler].wide_spacer,
            "백엔드가 필러도 wide_spacer로 평탄화한다(이 테스트의 전제)"
        );
        assert!(
            !snapshot.is_trailing_wide_spacer(filler),
            "필러는 앞 칸이 소유자가 아니라 진짜 뒷칸이 아니다"
        );
        // 끝점이 마지막 실제 글자 'e'(idx 4)일 때 필러(5)는 칠하지 않는다.
        assert!(
            !selection_covers_cell(&snapshot, filler, 0, 4, true),
            "end 밖의 필러를 칠하면 강조가 한 칸 더 나간다"
        );
        // "가"의 뒷칸은 여전히 소유자와 함께 칠한다(회귀 방지).
        let owner = snapshot.cols as usize; // row1 0열
        assert!(snapshot.visible_cells[owner].wide);
        assert!(snapshot.is_trailing_wide_spacer(owner + 1));
        assert!(selection_covers_cell(
            &snapshot,
            owner + 1,
            owner,
            owner,
            true
        ));
    }

    /// 행 끝이 wide 글자(한글 등)일 때 **강조가 글자 전체**를 덮어야 한다. 끝점이
    /// 자리 채움에서 앞 글자로 정규화되므로, 칠하기를 `end`로만 판정하면 마지막 글자가
    /// 반쪽만 칠해진다 — 복사는 정상이라 더 헷갈린다(2026-08-17 사용자 보고).
    #[test]
    fn wide_글자로_끝나는_선택은_자리_채움까지_칠한다() {
        for fixture in ["프로젝트", "設定", "项目"] {
            let snapshot = backend_snap(fixture);
            let spacer = first_wide_spacer(&snapshot);
            let owner = owning_wide_cell(&snapshot, spacer).expect("wide spacer owner");
            // 끝점을 owner로 준다 — normalize가 자리 채움을 이렇게 되돌린 결과와 같다.
            assert!(
                selection_covers_cell(&snapshot, owner, 0, owner, false),
                "{fixture}: wide 글자 자체는 선택 대상이다"
            );
            assert!(
                selection_covers_cell(&snapshot, spacer, 0, owner, true),
                "{fixture}: 앞 칸이 선택됐으면 자리 채움도 칠한다(end 밖이라도)"
            );
            // 앞 칸이 선택되지 않았으면 자리 채움만 홀로 칠하지 않는다.
            assert!(
                !selection_covers_cell(&snapshot, spacer, 0, owner, false),
                "{fixture}: 고아 자리 채움은 칠하지 않는다"
            );
        }
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
        let ctx = egui::Context::default();
        let mut snapshot = snap(6, 3, &["one", "two", "three"]);
        // 전 행 dirty인 스냅샷(대량 출력 직후 상태)
        snapshot.dirty_ranges = vec![CellRange {
            start: 0,
            end: 6 * 3,
        }];
        let mut cache = TerminalRenderCache::default();
        let generation = 7;
        // 첫 draw: 전 행 빌드(캐시 비어 있음)
        assert_eq!(
            draw_gen_for_test(&ctx, &mut cache, &snapshot, generation),
            3
        );
        // 같은 세대 재draw(= 같은 스냅샷 repaint): 재구성 0
        assert_eq!(
            draw_gen_for_test(&ctx, &mut cache, &snapshot, generation),
            0,
            "같은 스냅샷 repaint가 전 행을 재-shaping했다 (idle 낭비 회귀)"
        );
        // 새 세대(새 스냅샷): dirty를 다시 신뢰해 재구성
        assert_eq!(
            draw_gen_for_test(&ctx, &mut cache, &snapshot, generation + 1),
            3
        );
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

    // ---- 보존된 cols의 균일 축소(fit-width) ----

    /// pane 폭을 직접 정해 draw한 결과 — 실제 호출측(workspace.rs)처럼 pane rect로
    /// 자식 Ui를 만들어 그린다. 좌표계 회귀는 이 한 경로로만 본다.
    struct PaneDraw {
        rect: egui::Rect,
        origin: egui::Pos2,
        cell: egui::Vec2,
        /// 축소 전(설정 그대로의) 셀 크기 — 배율 판정 기준.
        base_cell: egui::Vec2,
        ime_cursor_rect: Option<egui::Rect>,
        rebuilt_rows: usize,
        shapes: Vec<egui::epaint::ClippedShape>,
    }

    /// `marker`를 주면 draw **직전에 같은 레이어**로 표식 rect를 그린다 — 변환 구간이
    /// 인접(부모) shape까지 삼키지 않는지 확인하는 용도다.
    fn draw_in_pane(
        ctx: &egui::Context,
        cache: &mut TerminalRenderCache,
        snapshot: &TerminalViewportSnapshot,
        pane_width: f32,
        generation: u64,
        selection: Option<(usize, usize)>,
        marker: Option<(egui::Rect, egui::Color32)>,
    ) -> PaneDraw {
        let mut measured = None;
        let mut full = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_min_size(egui::vec2(900.0, 400.0));
            if let Some((marker_rect, marker_color)) = marker {
                ui.painter().rect_filled(marker_rect, 0.0, marker_color);
            }
            let pane =
                egui::Rect::from_min_size(egui::pos2(20.0, 10.0), egui::vec2(pane_width, 200.0));
            let mut pane_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(pane)
                    .layout(egui::Layout::top_down(egui::Align::LEFT)),
            );
            let base_cell = cell_size(pane_ui.ctx(), m(13.0, 1.0));
            let output = draw(
                &mut pane_ui,
                snapshot,
                m(13.0, 1.0),
                cache,
                Some("한"),
                true, // 조합 표시와 후보창 좌표까지 같은 프레임에서 확인한다
                selection,
                generation,
            );
            measured = Some((
                output.response.rect,
                output.origin,
                output.cell_size,
                base_cell,
            ));
        });
        // headless 검사는 업로드할 텍스처를 명시적으로 폐기한다(egui 0.36 계약).
        full.textures_delta.clear();
        let (rect, origin, cell, base_cell) = measured.expect("terminal should be rendered");
        PaneDraw {
            rect,
            origin,
            cell,
            base_cell,
            ime_cursor_rect: full.platform_output.ime.map(|ime| ime.cursor_rect),
            rebuilt_rows: cache.rebuilt_rows_last_frame(),
            shapes: full.shapes,
        }
    }

    fn first_rect_with_fill(
        shapes: &[egui::epaint::ClippedShape],
        fill: egui::Color32,
    ) -> Option<egui::Rect> {
        shapes.iter().find_map(|clipped| match &clipped.shape {
            egui::Shape::Rect(rect_shape) if rect_shape.fill == fill => Some(rect_shape.rect),
            _ => None,
        })
    }

    /// 그려진 글자(갤리) shape들의 오른쪽 끝 — 마지막 열이 화면 안인지 보는 값이다.
    fn max_text_right(shapes: &[egui::epaint::ClippedShape]) -> Option<f32> {
        shapes
            .iter()
            .filter(|clipped| matches!(clipped.shape, egui::Shape::Text(_)))
            .map(|clipped| clipped.shape.visual_bounding_rect().right())
            .reduce(f32::max)
    }

    /// 배율이 정의되지 않는 입력은 전부 1.0 — 0 배율은 역변환이 불가능해 clip 계산이
    /// 깨지므로 기존 clip 경로로 돌려보낸다. 반대로 **작은 양수는 하한 없이 그대로**
    /// 쓴다(임의 최솟값에서 멈추면 그 아래에서 다시 우측 열이 잘린다).
    #[test]
    fn fit_width_scale은_가용폭을_맞추고_비정상_입력은_기존_clip을_유지한다() {
        // 넉넉한 폭 — 축소 없음
        assert_eq!(fit_width_scale(500.0, 8.0, 10), 1.0);
        // 딱 맞는 폭도 축소하지 않는다(86 - 6 = 80 = 8 × 10)
        assert_eq!(fit_width_scale(86.0, 8.0, 10), 1.0);
        // 정의 불가 입력
        for (width, cell, cols) in [
            (0.0, 8.0, 10),
            (HORIZONTAL_PADDING * 2.0, 8.0, 10), // 여백을 빼면 그리드 폭 0
            (-100.0, 8.0, 10),
            (f32::NAN, 8.0, 10),
            (f32::INFINITY, 8.0, 10),
            (100.0, 0.0, 10),
            (100.0, -8.0, 10),
            (100.0, f32::NAN, 10),
            (100.0, f32::INFINITY, 10),
            (100.0, 8.0, 0),
        ] {
            assert_eq!(
                fit_width_scale(width, cell, cols),
                1.0,
                "width={width}, cell={cell}, cols={cols}"
            );
        }
        // 실제 축소: (106 - 6) / (8 × 100) = 0.125
        assert!((fit_width_scale(106.0, 8.0, 100) - 0.125).abs() < 1e-6);
        // 아주 작은 양수도 하한으로 자르지 않는다: (6.5 - 6) / (8 × 500) = 0.000125
        let tiny = fit_width_scale(6.5, 8.0, 500);
        assert!(
            tiny > 0.0 && (tiny - 0.000_125).abs() < 1e-9,
            "임의 하한에서 clip하면 '한 화면에 전부' 요구가 깨진다: {tiny}"
        );
    }

    /// 보존된 cols가 pane보다 넓을 때: 그리드만 균일 축소해 **마지막 열까지** 화면 폭
    /// 안에 들어가야 하고, response/IME/RenderOutput이 모두 같은 화면 좌표여야 한다.
    /// 터미널 배경 rect와 인접(먼저 그린) shape는 변환 대상이 아니다.
    ///
    /// 축소된 글자의 가독성은 이 검사가 판정하지 않는다(실제 화면 확인 대기).
    #[test]
    fn 좁은_pane은_그리드만_균일_축소해_마지막_열까지_화면_안에_그린다() {
        let cols = 40u16;
        let line = "M".repeat(cols as usize);
        let mut snapshot = snap(cols, 2, &[&line, &line]);
        snapshot.cursor.visible = true;
        snapshot.cursor.col = 2;
        snapshot.cursor.row = 1;
        let selection = Some((cols as usize, cols as usize * 2 - 1)); // 둘째 행 전체
        let marker_color = egui::Color32::from_rgb(0x11, 0x22, 0x33);
        let marker_rect =
            egui::Rect::from_min_size(egui::pos2(700.0, 300.0), egui::vec2(10.0, 10.0));

        let mut cache = TerminalRenderCache::default();
        let pane_width = 160.0;
        let ctx = egui::Context::default();
        let drawn = draw_in_pane(
            &ctx,
            &mut cache,
            &snapshot,
            pane_width,
            next_gen(),
            selection,
            Some((marker_rect, marker_color)),
        );

        // 축소가 실제로 일어났고, 그리드 폭이 여백을 뺀 가용 폭에 정확히 맞는다.
        assert!(
            drawn.cell.x < drawn.base_cell.x,
            "cols={cols}가 pane({pane_width})보다 넓은데 축소되지 않았다: {:?}",
            drawn.cell
        );
        let grid_width = drawn.cell.x * cols as f32;
        assert!(
            (grid_width - grid_width_for_available(pane_width)).abs() < 0.05,
            "그리드 폭 {grid_width}이 가용 폭 {}와 다르다",
            grid_width_for_available(pane_width)
        );
        // 세로도 같은 배율(균일 변환) — 가로만 찌그러뜨리지 않는다.
        let ratio_x = drawn.cell.x / drawn.base_cell.x;
        let ratio_y = drawn.cell.y / drawn.base_cell.y;
        assert!(
            (ratio_x - ratio_y).abs() < 1e-4,
            "가로/세로 배율이 다르다: {ratio_x} vs {ratio_y}"
        );

        // origin은 변환의 고정점 — 좌측 여백 3px가 그대로다.
        assert!((drawn.origin.x - drawn.rect.left() - HORIZONTAL_PADDING).abs() < 0.01);
        let content_right = drawn.rect.right() - HORIZONTAL_PADDING;
        assert!(
            drawn.origin.x + grid_width <= content_right + 0.05,
            "마지막 열이 화면 밖이다: origin={:?}, grid_width={grid_width}, rect={:?}",
            drawn.origin,
            drawn.rect
        );

        // 실제로 발행된 글자 shape가 마지막 열까지 차 있어야 한다(잘라낸 게 아니다).
        let text_right = max_text_right(&drawn.shapes).expect("글자 shape가 있어야 한다");
        assert!(
            text_right <= content_right + 0.1,
            "글자가 화면 오른쪽을 넘었다: {text_right} > {content_right}"
        );
        assert!(
            text_right >= content_right - drawn.cell.x * 1.5,
            "마지막 열 글자가 빠졌다: {text_right}, content_right={content_right}"
        );

        // 선택 하이라이트도 같은 변환을 받는다(오버레이만 남는 어긋남 방지).
        let selection_rect =
            first_rect_with_fill(&drawn.shapes, egui::Color32::from_rgb(0x2d, 0x4f, 0x77))
                .expect("선택 rect가 있어야 한다");
        assert!(
            selection_rect.right() <= content_right + 0.1
                && selection_rect.right() >= content_right - drawn.cell.x * 1.5,
            "선택 하이라이트가 축소된 그리드와 어긋났다: {selection_rect:?}"
        );
        // 선택 rect는 변환 전에 양쪽 경계를 물리 픽셀에 반올림한다. 높이 오차의
        // 상한은 원본 1픽셀을 축소한 값이며, 셀 하나 이상 어긋나는 것은 허용하지 않는다.
        let pixel_rounding_slack = ratio_y / ctx.pixels_per_point() + 0.01;
        assert!(
            (selection_rect.height() - drawn.cell.y).abs() <= pixel_rounding_slack,
            "선택 rect 높이가 픽셀 반올림 오차를 넘었다: {selection_rect:?}, cell={:?}",
            drawn.cell
        );

        // 원점이 아닌 커서로 이동·크기 변환을 함께 검사한다.
        let ime_cursor = drawn.ime_cursor_rect.expect("IME 영역이 나와야 한다");
        let cursor_screen = drawn.origin + egui::vec2(2.0 * drawn.cell.x, drawn.cell.y);
        assert!(
            (ime_cursor.min - cursor_screen).length() < 0.01,
            "IME cursor_rect가 화면 셀 좌표와 어긋났다: {ime_cursor:?}, origin={:?}",
            drawn.origin
        );
        assert!(
            (ime_cursor.width() - drawn.cell.x).abs() < 0.01
                && (ime_cursor.height() - drawn.cell.y).abs() < 0.01,
            "IME cursor_rect가 축소된 셀 크기와 다르다: {ime_cursor:?}, cell={:?}",
            drawn.cell
        );

        // 터미널 배경 rect는 화면 좌표 그대로(변환 구간 밖) — 축소되면 pane 우측에
        // 앱 크롬이 비친다.
        let background =
            first_rect_with_fill(&drawn.shapes, TERMINAL_SURFACE_BG).expect("배경 rect");
        assert!(
            (background.width() - drawn.rect.width()).abs() < 0.01,
            "배경 rect가 그리드와 함께 축소됐다: {background:?}, rect={:?}",
            drawn.rect
        );

        // 먼저 그린 이웃 shape는 그대로 — 변환 구간이 부모/이웃을 삼키지 않았다.
        let marker = first_rect_with_fill(&drawn.shapes, marker_color).expect("표식 rect");
        assert!(
            (marker.min - marker_rect.min).length() < 0.01
                && (marker.width() - marker_rect.width()).abs() < 0.01,
            "인접 shape가 함께 변환됐다: {marker:?} != {marker_rect:?}"
        );
    }

    /// 폭만 바뀌는 리사이즈는 **행 갤리를 다시 shaping하지 않는다** — 축소를 font_size가
    /// 아니라 shape 변환으로 하는 이유다(캐시 키는 font_size 그대로).
    /// 축소가 필요 없는 pane에서는 기존 metrics와 완전히 같아야 한다.
    #[test]
    fn 폭_변화는_행_캐시를_무효화하지_않고_비축소_경로는_기존_metrics를_유지한다() {
        let cols = 40u16;
        let line = "M".repeat(cols as usize);
        let snapshot = snap(cols, 2, &[&line, &line]); // dirty_ranges 없음

        let mut cache = TerminalRenderCache::default();
        let ctx = egui::Context::default();
        let wide = draw_in_pane(&ctx, &mut cache, &snapshot, 600.0, next_gen(), None, None);
        assert_eq!(wide.rebuilt_rows, 2, "첫 draw는 빈 캐시라 전 행을 만든다");
        // 비축소 경로: 셀·원점이 기존과 동일하고 IME도 원래 셀 크기다.
        assert_eq!(
            wide.cell, wide.base_cell,
            "넓은 pane에서 축소가 일어나면 안 된다"
        );
        assert!((wide.origin.x - wide.rect.left() - HORIZONTAL_PADDING).abs() < 0.01);
        let wide_ime = wide.ime_cursor_rect.expect("IME 영역");
        assert!(
            (wide_ime.width() - wide.base_cell.x).abs() < 0.01,
            "비축소 경로의 IME cursor_rect가 바뀌었다: {wide_ime:?}"
        );

        // 새 스냅샷 세대 + 좁아진 폭: 폭은 캐시 키가 아니므로 재-shaping이 0이어야 한다.
        let narrow = draw_in_pane(&ctx, &mut cache, &snapshot, 160.0, next_gen(), None, None);
        assert_eq!(
            narrow.rebuilt_rows, 0,
            "폭 변화가 행 갤리 캐시를 무효화했다 — 축소가 font_size를 건드렸다는 뜻이다"
        );
        assert!(
            narrow.cell.x < wide.cell.x,
            "좁아진 pane에서 축소되지 않았다: {:?}",
            narrow.cell
        );
    }

    /// 행 캐시의 첫 글자 run — 축소 갤리 보관 상태를 직접 본다.
    fn first_text_run(cache: &TerminalRenderCache) -> &RowTextRun {
        cache
            .rows_cache
            .iter()
            .flatten()
            .flat_map(|row| row.text_runs.iter())
            .next()
            .expect("행 캐시에 글자 run이 있어야 한다")
    }

    /// preedit 조합 상자의 배경 rect(불투명 0xd8d8d8) — 커서(알파 0xa0)와 구분된다.
    fn preedit_box(shapes: &[egui::epaint::ClippedShape]) -> egui::Rect {
        first_rect_with_fill(shapes, egui::Color32::from_rgb(0xd8, 0xd8, 0xd8))
            .expect("preedit 배경 rect가 있어야 한다")
    }

    /// 축소는 `TextShape::transform`을 타는데, 이건 `Arc::make_mut`으로 갤리를 **깊은
    /// 복제**한다(epaint 0.35 text_shape.rs:110-168). 구간 변환에 매 프레임 맡기면
    /// 축소된 pane이 idle일 때도 전 행을 복제한다 — 같은 배율이면 run당 하나 보관한
    /// 축소 갤리를 그대로 재사용해야 한다.
    ///
    /// `rows_rebuilt == 0`만으로는 복제 없음을 증명하지 못하므로(원본 shaping과 축소
    /// 복제는 별개다) 보관한 Arc의 동일성(`ptr_eq`)을 직접 본다. 같은 Context에서
    /// 넓게 → 좁게 → 같은 폭 → 다시 넓게 → 더 좁게 순으로 그린다.
    ///
    /// 프레임 시간은 여기서 재지 않는다(실측 없음).
    #[test]
    fn 같은_배율_재draw는_축소_갤리를_재사용하고_배율이_바뀌면_교체한다() {
        let cols = 40u16;
        let line = "M".repeat(cols as usize);
        let snapshot = snap(cols, 2, &[&line, &line]);
        let mut cache = TerminalRenderCache::default();
        let ctx = egui::Context::default();

        // 1) 넓은 pane(비축소): 축소 갤리를 만들지 않는다.
        let wide = draw_in_pane(&ctx, &mut cache, &snapshot, 600.0, next_gen(), None, None);
        assert_eq!(wide.cell, wide.base_cell, "넓은 pane은 축소하지 않는다");
        assert!(
            first_text_run(&cache).scaled.is_none(),
            "비축소 경로가 축소 갤리를 만들었다"
        );
        let original = Arc::clone(&first_text_run(&cache).galley);
        let original_rect = original.rect;
        let wide_preedit = preedit_box(&wide.shapes);

        // 2) 좁은 pane: 축소 갤리가 생기고 **원본 갤리는 그대로**다.
        let narrow = draw_in_pane(&ctx, &mut cache, &snapshot, 160.0, next_gen(), None, None);
        let (bits_a, scaled_a) = first_text_run(&cache)
            .scaled
            .clone()
            .expect("축소 프레임이 축소 갤리를 보관해야 한다");
        assert!(
            Arc::ptr_eq(&first_text_run(&cache).galley, &original),
            "원본 갤리 Arc가 교체됐다 — 제자리 변환이 캐시를 오염시켰다"
        );
        assert_eq!(
            first_text_run(&cache).galley.rect,
            original_rect,
            "원본 갤리의 기하가 축소로 바뀌었다"
        );
        assert!(!Arc::ptr_eq(&scaled_a, &original));
        assert!(
            scaled_a.rect.width() < original_rect.width(),
            "축소 갤리가 실제로 작아지지 않았다: {:?} vs {original_rect:?}",
            scaled_a.rect
        );
        // preedit은 축소 갤리를 쓰지 않고 **구간 변환**을 그대로 탄다 — 축소 텍스트만
        // 제외하고 나머지 Text를 빼지 않았는지 여기서 확인한다.
        let ratio = narrow.cell.y / wide.cell.y;
        let narrow_preedit = preedit_box(&narrow.shapes);
        assert!(
            (narrow_preedit.height() - wide_preedit.height() * ratio).abs() < 0.05,
            "preedit이 구간 변환에서 빠졌다: {narrow_preedit:?} vs {wide_preedit:?} × {ratio}"
        );

        // 3) 같은 폭 재draw: 같은 Arc를 그대로 쓴다(복제 없음).
        let again = draw_in_pane(&ctx, &mut cache, &snapshot, 160.0, next_gen(), None, None);
        let (bits_b, scaled_b) = first_text_run(&cache).scaled.clone().expect("축소 갤리");
        assert_eq!(bits_a, bits_b, "같은 폭인데 배율이 달라졌다");
        assert!(
            Arc::ptr_eq(&scaled_a, &scaled_b),
            "같은 배율인데 축소 갤리를 다시 복제했다 (idle 프레임 낭비 회귀)"
        );
        assert_eq!(again.cell, narrow.cell);
        assert!(Arc::ptr_eq(&first_text_run(&cache).galley, &original));
        // 캐시에만 같은 Arc를 보관하고 실제 paint에서 다시 복제하는 회귀도 막는다.
        assert!(
            again.shapes.iter().any(|clipped| {
                matches!(&clipped.shape, egui::Shape::Text(text)
                if Arc::ptr_eq(&text.galley, &scaled_b))
            }),
            "화면에 발행한 글자가 축소 캐시를 그대로 사용해야 한다"
        );
        let preedit_height = |shapes: &[egui::epaint::ClippedShape]| {
            shapes
                .iter()
                .find_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(text) if text.galley.job.text == "한" => {
                        Some(text.galley.rect.height())
                    }
                    _ => None,
                })
                .expect("한글 조합 글자 shape가 있어야 한다")
        };
        assert!(
            (preedit_height(&narrow.shapes) - preedit_height(&wide.shapes) * ratio).abs() < 0.05,
            "조합 상자뿐 아니라 한글 글자도 같은 비율로 축소해야 한다"
        );

        // 4) 다시 넓은 pane: 배율이 1로 돌아가면 축소 캐시를 해제한다(보관 무한 증가 방지).
        let back = draw_in_pane(&ctx, &mut cache, &snapshot, 600.0, next_gen(), None, None);
        assert_eq!(back.cell, back.base_cell);
        assert!(
            first_text_run(&cache).scaled.is_none(),
            "비축소로 돌아온 뒤에도 축소 갤리가 남았다"
        );

        // 5) 다른 배율: 보관은 run당 하나라 교체된다.
        let narrower = draw_in_pane(&ctx, &mut cache, &snapshot, 120.0, next_gen(), None, None);
        let (bits_c, scaled_c) = first_text_run(&cache).scaled.clone().expect("축소 갤리");
        assert_ne!(bits_c, bits_a, "배율이 달라졌는데 이전 축소 갤리를 썼다");
        assert!(!Arc::ptr_eq(&scaled_c, &scaled_a));
        assert!(
            narrower.cell.x < narrow.cell.x,
            "더 좁은 pane인데 배율이 커졌다: {:?}",
            narrower.cell
        );
    }
}
