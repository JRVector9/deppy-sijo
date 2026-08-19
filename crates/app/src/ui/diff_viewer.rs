//! 메인 영역 실용형 diff 뷰어 — 행번호 + hunk 색 배경 + 접힌 문맥 + hunk 이동
//! (2026-08-15 스펙 §5). 문법 강조·인트라라인·미니맵은 의도적으로 범위 외.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Del,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct DiffHunk {
    // 스펙 §2 데이터 모델의 일부(hunk 헤더 시작 행번호) — gap 계산은 파싱 시점에
    // 이미 끝나 저장값을 다시 읽지 않는다. 렌더는 라인별 old_no/new_no만 쓰고 hunk
    // 헤더 자체는 표시하지 않는다(스펙 §5, "⋯ N행" 접힘 표시만 요구). 소비자가
    // 없어도 unified diff 모델의 일부로 필드는 유지한다 — 2026-08-15.
    #[allow(dead_code)]
    pub old_start: u32,
    #[allow(dead_code)]
    pub new_start: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffMode {
    Working,
    Branch,
}

#[derive(Clone, Debug, Default)]
pub struct FileDiffView {
    pub hunks: Vec<DiffHunk>,
    /// hunks[i]와 hunks[i+1] 사이의 접힌 구(old) 행수 — "⋯ N행" 표시용.
    pub gaps: Vec<u32>,
    pub binary: bool,
    pub truncated: bool,
}

/// `git diff --no-ext-diff` 출력 하나(파일 1개)를 분해한다. 잘린 입력(truncated)도
/// 마지막 완성 라인까지 파싱한다 — 상한은 수집 쪽(run_git_limited)이 이미 보장.
pub fn parse_unified(text: &str, truncated: bool) -> FileDiffView {
    let mut view = FileDiffView {
        truncated,
        ..Default::default()
    };
    let mut old_no = 0u32;
    let mut new_no = 0u32;
    // 이전 hunk의 헤더 선언 old-range 끝(one-past, 즉 다음 미표시 구 행번호).
    // 계획서 원안은 이 값을 hunk 본문의 실측 Context/Del 행 수로 계산했는데, 그 방식은
    // 헤더가 선언한 old-count와 실제 파싱된 본문 행 수가 어긋나면(예: 잘린 입력) 어긋난다.
    // 헤더의 old-count는 unified diff 포맷이 보장하는 값이라 이걸 그대로 쓰는 게 더
    // 안정적이고, gap 테스트 값(26)도 이 계산으로만 맞는다 — 실측 방식은 27을 낸다.
    let mut prev_old_end: Option<u32> = None;
    for line in text.lines() {
        if line.starts_with("Binary files ") {
            view.binary = true;
            continue;
        }
        if let Some(header) = line.strip_prefix("@@ ") {
            // "@@ -10,4 +10,5 @@ ..." — 시작 행번호와 개수(gap 계산용)가 필요하다.
            let parse_range = |tok: &str| -> (u32, u32) {
                let mut it = tok.trim_start_matches(['-', '+']).split(',');
                let start = it.next().and_then(|n| n.parse().ok()).unwrap_or(0);
                // 개수 생략(단일 행 hunk)은 unified diff 규약상 1.
                let count = it.next().and_then(|n| n.parse().ok()).unwrap_or(1);
                (start, count)
            };
            let mut parts = header.split(' ');
            let (old_start, old_count) = parts.next().map(parse_range).unwrap_or((0, 1));
            let (new_start, _new_count) = parts.next().map(parse_range).unwrap_or((0, 1));
            if let Some(prev_end) = prev_old_end {
                view.gaps.push(old_start.saturating_sub(prev_end));
            }
            prev_old_end = Some(old_start + old_count);
            view.hunks.push(DiffHunk {
                old_start,
                new_start,
                lines: Vec::new(),
            });
            old_no = old_start;
            new_no = new_start;
            continue;
        }
        let Some(hunk) = view.hunks.last_mut() else {
            continue; // 파일 헤더(diff --git/index/---/+++)는 건너뛴다.
        };
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Add, &line[1..]),
            Some(b'-') => (LineKind::Del, &line[1..]),
            Some(b' ') => (LineKind::Context, &line[1..]),
            _ => continue, // "\\ No newline at end of file" 등.
        };
        let (o, n) = match kind {
            LineKind::Context => {
                let pair = (Some(old_no), Some(new_no));
                old_no += 1;
                new_no += 1;
                pair
            }
            LineKind::Del => {
                let pair = (Some(old_no), None);
                old_no += 1;
                pair
            }
            LineKind::Add => {
                let pair = (None, Some(new_no));
                new_no += 1;
                pair
            }
        };
        hunk.lines.push(DiffLine {
            kind,
            old_no: o,
            new_no: n,
            text: text.to_owned(),
        });
    }
    view
}

