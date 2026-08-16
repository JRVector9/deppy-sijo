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
    let mut view = FileDiffView { truncated, ..Default::default() };
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
            view.hunks.push(DiffHunk { old_start, new_start, lines: Vec::new() });
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
        hunk.lines.push(DiffLine { kind, old_no: o, new_no: n, text: text.to_owned() });
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
    }

    pub fn set_view(&mut self, view: FileDiffView) {
        self.loading = false;
        self.display_rows = flatten_display_rows(&view);
        self.hunk_starts = hunk_start_indices(&self.display_rows);
        self.view = Some(view);
    }

    pub fn render(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
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
                if ui.small_button("↓").on_hover_text(catalog.t("git.hunk.next", &[])).clicked()
                    && self.current_hunk + 1 < self.hunk_starts.len()
                {
                    self.current_hunk += 1;
                    self.scroll_to_row = self.hunk_starts.get(self.current_hunk).copied();
                }
                if ui.small_button("↑").on_hover_text(catalog.t("git.hunk.prev", &[])).clicked()
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
        let rows = &self.display_rows;
        let dark_mode = ui.visuals().dark_mode;
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        let mut scroll = egui::ScrollArea::both().auto_shrink([false, false]);
        if let Some(target) = self.scroll_to_row.take() {
            scroll = scroll.vertical_scroll_offset(target as f32 * row_h);
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
                        egui::Frame::NONE.fill(bg).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.monospace(format!(
                                    "{:>5} {:>5} {sign} {}",
                                    no(l.old_no),
                                    no(l.new_no),
                                    l.text
                                ));
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
            vec![LineKind::Context, LineKind::Del, LineKind::Add, LineKind::Add, LineKind::Context]
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
                    DiffLine { kind: LineKind::Add, old_no: None, new_no: Some(1), text: "line1".into() },
                    DiffLine { kind: LineKind::Add, old_no: None, new_no: Some(2), text: "line2".into() },
                    DiffLine { kind: LineKind::Add, old_no: None, new_no: Some(3), text: "line3".into() },
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
                DiffHunk { old_start: 1, new_start: 1, lines: hunk1 },
                DiffHunk { old_start: 500, new_start: 500, lines: hunk2 },
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
                state.render(ui, &catalog);
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
            harness.query_by_label_contains("둘째_hunk_고유_행").is_none(),
            "초기 스크롤은 맨 위라 둘째 hunk는 가상화로 아직 그려지지 않아야 한다"
        );

        // ↑: 이미 첫 hunk(0)이라 하한에서 멈춘다 — 화면도 그대로다.
        harness.get_by_label("↑").click();
        harness.run();
        assert_eq!(harness.state().current_hunk, 0);
        assert!(harness.query_by_label_contains("둘째_hunk_고유_행").is_none());

        // ↓: 둘째 hunk로 스크롤 대상이 옮겨져 실제로 화면에 그려진다.
        harness.get_by_label("↓").click();
        harness.run();
        assert_eq!(harness.state().current_hunk, 1);
        assert!(
            harness.query_by_label_contains("둘째_hunk_고유_행").is_some(),
            "hunk 이동 버튼은 실제로 스크롤 위치를 옮겨야 한다"
        );

        // 다시 ↓: 이미 마지막 hunk(1)라 상한에서 멈춘다.
        harness.get_by_label("↓").click();
        harness.run();
        assert_eq!(harness.state().current_hunk, 1);
    }
}
