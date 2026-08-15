//! 사이드바 Git 탭 본문 — orca 스타일 읽기 전용 상태 패널 (2026-08-15 스펙:
//! docs/superpowers/specs/2026-08-15-git-panel-design.md).
//! leaf는 intent(GitPanelAction)만 반환하고 git 실행·뷰 전환은 App이 소유한다.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 섹션당 수집 상한 — diff_panel의 MAX_DIFF_FILES와 같은 값(512). 표시 기본값은
/// SECTION_COLLAPSED_ROWS이고 「모두 보기」로 펼친다.
pub const MAX_PANEL_FILES: usize = 512;
/// 접힘 상태에서 섹션당 보여주는 행 수 — orca 스크린샷 기준 한 화면 분량.
pub const SECTION_COLLAPSED_ROWS: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitFileRow {
    pub rel_path: String,
    /// porcelain XY 중 워킹트리(Y) 우선, Y가 공백이면 X. untracked(??)는 '?'.
    pub status: char,
    /// None = 바이너리 또는 untracked(numstat 없음).
    pub adds: Option<u32>,
    pub dels: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct GitPanelSnapshot {
    pub repo_root: PathBuf,
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub changes: Vec<GitFileRow>,
    pub committed: Vec<GitFileRow>,
    pub changes_truncated: bool,
    pub committed_truncated: bool,
}

/// `status --porcelain -z -uall` + `diff --numstat HEAD`를 경로로 병합한다.
/// porcelain 등장 순서를 유지한다(사용자가 보는 안정된 순서).
fn merge_status_rows(porcelain_z: &str, numstat: &str) -> Vec<GitFileRow> {
    let counts = parse_numstat(numstat);
    let mut rows = Vec::new();
    let mut fields = porcelain_z.split('\0').filter(|s| !s.is_empty());
    while let Some(entry) = fields.next() {
        if entry.len() < 4 {
            continue; // "XY " 접두 미달 — 손상 항목은 건너뛴다(패널 전체를 죽이지 않음).
        }
        let (xy, path) = entry.split_at(3);
        let mut chars = xy.chars();
        let x = chars.next().unwrap_or(' ');
        let y = chars.next().unwrap_or(' ');
        // rename/copy는 다음 NUL 필드가 원경로다 — 소비만 하고 표시는 새 경로.
        if x == 'R' || x == 'C' {
            let _ = fields.next();
        }
        let status = if x == '?' {
            '?'
        } else if y != ' ' {
            y // 워킹트리 우선 (스펙 §3)
        } else {
            x
        };
        let (adds, dels) = counts.get(path).copied().unwrap_or((None, None));
        rows.push(GitFileRow { rel_path: path.to_owned(), status, adds, dels });
        if rows.len() >= MAX_PANEL_FILES {
            break;
        }
    }
    rows
}

/// `diff --numstat` 한 줄 = "adds\tdels\tpath" (바이너리는 "-\t-").
fn parse_numstat(numstat: &str) -> std::collections::HashMap<String, (Option<u32>, Option<u32>)> {
    let mut out = std::collections::HashMap::new();
    for line in numstat.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        out.insert(path.to_owned(), (a.parse().ok(), d.parse().ok()));
    }
    out
}

/// committed 섹션: `diff --numstat base..HEAD` + `diff --name-status base..HEAD` 병합.
/// name-status 등장 순서를 유지한다.
fn merge_committed_rows(numstat: &str, name_status: &str) -> Vec<GitFileRow> {
    let counts = parse_numstat(numstat);
    let mut rows = Vec::new();
    for line in name_status.lines() {
        let mut parts = line.splitn(2, '\t');
        let (Some(status), Some(path)) = (parts.next(), parts.next()) else {
            continue;
        };
        // rename 라인("R100\told\tnew")은 마지막 필드가 새 경로다.
        let path = path.rsplit('\t').next().unwrap_or(path);
        let status = status.chars().next().unwrap_or('M');
        let (adds, dels) = counts.get(path).copied().unwrap_or((None, None));
        rows.push(GitFileRow { rel_path: path.to_owned(), status, adds, dels });
        if rows.len() >= MAX_PANEL_FILES {
            break;
        }
    }
    rows
}