#[derive(Clone, Copy, Debug)]
pub enum DisplayRow {
    Line { hunk: usize, line: usize },
    Gap { lines: u32 },
}

/// 렌더는 이 평탄 목록 위에서 show_rows 가상화로 돈다 — 대형 diff에서도 프레임 유계.
pub fn flatten_display_rows(view: &FileDiffView) -> Vec<DisplayRow> {
    let mut rows = Vec::new();
    for (h, hunk) in view.hunks.iter().enumerate() {
        if h > 0
            && let Some(gap) = view.gaps.get(h - 1).copied().filter(|g| *g > 0)
        {
            rows.push(DisplayRow::Gap { lines: gap });
        }
        for l in 0..hunk.lines.len() {
            rows.push(DisplayRow::Line { hunk: h, line: l });
        }
    }
    rows
}

/// `ScrollArea::show_rows`가 행 하나에 쓰는 실제 세로 간격. egui는 행 높이에
/// `item_spacing.y`를 더해 배치하므로, 특정 행으로 스크롤할 때도 같은 값을 곱해야
/// 어긋나지 않는다.
fn scroll_row_pitch(row_height: f32, spacing: &egui::style::Spacing) -> f32 {
    row_height + spacing.item_spacing.y
}

pub fn hunk_start_indices(rows: &[DisplayRow]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut last_hunk = usize::MAX;
    for (i, row) in rows.iter().enumerate() {
        if let DisplayRow::Line { hunk, .. } = row
            && *hunk != last_hunk
        {
            out.push(i);
            last_hunk = *hunk;
        }
    }
    out
}

/// 추가/삭제 행 배경색 — 이 저장소의 theme.rs/designall.rs에는 diff 전용 색이
/// 없다(2026-08-15 확인, Task 7 Step 3). designall::Tokens.success/error는 "그
/// 체계 밖의 일반 성공 표시가 생길 때" 용도로 예약돼 있어(designall.rs:14-16
/// 주석) 새로 끌어쓰지 말라는 경고가 있고, 애초에 라이트/다크 전용 diff 색도
/// 아니다 — 그래서 GitHub diff 배색을 참고해 라이트/다크 각각 하드코딩하고
/// `dark_mode`로 분기한다.
fn diff_line_bg(dark_mode: bool, kind: LineKind) -> egui::Color32 {
    match kind {
        LineKind::Add if dark_mode => egui::Color32::from_rgb(0x03, 0x3a, 0x16),
        LineKind::Add => egui::Color32::from_rgb(0xe6, 0xff, 0xec),
        LineKind::Del if dark_mode => egui::Color32::from_rgb(0x67, 0x06, 0x0c),
        LineKind::Del => egui::Color32::from_rgb(0xff, 0xeb, 0xe9),
        LineKind::Context => egui::Color32::TRANSPARENT,
    }
}

fn diff_line_sign(kind: LineKind) -> &'static str {
    match kind {
        LineKind::Add => "+",
        LineKind::Del => "-",
        LineKind::Context => " ",
    }
}

/// 검색 일치 배경 — designall::tokens에서만 고른다(하드코딩 금지, 라이트/다크 둘 다
/// 성립). diff 행 배경(add=초록/del=빨강, [`diff_line_bg`])과 색 계열이 겹치면 두
/// 배경이 싸우는지 눈으로 구분하기 어려워지므로 warning(호박)·accent(청록)를 쓴다 —
/// 반투명(gamma_multiply, work_history.rs의 배지 채움과 같은 관례)이라 밑에 깔린 행
/// 배경이 비쳐 보인다.
fn search_match_bg(tokens: crate::ui::designall::Tokens) -> egui::Color32 {
    tokens.warning.gamma_multiply(0.4)
}

/// 활성 일치 배경 — 나머지 일치([`search_match_bg`])와 다른 색이어야 ↑↓가 어디로
/// 갔는지 눈에 띈다(스펙 "우측 본문 강조·이동"). 불투명도를 더 높여 확실히 두드러지게.
fn search_active_match_bg(tokens: crate::ui::designall::Tokens) -> egui::Color32 {
    tokens.accent.gamma_multiply(0.6)
}

