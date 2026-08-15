//! 메인 영역 실용형 diff 뷰어 — 행번호 + hunk 색 배경 + 접힌 문맥 + hunk 이동
//! (2026-08-15 스펙 §5). 문법 강조·인트라라인·미니맵은 의도적으로 범위 외.

// Task 4는 파서만 구현한다. 렌더(Task 7)와 app.rs 배선(Task 10)이 아직 이 모듈을
// 소비하지 않아 전부 dead_code로 잡힌다 — git_panel.rs와 같은 관례(git_panel.rs:6-8
// 참고). 렌더/배선 태스크가 끝나면 이 allow를 제거한다.
#![allow(dead_code)]

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
    pub old_start: u32,
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

/// untracked 파일 — diff가 없으므로 파일 내용 전체를 추가로 합성한다(스펙 §3).
pub fn synth_added(content: &str, truncated: bool) -> FileDiffView {
    let lines: Vec<DiffLine> = content
        .lines()
        .enumerate()
        .map(|(i, text)| DiffLine {
            kind: LineKind::Add,
            old_no: None,
            new_no: Some(i as u32 + 1),
            text: text.to_owned(),
        })
        .collect();
    FileDiffView {
        hunks: vec![DiffHunk { old_start: 0, new_start: 1, lines }],
        gaps: Vec::new(),
        binary: false,
        truncated,
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
    fn untracked_파일_내용은_전량_추가로_합성한다() {
        let view = synth_added("line1\nline2\n", false);
        assert_eq!(view.hunks.len(), 1);
        assert!(view.hunks[0].lines.iter().all(|l| l.kind == LineKind::Add));
        assert_eq!(view.hunks[0].lines.len(), 2);
    }
}