/// `rev-list --left-right --count upstream...HEAD` → (ahead, behind).
/// 출력은 "behind\tahead"(왼쪽=upstream) 순서다.
fn parse_ahead_behind(output: &str) -> Option<(u32, u32)> {
    let mut parts = output.trim().split('\t');
    let behind: u32 = parts.next()?.trim().parse().ok()?;
    let ahead: u32 = parts.next()?.trim().parse().ok()?;
    Some((ahead, behind))
}

/// 렌더용 파일명/디렉터리 분리 — "crates/app/src/app.rs" → ("app.rs", "crates/app/src").
fn split_row_path(rel_path: &str) -> (&str, &str) {
    match rel_path.rsplit_once('/') {
        Some((dir, name)) => (name, dir),
        None => (rel_path, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain과_numstat을_경로로_병합한다() {
        // porcelain -z: "XY path\0" 반복. rename은 "R  new\0old\0".
        let porcelain = " M a.rs\0?? new.txt\0R  moved.rs\0old.rs\0MM both.rs\0";
        let numstat = "3\t1\ta.rs\n456\t221\tmoved.rs\n-\t-\tbin.png\n2\t0\tboth.rs\n";
        let rows = merge_status_rows(porcelain, numstat);
        // 순서는 porcelain 등장 순서를 유지한다.
        assert_eq!(rows.len(), 4);
        assert_eq!(
            (rows[0].rel_path.as_str(), rows[0].status, rows[0].adds, rows[0].dels),
            ("a.rs", 'M', Some(3), Some(1))
        );
        // untracked: numstat 없음 → 수치 None, 상태 '?'
        assert_eq!(
            (rows[1].rel_path.as_str(), rows[1].status, rows[1].adds),
            ("new.txt", '?', None)
        );
        // rename: 새 경로 기준, 상태는 X('R') — Y가 공백이므로.
        assert_eq!(
            (rows[2].rel_path.as_str(), rows[2].status, rows[2].adds),
            ("moved.rs", 'R', Some(456))
        );
        // staged+unstaged 겹침(XY="MM"): 워킹트리(Y) 우선 → 'M'.
        assert_eq!((rows[3].rel_path.as_str(), rows[3].status), ("both.rs", 'M'));
    }

    #[test]
    fn numstat만_있는_경로는_committed_파서가_그대로_담는다() {
        // committed 섹션: numstat + name-status 병합. 바이너리는 "-\t-".
        let numstat = "12\t13\tsrc/ui/workspace.rs\n-\t-\tassets/logo.png\n";
        let name_status = "M\tsrc/ui/workspace.rs\nA\tassets/logo.png\n";
        let rows = merge_committed_rows(numstat, name_status);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].rel_path.as_str(), rows[0].status, rows[0].adds, rows[0].dels),
            ("src/ui/workspace.rs", 'M', Some(12), Some(13))
        );
        assert_eq!(
            (rows[1].rel_path.as_str(), rows[1].status, rows[1].adds, rows[1].dels),
            ("assets/logo.png", 'A', None, None)
        );
    }

    #[test]
    fn ahead_behind는_left_right_count_출력을_읽는다() {
        assert_eq!(parse_ahead_behind("73\t4\n"), Some((4, 73)));
        assert_eq!(parse_ahead_behind("0\t0"), Some((0, 0)));
        assert_eq!(parse_ahead_behind("garbage"), None);
    }

    #[test]
    fn 파일명과_디렉터리를_분리한다() {
        assert_eq!(split_row_path("crates/app/src/app.rs"), ("app.rs", "crates/app/src"));
        assert_eq!(split_row_path("Cargo.toml"), ("Cargo.toml", ""));
    }
}