/// 한 diff 행을 강조 포함 하나의 `LayoutJob`으로 만든다. `matches`는 `text`(diff
/// 본문, 행번호 접두어 제외) 기준 바이트 범위여야 한다 — 접두어 길이만큼 이 함수가
/// 옮겨 붙인다. `active`는 그 행 기준(로컬) 일치 인덱스. 접두어와 본문을 하나의
/// job으로 합쳐야(별도 위젯 두 개로 나누지 않아야) `ui.horizontal`의 item_spacing이
/// 접두어와 본문 사이에 끼어들지 않는다.
fn diff_row_job(
    prefix: &str,
    text: &str,
    matches: &crate::ui::aux_search::Matches,
    active: Option<usize>,
    font_id: egui::FontId,
    match_bg: egui::Color32,
    active_bg: egui::Color32,
) -> egui::text::LayoutJob {
    let shift = prefix.len();
    let shifted = crate::ui::aux_search::Matches {
        ranges: matches
            .ranges
            .iter()
            .map(|r| r.start + shift..r.end + shift)
            .collect(),
        truncated: matches.truncated,
    };
    // color는 PLACEHOLDER — 위젯이 그릴 때 현재 텍스트 색으로 바꿔치기한다
    // (TextFormat::default().color는 GRAY라 그대로 두면 ui.monospace와 색이 달라진다).
    // background는 기본값(TRANSPARENT)으로 둬 일치하지 않는 구간은 행을 감싼 Frame의
    // 배경(add/del/context)이 그대로 비친다 — 강조가 기존 행 배경을 지우지 않는다.
    let base = egui::TextFormat {
        font_id,
        color: egui::Color32::PLACEHOLDER,
        ..Default::default()
    };
    let full = format!("{prefix}{text}");
    crate::ui::aux_search::highlighted_job(&full, &shifted, active, base, match_bg, active_bg)
}

/// 질의별 검색 캐시 — [`DiffViewerUi::render`]가 질의가 바뀔 때만
/// [`build_search_cache`]로 다시 채운다(거대 diff에서 매 프레임 전수 스캔 회피).
#[derive(Default)]
struct SearchCache {
    query: String,
    /// `display_rows[i]`에서 시작하는 전역(본문 전체) 일치 인덱스. 길이는 항상
    /// `display_rows.len() + 1`(마지막 원소는 총계 `total`인 sentinel) — 행 i의
    /// 일치 개수는 `row_match_start[i + 1] - row_match_start[i]`로 구한다.
    row_match_start: Vec<usize>,
    // `total`·`truncated`는 `DiffViewerUi::search_summary`로만 읽는다(App의
    // `{active}/{total}` 카운터).
    /// 본문 전체 일치 수([`crate::ui::aux_search::MAX_AUX_MATCHES`]에서 멈췄으면 그 이하).
    total: usize,
    truncated: bool,
}

/// `query`로 `rows` 전체를 한 번 훑어 행별 일치 시작 인덱스·총 일치 수·잘림 여부를
/// 센다. [`crate::ui::aux_search::MAX_AUX_MATCHES`]에 닿으면 그 뒤 행은 더 스캔하지 않는다
/// (스펙 "매칭 규칙"). 빈 질의는 일치 없음과 같다(`crate::ui::aux_search::find_matches`가
/// 그렇게 처리해 여기서 따로 분기하지 않는다).
fn build_search_cache(view: &FileDiffView, rows: &[DisplayRow], query: &str) -> SearchCache {
    let mut row_match_start = Vec::with_capacity(rows.len() + 1);
    let mut total = 0usize;
    let mut truncated = false;
    for row in rows {
        row_match_start.push(total);
        if truncated {
            continue;
        }
        if let DisplayRow::Line { hunk, line } = row {
            let text = &view.hunks[*hunk].lines[*line].text;
            let m = crate::ui::aux_search::find_matches(text, query);
            let remaining = crate::ui::aux_search::MAX_AUX_MATCHES - total;
            total += m.ranges.len().min(remaining);
            if total >= crate::ui::aux_search::MAX_AUX_MATCHES {
                truncated = true;
            }
        }
    }
    row_match_start.push(total);
    SearchCache {
        query: query.to_owned(),
        row_match_start,
        total,
        truncated,
    }
}

/// `row_match_start`(길이 `rows.len() + 1`, [`build_search_cache`] 산출물)에서 전역
/// 일치 인덱스 `active`를 담은 표시 행을 찾는다. `active`가 총 일치 수 밖이면
/// `None`(예: 캐시가 아직 새 활성 인덱스를 못 따라온 경계 프레임).
fn row_for_active_match(row_match_start: &[usize], active: usize) -> Option<usize> {
    let &total = row_match_start.last()?;
    if active >= total {
        return None;
    }
    let starts = &row_match_start[..row_match_start.len() - 1];
    let idx = starts.partition_point(|&start| start <= active);
    Some(idx.saturating_sub(1))
}

#[derive(Default)]
pub struct DiffViewerUi {
    view: Option<FileDiffView>,
    rel_path: String,
    mode: Option<DiffMode>,
    loading: bool,
    /// 다음 프레임에 이 표시 행으로 스크롤 — hunk ↑↓가 세팅한다.
    scroll_to_row: Option<usize>,
    current_hunk: usize,
    /// `flatten_display_rows(view)` 캐시 — view가 바뀔 때(open/set_view)만
    /// 재계산한다. render()가 매 프레임 이 무거운 재구성을 반복하면 스크롤과
    /// 무관하게 전체 diff를 매번 훑게 된다(2026-08-16 코드 리뷰 대응).
    display_rows: Vec<DisplayRow>,
    /// `hunk_start_indices(&display_rows)` 캐시 — display_rows와 같은 시점에 갱신한다.
    hunk_starts: Vec<usize>,
    /// 보조 검색 캐시 — 질의가 바뀔 때만 다시 채운다([`build_search_cache`], 스펙
    /// "우측 본문 강조·이동", 계획 Task 4: 거대 diff에서 매 프레임 전수 스캔 금지).
    search_cache: SearchCache,
    /// 직전 프레임에 스크롤을 트리거한 (질의, 활성 인덱스). 같은 값이 반복되는
    /// 프레임엔 다시 스크롤하지 않는다 — 매 프레임 강제하면 검색이 열린 동안
    /// 사용자가 본문을 자유롭게 스크롤할 수 없다(hunk ↑↓의 `scroll_to_row`와 같은
    /// 문제의식).
    search_scroll_key: Option<(String, usize)>,
}

impl DiffViewerUi {
    /// Git 보조 본문에서 파일 행을 클릭했을 때 App이 부른다(2026-08-15 2차, 스펙 §8-3).
    pub fn open(&mut self, rel_path: String, mode: DiffMode) {
        self.rel_path = rel_path;
        self.mode = Some(mode);
        self.view = None;
        self.loading = true;
        self.current_hunk = 0;
        self.scroll_to_row = None;
        self.display_rows = Vec::new();
        self.hunk_starts = Vec::new();
        // 검색 캐시는 display_rows 인덱스를 전제로 한다 — 파일이 바뀌면 그 전제가
        // 깨지므로 함께 비운다.
        self.search_cache = SearchCache::default();
        self.search_scroll_key = None;
    }

    pub fn set_view(&mut self, view: FileDiffView) {
        self.loading = false;
        self.display_rows = flatten_display_rows(&view);
        self.hunk_starts = hunk_start_indices(&self.display_rows);
        self.view = Some(view);
        self.search_cache = SearchCache::default();
        self.search_scroll_key = None;
    }

    /// 본문 전체 일치 수·잘림 여부 — App이 `{active}/{total}` 카운터를 그릴 때 쓴다.
    /// [`Self::render`] 호출 시 질의가 바뀔 때만 갱신되는 캐시를 그대로 돌려준다(매
    /// 프레임 전수 스캔 없음). App은 이 순서를 지켜야 한다: 이 값을 읽어 검색 바를
    /// 그린 뒤, (바뀐 질의가 있으면 반영한) `render`를 호출한다 — 그래야 검색 바가
    /// 그 프레임에 보여주는 카운트가 `render`가 방금 그린 강조와 같은 질의 기준이다
    /// (질의가 막 바뀐 프레임에는 `render` 호출 전이라 직전 질의의 값이 잠깐
    /// 보일 수 있다 — 1프레임 지연, 계획 Task 4 보고 참고).
    pub fn search_summary(&self) -> (usize, bool) {
        (self.search_cache.total, self.search_cache.truncated)
    }

    /// `search`는 (질의, 그 본문 기준 활성 일치 인덱스) — App이 소유한
    /// `AuxSearchState`에서 뽑아 넘긴다. `None`이면 보조 검색이 꺼져 있거나 이 diff
    /// 본문이 대상이 아니라는 뜻이라 강조 없이 예전처럼 그린다.
    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        search: Option<(&str, usize)>,
    ) {
        // 파일이 한 번도 선택되지 않았으면(Git 보조 본문이 방금 열렸거나 목록에서 아직
        // 아무 행도 클릭하지 않았으면) 안내 한 줄만 보인다 — 헤더·hunk 이동은 파일이
        // 선택된 뒤에나 의미가 있다(스펙 §8-3).
        let Some(mode) = self.mode else {
            ui.weak(catalog.t("git.diff.empty", &[]));
            return;
        };
        // ── 헤더: 경로 · 모드 라벨 · hunk ↑↓ ────────────────────────────
        ui.horizontal(|ui| {
            ui.monospace(&self.rel_path);
            let mode_key = match mode {
                DiffMode::Branch => "git.mode.branch",
                DiffMode::Working => "git.mode.working",
            };
            ui.weak(format!("({})", catalog.t(mode_key, &[])));
            if self.view.is_none() {
                return;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("↓")
                    .on_hover_text(catalog.t("git.hunk.next", &[]))
                    .clicked()
                    && self.current_hunk + 1 < self.hunk_starts.len()
                {
                    self.current_hunk += 1;
                    self.scroll_to_row = self.hunk_starts.get(self.current_hunk).copied();
                }
                if ui
                    .small_button("↑")
                    .on_hover_text(catalog.t("git.hunk.prev", &[]))
                    .clicked()
                    && self.current_hunk > 0
                {
                    self.current_hunk -= 1;
                    self.scroll_to_row = self.hunk_starts.get(self.current_hunk).copied();
                }
            });
        });
        ui.separator();

        let Some(view) = self.view.as_ref() else {
            ui.weak(catalog.t("diff.loading", &[]));
            return;
        };
        if view.binary {
            ui.weak(catalog.t("git.binary", &[]));
            return;
        }

        // ── 보조 검색: 질의가 바뀔 때만 캐시를 다시 채운다(매 프레임 전수 스캔
        // 회피). 빈 질의는 검색 안 함과 같다(aux_search 관례) — `active_search`로
        // 한 번에 접어 이후 코드가 "검색 없음"과 "빈 질의"를 따로 취급하지 않는다.
        let active_search = search.filter(|(query, _)| !query.is_empty());
        let query = active_search.map_or("", |(query, _)| query);
        let active = active_search.map(|(_, active)| active);
        if self.search_cache.query != query {
            self.search_cache = build_search_cache(view, &self.display_rows, query);
        }
        // 활성 일치가 바뀌었거나(또는 검색이 새로 열렸거나) 질의가 바뀐 프레임에만
        // 스크롤 대상을 세팅한다 — 매 프레임 세팅하면 검색이 열린 동안 사용자가
        // 본문을 자유롭게 스크롤할 수 없다(아래에서 소비하는 hunk ↑↓의
        // `scroll_to_row`와 같은 문제의식).
        let scroll_key = active.map(|active| (query.to_owned(), active));
        if scroll_key != self.search_scroll_key {
            self.search_scroll_key = scroll_key.clone();
            if let Some((_, active)) = scroll_key
                && let Some(row) = row_for_active_match(&self.search_cache.row_match_start, active)
            {
                self.scroll_to_row = Some(row);
            }
        }

        let rows = &self.display_rows;
        let row_match_start = &self.search_cache.row_match_start;
        let dark_mode = ui.visuals().dark_mode;
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        let mono_font = egui::TextStyle::Monospace.resolve(ui.style());
        let tokens = crate::ui::designall::tokens(ui.visuals());
        let match_bg = search_match_bg(tokens);
        let active_bg = search_active_match_bg(tokens);
        let mut scroll = egui::ScrollArea::both().auto_shrink([false, false]);
        if let Some(target) = self.scroll_to_row.take() {
            // `show_rows`가 실제로 쓰는 행 간격은 `row_h + item_spacing.y`다. 간격을 빼고
            // 계산하면 행마다 몇 px씩 어긋난 것이 누적돼, 긴 diff에서 hunk ↑↓가 목표를
            // 지나쳐 엉뚱한 곳에 멈춘다(2026-08-16 실측: 300행에서 약 900px 어긋남).
            // 원문 뷰어(transcript_viewer.rs)는 같은 자리에서 이미 간격을 더하고 있다.
            scroll = scroll
                .vertical_scroll_offset(target as f32 * scroll_row_pitch(row_h, ui.spacing()));
        }
        scroll.show_rows(ui, row_h, rows.len(), |ui, range| {
            for index in range {
                match rows[index] {
                    DisplayRow::Gap { lines } => {
                        let lines = lines.to_string();
                        ui.weak(catalog.t("git.gap_lines", &[("count", &lines)]));
                    }
                    DisplayRow::Line { hunk, line } => {
                        let l = &view.hunks[hunk].lines[line];
                        let bg = diff_line_bg(dark_mode, l.kind);
                        let sign = diff_line_sign(l.kind);
                        let no = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
                        let prefix = format!("{:>5} {:>5} {sign} ", no(l.old_no), no(l.new_no));
                        egui::Frame::NONE.fill(bg).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                // 검색은 diff 본문 텍스트(l.text)에서만 찾는다 — 행번호
                                // 접두어는 검색 대상이 아니다. 보이는 행만 훑으므로
                                // (show_rows 가상화) 거대 diff에서도 비용이 유계다.
                                let row_matches = (!query.is_empty())
                                    .then(|| crate::ui::aux_search::find_matches(&l.text, query))
                                    .filter(|m| !m.ranges.is_empty());
                                match row_matches {
                                    None => {
                                        ui.monospace(format!("{prefix}{}", l.text));
                                    }
                                    Some(m) => {
                                        let row_start =
                                            row_match_start.get(index).copied().unwrap_or(0);
                                        let row_end = row_match_start
                                            .get(index + 1)
                                            .copied()
                                            .unwrap_or(row_start);
                                        let local_active = active.and_then(|active| {
                                            // then_some은 인자를 즉시 계산하므로
                                            // active < row_start일 때 뺄셈이 오버플로한다
                                            // — then(||..)으로 지연 평가한다.
                                            (active >= row_start && active < row_end)
                                                .then(|| active - row_start)
                                        });
                                        let job = diff_row_job(
                                            &prefix,
                                            &l.text,
                                            &m,
                                            local_active,
                                            mono_font.clone(),
                                            match_bg,
                                            active_bg,
                                        );
                                        ui.label(job);
                                    }
                                }
                            });
                        });
                    }
                }
            }
        });
        if view.truncated {
            ui.weak(catalog.t("diff.truncated", &[]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,4 +10,5 @@ fn ctx() {
 context1
-old line
+new line
+added line
 context2
@@ -40,2 +41,2 @@
 tail1
-x
+y
";

    #[test]
    fn hunk와_행번호를_분해한다() {
        let view = parse_unified(SAMPLE, false);
        assert!(!view.binary);
        assert_eq!(view.hunks.len(), 2);
        let h = &view.hunks[0];
        assert_eq!((h.old_start, h.new_start), (10, 10));
        // 라인 태그: Context/Del/Add 순서 보존.
        let kinds: Vec<LineKind> = h.lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::Context,
                LineKind::Del,
                LineKind::Add,
                LineKind::Add,
                LineKind::Context
            ]
        );
        // 표시 행번호: Del은 구(10..), Add는 신(11..) 번호를 쓴다.
        assert_eq!(h.lines[1].old_no, Some(11));
        assert_eq!(h.lines[2].new_no, Some(11));
        // hunk 사이 접힌 구간: 첫 hunk 끝(구 13행) ~ 둘째 시작(구 40행) → 26행.
        assert_eq!(view.gaps, vec![26]);
    }

    #[test]
    fn 바이너리는_binary_플래그만_세운다() {
        let view = parse_unified("Binary files a/x.png and b/x.png differ\n", false);
        assert!(view.binary);
        assert!(view.hunks.is_empty());
    }

    #[test]
    fn 표시_행은_hunk와_gap을_순서대로_평탄화한다() {
        let view = parse_unified(SAMPLE, false);
        let rows = flatten_display_rows(&view);
        // hunk1(5행) + gap(1행) + hunk2(3행) = 9행. gap 행은 접힌 26행을 담는다.
        assert_eq!(rows.len(), 9);
        assert!(matches!(rows[5], DisplayRow::Gap { lines: 26 }));
        assert!(matches!(rows[0], DisplayRow::Line { hunk: 0, .. }));
        // hunk 시작 인덱스: hunk 이동 버튼이 이 인덱스로 스크롤한다.
        assert_eq!(hunk_start_indices(&rows), vec![0, 6]);
    }

    /// hunk ↑↓가 목표 행에 정확히 서려면 `show_rows`가 쓰는 간격과 **같은 값**을 곱해야
    /// 한다. 간격을 빼먹으면 행마다 어긋난 것이 누적돼 긴 diff에서 목표를 지나친다
    /// (2026-08-16 실측: 300행에서 약 900px).
    #[test]
    fn 스크롤_행_간격은_item_spacing을_포함한다() {
        let mut spacing = egui::style::Spacing::default();
        spacing.item_spacing.y = 4.0;
        assert_eq!(scroll_row_pitch(12.0, &spacing), 16.0);

        spacing.item_spacing.y = 0.0;
        assert_eq!(
            scroll_row_pitch(12.0, &spacing),
            12.0,
            "간격이 0이면 행 높이 그대로"
        );

        // 300행쯤 내려가면 간격을 뺀 계산과 눈에 띄게 벌어진다.
        spacing.item_spacing.y = 3.0;
        let with_spacing = 300.0 * scroll_row_pitch(12.0, &spacing);
        assert_eq!(with_spacing - 300.0 * 12.0, 900.0);
    }

    #[test]
    fn view_교체시_flatten_캐시가_새_내용으로_갱신된다() {
        // set_view가 display_rows/hunk_starts를 즉시 재계산하지 않으면 옛 뷰의
        // flatten 결과가 새 diff 화면에 그대로 쓰인다 — 캐싱 도입 회귀 방지용.
        let mut viewer = DiffViewerUi::default();
        viewer.set_view(parse_unified(SAMPLE, false));
        assert_eq!(viewer.display_rows.len(), 9, "hunk1(5)+gap(1)+hunk2(3)");
        assert_eq!(viewer.hunk_starts, vec![0, 6]);

        // 단일 hunk·3행짜리 별개 뷰로 교체 — 이전 뷰(9행)의 캐시가 남으면 안 된다.
        let second = FileDiffView {
            hunks: vec![DiffHunk {
                old_start: 1,
                new_start: 1,
                lines: vec![
                    DiffLine {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(1),
                        text: "line1".into(),
                    },
                    DiffLine {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(2),
                        text: "line2".into(),
                    },
                    DiffLine {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(3),
                        text: "line3".into(),
                    },
                ],
            }],
            gaps: Vec::new(),
            binary: false,
            truncated: false,
        };
        viewer.set_view(second);
        assert_eq!(
            viewer.display_rows.len(),
            3,
            "옛 뷰(9행)가 아니라 새 뷰(3행) 기준으로 갱신돼야 한다"
        );
        assert_eq!(viewer.hunk_starts, vec![0]);
    }

    /// 두 번째 hunk를 초기 스크롤 밖(가상화로 안 그려지는 위치)에 두고, hunk
    /// 이동 버튼이 실제로 그 hunk를 화면에 끌어오는지(=스크롤 대상 이동)와
    /// 경계에서 멈추는지(=클램프)를 함께 검증한다. 필러 50줄은 일부러 적당히
    /// 잡았다 — 너무 적으면 뷰 밖으로 안 나가고, 너무 많으면 scroll_to_row가
    /// 쓰는 row_h(spacing 미포함)와 실제 행 간격(spacing 포함) 오차가 누적돼
    /// show_rows가 목표 행을 창 안에 못 넣는다(2026-08-16, 300줄로 처음 시도했다가
    /// 확인).
    fn 큰_두_hunk_뷰() -> FileDiffView {
        let hunk1: Vec<DiffLine> = (0..50)
            .map(|i| DiffLine {
                kind: LineKind::Context,
                old_no: Some(i + 1),
                new_no: Some(i + 1),
                text: format!("채움 {i}"),
            })
            .collect();
        let hunk2 = vec![DiffLine {
            kind: LineKind::Context,
            old_no: Some(500),
            new_no: Some(500),
            text: "둘째_hunk_고유_행".to_owned(),
        }];
        FileDiffView {
            hunks: vec![
                DiffHunk {
                    old_start: 1,
                    new_start: 1,
                    lines: hunk1,
                },
                DiffHunk {
                    old_start: 500,
                    new_start: 500,
                    lines: hunk2,
                },
            ],
            gaps: Vec::new(),
            binary: false,
            truncated: false,
        }
    }

    fn harness_for(viewer: DiffViewerUi) -> egui_kittest::Harness<'static, DiffViewerUi> {
        egui_kittest::Harness::new_ui_state(
            |ui, state: &mut DiffViewerUi| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                state.render(ui, &catalog, None);
            },
            viewer,
        )
    }

    #[test]
    fn kittest_hunk_이동_버튼은_스크롤_대상을_옮기고_경계에서_클램프된다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = DiffViewerUi::default();
        viewer.open("src/big.rs".to_owned(), DiffMode::Working);
        viewer.set_view(큰_두_hunk_뷰());
        let mut harness = harness_for(viewer);
        harness.run();

        assert!(
            harness
                .query_by_label_contains("둘째_hunk_고유_행")
                .is_none(),
            "초기 스크롤은 맨 위라 둘째 hunk는 가상화로 아직 그려지지 않아야 한다"
        );

        // ↑: 이미 첫 hunk(0)이라 하한에서 멈춘다 — 화면도 그대로다.
        harness.get_by_label("↑").click();
        harness.run();
        assert_eq!(harness.state().current_hunk, 0);
        assert!(
            harness
                .query_by_label_contains("둘째_hunk_고유_행")
                .is_none()
        );

        // ↓: 둘째 hunk로 스크롤 대상이 옮겨져 실제로 화면에 그려진다.
        harness.get_by_label("↓").click();
        harness.run();
        assert_eq!(harness.state().current_hunk, 1);
        assert!(
            harness
                .query_by_label_contains("둘째_hunk_고유_행")
                .is_some(),
            "hunk 이동 버튼은 실제로 스크롤 위치를 옮겨야 한다"
        );

        // 다시 ↓: 이미 마지막 hunk(1)라 상한에서 멈춘다.
        harness.get_by_label("↓").click();
        harness.run();
        assert_eq!(harness.state().current_hunk, 1);
    }

    /// [`diff_row_job`]이 프리픽스(행번호)와 본문을 하나의 job으로 합칠 때, 일치하지
    /// 않는 구간(프리픽스 포함)의 배경은 `TextFormat` 기본값(TRANSPARENT)이어야 그
    /// 행을 감싼 Frame의 add/del/context 배경이 그대로 비친다 — "강조가 기존 행
    /// 배경을 지우지 않는다"는 요구의 직접 증거. 동시에 일치 구간마다 섹션이
    /// 갈라지는지("일치 강조 섹션")도 함께 본다.
    #[test]
    fn diff_row_job은_일치_구간만_배경을_칠하고_나머지는_투명하다() {
        let prefix = "PRE ";
        let text = "hello world hello";
        let matches = crate::ui::aux_search::find_matches(text, "hello");
        assert_eq!(matches.ranges.len(), 2, "hello가 두 번 나온다");

        let font_id = egui::FontId::monospace(12.0);
        let match_bg = egui::Color32::from_rgb(1, 2, 3);
        let active_bg = egui::Color32::from_rgb(4, 5, 6);
        let job = diff_row_job(prefix, text, &matches, None, font_id, match_bg, active_bg);

        // 프리픽스 + 본문이 하나의 job으로 합쳐져야 한다 — 위젯 두 개로 쪼개면
        // ui.horizontal의 item_spacing이 둘 사이에 끼어든다.
        assert_eq!(job.text, format!("{prefix}{text}"));

        let bgs: Vec<_> = job.sections.iter().map(|s| s.format.background).collect();
        assert_eq!(
            bgs,
            vec![
                egui::Color32::TRANSPARENT, // "PRE " — 일치 아님, 행 배경이 비쳐야 한다.
                match_bg,                   // 첫 "hello"
                egui::Color32::TRANSPARENT, // " world " — 일치 아님.
                match_bg,                   // 둘째 "hello"
            ],
            "일치 구간만 배경이 칠해지고 나머지는 투명해야 한다"
        );
    }

    /// 활성 일치는 나머지 일치와 다른 색을 받아야 ↑↓가 어디로 갔는지 보인다.
    #[test]
    fn diff_row_job은_활성_일치만_다른_색을_쓴다() {
        let text = "hello world hello";
        let matches = crate::ui::aux_search::find_matches(text, "hello");
        let font_id = egui::FontId::monospace(12.0);
        let match_bg = egui::Color32::from_rgb(1, 2, 3);
        let active_bg = egui::Color32::from_rgb(4, 5, 6);
        assert_ne!(match_bg, active_bg);

        // 로컬 인덱스 1(두 번째 "hello")이 활성.
        let job = diff_row_job("", text, &matches, Some(1), font_id, match_bg, active_bg);
        let bgs: Vec<_> = job.sections.iter().map(|s| s.format.background).collect();
        assert_eq!(
            bgs,
            vec![match_bg, egui::Color32::TRANSPARENT, active_bg],
            "활성 일치(둘째)만 active_bg, 나머지는 match_bg여야 한다"
        );
    }

    /// `render`가 매 프레임 본문 전체를 재스캔하지 않도록, 질의가 바뀔 때만
    /// [`build_search_cache`]가 다시 채운다. 같은 질의로 다시 그려도 총계는
    /// 그대로다(캐시 재사용) — 질의를 바꾸면 그 질의 기준으로 갱신돼야 한다.
    #[test]
    fn 검색_캐시는_질의가_바뀔_때만_다시_채워진다() {
        let mut viewer = DiffViewerUi::default();
        viewer.open("src/a.rs".to_owned(), DiffMode::Working);
        viewer.set_view(parse_unified(SAMPLE, false));
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let ctx = egui::Context::default();

        // SAMPLE: "context1"·"context2"가 한 줄씩(질의 "context" 2건),
        // "old line"·"new line"·"added line"에 "line"이 한 번씩(질의 "line" 3건).
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            viewer.render(ui, &catalog, Some(("context", 0)));
        });
        assert_eq!(viewer.search_summary(), (2, false));

        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            viewer.render(ui, &catalog, Some(("line", 0)));
        });
        assert_eq!(
            viewer.search_summary(),
            (3, false),
            "질의가 바뀌었으니 캐시가 새 질의 기준으로 다시 채워져야 한다"
        );

        // 같은 질의를 반복해도(캐시 재사용) 총계는 그대로다.
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            viewer.render(ui, &catalog, Some(("line", 0)));
        });
        assert_eq!(viewer.search_summary(), (3, false));
    }

    fn harness_with_search(
        viewer: DiffViewerUi,
        query: &str,
    ) -> egui_kittest::Harness<'static, (DiffViewerUi, String)> {
        egui_kittest::Harness::new_ui_state(
            |ui, state: &mut (DiffViewerUi, String)| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                let (viewer, query) = state;
                viewer.render(ui, &catalog, Some((query.as_str(), 0)));
            },
            (viewer, query.to_owned()),
        )
    }

    /// 활성 일치(기본 인덱스 0)가 초기 스크롤 밖(가상화로 안 그려지는 위치)에 있으면
    /// `render`가 그 행으로 스크롤해야 한다 — hunk ↑↓의 `scroll_to_row`/
    /// `scroll_row_pitch` 관례를 그대로 재사용한다는 것의 증거.
    #[test]
    fn kittest_활성_일치가_있는_행으로_스크롤한다() {
        use egui_kittest::kittest::Queryable;

        let mut viewer = DiffViewerUi::default();
        viewer.open("src/big.rs".to_owned(), DiffMode::Working);
        viewer.set_view(큰_두_hunk_뷰());
        let mut harness = harness_with_search(viewer, "둘째_hunk_고유_행");
        harness.run();

        assert!(
            harness
                .query_by_label_contains("둘째_hunk_고유_행")
                .is_some(),
            "일치가 있는(가상화로 초기엔 안 보이던) 행으로 첫 렌더에서 스크롤돼야 한다"
        );
    }
}
