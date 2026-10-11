//! 사이드바 Git 탭 본문 — orca 스타일 읽기 전용 상태 패널 (2026-08-15 스펙:
//! docs/superpowers/specs/2026-08-15-git-panel-design.md).
//! leaf는 intent(GitPanelAction)만 반환하고 git 실행·뷰 전환은 App이 소유한다.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 섹션당 수집 상한 — diff_panel의 MAX_DIFF_FILES와 같은 값(512). 표시 기본값은
/// SECTION_COLLAPSED_ROWS이고 「모두 보기」로 펼친다.
pub const MAX_PANEL_FILES: usize = 512;
/// 「모두 보기」 전 탭당 보여주는 파일 행 수 — orca 스크린샷 기준 한 화면 분량.
pub const SECTION_COLLAPSED_ROWS: usize = 10;
/// 워크트리 목록 상한 — 목록은 사람이 훑는 것이라 32면 충분하고, 초과분은 잘림 표시만 한다.
pub const MAX_WORKTREE_ROWS: usize = 32;
/// 셸 스폰 경로 상한 — App의 `spawn_shell_at`이 같은 검사를 하지만 leaf도 넘기지 않는다.
pub const WORKTREE_PATH_MAX_BYTES: usize = 4096;
/// 브랜치 커밋 목록 상한 — 사람이 훑는 목록이라 64면 충분하고, 초과분은 잘림 표시만 한다.
pub const MAX_BRANCH_COMMITS: usize = 64;
/// [`CommitSha`] 최대 길이 — SHA-256 저장소의 전체 sha(64자)까지 담는다.
const COMMIT_SHA_MAX_LEN: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitFileRow {
    pub rel_path: String,
    /// porcelain XY 중 워킹트리(Y) 우선, Y가 공백이면 X. untracked(??)는 '?'.
    pub status: char,
    /// None = 바이너리 또는 untracked(numstat 없음).
    pub adds: Option<u32>,
    pub dels: Option<u32>,
}

/// `git worktree list --porcelain` 한 항목. 클릭하면 App이 이 경로에서 셸을 연다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitWorktreeRow {
    pub path: String,
    /// 표시용 마지막 경로 요소. 경로가 루트라 요소가 없으면 경로 전체를 쓴다.
    pub name: String,
    /// None = detached HEAD.
    pub branch: Option<String>,
    /// 지금 보고 있는 워크트리(= snapshot.repo_root)인가.
    pub current: bool,
}

/// 커밋 sha(16진 문자열). `DiffMode`가 `Copy`라 String이 아니라 고정 길이 배열에 담는다.
/// `parse`만 값을 만들므로 안의 바이트는 항상 ASCII 16진수다.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CommitSha {
    hex: [u8; COMMIT_SHA_MAX_LEN],
    len: u8,
}

impl CommitSha {
    /// 4~64자 16진수만 받는다(git의 최소 약어 길이 4). git 인자로 그대로 넘기므로
    /// 옵션(`-`)이나 경로로 해석될 값은 여기서 걸러진다.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if !(4..=COMMIT_SHA_MAX_LEN).contains(&bytes.len())
            || !bytes.iter().all(u8::is_ascii_hexdigit)
        {
            return None;
        }
        let mut hex = [0; COMMIT_SHA_MAX_LEN];
        hex[..bytes.len()].copy_from_slice(bytes);
        Some(Self {
            hex,
            len: u8::try_from(bytes.len()).ok()?,
        })
    }

    pub fn as_str(&self) -> &str {
        // parse가 ASCII 16진수만 담으므로 실패하지 않는다.
        std::str::from_utf8(&self.hex[..usize::from(self.len)]).unwrap_or_default()
    }

    /// 화면용 앞 7자 — git에는 항상 전체 값(`as_str`)을 넘긴다.
    pub fn short(&self) -> &str {
        let text = self.as_str();
        &text[..text.len().min(7)]
    }
}

impl std::fmt::Debug for CommitSha {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 브랜치 커밋 한 항목 — `git log`의 짧은 sha·제목·커밋 시각(2026-10-11 PR 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitCommitRow {
    pub sha: CommitSha,
    pub subject: String,
    /// 커밋(committer) 시각, unix 초.
    pub time: i64,
}

/// 커밋 하나가 바꾼 파일 — committed 섹션과 같은 행 모델, 같은 상한(MAX_PANEL_FILES).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitCommitFiles {
    pub rows: Vec<GitFileRow>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default)]
pub struct GitPanelSnapshot {
    // 스펙 §2 데이터 모델의 일부 — 수집 시점의 `git_cli::repo_root` 결과를 그대로
    // 담아 둔다. 목록은 상대 경로(rel_path)로 그리고, 경로 동작(우클릭 메뉴·diff 헤더의
    // 경로 복사·세션에 넣기·Finder, 2026-10-11 PR 5)이 이 값 + rel_path로 절대 경로를
    // 만든다. 파일 diff 수집은 App이 넘긴 cwd로 repo_root를 다시 구해(collect_file_diff)
    // 이 필드를 쓰지 않는다.
    pub repo_root: PathBuf,
    pub branch: String,
    pub upstream: Option<String>,
    /// `upstream`이 추적 브랜치가 아니라 `origin/HEAD` 폴백인가 — 그때 ahead/behind는
    /// push·pull로 맞출 「보낼/받을 커밋」이 아니라 기준 대비 앞섬·뒤짐이다.
    pub upstream_fallback: bool,
    pub ahead: u32,
    pub behind: u32,
    pub changes: Vec<GitFileRow>,
    pub committed: Vec<GitFileRow>,
    pub changes_truncated: bool,
    pub committed_truncated: bool,
    /// `origin` remote가 GitHub면 `https://github.com/OWNER/REPO`로 정규화한 값.
    /// GitHub가 아니거나 remote 조회 실패 시 None(스펙 §4 — ↗ 아이콘 숨김 조건).
    /// (Task 10 Step 7 소급 요구 — collect_snapshot에서 remote 조회 실패해도
    /// 스냅샷 전체를 죽이지 않고 None으로만 담는다.)
    pub remote_https_base: Option<String>,
    pub worktrees: Vec<GitWorktreeRow>,
    pub worktrees_truncated: bool,
    /// `committed`와 같은 기준(merge-base..HEAD)의 커밋, 최신순(2026-10-11 PR 4).
    /// upstream이 없거나 조회가 실패하면 비어 있다.
    pub commits: Vec<GitCommitRow>,
    pub commits_truncated: bool,
}

/// `status --porcelain -z -uall` + `diff --numstat HEAD`를 경로로 병합한다.
/// porcelain 등장 순서를 유지한다(사용자가 보는 안정된 순서). 반환하는 bool은
/// 상한(MAX_PANEL_FILES) 때문에 항목을 실제로 버렸는지다 — 딱 상한만큼만 있고
/// 더는 없으면 false(diff_panel::build_file_rows와 같은 패턴, 중요 리뷰 지적 3번).
fn merge_status_rows(porcelain_z: &str, numstat: &str) -> (Vec<GitFileRow>, bool) {
    let counts = parse_numstat(numstat);
    let mut rows = Vec::new();
    let mut truncated = false;
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
        // unmerged 짝(git-status(1) Short Format 표)은 X·Y 어느 쪽을 골라도 충돌을 못 보인다.
        let unmerged = matches!(
            (x, y),
            ('D', 'D')
                | ('A', 'U')
                | ('U', 'D')
                | ('U', 'A')
                | ('D', 'U')
                | ('A', 'A')
                | ('U', 'U')
        );
        let status = if unmerged {
            'U'
        } else if x == '?' {
            '?'
        } else if y != ' ' {
            y // 워킹트리 우선 (스펙 §3)
        } else {
            x
        };
        // 새 항목을 담기 *전에* 상한을 검사한다 — 이미 상한만큼 담은 뒤에 검사하면
        // 정확히 상한일 때와 넘칠 때를 구분할 수 없다.
        if rows.len() >= MAX_PANEL_FILES {
            truncated = true;
            break;
        }
        let (adds, dels) = counts.get(path).copied().unwrap_or((None, None));
        rows.push(GitFileRow {
            rel_path: path.to_owned(),
            status,
            adds,
            dels,
        });
    }
    (rows, truncated)
}

/// `diff --numstat -z` 파싱 — 항목 = "adds\tdels\tpath\0"(바이너리는 "-\t-"). rename/copy는
/// 경로 칸이 비고 다음 두 NUL 필드가 원경로·새 경로다(새 경로로 센다). `-z` 없이 받으면
/// 한글·공백·제어문자 경로가 C-quote(`"\355…"`)돼 porcelain 경로와 짝지어지지 않았다.
fn parse_numstat(numstat_z: &str) -> std::collections::HashMap<String, (Option<u32>, Option<u32>)> {
    let mut out = std::collections::HashMap::new();
    let mut fields = numstat_z.split('\0');
    while let Some(head) = fields.next() {
        let mut parts = head.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let path = if path.is_empty() {
            let _source = fields.next();
            match fields.next() {
                Some(destination) => destination,
                None => break,
            }
        } else {
            path
        };
        out.insert(path.to_owned(), (a.parse().ok(), d.parse().ok()));
    }
    out
}

/// committed 섹션: `diff --numstat -z base..HEAD` + `diff --name-status -z base..HEAD` 병합.
/// name-status 등장 순서를 유지한다. 반환하는 bool의 의미는 merge_status_rows와
/// 같다 — 정확히 상한만큼이면 false, 하나라도 더 버렸으면 true.
fn merge_committed_rows(numstat_z: &str, name_status_z: &str) -> (Vec<GitFileRow>, bool) {
    let counts = parse_numstat(numstat_z);
    let mut rows = Vec::new();
    let mut truncated = false;
    // 항목 = "상태\0경로\0", rename/copy는 "R100\0원경로\0새경로\0"(새 경로를 쓴다).
    let mut fields = name_status_z.split('\0');
    while let Some(status) = fields.next() {
        if status.is_empty() {
            continue;
        }
        let Some(mut path) = fields.next() else {
            break;
        };
        let status = status.chars().next().unwrap_or('M');
        if status == 'R' || status == 'C' {
            let Some(destination) = fields.next() else {
                break;
            };
            path = destination;
        }
        if rows.len() >= MAX_PANEL_FILES {
            truncated = true;
            break;
        }
        let (adds, dels) = counts.get(path).copied().unwrap_or((None, None));
        rows.push(GitFileRow {
            rel_path: path.to_owned(),
            status,
            adds,
            dels,
        });
    }
    (rows, truncated)
}

/// `git worktree list --porcelain -z` 파싱. `-z`는 git-worktree(1) 표현으로 "경로에
/// 개행이 있을 때 출력을 파싱할 수 있게 한다" — 필드가 줄바꿈이 아니라 NUL로
/// 끝난다(빈 줄 대신 빈 필드가 항목 경계). `worktree <path>` 필드가 새 항목을 열고,
/// `branch refs/heads/<name>`이 브랜치, `detached`는 None, `bare`는 **버린다**
/// (체크아웃이 없어 셸을 열 수 없다). `locked`/`prunable` 필드는 무시한다.
/// 반환하는 bool은 상한(MAX_WORKTREE_ROWS) 때문에 항목을 실제로 버렸는지다 —
/// bare로 버린 항목은 잘림이 아니라 애초에 표시 대상이 아니므로 포함하지 않는다.
fn parse_worktree_list(porcelain: &str, repo_root: &Path) -> (Vec<GitWorktreeRow>, bool) {
    let mut rows: Vec<GitWorktreeRow> = Vec::new();
    let mut truncated = false;
    let mut path: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut bare = false;
    let mut flush = |path: &mut Option<String>, branch: &mut Option<String>, bare: &mut bool| {
        let taken = path.take();
        let taken_branch = branch.take();
        let was_bare = std::mem::replace(bare, false);
        let Some(taken) = taken else { return };
        if was_bare {
            return;
        }
        // 새 항목을 담기 *전에* 상한을 검사한다 — merge_status_rows와 같은 이유.
        if rows.len() >= MAX_WORKTREE_ROWS {
            truncated = true;
            return;
        }
        let as_path = Path::new(&taken);
        let name = as_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| taken.clone());
        let current = as_path == repo_root;
        rows.push(GitWorktreeRow {
            path: taken,
            name,
            branch: taken_branch,
            current,
        });
    };
    // NUL 구분 필드 — 경로에 개행이 있어도 안 잘린다(빈 문자열 필터는 안 쓴다: 빈
    // 필드 자체가 non-z의 빈 줄과 같은 항목 경계 신호라 flush 판단에 쓰인다).
    for field in porcelain.split('\0') {
        if let Some(rest) = field.strip_prefix("worktree ") {
            flush(&mut path, &mut branch, &mut bare);
            path = Some(rest.to_owned()); // trim 금지 — 경로 끝 공백도 유효한 경로다.
        } else if let Some(rest) = field.strip_prefix("branch refs/heads/") {
            branch = Some(rest.to_owned());
        } else if field == "bare" {
            bare = true;
        }
    }
    flush(&mut path, &mut branch, &mut bare);
    (rows, truncated)
}

/// `git log -z --format=%H%x00%s%x00%ct` 파싱 — 필드도 항목도 NUL로 끝나므로 NUL로만
/// 나누면 제목에 탭·따옴표·유니코드가 있어도 안 어긋난다(git은 커밋 메시지에 NUL을
/// 허용하지 않는다). 순서는 git log 출력(최신순) 그대로다. sha·시각이 손상된 항목은
/// 건너뛰고, 반환하는 bool은 상한(MAX_BRANCH_COMMITS) 때문에 항목을 실제로 버렸는지다.
fn parse_branch_commits(log_z: &str) -> (Vec<GitCommitRow>, bool) {
    // 마지막 NUL 뒤는 바이트 상한에서 잘린 미완성 조각이다 — 버린다.
    let terminated = log_z.rfind('\0').map_or("", |end| &log_z[..end]);
    let mut fields = terminated.split('\0');
    let mut rows = Vec::new();
    let mut truncated = false;
    while let (Some(sha), Some(subject), Some(time)) = (fields.next(), fields.next(), fields.next())
    {
        let (Some(sha), Ok(time)) = (CommitSha::parse(sha), time.parse()) else {
            continue;
        };
        // 새 항목을 담기 *전에* 상한을 검사한다 — merge_status_rows와 같은 이유.
        if rows.len() >= MAX_BRANCH_COMMITS {
            truncated = true;
            break;
        }
        rows.push(GitCommitRow {
            sha,
            subject: subject.to_owned(),
            time,
        });
    }
    (rows, truncated)
}

/// 셸을 열 수 있는 경로인가 — 빈 값·상한 초과·NUL은 클릭 intent를 만들지 않는다.
fn worktree_path_is_spawnable(path: &str) -> bool {
    !path.is_empty() && path.len() <= WORKTREE_PATH_MAX_BYTES && !path.as_bytes().contains(&0)
}

/// 워크트리가 메인 하나뿐이면 워크트리 탭을 그리지 않는다 — 정보가 0이다.
fn worktree_section_visible(snapshot: &GitPanelSnapshot) -> bool {
    snapshot.worktrees.len() > 1
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

/// 목록용 폴더 표시 — 끝 두 단계만 남기고 앞은 「…/」로 줄인다. 300pt 목록에서
/// 앞부분부터 잘리면 `crates/…`만 남아 같은 저장소의 파일끼리 구분되지 않았다
/// (2026-10-11 시안). 전체 경로는 행 hover와 diff 헤더에 그대로 있다.
fn short_dir(dir: &str) -> std::borrow::Cow<'_, str> {
    let mut parts = dir.rsplitn(3, '/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(last), Some(second), Some(_)) => format!("…/{second}/{last}").into(),
        _ => dir.into(),
    }
}

/// 좌측 목록 필터 — `rel_path`가 `filter`에 대소문자 무시로 걸리는 행만 남긴다
/// (2026-08-18 스펙 §좌측 목록 필터). 빈 `filter`는 필터 없음과 같아 전부 통과시킨다
/// — `aux_search::contains_match`는 빈 needle이면 항상 거짓을 돌려주므로 이 분기가
/// 없으면 필터가 없을 때도 전부 걸러진다. `changes`/`committed` 행에만 쓰고
/// 워크트리 탭에는 쓰지 않는다(경로 목록이라 검색 대상이 아니다).
fn filter_file_rows<'a>(rows: &'a [GitFileRow], filter: &str) -> Vec<&'a GitFileRow> {
    rows.iter()
        .filter(|row| {
            filter.is_empty() || crate::ui::aux_search::contains_match(&row.rel_path, filter)
        })
        .collect()
}

/// ahead/추가는 성공색, behind/삭제는 에러색 — 세션/에이전트 진행 상태가 아니라
/// 일반 git 통계라 `agent_visuals::status_color`가 아니라 designall 토큰을 쓴다.
/// `agent_visuals::status_color`는 dark_mode 분기가 없는 다크 전용 파스텔이라
/// 라이트 테마에서 대비가 낮았다(중요 리뷰 지적 4번) — designall.rs의
/// `Tokens::success`/`error`는 정확히 이 "체계 밖의 일반 성공/실패 표시" 용도로
/// 예약돼 있다(그 파일 상단 주석 참고).
fn stat_color(tokens: crate::ui::designall::Tokens, positive: bool) -> egui::Color32 {
    if positive {
        tokens.success
    } else {
        tokens.error
    }
}

/// porcelain 상태 문자 → (배지 글자, 색, hover 설명 i18n 키). 행 끝의 `M`·`?` 글자만으로는
/// 의미가 읽히지 않아(2026-10-11 시안) 행 앞의 색 배지와 설명으로 바꾼다.
fn status_badge_style(
    status: char,
    tokens: crate::ui::designall::Tokens,
) -> (char, egui::Color32, &'static str) {
    match status {
        'M' => ('M', tokens.warning, "git.status.modified"),
        'A' => ('A', tokens.success, "git.status.added"),
        'D' => ('D', tokens.error, "git.status.deleted"),
        'R' => ('R', tokens.accent, "git.status.renamed"),
        'C' => ('C', tokens.accent, "git.status.copied"),
        'U' => ('U', tokens.error, "git.status.conflict"),
        '?' => ('N', tokens.muted_text, "git.status.untracked"),
        other => (other, tokens.muted_text, "git.status.other"),
    }
}

/// 18pt 색 배지 하나. hover 전용 sense라 행 클릭을 가로채지 않고, 설명은 행 hover가 맡는다.
fn status_badge(ui: &mut egui::Ui, status: char, tokens: crate::ui::designall::Tokens) {
    let (letter, color, _) = status_badge_style(status, tokens);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 4.0, color.gamma_multiply(0.16));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        letter,
        egui::FontId::monospace(10.5),
        color,
    );
}

/// 경로 동작(경로 복사·세션에 경로 넣기·Finder에서 보기)의 대상 파일 — 행 우클릭 메뉴와
/// diff 헤더 버튼이 같이 쓴다(2026-10-11 PR 5). 복사는 leaf가 끝내고(파일 트리
/// 「경로 복사」와 같은 `ctx.copy_text`), 나머지 둘은 App intent로 올린다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitPathTarget {
    /// 스냅샷의 repo_root + rel_path.
    pub abs_path: PathBuf,
    /// 'D' 행 — 그 경로에 파일이 없어 세션에 넣기·Finder는 끈다(복사는 남긴다).
    pub deleted: bool,
}

impl GitPathTarget {
    fn for_row(repo_root: &Path, row: &GitFileRow) -> Self {
        Self {
            abs_path: repo_root.join(&row.rel_path),
            deleted: row.status == 'D',
        }
    }

    /// 클립보드에 넣을 문자열 — 파일 트리 「경로 복사」와 같은 `display()` 표기.
    pub fn copy_text(&self) -> String {
        self.abs_path.display().to_string()
    }

    /// 「세션에 경로 넣기」 intent. 삭제된 파일이거나 파일 트리의 경로 검증
    /// (`FileTreePathPayload::try_new` — NUL·길이 상한)을 못 넘으면 None이고 버튼을 끈다.
    pub fn insert_action(&self) -> Option<GitPanelAction> {
        self.existing_path()
            .map(|path| GitPanelAction::InsertPath { path })
    }

    /// 「Finder에서 보기」 intent — 끄는 조건은 [`Self::insert_action`]과 같다.
    pub fn reveal_action(&self) -> Option<GitPanelAction> {
        self.existing_path()
            .map(|path| GitPanelAction::RevealInFinder { path })
    }

    fn existing_path(&self) -> Option<PathBuf> {
        if self.deleted {
            return None;
        }
        crate::ui::file_tree::FileTreePathPayload::try_new(self.abs_path.clone())
            .ok()
            .map(crate::ui::file_tree::FileTreePathPayload::into_path)
    }
}

/// 파일 행 우클릭 메뉴(2026-10-11 PR 5) — 경로 복사 · 세션에 경로 넣기 · Finder에서 보기.
/// 행 렌더(PR 2가 다시 짠다)와 섞이지 않게 별도 함수로 두고 행 응답에 호출 하나로 붙인다.
fn file_row_context_menu(
    response: &egui::Response,
    repo_root: &Path,
    row: &GitFileRow,
    catalog: &i18n::Catalog,
) -> Option<GitPanelAction> {
    let mut action = None;
    response.context_menu(|ui| {
        let target = GitPathTarget::for_row(repo_root, row);
        if ui
            .button(catalog.t("git.path.copy", &[]))
            .on_hover_text(catalog.t("git.path.copy_hint", &[]))
            .clicked()
        {
            ui.ctx().copy_text(target.copy_text());
            ui.close();
        }
        let insert = target.insert_action();
        if ui
            .add_enabled(
                insert.is_some(),
                egui::Button::new(catalog.t("git.path.insert", &[])),
            )
            .on_hover_text(catalog.t("git.path.insert_hint", &[]))
            .on_disabled_hover_text(catalog.t("git.path.deleted", &[]))
            .clicked()
        {
            action = insert;
            ui.close();
        }
        let reveal = target.reveal_action();
        if ui
            .add_enabled(
                reveal.is_some(),
                egui::Button::new(catalog.t("git.path.reveal", &[])),
            )
            .on_hover_text(catalog.t("git.path.reveal_hint", &[]))
            .on_disabled_hover_text(catalog.t("git.path.deleted", &[]))
            .clicked()
        {
            action = reveal;
            ui.close();
        }
    });
    action
}

/// `git remote get-url origin` 출력을 OWNER/REPO 기준 GitHub HTTPS URL로 정규화한다.
/// 지원 형식: `https://github.com/OWNER/REPO(.git)`, `git@github.com:OWNER/REPO(.git)`.
/// github.com이 아니거나 OWNER/REPO 형태가 아니면 None(스펙 §4 — ↗ 아이콘 숨김 조건).
fn normalize_github_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let rest = rest.trim_matches('/');
    let mut parts = rest.split('/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
        return None; // OWNER/REPO 정확히 2세그먼트가 아니면(빈 값 포함) 거부.
    };
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("https://github.com/{owner}/{repo}"))
}

/// git 수집 타임아웃/바이트 상한 — diff_panel과 동일 정책(2026-08-15 스펙 §3).
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_LIST_BYTES: usize = 200 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitPanelErrorCode {
    NoRepo,
    CollectionFailed,
}

/// 세션 cwd에서 패널 스냅샷을 수집한다. **App host 스레드에서만 부른다**(blocking git).
/// 비교 기준 — 추적 브랜치(`@{u}`), 없으면 `origin/HEAD`(기본 브랜치) 폴백(스펙 §3).
/// bool은 폴백인지다. 스냅샷과 브랜치 diff가 같은 기준을 써야 커밋 탭의 파일이 열린다.
fn resolve_upstream(
    run: impl Fn(&[&str]) -> Result<(String, bool), GitPanelErrorCode>,
) -> Option<(String, bool)> {
    let trimmed = |out: Result<(String, bool), GitPanelErrorCode>| {
        out.ok()
            .map(|(text, _)| text.trim().to_owned())
            .filter(|text| !text.is_empty())
    };
    trimmed(run(&[
        "rev-parse",
        "--abbrev-ref",
        "--symbolic-full-name",
        "@{u}",
    ]))
    .map(|upstream| (upstream, false))
    .or_else(|| {
        trimmed(run(&[
            "symbolic-ref",
            "--short",
            "refs/remotes/origin/HEAD",
        ]))
        .map(|upstream| (upstream, true))
    })
}

pub fn collect_snapshot(cwd: &Path) -> Result<GitPanelSnapshot, GitPanelErrorCode> {
    let repo_root =
        crate::git_cli::repo_root(cwd, GIT_TIMEOUT).map_err(|_| GitPanelErrorCode::NoRepo)?;
    let run = |args: &[&str]| -> Result<(String, bool), GitPanelErrorCode> {
        crate::git_cli::run_git_limited(&repo_root, args, GIT_TIMEOUT, MAX_LIST_BYTES)
            .map_err(|_| GitPanelErrorCode::CollectionFailed)
    };

    // 빈 repo(커밋 0개)는 여기서 CollectionFailed로 떨어진다(HEAD가 없어 rev-parse 실패).
    // 빈 repo에서 status만이라도 보여주는 건 범위 외(스펙 §6 "섹션 단위 오류" 대상, 2026-08-15).
    let (branch_raw, _) = run(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let branch = if branch_raw.trim() == "HEAD" {
        // detached — 짧은 SHA로 표시.
        run(&["rev-parse", "--short", "HEAD"])?.0.trim().to_owned()
    } else {
        branch_raw.trim().to_owned()
    };

    // 업스트림: 추적 브랜치 → origin/HEAD 폴백 → None(스펙 §3).
    let (upstream, upstream_fallback) = match resolve_upstream(run) {
        Some((upstream, fallback)) => (Some(upstream), fallback),
        None => (None, false),
    };

    let (mut ahead, mut behind) = (0, 0);
    let mut committed = Vec::new();
    let mut committed_truncated = false;
    let mut commits = Vec::new();
    let mut commits_truncated = false;
    if let Some(upstream) = upstream.as_deref() {
        let range = format!("{upstream}...HEAD");
        if let Ok((counts, _)) = run(&["rev-list", "--left-right", "--count", &range])
            && let Some((a, b)) = parse_ahead_behind(&counts)
        {
            (ahead, behind) = (a, b);
        }
        if let Ok((base, _)) = run(&["merge-base", upstream, "HEAD"]) {
            let base = base.trim().to_owned();
            let range = format!("{base}..HEAD");
            let (numstat, t1) = run(&["diff", "--no-ext-diff", "--numstat", "-z", &range])?;
            let (names, t2) = run(&["diff", "--no-ext-diff", "--name-status", "-z", &range])?;
            let (rows, rows_truncated) = merge_committed_rows(&numstat, &names);
            committed = rows;
            committed_truncated = t1 || t2 || rows_truncated;
            // 같은 기준의 커밋 목록. 이 조회가 실패해도 스냅샷 전체는 죽이지 않는다
            // (remote 조회와 같은 규칙). 상한보다 하나 더 받아야 정확히 상한인지
            // 넘쳤는지를 가를 수 있다(2026-10-11 PR 4).
            let max_count = format!("--max-count={}", MAX_BRANCH_COMMITS + 1);
            if let Ok((log, log_truncated)) =
                // 전체 sha(`%H`) — 짧은 sha는 같은 이름의 브랜치나 나중의 충돌로 다른 커밋을
                // 가리킬 수 있다. `log.showSignature` 설정이 서명 검증 문구를 stdout에
                // 섞으면 sha 파싱이 깨져 커밋이 빠지므로 끈다(2026-10-11 리뷰).
                run(&[
                    "log",
                    "-z",
                    "--no-show-signature",
                    &max_count,
                    "--format=%H%x00%s%x00%ct",
                    &range,
                ])
            {
                let (rows, rows_truncated) = parse_branch_commits(&log);
                commits = rows;
                commits_truncated = log_truncated || rows_truncated;
            }
        }
    }

    let (porcelain, t3) = run(&["status", "--porcelain", "-z", "-uall"])?;
    let (numstat, t4) = run(&["diff", "--no-ext-diff", "--numstat", "-z", "HEAD"])?;
    let (changes, changes_rows_truncated) = merge_status_rows(&porcelain, &numstat);
    let changes_truncated = t3 || t4 || changes_rows_truncated;

    // origin remote → GitHub HTTPS 정규화. 실패(원격 없음/비GitHub)해도 None만 담고
    // 스냅샷 전체는 죽이지 않는다(Task 10 Step 7 소급 요구, 2026-08-15).
    let remote_https_base = run(&["remote", "get-url", "origin"])
        .ok()
        .and_then(|(s, _)| normalize_github_remote(s.trim()));

    // 워크트리 목록 — 스냅샷과 같은 IO 왕복에서 한 번만 부른다(스펙 §8-4).
    // 실패해도 스냅샷 전체를 죽이지 않는다(섹션 단위 오류 원칙, §6). -z: 경로에
    // 개행이 있어도 항목이 안 잘리게(중요 리뷰 지적 2번).
    let (worktrees, worktrees_truncated) = match run(&["worktree", "list", "--porcelain", "-z"]) {
        Ok((listing, truncated)) => {
            let (rows, parse_truncated) = parse_worktree_list(&listing, &repo_root);
            (rows, truncated || parse_truncated)
        }
        Err(_) => (Vec::new(), false),
    };

    Ok(GitPanelSnapshot {
        repo_root,
        branch,
        upstream,
        upstream_fallback,
        ahead,
        behind,
        changes,
        committed,
        changes_truncated,
        committed_truncated,
        remote_https_base,
        worktrees,
        worktrees_truncated,
        commits,
        commits_truncated,
    })
}

/// App host 스레드에서 실행할 git 패널 IO. capacity-1 — App이 in-flight 1개만 유지
/// (기존 `pending_app_host_action` 큐 규칙, 2026-08-15 Task 10).
#[derive(Debug)]
pub enum GitPanelIoRequest {
    Snapshot,
    /// Git 보조 본문의 파일 행 클릭이 요청한다(2026-08-15 2차, 스펙 §8-3).
    FileDiff {
        rel_path: String,
        mode: crate::ui::diff_viewer::DiffMode,
    },
    /// 커밋 목록에서 커밋을 펼칠 때 그 커밋이 바꾼 파일을 요청한다(2026-10-11 PR 4).
    CommitFiles {
        sha: CommitSha,
    },
}

#[derive(Debug)]
pub struct GitPanelIoIntent {
    pub generation: u64,
    pub cwd: PathBuf,
    pub request: GitPanelIoRequest,
}

#[derive(Debug)]
pub enum GitPanelIoResult {
    Snapshot(Result<GitPanelSnapshot, GitPanelErrorCode>),
    FileDiff(Result<crate::ui::diff_viewer::FileDiffView, GitPanelErrorCode>),
    /// 요청한 sha를 함께 돌려준다 — 패널은 지금 기다리는 커밋의 결과만 받는다.
    CommitFiles {
        sha: CommitSha,
        result: Result<GitCommitFiles, GitPanelErrorCode>,
    },
}

#[derive(Debug)]
pub struct GitPanelIoCompletion {
    pub generation: u64,
    pub result: GitPanelIoResult,
}

/// host 스레드 실행 — collect_snapshot, 파일 diff 또는 커밋 파일 수집(스펙 §3).
pub fn execute_io(intent: GitPanelIoIntent) -> GitPanelIoCompletion {
    let result = match &intent.request {
        GitPanelIoRequest::Snapshot => GitPanelIoResult::Snapshot(collect_snapshot(&intent.cwd)),
        GitPanelIoRequest::FileDiff { rel_path, mode } => {
            GitPanelIoResult::FileDiff(collect_file_diff(&intent.cwd, rel_path, *mode))
        }
        GitPanelIoRequest::CommitFiles { sha } => GitPanelIoResult::CommitFiles {
            sha: *sha,
            result: collect_commit_files(&intent.cwd, *sha),
        },
    };
    GitPanelIoCompletion {
        generation: intent.generation,
        result,
    }
}

/// 파일 하나의 diff를 수집한다 — Working은 워킹트리(untracked는 파일 전량 추가로
/// 합성), Branch는 upstream과의 merge-base 기준(스펙 §3).
fn collect_file_diff(
    cwd: &Path,
    rel_path: &str,
    mode: crate::ui::diff_viewer::DiffMode,
) -> Result<crate::ui::diff_viewer::FileDiffView, GitPanelErrorCode> {
    use crate::ui::diff_viewer::{DiffMode, parse_unified};
    let repo_root =
        crate::git_cli::repo_root(cwd, GIT_TIMEOUT).map_err(|_| GitPanelErrorCode::NoRepo)?;
    // 경로 인젝션 방어: rel_path는 스냅샷의 porcelain 출력에서 온 값이지만,
    // "--" 뒤에 둬 옵션 해석을 차단하고 NUL/절대경로는 거부한다.
    if rel_path.is_empty() || rel_path.contains('\0') || rel_path.starts_with('/') {
        return Err(GitPanelErrorCode::CollectionFailed);
    }
    let run = |args: &[&str]| {
        crate::git_cli::run_git_limited(&repo_root, args, GIT_TIMEOUT, MAX_LIST_BYTES)
            .map_err(|_| GitPanelErrorCode::CollectionFailed)
    };
    match mode {
        DiffMode::Working => {
            let (text, truncated) = run(&["diff", "--no-ext-diff", "HEAD", "--", rel_path])?;
            if text.trim().is_empty() {
                // untracked — `git diff HEAD`는 untracked를 보여주지 않는다. 예전엔
                // std::fs::read로 파일 전체를 무계 메모리에 올린 뒤 MAX_LIST_BYTES로
                // 잘랐다(치명 리뷰 지적 — 읽기 자체가 무계). diff_panel의
                // append_untracked_section과 같은 방식으로 `--no-index /dev/null`을
                // 유계 실행기(run_git_limited)로 돌려 이미 unified diff 형태의
                // 전량 추가 출력을 받는다. `--`로 옵션 오인을 막는다.
                let (untracked, untracked_truncated) = run(&[
                    "diff",
                    "--no-ext-diff",
                    "--no-index",
                    "--",
                    "/dev/null",
                    rel_path,
                ])?;
                return Ok(parse_unified(&untracked, untracked_truncated));
            }
            Ok(parse_unified(&text, truncated))
        }
        DiffMode::Branch => {
            let (upstream, _) = resolve_upstream(run).ok_or(GitPanelErrorCode::CollectionFailed)?;
            let (base, _) = run(&["merge-base", upstream.as_str(), "HEAD"])?;
            let range = format!("{}..HEAD", base.trim());
            let (text, truncated) = run(&["diff", "--no-ext-diff", &range, "--", rel_path])?;
            Ok(parse_unified(&text, truncated))
        }
        DiffMode::Commit(sha) => {
            // 루트 커밋은 부모가 없어도 `git show`가 새 파일 diff로 보여 준다. 머지 커밋은
            // collect_commit_files와 같은 기준(첫 부모 대비)이다.
            let (text, truncated) = run(&[
                "show",
                "--no-ext-diff",
                "--first-parent",
                "--format=",
                sha.as_str(),
                "--",
                rel_path,
            ])?;
            Ok(parse_unified(&text, truncated))
        }
    }
}

/// 커밋 하나가 바꾼 파일 — committed 섹션과 같은 numstat + name-status 병합
/// (merge_committed_rows, 상한 MAX_PANEL_FILES). 루트 커밋은 `git show`가 빈 트리
/// 대비로 보여 준다. 머지 커밋은 첫 부모 대비로 본다 — 기본값(--cc)은 충돌 없는
/// 머지의 name-status를 비워 머지가 들여온 파일이 목록에서 사라진다(2026-10-11 실측).
/// `--diff-merges=first-parent`(git 2.31+)가 아니라 `--first-parent`를 쓴다 — 최신 git에서는
/// 같은 뜻이고, 옛 git에서도 옵션 오류로 모든 커밋이 실패하지 않고 기본값으로 돈다.
fn collect_commit_files(cwd: &Path, sha: CommitSha) -> Result<GitCommitFiles, GitPanelErrorCode> {
    let repo_root =
        crate::git_cli::repo_root(cwd, GIT_TIMEOUT).map_err(|_| GitPanelErrorCode::NoRepo)?;
    let run = |format: &str| {
        crate::git_cli::run_git_limited(
            &repo_root,
            &[
                "show",
                "--no-ext-diff",
                "--first-parent",
                format,
                "-z",
                "--format=",
                sha.as_str(),
                "--",
            ],
            GIT_TIMEOUT,
            MAX_LIST_BYTES,
        )
        .map_err(|_| GitPanelErrorCode::CollectionFailed)
    };
    let (numstat, t1) = run("--numstat")?;
    let (names, t2) = run("--name-status")?;
    let (rows, rows_truncated) = merge_committed_rows(&numstat, &names);
    Ok(GitCommitFiles {
        rows,
        truncated: t1 || t2 || rows_truncated,
    })
}

/// 패널이 App에 요청하는 intent — leaf는 git도 뷰 전환도 직접 하지 않는다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitPanelAction {
    Refresh,
    /// upstream의 GitHub 브랜치 페이지 열기 — URL 구성은 App이 remote를 보고 한다.
    OpenRemoteBranch,
    ShowFileDiff {
        rel_path: String,
        mode: crate::ui::diff_viewer::DiffMode,
    },
    /// 워크트리 행 클릭 — App이 그 경로에서 새 셸을 연다(워크트리를 만들지도 지우지도 않는다).
    OpenWorktreeShell {
        path: String,
    },
    /// 커밋 행을 펼쳤는데 그 커밋의 파일 목록이 없다 — App이 불러와
    /// `set_commit_files`로 돌려준다(2026-10-11 PR 4).
    LoadCommitFiles {
        sha: CommitSha,
    },
    /// 「세션에 경로 넣기」(2026-10-11 PR 5) — App이 파일 트리 「터미널에 경로 삽입」
    /// (`SidebarAction::InsertPath`)과 같은 경로로 포커스 세션에 넣는다. `path`는
    /// repo_root 기준 절대 경로이고 [`GitPathTarget::insert_action`]이 검증한 값이다.
    InsertPath {
        path: PathBuf,
    },
    /// 「Finder에서 보기」 — App이 세션 「Finder에서 폴더 열기」와 같은 자리
    /// (`AppHostIoAction::OpenPath`)로 이 파일이 든 폴더를 연다.
    RevealInFinder {
        path: PathBuf,
    },
}

#[derive(Default)]
pub struct GitPanelUi {
    snapshot: Option<Result<GitPanelSnapshot, GitPanelErrorCode>>,
    loading: bool,
    changes_show_all: bool,
    committed_show_all: bool,
    /// 오른쪽 diff가 보여 주는 파일 — 이 패널이 `ShowFileDiff`를 낼 때 기록한다.
    /// diff 뷰어를 여는 경로가 그 액션 하나뿐이고 두 UI는 `reset_git_surfaces`에서
    /// 함께 초기화되므로 따로 동기화할 필요가 없다. 같은 경로도 모드가 다르면 다른 행이다.
    selected: Option<(String, crate::ui::diff_viewer::DiffMode)>,
    commit_list: CommitListState,
    /// 지금 보이는 목록 탭. 스냅샷을 새로 받아도 유지하고, `reset_git_surfaces`에서
    /// 패널과 함께 변경 탭으로 돌아간다.
    tab: GitPanelTab,
    /// 「목록 / 폴더별」(2026-10-11 PR 3) — 변경·커밋 탭이 같은 보기를 쓴다.
    layout: GitListLayout,
    /// 폴더별에서 접은 폴더 — 탭마다 따로다(`*_show_all`과 같은 짝). 사용자가 누른 만큼만
    /// 늘고 `reset_git_surfaces`에서 패널과 함께 비워진다.
    changes_collapsed_dirs: std::collections::HashSet<String>,
    committed_collapsed_dirs: std::collections::HashSet<String>,
    /// 첫 파일 자동 선택 대기(2026-10-11 PR 3) — 스냅샷이 오면 켜지고 다음 렌더가 한 번
    /// 소비한다. 고른 파일이 이미 있으면(사용자든 자동이든) 소비만 하고 고르지 않는다.
    auto_select_pending: bool,
    /// 다른 입력(교차 워크스페이스의 붙인 터미널)이 키를 쥐고 있다 — ↑↓를 건드리지 않는다.
    /// App이 매 프레임 render 전에 넘긴다.
    keys_blocked: bool,
}

/// 목록 탭(2026-10-11 PR 2) — 예전 접는 섹션 세 개를 대신한다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
enum GitPanelTab {
    #[default]
    Changes,
    Commits,
    Worktrees,
}

/// 변경·커밋 탭 파일 행의 보기(2026-10-11 PR 3) — 폴더별은 폴더 머리 아래에 파일명만 둔다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum GitListLayout {
    #[default]
    Flat,
    Folders,
}

/// 작업 트리가 깨끗할 때 파일을 고르지 않은 diff 영역이 보일 브랜치 요약(2026-10-11 PR 3).
/// `DiffViewerUi::render` 시그니처가 소스 고정이라 App이 [`GitPanelUi::clean_summary`]로
/// 받아 `DiffViewerUi::set_clean_summary`로 넘긴다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitCleanSummary {
    /// None = 업스트림 없음(보낼/받을 커밋을 셀 수 없다).
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

impl GitPanelUi {
    pub fn set_loading(&mut self) {
        self.loading = true;
        // 새 스냅샷이 오는 중이다 — 낡은 스냅샷으로 고르면 그 파일 diff 요청이 새 스냅샷과
        // 겹친다. 새 스냅샷이 도착하면 다시 켜진다.
        self.auto_select_pending = false;
    }

    /// 다른 입력이 키를 쥐고 있는지 — App이 render 전에 넘긴다.
    pub fn set_keys_blocked(&mut self, blocked: bool) {
        self.keys_blocked = blocked;
    }

    /// 오른쪽 diff가 보여 주는 파일이 있는가 — 없으면 App이 diff 뷰어를 비운다.
    pub fn has_selection(&self) -> bool {
        self.selected.is_some()
    }

    pub fn set_snapshot(&mut self, result: Result<GitPanelSnapshot, GitPanelErrorCode>) {
        self.loading = false;
        // 다른 repo로 바뀌었다(세션 「변경 보기」 등) — 옛 repo 경로의 선택·펼친 커밋이 남으면
        // 새 repo의 같은 경로 행이 선택으로 보인다. App은 같은 조건에서 diff 뷰어를 비운다.
        if let (Some(previous), Ok(next)) = (self.repo_root(), result.as_ref())
            && previous != next.repo_root.as_path()
        {
            self.selected = None;
            self.commit_list = CommitListState::default();
        }
        // 고른 파일이 커밋 등으로 그 목록에서 사라졌으면 선택을 비운다 — 남으면 사라진 파일의
        // diff가 남고 다음 변경도 자동으로 열리지 않는다. 커밋 파일은 스냅샷 밖이라 그대로다.
        if let (Some((path, mode)), Ok(next)) = (self.selected.as_ref(), result.as_ref()) {
            use crate::ui::diff_viewer::DiffMode;
            let rows = match mode {
                DiffMode::Working => Some(&next.changes),
                DiffMode::Branch => Some(&next.committed),
                DiffMode::Commit(_) => None,
            };
            if rows.is_some_and(|rows| !rows.iter().any(|row| &row.rel_path == path)) {
                self.selected = None;
            }
        }
        // 고른 파일이 있는지는 render가 본다 — 선택을 지키는 새로고침은 켜져도 고르지 않는다.
        self.auto_select_pending = result.is_ok();
        self.snapshot = Some(result);
    }

    /// 지금 스냅샷의 repo 루트 — 다른 repo의 스냅샷이 왔는지 가르는 데 쓴다.
    fn repo_root(&self) -> Option<&Path> {
        Some(self.snapshot.as_ref()?.as_ref().ok()?.repo_root.as_path())
    }

    /// `LoadCommitFiles`의 결과 — 지금 기다리는 커밋의 것만 받고, 다른 커밋으로 넘어간
    /// 뒤 늦게 온 결과는 버린다.
    pub fn set_commit_files(
        &mut self,
        sha: CommitSha,
        result: Result<GitCommitFiles, GitPanelErrorCode>,
    ) {
        if let Some((waiting, state)) = &mut self.commit_list.loaded
            && *waiting == sha
        {
            *state = match result {
                Ok(files) => CommitFilesState::Ready(files),
                Err(_) => CommitFilesState::Failed,
            };
        }
    }

    /// ↗ 버튼이 쓸 (remote_https_base, branch) — 둘 다 있어야 Some. 스냅샷이 이미
    /// remote를 정규화해 담아 두므로 App이 IO 없이 즉시 URL을 구성할 수 있다
    /// (스펙 §4, Task 10 Step 7).
    pub fn remote_target(&self) -> Option<(String, String)> {
        let snap = self.snapshot.as_ref()?.as_ref().ok()?;
        let base = snap.remote_https_base.clone()?;
        Some((base, snap.branch.clone()))
    }

    /// diff 헤더 경로 버튼이 쓸 대상 — 지금 스냅샷에서 그 모드 섹션의 행을 찾아 절대
    /// 경로와 삭제 여부를 만든다. 행이 없으면(스냅샷이 그새 바뀌었으면) None.
    pub fn path_target(
        &self,
        rel_path: &str,
        mode: crate::ui::diff_viewer::DiffMode,
    ) -> Option<GitPathTarget> {
        use crate::ui::diff_viewer::DiffMode;
        let snap = self.snapshot.as_ref()?.as_ref().ok()?;
        let rows = match mode {
            DiffMode::Working => &snap.changes,
            DiffMode::Branch => &snap.committed,
            // 펼친 커밋의 파일 목록 — 불러오기 전이거나 다른 커밋이면 대상이 없다.
            DiffMode::Commit(sha) => match &self.commit_list.loaded {
                Some((loaded, CommitFilesState::Ready(files))) if *loaded == sha => &files.rows,
                _ => return None,
            },
        };
        rows.iter()
            .find(|row| row.rel_path == rel_path)
            .map(|row| GitPathTarget::for_row(&snap.repo_root, row))
    }

    /// 깨끗할 때 diff 영역 요약 — 스냅샷이 있고 작업 중 변경이 없을 때만 Some.
    pub fn clean_summary(&self) -> Option<GitCleanSummary> {
        let snap = self.snapshot.as_ref()?.as_ref().ok()?;
        snap.changes.is_empty().then(|| GitCleanSummary {
            upstream: snap.upstream.clone(),
            ahead: snap.ahead,
            behind: snap.behind,
        })
    }

    /// `filter`: 보조 검색 좌측 목록 필터(2026-08-18 스펙) — 빈 문자열이면 필터 없음.
    /// 변경·커밋 탭의 파일 행만 `rel_path` 기준으로 거르고, 커밋 목록과 워크트리 탭은
    /// 그대로 둔다(경로 목록이라 검색 대상이 아니고, 통째로 사라지면 혼란스럽다).
    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        filter: &str,
    ) -> Option<GitPanelAction> {
        // 필드별로 분해해 빌린다 — snapshot은 &만 필요하고 show_all·tab 등은
        // 동시에 &mut가 필요해, self를 통째로 들고 있으면(예전엔 self.snapshot을
        // clone()해 이 충돌을 피했다) 프레임마다 최대 ~1056개 String을 복제했다
        // (중요 리뷰 지적 5번). 필드마다 서로 다른 경로라 disjoint borrow로 풀린다.
        let Self {
            snapshot,
            loading,
            changes_show_all,
            committed_show_all,
            selected,
            commit_list,
            tab,
            layout,
            changes_collapsed_dirs,
            committed_collapsed_dirs,
            auto_select_pending,
            keys_blocked,
        } = self;
        let snap = match snapshot.as_ref() {
            None => {
                ui.weak(catalog.t("diff.loading", &[]));
                return if *loading {
                    None
                } else {
                    Some(GitPanelAction::Refresh)
                };
            }
            Some(Err(GitPanelErrorCode::NoRepo)) => {
                ui.weak(catalog.t("diff.no_cwd", &[]));
                return None;
            }
            Some(Err(GitPanelErrorCode::CollectionFailed)) => {
                ui.weak(catalog.t("git.error.snapshot", &[]));
                return None;
            }
            Some(Ok(snap)) => snap,
        };

        let tokens = crate::ui::designall::tokens(ui.visuals());
        let mut action = branch_card(ui, snap, tokens, catalog);
        ui.add_space(6.0);

        let filtered_changes = filter_file_rows(&snap.changes, filter);
        let filtered_committed = filter_file_rows(&snap.committed, filter);

        // ── 탭: 변경 / 커밋 / 워크트리 ──────────────────────────────────
        // 워크트리 탭은 예전 섹션과 같은 조건으로 숨긴다. 새로고침으로 숨은 탭에 머물게
        // 되면(워크트리가 하나만 남음) 빈 화면 대신 변경 탭으로 돌린다.
        let worktrees_visible = worktree_section_visible(snap);
        if *tab == GitPanelTab::Worktrees && !worktrees_visible {
            *tab = GitPanelTab::Changes;
        }
        // 변경 개수는 거른 뒤 기준(예전 섹션 머리와 같다). 커밋·워크트리는 필터 대상이 아니다.
        let mut tabs = vec![
            (
                GitPanelTab::Changes,
                catalog.t("git.tab.changes", &[]),
                tab_count(filtered_changes.len(), snap.changes_truncated),
            ),
            (
                GitPanelTab::Commits,
                catalog.t("git.tab.commits", &[]),
                tab_count(snap.commits.len(), snap.commits_truncated),
            ),
        ];
        if worktrees_visible {
            tabs.push((
                GitPanelTab::Worktrees,
                catalog.t("git.section.worktrees", &[]),
                tab_count(snap.worktrees.len(), snap.worktrees_truncated),
            ));
        }
        tab_bar(ui, tab, &tabs, tokens);
        ui.add_space(4.0);

        // ── 합계 줄: 지금 탭의 파일 행(거른 뒤) ─────────────────────────
        match *tab {
            GitPanelTab::Changes => {
                totals_line(
                    ui,
                    &filtered_changes,
                    changes_show_all,
                    layout,
                    tokens,
                    catalog,
                );
            }
            GitPanelTab::Commits => {
                totals_line(
                    ui,
                    &filtered_committed,
                    committed_show_all,
                    layout,
                    tokens,
                    catalog,
                );
            }
            GitPanelTab::Worktrees => {}
        }

        // ── 지금 탭의 목록 줄(폴더 머리 + 파일 행) — 그리기와 ↑↓ 이동이 같은 목록을 쓴다 ──
        let current = *tab;
        let (items, tab_mode) = match current {
            GitPanelTab::Changes => (
                list_items(
                    &filtered_changes,
                    *layout,
                    *changes_show_all,
                    changes_collapsed_dirs,
                ),
                crate::ui::diff_viewer::DiffMode::Working,
            ),
            GitPanelTab::Commits => (
                list_items(
                    &filtered_committed,
                    *layout,
                    *committed_show_all,
                    committed_collapsed_dirs,
                ),
                crate::ui::diff_viewer::DiffMode::Branch,
            ),
            GitPanelTab::Worktrees => (Vec::new(), crate::ui::diff_viewer::DiffMode::Working),
        };

        // ── 첫 파일 자동 선택(2026-10-11 PR 3): 스냅샷이 오고 고른 파일이 없으면 변경 탭의
        // 보이는 첫 파일을 한 번 연다. 변경이 없거나 다른 탭이면 고르지 않는다(탭을 넘기지
        // 않는다). 같은 프레임의 다른 의도(⟳ 등)가 먼저고, 그 새로고침의 스냅샷이 다시 켠다.
        if std::mem::take(auto_select_pending)
            && current == GitPanelTab::Changes
            && selected.is_none()
            && action.is_none()
            && let Some(row) = items.iter().find_map(|item| match item {
                ListItem::File(row) => Some(*row),
                ListItem::Folder { .. } => None,
            })
        {
            *selected = Some((row.rel_path.clone(), tab_mode));
            action = Some(GitPanelAction::ShowFileDiff {
                rel_path: row.rel_path.clone(),
                mode: tab_mode,
            });
        }

        // ── ↑↓: 지금 탭의 보이는 파일 행을 표시 순서로 옮겨 다니며 그 diff를 연다
        // (2026-10-11 PR 3). 이동할 행이 없으면 키를 소비하지 않는다.
        let mut reveal = false;
        if !*keys_blocked
            && let Some(step) = git_key_step(ui.ctx(), egui::Key::ArrowUp, egui::Key::ArrowDown)
        {
            // 위 목록 줄의 파일 행(접은 폴더 제외), 커밋 탭이면 이어서 펼친 커밋의 파일 행.
            let mut nav: Vec<(&str, crate::ui::diff_viewer::DiffMode)> = items
                .iter()
                .filter_map(|item| match item {
                    ListItem::File(row) => Some((row.rel_path.as_str(), tab_mode)),
                    ListItem::Folder { .. } => None,
                })
                .collect();
            if current == GitPanelTab::Commits
                && let Some((sha, rows)) = commit_list.expanded_files()
                && snap.commits.iter().any(|commit| commit.sha == sha)
            {
                nav.extend(rows.iter().map(|row| {
                    (
                        row.rel_path.as_str(),
                        crate::ui::diff_viewer::DiffMode::Commit(sha),
                    )
                }));
            }
            if !nav.is_empty() {
                consume_git_keys(ui.ctx(), egui::Key::ArrowUp, egui::Key::ArrowDown);
                if let Some(next) = step_selection(&nav, selected.as_ref(), step) {
                    let (rel_path, mode) = nav[next];
                    *selected = Some((rel_path.to_owned(), mode));
                    action = Some(GitPanelAction::ShowFileDiff {
                        rel_path: rel_path.to_owned(),
                        mode,
                    });
                    reveal = true;
                }
            }
        }

        egui::ScrollArea::vertical()
            .id_salt(("git-panel-tab-scroll", current))
            .auto_shrink([false, false])
            .show(ui, |ui| match current {
                GitPanelTab::Changes => {
                    if snap.changes.is_empty() {
                        if clean_card(ui, snap.ahead, tokens, catalog) {
                            *tab = GitPanelTab::Commits;
                        }
                        return;
                    }
                    if snap.changes_truncated {
                        truncation_banner(ui, tokens, catalog);
                    }
                    render_file_rows(
                        ui,
                        catalog,
                        &snap.repo_root,
                        &items,
                        "changes",
                        tab_mode,
                        changes_collapsed_dirs,
                        selected,
                        reveal,
                        &mut action,
                    );
                }
                GitPanelTab::Commits => {
                    let Some(upstream) = snap.upstream.as_deref() else {
                        // 비교할 기준이 없으면 브랜치 커밋을 셀 수 없다(collect_snapshot).
                        ui.weak(catalog.t("git.branch.no_upstream_hint", &[]));
                        return;
                    };
                    // 소제목 — 이 탭의 파일이 「브랜치 커밋」(Branch 모드 diff)이고 기준이 무엇인지.
                    ui.horizontal_wrapped(|ui| {
                        ui.weak(catalog.t("git.section.committed", &[]));
                        ui.weak("·");
                        ui.weak(catalog.t("git.commits.since", &[("base", upstream)]));
                    });
                    if snap.committed.is_empty() && snap.commits.is_empty() {
                        ui.weak(catalog.t("git.commits.empty", &[]));
                        return;
                    }
                    if snap.committed_truncated {
                        truncation_banner(ui, tokens, catalog);
                    }
                    if !snap.committed.is_empty() {
                        render_file_rows(
                            ui,
                            catalog,
                            &snap.repo_root,
                            &items,
                            "committed",
                            tab_mode,
                            committed_collapsed_dirs,
                            selected,
                            reveal,
                            &mut action,
                        );
                    }
                    // 브랜치 커밋 목록 — 파일 행 아래(2026-10-11 PR 4).
                    render_commit_list(
                        ui,
                        catalog,
                        tokens,
                        snap,
                        commit_list,
                        selected,
                        reveal,
                        &mut action,
                    );
                }
                GitPanelTab::Worktrees => {
                    for row in &snap.worktrees {
                        // 파일 행과 같은 패턴 — 자식 Label이 클릭을 삼키지 않도록
                        // 스코프 자체를 하나의 논리 위젯으로 만든다(위 file row 주석 참고).
                        let scope = ui.scope_builder(
                            egui::UiBuilder::new()
                                .id_salt(("git-panel-worktree", row.path.as_str()))
                                .sense(egui::Sense::click()),
                            |ui| {
                                // 글자 선택을 끈다 — 선택 가능한 Label이 행 클릭을 가로챘다(2026-10-11 리뷰).
                                ui.style_mut().interaction.selectable_labels = false;
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(&row.name);
                                    match row.branch.as_deref() {
                                        Some(branch) => ui.weak(branch),
                                        None => ui.weak(catalog.t("git.worktree.detached", &[])),
                                    };
                                    if row.current {
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| ui.weak(catalog.t("git.worktree.current", &[])),
                                        );
                                    }
                                });
                            },
                        );
                        let response = scope
                            .response
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text(catalog.t("git.worktree.open_hint", &[]));
                        // 접근성 이름을 경로로 명시한다 — kittest가 Role::Button + 이
                        // 라벨로 행 전체를 정확히 겨냥할 수 있다(위 file row 주석과 동일 이유).
                        response.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                ui.is_enabled(),
                                row.path.as_str(),
                            )
                        });
                        if response.clicked() && worktree_path_is_spawnable(&row.path) {
                            action = Some(GitPanelAction::OpenWorktreeShell {
                                path: row.path.clone(),
                            });
                        }
                    }
                    if snap.worktrees_truncated {
                        ui.weak(catalog.t("git.worktrees_truncated", &[]));
                    }
                }
            });
        action
    }
}

/// 브랜치 카드 — 1줄: 브랜치명(모노, 넘치면 말줄임) + ↗·⟳, 2줄: `→ upstream`과 보낼/받을
/// 커밋 배지(2026-10-11 시안). ↑3·↓1 색 숫자만으로는 의미가 읽히지 않았고, 업스트림
/// 없음은 회색 한 줄이라 지나치기 쉬웠다.
fn branch_card(
    ui: &mut egui::Ui,
    snap: &GitPanelSnapshot,
    tokens: crate::ui::designall::Tokens,
    catalog: &i18n::Catalog,
) -> Option<GitPanelAction> {
    let mut action = None;
    egui::Frame::NONE
        .stroke(egui::Stroke::new(1.0, tokens.separator))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("⟳")
                        .on_hover_text(catalog.t("diff.refresh", &[]))
                        .clicked()
                    {
                        action = Some(GitPanelAction::Refresh);
                    }
                    // upstream이 있고 remote가 GitHub일 때만 보인다(스펙 §4 숨김 조건).
                    // collect_snapshot이 remote_https_base로 이 조건을 계산해 둔다
                    // (2026-08-15 조정 — 카드로 옮기면서 조건은 그대로).
                    if snap.upstream.is_some()
                        && snap.remote_https_base.is_some()
                        && ui
                            .small_button("↗")
                            .on_hover_text(catalog.t("git.open_remote", &[]))
                            .clicked()
                    {
                        action = Some(GitPanelAction::OpenRemoteBranch);
                    }
                    // 버튼을 뺀 남은 폭에 왼쪽부터 그리고, 넘치면 말줄임한다 — egui Label은
                    // 잘린 글자의 전체를 hover로 보여 준다.
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&snap.branch).monospace().strong(),
                            )
                            .truncate(),
                        );
                    });
                });
            });
            ui.horizontal_wrapped(|ui| match snap.upstream.as_deref() {
                Some(upstream) => {
                    // 추적 브랜치가 아니라 기본 브랜치 폴백이면 push·pull로 맞출 수 있는
                    // 「보낼/받을 커밋」이 아니다 — 업스트림 없음을 알리고 기준 대비로 센다.
                    let (ahead_key, behind_key, same_key) = if snap.upstream_fallback {
                        no_upstream_pill(ui, tokens, catalog);
                        (
                            "git.branch.ahead_base",
                            "git.branch.behind_base",
                            "git.branch.same_as_base",
                        )
                    } else {
                        (
                            "git.branch.ahead",
                            "git.branch.behind",
                            "git.branch.in_sync",
                        )
                    };
                    ui.weak("→");
                    ui.add(egui::Label::new(egui::RichText::new(upstream).monospace()).truncate());
                    if snap.ahead > 0 {
                        let text = catalog.t(ahead_key, &[("count", &snap.ahead.to_string())]);
                        pill(
                            ui,
                            text,
                            tokens.success,
                            tokens.success.gamma_multiply(0.14),
                            egui::Stroke::NONE,
                        );
                    }
                    if snap.behind > 0 {
                        let text = catalog.t(behind_key, &[("count", &snap.behind.to_string())]);
                        pill(
                            ui,
                            text,
                            tokens.warning,
                            tokens.warning.gamma_multiply(0.14),
                            egui::Stroke::NONE,
                        );
                    }
                    if snap.ahead == 0 && snap.behind == 0 {
                        let text = catalog.t(same_key, &[]);
                        let fill = tokens.input_background;
                        pill(ui, text, tokens.muted_text, fill, egui::Stroke::NONE);
                    }
                }
                None => no_upstream_pill(ui, tokens, catalog),
            });
        });
    action
}

/// 「업스트림 없음」 경고 배지 — hover로 push하면 추적 브랜치가 생긴다는 이유를 보인다.
fn no_upstream_pill(
    ui: &mut egui::Ui,
    tokens: crate::ui::designall::Tokens,
    catalog: &i18n::Catalog,
) {
    pill(
        ui,
        catalog.t("git.upstream_none", &[]),
        tokens.warning,
        egui::Color32::TRANSPARENT,
        egui::Stroke::new(1.0, tokens.warning.gamma_multiply(0.5)),
    )
    .on_hover_text(catalog.t("git.branch.no_upstream_hint", &[]));
}

/// 브랜치 카드의 상태 배지 — 둥근 면(또는 테두리)과 글자를 직접 칠한다. hover 전용
/// sense라 클릭을 받지 않고, 접근성 이름은 배지 글자다.
fn pill(
    ui: &mut egui::Ui,
    text: String,
    color: egui::Color32,
    fill: egui::Color32,
    stroke: egui::Stroke,
) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.clone(), egui::FontId::proportional(11.5), color);
    let size = egui::vec2(galley.size().x + 14.0, 20.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter()
        .rect(rect, 10.0, fill, stroke, egui::StrokeKind::Inside);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, color);
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), &text));
    response
}

/// 탭 개수 배지 글자 — 상한에 잘린 목록은 개수가 하한이라 「+」를 붙인다(시안의 512+).
fn tab_count(count: usize, truncated: bool) -> String {
    if truncated {
        format!("{count}+")
    } else {
        count.to_string()
    }
}

/// 탭 버튼 줄 — 한 줄을 채우는 세그먼트, 이름 옆에 개수 배지. 지금 탭은 선택 면과 2pt
/// accent 밑줄(시안의 segtabs). 탭마다 글자 폭에 남는 폭을 똑같이 나눠 더한다 — 시안처럼
/// 똑같은 폭으로 자르면 300pt 목록에서 「워크트리」·「ワークツリー」가 몇 pt 모자라
/// 다음 줄로 밀렸다. 글자 폭 합이 줄보다 넓을 때만 다음 줄로 넘긴다.
/// 접근성 이름은 「이름 개수」다.
fn tab_bar(
    ui: &mut egui::Ui,
    current: &mut GitPanelTab,
    tabs: &[(GitPanelTab, String, String)],
    tokens: crate::ui::designall::Tokens,
) {
    const HEIGHT: f32 = 28.0;
    const GAP: f32 = 1.0;
    const PADDING: f32 = 12.0;
    const BADGE_GAP: f32 = 5.0;
    // 글자를 먼저 배치해 탭마다 필요한 폭을 안다.
    let laid_out: Vec<_> = tabs
        .iter()
        .map(|(value, name, count)| {
            let is_current = *current == *value;
            let text_color = if is_current {
                tokens.text
            } else {
                tokens.muted_text
            };
            let name_galley = ui.painter().layout_no_wrap(
                name.clone(),
                egui::FontId::proportional(13.0),
                text_color,
            );
            let count_galley = ui.painter().layout_no_wrap(
                count.clone(),
                egui::FontId::proportional(11.0),
                tokens.text,
            );
            let badge = egui::vec2((count_galley.size().x + 10.0).max(20.0), 18.0);
            let content = name_galley.size().x + BADGE_GAP + badge.x;
            (is_current, name_galley, count_galley, badge, content)
        })
        .collect();
    let natural: f32 = laid_out
        .iter()
        .map(|(.., content)| content + PADDING)
        .sum::<f32>()
        + GAP * (tabs.len() as f32 - 1.0);
    let extra = ((ui.available_width() - natural) / tabs.len() as f32)
        .max(0.0)
        .floor();
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(GAP, GAP);
        for (
            index,
            ((value, name, count), (is_current, name_galley, count_galley, badge, content)),
        ) in tabs.iter().zip(laid_out).enumerate()
        {
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(content + PADDING + extra, HEIGHT),
                egui::Sense::click(),
            );
            // 바깥 끝 탭만 모서리를 둥글린다 — 붙은 세그먼트 하나처럼 보인다.
            let mut corner = egui::CornerRadius::ZERO;
            if index == 0 {
                (corner.nw, corner.sw) = (6, 6);
            }
            if index + 1 == tabs.len() {
                (corner.ne, corner.se) = (6, 6);
            }
            let fill = if is_current {
                tokens.selected_background
            } else if response.hovered() {
                tokens.hover_background
            } else {
                tokens.input_background
            };
            let painter = ui.painter();
            painter.rect_filled(rect, corner, fill);
            if is_current {
                let underline = egui::Rect::from_min_max(
                    egui::pos2(rect.left(), rect.bottom() - 2.0),
                    rect.right_bottom(),
                );
                painter.rect_filled(underline, corner, tokens.accent);
            }
            let left = rect.center().x - content / 2.0;
            let badge_rect = egui::Rect::from_min_size(
                egui::pos2(
                    left + name_galley.size().x + BADGE_GAP,
                    rect.center().y - badge.y / 2.0,
                ),
                badge,
            );
            let name_pos = egui::pos2(left, rect.center().y - name_galley.size().y / 2.0);
            painter.galley(name_pos, name_galley, tokens.text);
            painter.rect_filled(badge_rect, 9.0, tokens.separator);
            painter.galley(
                badge_rect.center() - count_galley.size() / 2.0,
                count_galley,
                tokens.text,
            );
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::Button,
                    ui.is_enabled(),
                    is_current,
                    format!("{name} {count}"),
                )
            });
            if response
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                *current = *value;
            }
        }
    });
}

/// 탭 아래 합계 줄 — 「N개 파일 · +a −d」(거른 뒤 행 기준). 오른쪽 끝에 「목록 / 폴더별」
/// (2026-10-11 PR 3), 행이 표시 상한을 넘으면 그 왼쪽에 「모두 보기」 토글(예전 섹션 머리에
/// 있던 것). 행이 없으면 그리지 않는다.
fn totals_line(
    ui: &mut egui::Ui,
    rows: &[&GitFileRow],
    show_all: &mut bool,
    layout: &mut GitListLayout,
    tokens: crate::ui::designall::Tokens,
    catalog: &i18n::Catalog,
) {
    if rows.is_empty() {
        return;
    }
    // 512행 × u32 증감 합은 u32를 넘을 수 있다.
    let adds: u64 = rows.iter().filter_map(|row| row.adds).map(u64::from).sum();
    let dels: u64 = rows.iter().filter_map(|row| row.dels).map(u64::from).sum();
    ui.horizontal(|ui| {
        ui.weak(catalog.t("git.files.total", &[("count", &rows.len().to_string())]));
        ui.weak("·");
        ui.colored_label(stat_color(tokens, true), format!("+{adds}"));
        ui.colored_label(stat_color(tokens, false), format!("−{dels}"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // 오른쪽부터 그린다 — 「목록」「폴더별」 순으로 보인다.
            for (value, key) in [
                (GitListLayout::Folders, "git.layout.folders"),
                (GitListLayout::Flat, "git.layout.flat"),
            ] {
                if ui
                    .selectable_label(*layout == value, catalog.t(key, &[]))
                    .clicked()
                {
                    *layout = value;
                }
            }
            if rows.len() > SECTION_COLLAPSED_ROWS {
                ui.add_space(6.0);
                if ui
                    .selectable_label(*show_all, catalog.t("git.show_all", &[]))
                    .clicked()
                {
                    *show_all = !*show_all;
                }
            }
        });
    });
}

/// 「모두 보기」를 켜지 않았으면 표시 상한(SECTION_COLLAPSED_ROWS)까지만 남긴다.
fn visible_rows<'a, 'r>(rows: &'a [&'r GitFileRow], show_all: bool) -> &'a [&'r GitFileRow] {
    if show_all {
        rows
    } else {
        &rows[..rows.len().min(SECTION_COLLAPSED_ROWS)]
    }
}

/// 변경·커밋 탭 목록의 한 줄(2026-10-11 PR 3) — 폴더별 보기의 폴더 머리 또는 파일 행.
/// 그리기와 ↑↓ 이동이 같은 목록을 써서 화면 순서와 이동 순서가 어긋나지 않는다.
#[derive(Debug)]
enum ListItem<'r> {
    /// `dir`는 루트면 빈 문자열. `count`는 그 폴더의(거른 뒤) 전체 파일 수다 — 접었거나
    /// 「모두 보기」 전이라 일부만 보여도 같다.
    Folder {
        dir: &'r str,
        count: usize,
        collapsed: bool,
    },
    File(&'r GitFileRow),
}

/// 파일 행을 폴더(디렉터리)별로 묶는다 — 폴더는 처음 나온 순서, 폴더 안 파일은 입력
/// (porcelain·name-status) 순서 그대로. 루트의 파일은 빈 폴더("")에 묶인다.
fn group_by_folder<'r>(rows: &[&'r GitFileRow]) -> Vec<(&'r str, Vec<&'r GitFileRow>)> {
    let mut groups: Vec<(&'r str, Vec<&'r GitFileRow>)> = Vec::new();
    let mut index: std::collections::HashMap<&'r str, usize> = std::collections::HashMap::new();
    for &row in rows {
        let (_, dir) = split_row_path(&row.rel_path);
        let at = *index.entry(dir).or_insert_with(|| {
            groups.push((dir, Vec::new()));
            groups.len() - 1
        });
        groups[at].1.push(row);
    }
    groups
}

/// 지금 탭의 목록 줄. 「모두 보기」 전 상한(SECTION_COLLAPSED_ROWS)은 두 보기 모두 **보이는
/// 파일 행만** 그 보기의 표시 순서로 센다 — 폴더 머리와 접은 폴더의 파일은 세지 않는다.
/// 상한이 다 차면 그 뒤 폴더는 머리째 빠지고 「모두 보기」로 펼친다.
fn list_items<'r>(
    rows: &[&'r GitFileRow],
    layout: GitListLayout,
    show_all: bool,
    collapsed: &std::collections::HashSet<String>,
) -> Vec<ListItem<'r>> {
    if layout == GitListLayout::Flat {
        return visible_rows(rows, show_all)
            .iter()
            .copied()
            .map(ListItem::File)
            .collect();
    }
    let mut budget = if show_all {
        usize::MAX
    } else {
        SECTION_COLLAPSED_ROWS
    };
    let mut items = Vec::new();
    for (dir, files) in group_by_folder(rows) {
        if budget == 0 {
            break;
        }
        let is_collapsed = collapsed.contains(dir);
        items.push(ListItem::Folder {
            dir,
            count: files.len(),
            collapsed: is_collapsed,
        });
        // 접은 폴더의 파일은 보이지 않으니 상한을 쓰지 않는다 — 접으면 그 자리를 다음 폴더가 쓴다.
        if is_collapsed {
            continue;
        }
        let shown = files.len().min(budget);
        budget -= shown;
        items.extend(files[..shown].iter().copied().map(ListItem::File));
    }
    items
}

/// ↑↓ 한 번의 다음 선택 — `nav`(표시 순서의 보이는 파일 행) 안의 인덱스. 선택이 없거나
/// `nav` 밖이면(접은 폴더·다른 탭) 방향과 상관없이 첫 행이고, 양 끝에서는 멈춘다. 옮길
/// 곳이 없으면(같은 행) None — 같은 파일의 diff를 다시 열지 않는다.
fn step_selection(
    nav: &[(&str, crate::ui::diff_viewer::DiffMode)],
    selected: Option<&(String, crate::ui::diff_viewer::DiffMode)>,
    step: isize,
) -> Option<usize> {
    let last = nav.len().checked_sub(1)?;
    if step == 0 {
        return None;
    }
    let current = selected.and_then(|(path, mode)| {
        nav.iter()
            .position(|(row_path, row_mode)| row_path == path && row_mode == mode)
    });
    let next = match current {
        None => 0,
        Some(at) => at.saturating_add_signed(step).min(last),
    };
    (current != Some(next)).then_some(next)
}

/// Git 본문 키(↑↓·N/P)로 받을 눌림인가 — ⌘·Ctrl·Alt가 함께 눌렸으면 다른 단축키라
/// 세지 않는다(Shift는 대문자 N처럼 같은 키로 본다). 길게 눌러 반복되는 눌림도 센다.
fn is_plain_press(event: &egui::Event, key: egui::Key) -> bool {
    matches!(
        event,
        egui::Event::Key { key: pressed, pressed: true, modifiers, .. }
            if *pressed == key
                && !(modifiers.alt || modifiers.ctrl || modifiers.command || modifiers.mac_cmd)
    )
}

/// Git 보조 본문 키 한 쌍(↑/↓, P/N)의 이번 프레임 순이동 — `next` 눌림 +1, `prev` −1.
/// 눌림이 없거나 받을 수 없는 프레임이면 None. 이 본문이 보이는 프레임에는 터미널이
/// 입력 소유자가 아니므로(2026-08-15 스펙 §8-3) 남은 소유자는 텍스트 입력(보조 검색창
/// 등)이다 — 그 포커스나 IME 조합 중이면 받지 않는다. 입력은 건드리지 않는다: 쓰기로
/// 했으면 [`consume_git_keys`]로 지우고, 쓸 데가 없으면 다른 소비자에게 남긴다.
pub(crate) fn git_key_step(ctx: &egui::Context, prev: egui::Key, next: egui::Key) -> Option<isize> {
    // 열린 팝업(행 우클릭 메뉴·콤보)이 있으면 그 팝업의 키다 — 뒤의 선택·diff를 옮기지 않는다.
    if ctx.text_edit_focused()
        || terminal::renderer_egui::frame_has_active_preedit(ctx)
        || ctx.any_popup_open()
    {
        return None;
    }
    let (back, forward) = ctx.input(|input| {
        input.events.iter().fold((0, 0), |(back, forward), event| {
            (
                back + isize::from(is_plain_press(event, prev)),
                forward + isize::from(is_plain_press(event, next)),
            )
        })
    });
    (back + forward > 0).then_some(forward - back)
}

/// [`git_key_step`]이 센 눌림을 이번 프레임 입력에서 지운다.
pub(crate) fn consume_git_keys(ctx: &egui::Context, prev: egui::Key, next: egui::Key) {
    ctx.input_mut(|input| {
        input
            .events
            .retain(|event| !is_plain_press(event, prev) && !is_plain_press(event, next));
    });
}

/// 변경·커밋 탭의 목록 줄(폴더 머리 + 파일 행). 호출부는 거르기 전 목록이 비어 있지 않을
/// 때만 부른다 — 그래서 0줄이면 필터가 다 걸러낸 것이고, 「왜 안 보이지」가 되지 않게
/// 안내 한 줄을 둔다(2026-08-18 스펙 §좌측 목록 필터). 폴더 머리를 누르면 그 탭의
/// `collapsed_dirs`에서 접힘을 뒤집고, 묶음 안의 파일 행은 들여 쓰고 폴더를 숨긴다.
#[allow(clippy::too_many_arguments)]
fn render_file_rows(
    ui: &mut egui::Ui,
    catalog: &i18n::Catalog,
    repo_root: &Path,
    items: &[ListItem<'_>],
    salt: &str,
    mode: crate::ui::diff_viewer::DiffMode,
    collapsed_dirs: &mut std::collections::HashSet<String>,
    selected: &mut Option<(String, crate::ui::diff_viewer::DiffMode)>,
    reveal: bool,
    action: &mut Option<GitPanelAction>,
) {
    if items.is_empty() {
        ui.weak(catalog.t("search.no_match", &[]));
        return;
    }
    let tokens = crate::ui::designall::tokens(ui.visuals());
    // 폴더별 보기는 언제나 폴더 머리로 시작한다.
    let grouped = matches!(items.first(), Some(ListItem::Folder { .. }));
    let options = FileRowOptions {
        hide_dir: grouped,
        reveal_selected: reveal,
    };
    for item in items {
        let row = match item {
            ListItem::Folder {
                dir,
                count,
                collapsed,
            } => {
                // 누르면 접힘을 뒤집는다 — 접혀 있었으면 지워서 펴고, 아니면 넣어서 접는다.
                if folder_header(
                    ui,
                    ("git-panel-folder", salt, *dir),
                    dir,
                    *count,
                    *collapsed,
                ) && !collapsed_dirs.remove(*dir)
                {
                    collapsed_dirs.insert((*dir).to_owned());
                }
                continue;
            }
            ListItem::File(row) => row,
        };
        let id_salt = ("git-panel-row", salt, row.rel_path.as_str());
        let clicked = if grouped {
            ui.indent(
                ("git-panel-folder-row", salt, row.rel_path.as_str()),
                |ui| {
                    file_row(
                        ui, id_salt, repo_root, row, mode, selected, options, tokens, catalog,
                    )
                },
            )
            .inner
        } else {
            file_row(
                ui, id_salt, repo_root, row, mode, selected, options, tokens, catalog,
            )
        };
        if let Some(clicked) = clicked {
            *action = Some(clicked);
        }
    }
}

/// 폴더별 묶음의 머리 행 — 접기 표시(∨/›, 커밋 행과 같은 글자) · 폴더 경로(넘치면 말줄임,
/// 루트는 「.」) · 파일 수. 누른 프레임에 true. 파일 행과 같은 scope 패턴이고 접근성
/// 이름은 탭 버튼처럼 「폴더 개수」다.
fn folder_header(
    ui: &mut egui::Ui,
    id_salt: impl egui::AsIdSalt,
    dir: &str,
    count: usize,
    collapsed: bool,
) -> bool {
    let label = if dir.is_empty() { "." } else { dir };
    let scope = ui.scope_builder(
        egui::UiBuilder::new()
            .id_salt(id_salt)
            .sense(egui::Sense::click()),
        |ui| {
            // 글자 선택을 끈다 — 선택 가능한 Label이 행 클릭을 가로챘다(2026-10-11 리뷰).
            ui.style_mut().interaction.selectable_labels = false;
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.weak(if collapsed { "›" } else { "∨" });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(count.to_string());
                    // 개수를 뺀 남은 폭에 왼쪽부터 그리고, 넘치면 말줄임한다.
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(label).weak()).truncate());
                    });
                });
            });
        },
    );
    let response = scope
        .response
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            ui.is_enabled(),
            format!("{label} {count}"),
        )
    });
    response.clicked()
}

/// 작업 트리가 깨끗할 때의 카드 — 보낼 커밋이 있으면 커밋 탭으로 가는 링크를 둔다.
/// 링크를 누른 프레임에 true.
fn clean_card(
    ui: &mut egui::Ui,
    ahead: u32,
    tokens: crate::ui::designall::Tokens,
    catalog: &i18n::Catalog,
) -> bool {
    let mut to_commits = false;
    ui.add_space(8.0);
    egui::Frame::NONE
        .stroke(egui::Stroke::new(1.0, tokens.separator))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("✓").size(20.0).color(tokens.success));
                ui.label(catalog.t("diff.clean", &[]));
                if ahead > 0
                    && ui
                        .link(catalog.t("git.clean.view_ahead", &[("count", &ahead.to_string())]))
                        .clicked()
                {
                    to_commits = true;
                }
            });
        });
    to_commits
}

/// 상한(MAX_PANEL_FILES)·바이트 상한에 잘린 목록의 배너 — 예전엔 목록 끝의 흐린 한
/// 줄이라 긴 목록에서 보이지 않았다(2026-10-11 시안). 목록 위에 경고 테두리로 둔다.
fn truncation_banner(
    ui: &mut egui::Ui,
    tokens: crate::ui::designall::Tokens,
    catalog: &i18n::Catalog,
) {
    egui::Frame::NONE
        .fill(tokens.warning.gamma_multiply(0.08))
        .stroke(egui::Stroke::new(1.0, tokens.warning))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // 행 상한(512)이나 바이트 상한으로 잘렸다 — 「git diff」가 아니라 목록 안내다.
            ui.label(catalog.t(
                "git.list.truncated",
                &[("count", &MAX_PANEL_FILES.to_string())],
            ));
        });
    ui.add_space(4.0);
}

/// 파일 행 표시 옵션(2026-10-11 PR 3).
#[derive(Clone, Copy)]
struct FileRowOptions {
    /// 폴더별 묶음 안 — 머리가 폴더를 보이므로 행에서는 폴더를 숨긴다.
    hide_dir: bool,
    /// ↑↓로 선택을 옮긴 프레임 — 선택 행이 스크롤 밖이면 보이게 끌어온다.
    reveal_selected: bool,
}

/// 파일 행 하나 — 상태 배지·파일명·줄인 폴더·증감, 선택이면 면과 2pt accent 레일.
/// 변경/브랜치 커밋 섹션과 커밋별 파일 목록이 같이 쓴다. 클릭하면 선택으로 기록하고
/// 그 모드의 `ShowFileDiff`를, 우클릭 메뉴에서 고르면 그 경로 동작을 돌려준다.
#[allow(clippy::too_many_arguments)]
fn file_row(
    ui: &mut egui::Ui,
    id_salt: impl egui::AsIdSalt,
    repo_root: &Path,
    row: &GitFileRow,
    mode: crate::ui::diff_viewer::DiffMode,
    selected: &mut Option<(String, crate::ui::diff_viewer::DiffMode)>,
    options: FileRowOptions,
    tokens: crate::ui::designall::Tokens,
    catalog: &i18n::Catalog,
) -> Option<GitPanelAction> {
    let (name, dir) = split_row_path(&row.rel_path);
    let is_selected = selected
        .as_ref()
        .is_some_and(|(path, m)| path == &row.rel_path && *m == mode);
    // 배경은 행 크기를 안 뒤에 칠해야 하므로 자리만 먼저 잡는다(글자 아래에 깔림).
    let background = ui.painter().add(egui::Shape::Noop);
    // 계획서 원안은 `ui.horizontal(..).response.interact(Sense::click())`로
    // 행 전체를 클릭 가능하게 했는데, kittest로 돌려보면 자식 Label의
    // 텍스트 자체를 클릭했을 때 부모로 새지 않는다 — egui는 포인터 아래
    // "가장 안쪽" 위젯을 hit-test로 고르고, 그 위젯이 Sense::hover뿐이라도
    // 부모로 폴백하지 않는다(빈 여백을 클릭하면 잡히는 것으로 실측
    // 확인). 이 저장소는 이미 같은 문제를 겪었고(work_history.rs 카드/
    // 헤더 토글, 커밋 0e934c0) `scope_builder(UiBuilder::sense(click))` +
    // 명시적 `widget_info(Role::Button)`로 스코프 자체를 하나의 논리
    // 위젯으로 만드는 패턴을 쓴다 — 그 관례로 맞춘다
    // (2026-08-15 조정).
    let toggle = ui.scope_builder(
        egui::UiBuilder::new()
            .id_salt(id_salt)
            .sense(egui::Sense::click()),
        |ui| {
            // 글자 선택을 끈다 — egui Label은 기본이 선택 가능이라 click을 감지해 파일명·폴더
            // 글자 위의 클릭·우클릭·hover를 행 대신 가로챘다(2026-10-11 리뷰).
            ui.style_mut().interaction.selectable_labels = false;
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                // 증감을 오른쪽부터 먼저 놓고, 남은 폭에 배지·파일명·폴더를 말줄임으로 그린다 —
                // 좁은 패널에서 긴 이름이 증감 숫자를 덮거나 밖으로 밀지 않게.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(d) = row.dels.filter(|d| *d > 0) {
                        ui.colored_label(stat_color(tokens, false), format!("−{d}"));
                    }
                    if let Some(a) = row.adds.filter(|a| *a > 0) {
                        ui.colored_label(stat_color(tokens, true), format!("+{a}"));
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        status_badge(ui, row.status, tokens);
                        ui.add(egui::Label::new(name).truncate());
                        if !dir.is_empty() && !options.hide_dir {
                            ui.add(
                                egui::Label::new(egui::RichText::new(short_dir(dir)).weak())
                                    .truncate(),
                            );
                        }
                    });
                });
            });
        },
    );
    // 상태 설명은 배지가 아니라 행 hover에 둔다 — 배지에도 두면 두 말풍선이 같은 자리에 겹쳤다.
    let (_, _, status_key) = status_badge_style(row.status, tokens);
    let response = toggle
        .response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(format!("{} · {}", catalog.t(status_key, &[]), row.rel_path));
    let rect = response.rect;
    if response.has_focus() {
        // Tab으로 옮긴 키보드 포커스가 보이게 한다.
        ui.painter().rect_stroke(
            rect,
            4.0,
            egui::Stroke::new(1.0, tokens.accent),
            egui::StrokeKind::Inside,
        );
    }
    if let Some(fill) = crate::ui::designall::row_fill(tokens, is_selected, response.hovered()) {
        ui.painter()
            .set(background, egui::Shape::rect_filled(rect, 4.0, fill));
    }
    if is_selected {
        // 사이드 레일 선택과 같은 2pt accent 레일.
        let rail = egui::Rect::from_min_max(
            rect.left_top(),
            egui::pos2(rect.left() + 2.0, rect.bottom()),
        );
        ui.painter().rect_filled(rail, 0.0, tokens.accent);
        if options.reveal_selected {
            response.scroll_to_me(None);
        }
    }
    // 접근성 이름을 rel_path로 명시한다 — kittest가 Role::Button + 이
    // 라벨로 행 전체(자식 Label이 아니라)를 정확히 겨냥할 수 있다.
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            ui.is_enabled(),
            is_selected,
            row.rel_path.as_str(),
        )
    });
    if let Some(menu) = file_row_context_menu(&response, repo_root, row, catalog) {
        return Some(menu);
    }
    if !response.clicked() {
        return None;
    }
    *selected = Some((row.rel_path.clone(), mode));
    Some(GitPanelAction::ShowFileDiff {
        rel_path: row.rel_path.clone(),
        mode,
    })
}

/// 브랜치 커밋 목록의 펼침 상태(2026-10-11 PR 4). 한 번에 커밋 하나만 펼치고 파일
/// 목록도 그 하나분만 둔다 — 메모리가 MAX_PANEL_FILES 행으로 유계이고, App의 git IO는
/// latest-only라 여러 커밋을 동시에 불러오면 먼저 요청한 쪽 완료가 버려져 「불러오는
/// 중」에 멈춘 커밋이 화면에 남는다.
#[derive(Default)]
struct CommitListState {
    expanded: Option<CommitSha>,
    /// 마지막으로 요청한 커밋과 그 파일 목록 상태 — 접어도 남겨 둬 다시 펼칠 때 쓴다.
    loaded: Option<(CommitSha, CommitFilesState)>,
}

enum CommitFilesState {
    Loading,
    Ready(GitCommitFiles),
    Failed,
}

impl CommitListState {
    /// 커밋 행 클릭 — 펼침을 토글하고, 펼쳤는데 그 커밋의 파일 목록이 아직 없으면
    /// 로드 액션을 돌려준다. 실패했거나 결과가 오지 않은(latest-only로 버려졌을 수 있는)
    /// 커밋은 다시 펼칠 때 재요청한다 — 접었다 펴기가 곧 재시도다.
    fn toggle(&mut self, sha: CommitSha) -> Option<GitPanelAction> {
        if self.expanded == Some(sha) {
            self.expanded = None;
            return None;
        }
        self.expanded = Some(sha);
        if matches!(&self.loaded, Some((loaded, CommitFilesState::Ready(_))) if *loaded == sha) {
            return None;
        }
        self.loaded = Some((sha, CommitFilesState::Loading));
        Some(GitPanelAction::LoadCommitFiles { sha })
    }

    /// 펼쳤고 파일 목록까지 받은 커밋과 그 파일 — 커밋 탭 ↑↓가 브랜치 파일 다음에 이어
    /// 간다(2026-10-11 PR 3). 불러오는 중·실패면 파일 행이 없으니 None.
    fn expanded_files(&self) -> Option<(CommitSha, &[GitFileRow])> {
        let sha = self.expanded?;
        match &self.loaded {
            Some((loaded, CommitFilesState::Ready(files))) if *loaded == sha => {
                Some((sha, files.rows.as_slice()))
            }
            _ => None,
        }
    }
}

/// 커밋 탭의 브랜치 커밋 파일 행 아래 커밋 목록 — 행: ›/∨ · sha(모노, accent) ·
/// 제목(말줄임, 전체는 hover) · 상대 시각. 커밋 행 클릭은 펼침을 토글하고, 펼친 커밋의
/// 파일 행 클릭은 그 커밋 기준 diff를 연다. 좌측 목록 필터는 걸지 않는다(파일 경로
/// 필터라 커밋은 대상이 아니다 — 워크트리 탭과 같은 규칙). `reveal`: ↑↓로 선택을 옮긴
/// 프레임(펼친 커밋의 파일 행도 ↑↓ 대상이다, 2026-10-11 PR 3).
#[allow(clippy::too_many_arguments)]
fn render_commit_list(
    ui: &mut egui::Ui,
    catalog: &i18n::Catalog,
    tokens: crate::ui::designall::Tokens,
    snap: &GitPanelSnapshot,
    state: &mut CommitListState,
    selected: &mut Option<(String, crate::ui::diff_viewer::DiffMode)>,
    reveal: bool,
    action: &mut Option<GitPanelAction>,
) {
    if snap.commits.is_empty() {
        return;
    }
    let now = crate::ui::work_history::unix_now();
    ui.add_space(4.0);
    ui.weak(catalog.t(
        "git.commits.title",
        &[("count", &snap.commits.len().to_string())],
    ));
    for commit in &snap.commits {
        let open = state.expanded == Some(commit.sha);
        // 파일 행과 같은 패턴 — 자식 Label이 클릭을 삼키지 않도록 스코프 자체를 하나의
        // 논리 위젯으로 만든다(render의 file row 주석 참고).
        let scope = ui.scope_builder(
            egui::UiBuilder::new()
                .id_salt(("git-panel-commit", commit.sha.as_str()))
                .sense(egui::Sense::click()),
            |ui| {
                // 글자 선택을 끈다 — 선택 가능한 Label이 행 클릭을 가로챘다(2026-10-11 리뷰).
                ui.style_mut().interaction.selectable_labels = false;
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.weak(if open { "∨" } else { "›" });
                    ui.label(
                        egui::RichText::new(commit.sha.short())
                            .monospace()
                            .color(tokens.accent),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(crate::ui::work_history::relative_age_text(
                            catalog,
                            commit.time,
                            now,
                        ));
                        // 시각을 뺀 남은 폭에 왼쪽부터 그리고, 넘치면 말줄임한다.
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(egui::Label::new(commit.subject.as_str()).truncate());
                        });
                    });
                });
            },
        );
        let response = scope
            .response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(commit.subject.as_str());
        // 접근성 이름을 「sha 제목」으로 명시한다 — kittest가 Role::Button + 이 라벨로
        // 행 전체를 겨냥한다(파일 행과 같은 이유).
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                ui.is_enabled(),
                format!("{} {}", commit.sha.short(), commit.subject),
            )
        });
        if response.clicked()
            && let Some(load) = state.toggle(commit.sha)
        {
            *action = Some(load);
        }
        if state.expanded == Some(commit.sha) {
            ui.indent(("git-panel-commit-files", commit.sha.as_str()), |ui| {
                render_commit_files(
                    ui,
                    catalog,
                    tokens,
                    &snap.repo_root,
                    commit.sha,
                    state,
                    selected,
                    reveal,
                    action,
                );
            });
        }
    }
    if snap.commits_truncated {
        ui.weak(catalog.t(
            "git.commits_truncated",
            &[("count", &MAX_BRANCH_COMMITS.to_string())],
        ));
    }
}

/// 펼친 커밋의 파일 행 — 불러오는 중·실패·바뀐 파일 없음 안내를 포함한다.
#[allow(clippy::too_many_arguments)]
fn render_commit_files(
    ui: &mut egui::Ui,
    catalog: &i18n::Catalog,
    tokens: crate::ui::designall::Tokens,
    repo_root: &Path,
    sha: CommitSha,
    state: &CommitListState,
    selected: &mut Option<(String, crate::ui::diff_viewer::DiffMode)>,
    reveal: bool,
    action: &mut Option<GitPanelAction>,
) {
    let files = match &state.loaded {
        Some((loaded, CommitFilesState::Ready(files))) if *loaded == sha => files,
        Some((loaded, CommitFilesState::Failed)) if *loaded == sha => {
            ui.weak(catalog.t("git.commit.files_error", &[]));
            return;
        }
        _ => {
            ui.weak(catalog.t("diff.loading", &[]));
            return;
        }
    };
    if files.rows.is_empty() {
        ui.weak(catalog.t("git.commit.files_empty", &[]));
    }
    let mode = crate::ui::diff_viewer::DiffMode::Commit(sha);
    let options = FileRowOptions {
        hide_dir: false,
        reveal_selected: reveal,
    };
    for row in &files.rows {
        // 변경/브랜치 커밋 섹션과 같은 행(배지·줄인 폴더·선택 강조).
        let id_salt = ("git-panel-commit-file", sha.as_str(), row.rel_path.as_str());
        if let Some(clicked) = file_row(
            ui, id_salt, repo_root, row, mode, selected, options, tokens, catalog,
        ) {
            *action = Some(clicked);
        }
    }
    if files.truncated {
        ui.weak(catalog.t(
            "git.list.truncated",
            &[("count", &MAX_PANEL_FILES.to_string())],
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain과_numstat을_경로로_병합한다() {
        // porcelain -z: "XY path\0" 반복. rename은 "R  new\0old\0".
        let porcelain = " M a.rs\0?? new.txt\0R  moved.rs\0old.rs\0MM both.rs\0";
        // numstat -z: "adds\tdels\tpath\0", rename은 경로 칸이 비고 "\0old\0new\0"가 따른다.
        let numstat = "3\t1\ta.rs\x00456\t221\t\0old.rs\0moved.rs\0-\t-\tbin.png\x00\
                       2\t0\tboth.rs\0";
        let (rows, truncated) = merge_status_rows(porcelain, numstat);
        // 순서는 porcelain 등장 순서를 유지한다.
        assert_eq!(rows.len(), 4);
        assert!(!truncated);
        assert_eq!(
            (
                rows[0].rel_path.as_str(),
                rows[0].status,
                rows[0].adds,
                rows[0].dels
            ),
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
        assert_eq!(
            (rows[3].rel_path.as_str(), rows[3].status),
            ("both.rs", 'M')
        );
    }

    /// 충돌(unmerged) 항목은 XY가 AA·DD·UD처럼 보여 워킹트리(Y) 우선 규칙으로는 「추가」·
    /// 「삭제」 배지가 됐다 — 충돌인데 평범한 추가로 보이고, UD/DD는 삭제로 판정돼 경로
    /// 동작까지 막혔다(2026-10-11 리뷰). unmerged 짝은 모두 'U'다.
    #[test]
    fn 충돌_항목은_xy와_무관하게_충돌_상태다() {
        let porcelain = "AA both.rs\0UD ours.rs\0DU theirs.rs\0DD gone.rs\0UU edit.rs\0\
                         AU a.rs\0UA b.rs\0 M plain.rs\0";
        let (rows, _) = merge_status_rows(porcelain, "");
        let statuses: Vec<(&str, char)> = rows
            .iter()
            .map(|row| (row.rel_path.as_str(), row.status))
            .collect();
        assert_eq!(
            statuses,
            vec![
                ("both.rs", 'U'),
                ("ours.rs", 'U'),
                ("theirs.rs", 'U'),
                ("gone.rs", 'U'),
                ("edit.rs", 'U'),
                ("a.rs", 'U'),
                ("b.rs", 'U'),
                ("plain.rs", 'M'),
            ]
        );
    }

    #[test]
    fn numstat만_있는_경로는_committed_파서가_그대로_담는다() {
        // committed 섹션: numstat + name-status 병합. 바이너리는 "-\t-".
        // -z: numstat "adds\tdels\tpath\0", name-status "상태\0경로\0"(rename은 "R100\0old\0new\0").
        let numstat = "12\t13\tsrc/ui/workspace.rs\0-\t-\tassets/logo.png\x00\
                       5\t0\t\0old name.rs\0새 이름.rs\0";
        let name_status =
            "M\0src/ui/workspace.rs\0A\0assets/logo.png\0R100\0old name.rs\0새 이름.rs\0";
        let (rows, truncated) = merge_committed_rows(numstat, name_status);
        assert_eq!(rows.len(), 3);
        assert!(!truncated);
        assert_eq!(
            (
                rows[0].rel_path.as_str(),
                rows[0].status,
                rows[0].adds,
                rows[0].dels
            ),
            ("src/ui/workspace.rs", 'M', Some(12), Some(13))
        );
        assert_eq!(
            (
                rows[1].rel_path.as_str(),
                rows[1].status,
                rows[1].adds,
                rows[1].dels
            ),
            ("assets/logo.png", 'A', None, None)
        );
        // rename은 새 경로로, 증감도 새 경로에 짝지어진다.
        assert_eq!(
            (rows[2].rel_path.as_str(), rows[2].status, rows[2].adds),
            ("새 이름.rs", 'R', Some(5))
        );
    }

    // 중요 리뷰 지적 3번 회귀 — 벡터가 이미 상한으로 잘린 뒤에 len() >= 상한을
    // 검사하면 "딱 상한이라 안 잘림"과 "넘쳐서 버림"을 구분 못 한다. 정확히
    // 상한이면 false, 하나 더 있으면 true를 고정한다.
    #[test]
    fn merge_status_rows는_상한에_정확히_닿으면_잘림이_아니다() {
        let mut porcelain = String::new();
        for i in 0..MAX_PANEL_FILES {
            porcelain.push_str(&format!(" M f{i}.rs\0"));
        }
        let (rows, truncated) = merge_status_rows(&porcelain, "");
        assert_eq!(rows.len(), MAX_PANEL_FILES);
        assert!(!truncated, "정확히 상한이면 잃은 항목이 없다");
    }

    #[test]
    fn merge_status_rows는_상한을_하나_넘으면_잘린다() {
        let mut porcelain = String::new();
        for i in 0..(MAX_PANEL_FILES + 1) {
            porcelain.push_str(&format!(" M f{i}.rs\0"));
        }
        let (rows, truncated) = merge_status_rows(&porcelain, "");
        assert_eq!(rows.len(), MAX_PANEL_FILES);
        assert!(truncated, "하나라도 넘치면 잘림 표시가 있어야 한다");
    }

    #[test]
    fn merge_committed_rows는_상한에_정확히_닿으면_잘림이_아니다() {
        let mut name_status = String::new();
        for i in 0..MAX_PANEL_FILES {
            name_status.push_str(&format!("M\0f{i}.rs\0"));
        }
        let (rows, truncated) = merge_committed_rows("", &name_status);
        assert_eq!(rows.len(), MAX_PANEL_FILES);
        assert!(!truncated, "정확히 상한이면 잃은 항목이 없다");
    }

    #[test]
    fn merge_committed_rows는_상한을_하나_넘으면_잘린다() {
        let mut name_status = String::new();
        for i in 0..(MAX_PANEL_FILES + 1) {
            name_status.push_str(&format!("M\0f{i}.rs\0"));
        }
        let (rows, truncated) = merge_committed_rows("", &name_status);
        assert_eq!(rows.len(), MAX_PANEL_FILES);
        assert!(truncated, "하나라도 넘치면 잘림 표시가 있어야 한다");
    }

    #[test]
    fn ahead_behind는_left_right_count_출력을_읽는다() {
        assert_eq!(parse_ahead_behind("73\t4\n"), Some((4, 73)));
        assert_eq!(parse_ahead_behind("0\t0"), Some((0, 0)));
        assert_eq!(parse_ahead_behind("garbage"), None);
    }

    #[test]
    fn 파일명과_디렉터리를_분리한다() {
        assert_eq!(
            split_row_path("crates/app/src/app.rs"),
            ("app.rs", "crates/app/src")
        );
        assert_eq!(split_row_path("Cargo.toml"), ("Cargo.toml", ""));
    }

    #[test]
    fn short_dir는_끝_두_단계만_남기고_앞을_줄인다() {
        assert_eq!(short_dir("crates/app/src/ui"), "…/src/ui");
        assert_eq!(short_dir("crates/app"), "crates/app");
        assert_eq!(short_dir("src"), "src");
        assert_eq!(short_dir(""), "");
    }

    // 중요 리뷰 지적 4번 회귀 — ahead/behind·+/− 색은 designall::tokens에서만
    // 고른 값이라 라이트/다크 둘 다 자동으로 성립해야 한다(transcript_viewer.rs
    // 역할별_배경_테스트와 같은 관례). agent_visuals::status_color는 dark_mode
    // 분기가 없어 라이트 테마에서 대비가 낮았다.
    #[test]
    fn stat_color는_ahead_added는_성공색_behind_dels는_에러색이다() {
        for tokens in [crate::ui::designall::DARK, crate::ui::designall::LIGHT] {
            assert_eq!(stat_color(tokens, true), tokens.success);
            assert_eq!(stat_color(tokens, false), tokens.error);
            assert_ne!(
                tokens.success, tokens.error,
                "성공/에러 색은 서로 달라야 한다"
            );
            assert_ne!(
                tokens.success, tokens.app_background,
                "성공색은 배경과 구분돼야 한다"
            );
            assert_ne!(
                tokens.error, tokens.app_background,
                "에러색은 배경과 구분돼야 한다"
            );
        }
        assert_ne!(
            crate::ui::designall::DARK.success,
            crate::ui::designall::LIGHT.success,
            "다크/라이트 팔레트는 서로 달라야 한다"
        );
        assert_ne!(
            crate::ui::designall::DARK.error,
            crate::ui::designall::LIGHT.error,
            "다크/라이트 팔레트는 서로 달라야 한다"
        );
    }

    #[test]
    fn 상태_배지는_porcelain_문자를_색과_설명_키로_바꾼다() {
        let tokens = crate::ui::designall::DARK;
        assert_eq!(
            status_badge_style('M', tokens),
            ('M', tokens.warning, "git.status.modified")
        );
        assert_eq!(
            status_badge_style('A', tokens),
            ('A', tokens.success, "git.status.added")
        );
        assert_eq!(
            status_badge_style('D', tokens),
            ('D', tokens.error, "git.status.deleted")
        );
        assert_eq!(
            status_badge_style('R', tokens),
            ('R', tokens.accent, "git.status.renamed")
        );
        assert_eq!(
            status_badge_style('C', tokens),
            ('C', tokens.accent, "git.status.copied")
        );
        assert_eq!(
            status_badge_style('U', tokens),
            ('U', tokens.error, "git.status.conflict")
        );
        // untracked는 `?` 대신 「새 파일」의 N으로 보인다.
        assert_eq!(
            status_badge_style('?', tokens),
            ('N', tokens.muted_text, "git.status.untracked")
        );
        assert_eq!(
            status_badge_style('T', tokens),
            ('T', tokens.muted_text, "git.status.other")
        );
    }

    // `git worktree list --porcelain -z` 실물 출력(2026-08-16, `xxd`로 실측)과 같은
    // 형식 — 필드가 NUL로 끝나고, 빈 필드(연속 NUL)가 항목 경계다.
    const WORKTREE_PORCELAIN: &str = "worktree /repo\0HEAD 1111111111111111111111111111111111111111\0branch refs/heads/main\0\0worktree /repo/.deppy/worktrees/alpha\0HEAD 2222222222222222222222222222222222222222\0branch refs/heads/deppy/alpha\0\0worktree /repo/detached\0HEAD 3333333333333333333333333333333333333333\0detached\0\0worktree /repo/bare\0bare\0\0";

    #[test]
    fn 워크트리_목록은_브랜치와_현재를_구분한다() {
        let (rows, truncated) = parse_worktree_list(
            WORKTREE_PORCELAIN,
            Path::new("/repo/.deppy/worktrees/alpha"),
        );
        assert!(!truncated);
        // bare 항목은 체크아웃이 없어 셸을 열 수 없다 — 목록에서 뺀다.
        assert_eq!(rows.len(), 3, "bare는 제외한다: {rows:?}");
        assert_eq!(rows[0].name, "repo");
        assert_eq!(rows[0].branch.as_deref(), Some("main"));
        assert!(!rows[0].current);
        assert_eq!(rows[1].name, "alpha");
        assert_eq!(rows[1].branch.as_deref(), Some("deppy/alpha"));
        assert!(rows[1].current, "repo_root와 같은 경로가 현재 워크트리다");
        assert_eq!(rows[2].branch, None, "detached는 브랜치가 없다");
    }

    #[test]
    fn 워크트리_목록은_상한을_하나_넘으면_잘린다() {
        let mut porcelain = String::new();
        for index in 0..(MAX_WORKTREE_ROWS + 5) {
            porcelain.push_str(&format!(
                "worktree /repo/w{index}\0HEAD {index:040}\0branch refs/heads/b{index}\0\0"
            ));
        }
        let (rows, truncated) = parse_worktree_list(&porcelain, Path::new("/repo"));
        assert_eq!(rows.len(), MAX_WORKTREE_ROWS);
        assert!(truncated, "5개 넘치면 잘림 표시가 있어야 한다");
    }

    // 중요 리뷰 지적 3번 회귀 — 딱 상한만큼이면 아무것도 안 잃었으니 잘림이 아니다.
    #[test]
    fn 워크트리_목록은_상한에_정확히_닿으면_잘림이_아니다() {
        let mut porcelain = String::new();
        for index in 0..MAX_WORKTREE_ROWS {
            porcelain.push_str(&format!(
                "worktree /repo/w{index}\0HEAD {index:040}\0branch refs/heads/b{index}\0\0"
            ));
        }
        let (rows, truncated) = parse_worktree_list(&porcelain, Path::new("/repo"));
        assert_eq!(rows.len(), MAX_WORKTREE_ROWS);
        assert!(!truncated, "정확히 상한이면 잃은 항목이 없다");
    }

    #[test]
    fn 워크트리_잠금_줄은_무시한다() {
        let porcelain =
            "worktree /repo\0HEAD 1111\0branch refs/heads/main\0locked\0prunable gone\0\0";
        let (rows, truncated) = parse_worktree_list(porcelain, Path::new("/repo"));
        assert!(!truncated);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].branch.as_deref(), Some("main"));
    }

    // 중요 리뷰 지적 2번 회귀 — trim()이 경로 끝 공백까지 지워 존재하지 않는
    // 경로를 만들었다. trim을 없앤 뒤에도 실제로 보존되는지 고정한다.
    #[test]
    fn 워크트리_경로_끝_공백은_보존된다() {
        let porcelain = "worktree /repo/trailing \0HEAD 1111\0branch refs/heads/main\0\0";
        let (rows, _truncated) = parse_worktree_list(porcelain, Path::new("/repo"));
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].path, "/repo/trailing ",
            "경로 끝 공백이 지워지면 안 된다"
        );
    }

    // 중요 리뷰 지적 2번 회귀 — `-z` 없이는 `.lines()`가 경로 내부 개행에서 항목을
    // 둘로 쪼갰다. git-worktree(1): "-z ... makes it possible to parse the output
    // when the entry contains a newline". NUL 구분이라 개행이 있어도 필드 하나로 남는다.
    #[test]
    fn 워크트리_경로에_개행이_있어도_안_잘린다() {
        let porcelain = "worktree /repo/weird\nname\0HEAD 1111\0branch refs/heads/main\0\0";
        let (rows, _truncated) = parse_worktree_list(porcelain, Path::new("/repo"));
        assert_eq!(
            rows.len(),
            1,
            "개행이 있어도 항목 하나로 남아야 한다: {rows:?}"
        );
        assert_eq!(rows[0].path, "/repo/weird\nname");
    }

    use std::time::Duration;
    const T: Duration = Duration::from_secs(10);

    fn temp_repo(tag: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "deppy-gitpanel-{tag}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        crate::git_cli::run_git(&dir, &["init", "-q", "-b", "main"], T).unwrap();
        crate::git_cli::run_git(&dir, &["config", "user.email", "t@t"], T).unwrap();
        crate::git_cli::run_git(&dir, &["config", "user.name", "t"], T).unwrap();
        dir
    }

    fn commit_all(repo: &std::path::Path, msg: &str) {
        crate::git_cli::run_git(repo, &["add", "-A"], T).unwrap();
        crate::git_cli::run_git(repo, &["commit", "-q", "-m", msg], T).unwrap();
    }

    #[test]
    fn collect_snapshot은_브랜치와_변경_목록을_수집한다() {
        let repo = temp_repo("snap");
        std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
        commit_all(&repo, "base");
        // 워킹트리 변경 1 + untracked 1
        std::fs::write(repo.join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        std::fs::write(repo.join("new.txt"), "hi\n").unwrap();

        let snap = collect_snapshot(&repo).expect("collect");
        assert_eq!(snap.branch, "main");
        // upstream이 없는 로컬 repo: committed 섹션은 비고 ahead/behind는 0.
        assert_eq!(snap.upstream, None);
        assert_eq!(snap.committed.len(), 0);
        let paths: Vec<&str> = snap.changes.iter().map(|r| r.rel_path.as_str()).collect();
        assert!(paths.contains(&"a.rs") && paths.contains(&"new.txt"));
        let a = snap.changes.iter().find(|r| r.rel_path == "a.rs").unwrap();
        assert_eq!((a.status, a.adds), ('M', Some(1)));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn collect_snapshot은_upstream이_있으면_committed와_ahead_behind를_채운다() {
        // "원격"을 흉내내는 로컬 클론: origin = 다른 로컬 repo.
        let origin = temp_repo("origin");
        std::fs::write(origin.join("f.rs"), "one\n").unwrap();
        commit_all(&origin, "c1");
        let clone_dir = std::env::temp_dir().join(format!(
            "deppy-gitpanel-clone-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::git_cli::run_git(
            origin.parent().unwrap(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                clone_dir.to_str().unwrap(),
            ],
            T,
        )
        .unwrap();
        crate::git_cli::run_git(&clone_dir, &["config", "user.email", "t@t"], T).unwrap();
        crate::git_cli::run_git(&clone_dir, &["config", "user.name", "t"], T).unwrap();
        // 로컬 커밋 1개 → ahead=1, behind=0, committed에 f.rs.
        std::fs::write(clone_dir.join("f.rs"), "one\ntwo\n").unwrap();
        commit_all(&clone_dir, "local work");

        let snap = collect_snapshot(&clone_dir).expect("collect");
        assert!(snap.upstream.as_deref().unwrap_or("").contains("origin/"));
        assert_eq!((snap.ahead, snap.behind), (1, 0));
        assert_eq!(snap.committed.len(), 1);
        assert_eq!(snap.committed[0].rel_path, "f.rs");
        // origin이 로컬 경로(비GitHub)이므로 remote_https_base는 None이어야 한다.
        assert_eq!(snap.remote_https_base, None);
        std::fs::remove_dir_all(&origin).ok();
        std::fs::remove_dir_all(&clone_dir).ok();
    }

    /// 테스트용 — origin을 흉내내는 로컬 repo를 clone하고 커밋 작성자를 설정한다.
    fn clone_with_upstream(origin: &std::path::Path, tag: &str) -> std::path::PathBuf {
        let clone_dir = std::env::temp_dir().join(format!(
            "deppy-gitpanel-clone-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::git_cli::run_git(
            origin.parent().unwrap(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                clone_dir.to_str().unwrap(),
            ],
            T,
        )
        .unwrap();
        crate::git_cli::run_git(&clone_dir, &["config", "user.email", "t@t"], T).unwrap();
        crate::git_cli::run_git(&clone_dir, &["config", "user.name", "t"], T).unwrap();
        clone_dir
    }

    /// 한글·공백 경로를 `-z` 없이 받으면 git이 C-quote(`"\355…"`)해 돌려줘 행 경로·증감·
    /// diff·경로 동작이 전부 어긋났다(2026-10-11 리뷰). numstat/name-status를 `-z`로 받는다.
    #[test]
    fn 한글_경로도_변경_브랜치_커밋_파일에서_원래_경로로_읽는다() {
        use crate::ui::diff_viewer::DiffMode;

        let origin = temp_repo("origin-utf8");
        std::fs::write(origin.join("기본.rs"), "one\n").unwrap();
        commit_all(&origin, "c1");
        let clone_dir = clone_with_upstream(&origin, "utf8");
        std::fs::write(clone_dir.join("한글 파일.rs"), "a\nb\n").unwrap();
        commit_all(&clone_dir, "add korean");
        std::fs::write(clone_dir.join("기본.rs"), "one\ntwo\n").unwrap();

        let snap = collect_snapshot(&clone_dir).expect("collect");
        let change = snap
            .changes
            .iter()
            .find(|row| row.rel_path == "기본.rs")
            .expect("변경 행은 원래 경로여야 한다");
        assert_eq!(change.adds, Some(1), "변경 증감이 경로와 짝지어져야 한다");
        let committed = snap
            .committed
            .iter()
            .find(|row| row.rel_path == "한글 파일.rs")
            .expect("브랜치 커밋 행은 원래 경로여야 한다");
        assert_eq!(committed.adds, Some(2));

        let sha = snap.commits[0].sha;
        let files = collect_commit_files(&clone_dir, sha).expect("commit files");
        assert_eq!(files.rows[0].rel_path, "한글 파일.rs");
        assert_eq!(files.rows[0].adds, Some(2));
        let view =
            collect_file_diff(&clone_dir, "한글 파일.rs", DiffMode::Commit(sha)).expect("diff");
        assert!(
            !view.hunks.is_empty(),
            "원래 경로로 커밋 diff를 찾아야 한다"
        );
        std::fs::remove_dir_all(&origin).ok();
        std::fs::remove_dir_all(&clone_dir).ok();
    }

    /// 추적 브랜치 없는 새 브랜치는 `origin/HEAD`를 기준으로 쓰고 그 사실을 표시한다. 브랜치
    /// 커밋 파일의 diff도 같은 기준을 써야 한다 — 예전 diff는 `@{u}`만 봐서 열리지 않았다.
    #[test]
    fn 추적_브랜치가_없으면_origin_head를_기준으로_쓰고_diff도_열린다() {
        use crate::ui::diff_viewer::DiffMode;

        let origin = temp_repo("origin-fallback");
        std::fs::write(origin.join("f.rs"), "one\n").unwrap();
        commit_all(&origin, "c1");
        let clone_dir = clone_with_upstream(&origin, "fallback");
        crate::git_cli::run_git(&clone_dir, &["switch", "-q", "-c", "feat/local"], T).unwrap();
        std::fs::write(clone_dir.join("f.rs"), "one\ntwo\n").unwrap();
        commit_all(&clone_dir, "local work");

        let snap = collect_snapshot(&clone_dir).expect("collect");
        assert!(
            snap.upstream
                .as_deref()
                .unwrap_or("")
                .starts_with("origin/")
        );
        assert!(
            snap.upstream_fallback,
            "추적 브랜치가 아닌 폴백임을 표시해야 한다"
        );
        assert_eq!(snap.committed.len(), 1);
        let view = collect_file_diff(&clone_dir, "f.rs", DiffMode::Branch).expect("branch diff");
        assert!(
            !view.hunks.is_empty(),
            "폴백 기준으로 브랜치 diff가 열려야 한다"
        );
        std::fs::remove_dir_all(&origin).ok();
        std::fs::remove_dir_all(&clone_dir).ok();
    }

    // ── 브랜치 커밋 목록(2026-10-11 PR 4) ─────────────────────────────

    /// `git log -z --format=%h%x00%s%x00%ct` 한 항목 — 필드도 항목도 NUL로 끝난다(실측).
    fn log_record(sha: &str, subject: &str, time: i64) -> String {
        format!("{sha}\0{subject}\0{time}\0")
    }

    #[test]
    fn 커밋_로그_파서는_빈_출력이면_빈_목록이다() {
        assert_eq!(parse_branch_commits(""), (Vec::new(), false));
    }

    #[test]
    fn 커밋_로그_파서는_상한에_정확히_닿으면_잘림이_아니다() {
        let log: String = (0..MAX_BRANCH_COMMITS)
            .map(|i| log_record(&format!("{i:07x}"), "s", 1))
            .collect();
        let (rows, truncated) = parse_branch_commits(&log);
        assert_eq!(rows.len(), MAX_BRANCH_COMMITS);
        assert!(!truncated, "정확히 상한이면 잃은 항목이 없다");
    }

    #[test]
    fn 커밋_로그_파서는_상한을_하나_넘으면_잘린다() {
        let log: String = (0..(MAX_BRANCH_COMMITS + 1))
            .map(|i| log_record(&format!("{i:07x}"), "s", 1))
            .collect();
        let (rows, truncated) = parse_branch_commits(&log);
        assert_eq!(rows.len(), MAX_BRANCH_COMMITS);
        assert!(truncated, "하나라도 넘치면 잘림 표시가 있어야 한다");
        // 순서는 git log 출력 순서(최신순) 그대로다 — 버리는 쪽은 가장 오래된 커밋이다.
        assert_eq!(rows[0].sha.as_str(), "0000000");
    }

    #[test]
    fn 커밋_로그_파서는_탭과_유니코드_제목을_그대로_담는다() {
        let subject = "fix:\t탭 «인용» \"따옴표\" ✓ 🚀";
        let log = log_record("a1b2c3d", subject, 1_791_680_568) + &log_record("9f8e7d6", "", 7);
        let (rows, truncated) = parse_branch_commits(&log);
        assert!(!truncated);
        assert_eq!(
            rows,
            vec![
                GitCommitRow {
                    sha: CommitSha::parse("a1b2c3d").unwrap(),
                    subject: subject.to_owned(),
                    time: 1_791_680_568,
                },
                // 빈 제목(--allow-empty-message)도 필드 정렬을 깨지 않는다.
                GitCommitRow {
                    sha: CommitSha::parse("9f8e7d6").unwrap(),
                    subject: String::new(),
                    time: 7,
                },
            ]
        );
    }

    /// 바이트 상한(run_git_limited)에서 잘린 출력은 마지막 NUL 뒤 조각이 미완성이다 —
    /// 시각 "17"처럼 잘린 숫자를 온전한 값으로 읽으면 안 된다.
    #[test]
    fn 커밋_로그_파서는_nul로_끝나지_않은_조각을_버린다() {
        let log = log_record("a1b2c3d", "온전함", 100) + "9f8e7d6\x00잘린 제목\x0017";
        let (rows, _) = parse_branch_commits(&log);
        assert_eq!(rows.len(), 1, "미완성 항목은 버린다: {rows:?}");
        assert_eq!(rows[0].subject, "온전함");
    }

    #[test]
    fn 커밋_로그_파서는_sha나_시각이_손상된_항목을_건너뛴다() {
        let log = log_record("not-hex", "a", 1)
            + &log_record("a1b2c3d", "b", 2)
            + &log_record("9f8e7d6", "c", -1)
            + "1234567\0d\0x\0"; // 시각이 숫자가 아니다
        let (rows, truncated) = parse_branch_commits(&log);
        assert!(!truncated);
        let subjects: Vec<&str> = rows.iter().map(|r| r.subject.as_str()).collect();
        assert_eq!(subjects, vec!["b", "c"]);
    }

    #[test]
    fn commit_sha는_4자_이상_64자_이하_16진수만_받는다() {
        let sha = CommitSha::parse("a1B2c3d").expect("16진수");
        assert_eq!(sha.as_str(), "a1B2c3d");
        assert_eq!(format!("{sha:?}"), "a1B2c3d");
        let full = "f".repeat(64);
        assert_eq!(CommitSha::parse(&full).unwrap().as_str(), full);
        // git 인자로 넘기므로 옵션·경로로 읽힐 값은 여기서 막는다.
        for bad in [
            "",
            "abc",
            "-n1",
            "a1b2c3d ",
            "a1b2/c3d",
            "g123456",
            &"f".repeat(65),
        ] {
            assert_eq!(CommitSha::parse(bad), None, "{bad:?}");
        }
    }

    /// committed 파일 목록과 **같은 기준**(merge-base..HEAD)의 커밋만, 최신순으로 담는다.
    #[test]
    fn collect_snapshot은_기준_이후_커밋만_최신순으로_담는다() {
        let origin = temp_repo("commits-origin");
        std::fs::write(origin.join("f.rs"), "one\n").unwrap();
        commit_all(&origin, "기준 이전 커밋");
        let clone_dir = std::env::temp_dir().join(format!(
            "deppy-gitpanel-commits-clone-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::git_cli::run_git(
            origin.parent().unwrap(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                clone_dir.to_str().unwrap(),
            ],
            T,
        )
        .unwrap();
        crate::git_cli::run_git(&clone_dir, &["config", "user.email", "t@t"], T).unwrap();
        crate::git_cli::run_git(&clone_dir, &["config", "user.name", "t"], T).unwrap();
        std::fs::write(clone_dir.join("f.rs"), "one\ntwo\n").unwrap();
        commit_all(&clone_dir, "첫 로컬\t작업");
        std::fs::write(clone_dir.join("g.rs"), "g\n").unwrap();
        commit_all(&clone_dir, "둘째 로컬 작업 ✓");

        let snap = collect_snapshot(&clone_dir).expect("collect");
        let subjects: Vec<&str> = snap.commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, vec!["둘째 로컬 작업 ✓", "첫 로컬\t작업"]);
        assert!(!snap.commits_truncated);
        let head = crate::git_cli::run_git(&clone_dir, &["rev-parse", "HEAD"], T).unwrap();
        // 전체 sha로 담는다 — 짧은 sha는 같은 이름의 브랜치나 나중의 충돌로 다른 커밋을
        // 가리킬 수 있다(2026-10-11 리뷰). 화면에는 앞 7자만 보인다.
        assert_eq!(
            snap.commits[0].sha.as_str(),
            head.trim(),
            "첫 항목은 HEAD다"
        );
        assert_eq!(snap.commits[0].sha.short(), &head.trim()[..7]);
        assert!(snap.commits.iter().all(|c| c.time > 0));
        std::fs::remove_dir_all(&origin).ok();
        std::fs::remove_dir_all(&clone_dir).ok();
    }

    /// 커밋 목록과 같은 형태(전체 sha)의 HEAD.
    fn head_sha(repo: &std::path::Path) -> CommitSha {
        let out = crate::git_cli::run_git(repo, &["rev-parse", "HEAD"], T).unwrap();
        CommitSha::parse(out.trim()).unwrap()
    }

    fn file_row(path: &str, status: char, adds: u32, dels: u32) -> GitFileRow {
        GitFileRow {
            rel_path: path.into(),
            status,
            adds: Some(adds),
            dels: Some(dels),
        }
    }

    /// App이 쓰는 IO 진입점(execute_io)으로 본다 — 세대·sha를 그대로 돌려주고, 루트
    /// 커밋도 부모 없이 전량 추가로 보인다.
    #[test]
    fn 커밋_파일_io는_그_커밋이_바꾼_파일만_돌려준다() {
        let repo = temp_repo("commit-files");
        std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
        commit_all(&repo, "root");
        let root = head_sha(&repo);
        std::fs::write(repo.join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs/b.md"), "b\n").unwrap();
        commit_all(&repo, "second");
        let second = head_sha(&repo);
        // 커밋하지 않은 변경은 커밋 파일 목록에 섞이면 안 된다.
        std::fs::write(repo.join("a.rs"), "dirty\n").unwrap();

        let files = |sha: CommitSha| match execute_io(GitPanelIoIntent {
            generation: 9,
            cwd: repo.clone(),
            request: GitPanelIoRequest::CommitFiles { sha },
        }) {
            GitPanelIoCompletion {
                generation: 9,
                result: GitPanelIoResult::CommitFiles { sha: got, result },
            } => {
                assert_eq!(got, sha);
                result.expect("collect")
            }
            other => panic!("커밋 파일 결과여야 한다: {other:?}"),
        };
        assert_eq!(
            files(second),
            GitCommitFiles {
                rows: vec![
                    file_row("a.rs", 'M', 1, 0),
                    file_row("docs/b.md", 'A', 1, 0)
                ],
                truncated: false,
            }
        );
        assert_eq!(
            files(root).rows,
            vec![file_row("a.rs", 'A', 1, 0)],
            "루트 커밋은 빈 트리 대비 전량 추가다"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn collect_file_diff는_커밋_모드에서_그_커밋의_변경만_읽는다() {
        use crate::ui::diff_viewer::{DiffMode, LineKind};
        let repo = temp_repo("commit-diff");
        std::fs::write(repo.join("a.rs"), "one\n").unwrap();
        commit_all(&repo, "root");
        let root = head_sha(&repo);
        std::fs::write(repo.join("a.rs"), "one\ntwo\n").unwrap();
        commit_all(&repo, "second");
        let second = head_sha(&repo);
        std::fs::write(repo.join("a.rs"), "one\ntwo\nthree\n").unwrap();

        let lines = |sha: CommitSha| -> Vec<(LineKind, String)> {
            let view = collect_file_diff(&repo, "a.rs", DiffMode::Commit(sha)).expect("collect");
            assert!(!view.truncated && !view.binary);
            view.hunks
                .iter()
                .flat_map(|h| h.lines.iter())
                .map(|l| (l.kind, l.text.clone()))
                .collect()
        };
        assert_eq!(
            lines(second),
            vec![
                (LineKind::Context, "one".to_owned()),
                (LineKind::Add, "two".to_owned())
            ],
            "그 커밋의 변경만 — 워킹트리의 three는 섞이지 않는다"
        );
        assert_eq!(
            lines(root),
            vec![(LineKind::Add, "one".to_owned())],
            "루트 커밋은 부모가 없어도 전량 추가로 보인다"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    /// 머지 커밋은 첫 부모 대비로 본다. `git show` 기본값(--cc)은 충돌 없는 머지의
    /// name-status와 diff를 비워, 머지가 브랜치에 들여온 파일이 통째로 사라졌다
    /// (2026-10-11 실측).
    #[test]
    fn 커밋_수집은_머지_커밋을_첫_부모_대비로_본다() {
        use crate::ui::diff_viewer::{DiffMode, LineKind};
        let repo = temp_repo("commit-merge");
        std::fs::write(repo.join("f.rs"), "f\n").unwrap();
        commit_all(&repo, "root");
        crate::git_cli::run_git(&repo, &["checkout", "-q", "-b", "side"], T).unwrap();
        std::fs::write(repo.join("m.rs"), "m\n").unwrap();
        commit_all(&repo, "side");
        crate::git_cli::run_git(&repo, &["checkout", "-q", "main"], T).unwrap();
        std::fs::write(repo.join("f.rs"), "f\nmain\n").unwrap();
        commit_all(&repo, "main work");
        crate::git_cli::run_git(&repo, &["merge", "-q", "--no-edit", "side"], T).unwrap();
        let merge = head_sha(&repo);

        let files = collect_commit_files(&repo, merge).expect("collect");
        assert_eq!(files.rows, vec![file_row("m.rs", 'A', 1, 0)]);
        let view = collect_file_diff(&repo, "m.rs", DiffMode::Commit(merge)).expect("collect");
        let lines: Vec<(LineKind, &str)> = view
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .map(|l| (l.kind, l.text.as_str()))
            .collect();
        assert_eq!(lines, vec![(LineKind::Add, "m")]);
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn 커밋_펼침은_파일_목록이_없을_때만_로드를_요청한다() {
        let a = CommitSha::parse("a1b2c3d").unwrap();
        let b = CommitSha::parse("9f8e7d6").unwrap();
        let load = |sha| Some(GitPanelAction::LoadCommitFiles { sha });
        let mut panel = GitPanelUi::default();

        // 처음 펼침 → 로드 요청, 다시 클릭 → 접힘(요청 없음).
        assert_eq!(panel.commit_list.toggle(a), load(a));
        assert_eq!(panel.commit_list.toggle(a), None);
        assert_eq!(panel.commit_list.expanded, None);
        // 결과가 오기 전에 다시 펼치면 재요청한다 — App의 git IO는 latest-only라
        // 앞 요청의 완료가 버려졌을 수 있다(접었다 펴기가 곧 재시도).
        assert_eq!(panel.commit_list.toggle(a), load(a));
        panel.set_commit_files(a, Ok(GitCommitFiles::default()));
        // 불러온 뒤에는 접었다 펴도 다시 요청하지 않는다.
        assert_eq!(panel.commit_list.toggle(a), None);
        assert_eq!(panel.commit_list.toggle(a), None);
        assert_eq!(panel.commit_list.expanded, Some(a));
        // 다른 커밋을 펼치면 앞 커밋은 접히고(한 번에 하나) 새로 요청한다.
        assert_eq!(panel.commit_list.toggle(b), load(b));
        assert_eq!(panel.commit_list.expanded, Some(b));
        // 늦게 온 다른 커밋의 결과는 지금 기다리는 커밋 자리를 덮지 않는다.
        panel.set_commit_files(a, Ok(GitCommitFiles::default()));
        assert!(matches!(
            panel.commit_list.loaded,
            Some((sha, CommitFilesState::Loading)) if sha == b
        ));
        // 실패는 다시 펼칠 때 재시도한다.
        panel.set_commit_files(b, Err(GitPanelErrorCode::CollectionFailed));
        assert!(matches!(
            panel.commit_list.loaded,
            Some((sha, CommitFilesState::Failed)) if sha == b
        ));
        assert_eq!(panel.commit_list.toggle(b), None);
        assert_eq!(panel.commit_list.toggle(b), load(b));
    }

    #[test]
    fn kittest_커밋을_펼치면_파일_로드를_요청하고_파일_클릭은_커밋_diff를_연다() {
        use egui_kittest::kittest::Queryable;

        struct State {
            panel: GitPanelUi,
            action: Option<GitPanelAction>,
        }

        let sha = CommitSha::parse("a1b2c3d").unwrap();
        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "feat".into(),
            upstream: Some("origin/main".into()),
            committed: vec![file_row("src/branch.rs", 'M', 1, 0)],
            commits: vec![GitCommitRow {
                sha,
                subject: "첫 커밋".into(),
                time: 0,
            }],
            ..Default::default()
        }));
        // 커밋 목록은 커밋 탭에 있다(2026-10-11 PR 2).
        panel.tab = GitPanelTab::Commits;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut State| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                if let Some(action) = state.panel.render(ui, &catalog, "") {
                    state.action = Some(action);
                }
            },
            State {
                panel,
                action: None,
            },
        );
        harness.run();

        // 커밋 행 전체를 접근성 이름(sha + 제목)으로 겨냥한다 — 파일 행과 같은 패턴.
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "a1b2c3d 첫 커밋")
            .click();
        harness.run();
        assert_eq!(
            harness.state().action,
            Some(GitPanelAction::LoadCommitFiles { sha }),
            "처음 펼친 커밋은 파일 목록 로드를 요청해야 한다"
        );
        harness.get_by_label(&catalog.t("diff.loading", &[]));

        // 실패는 그 커밋 자리에 오류로 보이고, 접었다 다시 펴면 재요청한다.
        harness
            .state_mut()
            .panel
            .set_commit_files(sha, Err(GitPanelErrorCode::CollectionFailed));
        harness.run();
        harness.get_by_label(&catalog.t("git.commit.files_error", &[]));
        harness.state_mut().action = None;
        for _ in 0..2 {
            harness
                .get_by_role_and_label(egui::accesskit::Role::Button, "a1b2c3d 첫 커밋")
                .click();
            harness.run();
        }
        assert_eq!(
            harness.state().action,
            Some(GitPanelAction::LoadCommitFiles { sha })
        );

        harness.state_mut().panel.set_commit_files(
            sha,
            Ok(GitCommitFiles {
                rows: vec![file_row("src/commit_only.rs", 'A', 3, 0)],
                truncated: false,
            }),
        );
        harness.run();
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "src/commit_only.rs")
            .click();
        harness.run();
        assert_eq!(
            harness.state().action,
            Some(GitPanelAction::ShowFileDiff {
                rel_path: "src/commit_only.rs".to_owned(),
                mode: crate::ui::diff_viewer::DiffMode::Commit(sha),
            }),
            "커밋 파일 행 클릭은 그 커밋 기준 diff를 열어야 한다"
        );
        // 섹션 파일 행과 같은 file_row라 클릭한 커밋 파일이 선택으로 남는다.
        assert_eq!(
            harness.state().panel.selected,
            Some((
                "src/commit_only.rs".to_owned(),
                crate::ui::diff_viewer::DiffMode::Commit(sha)
            ))
        );
    }

    #[test]
    fn repo가_아니면_no_repo_오류다() {
        let dir =
            std::env::temp_dir().join(format!("deppy-gitpanel-norepo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            collect_snapshot(&dir).unwrap_err(),
            GitPanelErrorCode::NoRepo
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // 치명 리뷰 지적 1번 회귀 — 예전엔 untracked 파일을 std::fs::read로 전량
    // 메모리에 올린 뒤 MAX_LIST_BYTES로 잘랐다(읽기 자체가 무계). 지금은
    // diff_panel::append_untracked_section과 같은 `--no-index /dev/null` 유계
    // 실행기를 쓰고, 결과가 이미 unified diff라 parse_unified로 해석된다.
    #[test]
    fn collect_file_diff는_untracked_파일_내용을_git_no_index로_수집한다() {
        let repo = temp_repo("filediff-untracked");
        std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
        commit_all(&repo, "base");
        std::fs::write(repo.join("new.txt"), "hello\nworld\n").unwrap();

        let view = collect_file_diff(&repo, "new.txt", crate::ui::diff_viewer::DiffMode::Working)
            .expect("collect");
        assert!(!view.truncated);
        assert!(!view.binary);
        let lines: Vec<&str> = view
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(lines, vec!["hello", "world"]);
        assert!(
            view.hunks.iter().all(|h| h
                .lines
                .iter()
                .all(|l| matches!(l.kind, crate::ui::diff_viewer::LineKind::Add))),
            "untracked는 전량 추가로 보여야 한다"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn collect_file_diff는_거대_untracked_파일도_상한에서_잘린다() {
        // fs::read 시절엔 파일 전체(수백MB 가능)를 읽고 나서야 잘랐다 — 읽기
        // 자체가 무계였다. 지금은 run_git_limited가 MAX_LIST_BYTES에서 git
        // 프로세스를 죽이므로 수집 바이트 자체가 유계다.
        let repo = temp_repo("filediff-untracked-huge");
        std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
        commit_all(&repo, "base");
        let huge = "x".repeat(MAX_LIST_BYTES * 2);
        std::fs::write(repo.join("big.txt"), &huge).unwrap();

        let view = collect_file_diff(&repo, "big.txt", crate::ui::diff_viewer::DiffMode::Working)
            .expect("collect");
        assert!(view.truncated, "상한을 넘는 파일은 잘림 표시가 있어야 한다");
        let collected_len: usize = view
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .map(|l| l.text.len())
            .sum();
        assert!(
            collected_len < huge.len(),
            "전량이 아니라 상한만큼만 수집돼야 한다: {collected_len}"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn 스냅샷은_자기_워크트리를_현재로_표시한다() {
        let repo = temp_repo("worktree_self");
        std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
        commit_all(&repo, "base");

        let snap = collect_snapshot(&repo).expect("스냅샷");
        assert_eq!(snap.worktrees.len(), 1, "새 repo는 메인 워크트리 하나뿐");
        assert!(snap.worktrees[0].current);
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn normalize_github_remote_https는_git_접미사를_떼고_정규화한다() {
        assert_eq!(
            normalize_github_remote("https://github.com/rust-lang/rust.git"),
            Some("https://github.com/rust-lang/rust".to_string())
        );
        assert_eq!(
            normalize_github_remote("https://github.com/rust-lang/rust"),
            Some("https://github.com/rust-lang/rust".to_string())
        );
    }

    #[test]
    fn normalize_github_remote_ssh_형식도_https로_정규화한다() {
        assert_eq!(
            normalize_github_remote("git@github.com:rust-lang/rust.git"),
            Some("https://github.com/rust-lang/rust".to_string())
        );
        assert_eq!(
            normalize_github_remote("git@github.com:rust-lang/rust"),
            Some("https://github.com/rust-lang/rust".to_string())
        );
    }

    #[test]
    fn normalize_github_remote_비github_remote는_none이다() {
        assert_eq!(
            normalize_github_remote("https://gitlab.com/foo/bar.git"),
            None
        );
        assert_eq!(
            normalize_github_remote("git@bitbucket.org:foo/bar.git"),
            None
        );
        assert_eq!(
            normalize_github_remote("/Users/t/tmp/some-local-repo"),
            None
        );
    }

    // 계획서 원안은 harness 통신에 `ui.ctx().memory_mut(..).insert_temp`와
    // `get_by_label_contains`를 썼는데, 이 저장소의 기존 kittest 관례
    // (fleet.rs `건너뛰기는_다음_항목을_히어로로_올린다`, workspace.rs의 `new_ui_state`
    // 테스트들)는 렌더 결과를 State 구조체 필드에 담아 `harness.state()`로 읽는다.
    // 관례 쪽이 정답이라 그 패턴으로 다시 썼다(2026-08-15 조정).
    #[test]
    fn kittest_행_클릭은_show_file_diff_액션을_낸다() {
        use egui_kittest::kittest::Queryable;

        struct State {
            panel: GitPanelUi,
            action: Option<GitPanelAction>,
        }

        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "main".into(),
            changes: vec![GitFileRow {
                rel_path: "src/a.rs".into(),
                status: 'M',
                adds: Some(3),
                dels: Some(1),
            }],
            ..Default::default()
        }));

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut State| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                if let Some(action) = state.panel.render(ui, &catalog, "") {
                    state.action = Some(action);
                }
            },
            State {
                panel,
                action: None,
            },
        );
        harness.run();
        // 자식 Label("a.rs") 자체를 클릭하면 부모 스코프로 이벤트가 새지 않는다
        // (egui hit-test는 포인터 아래 가장 안쪽 위젯을 고른다) — 그래서 행에
        // 명시적으로 심어 둔 accessible 이름(Role::Button + rel_path)으로 행 전체를
        // 겨냥한다. work_history.rs 카드 클릭 테스트와 같은 질의 방식(2026-08-15).
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "src/a.rs")
            .click();
        harness.run();

        assert_eq!(
            harness.state().action,
            Some(GitPanelAction::ShowFileDiff {
                rel_path: "src/a.rs".to_owned(),
                mode: crate::ui::diff_viewer::DiffMode::Working,
            }),
            "변경 사항 행 클릭은 Working 모드 ShowFileDiff를 내야 한다"
        );
    }

    #[test]
    fn kittest_클릭한_파일은_모드까지_구분해_선택으로_남는다() {
        use egui_kittest::kittest::Queryable;

        struct State {
            panel: GitPanelUi,
        }

        let row = |path: &str| GitFileRow {
            rel_path: path.into(),
            status: 'M',
            adds: Some(1),
            dels: Some(1),
        };
        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "main".into(),
            // 브랜치 커밋은 upstream 기준으로만 수집된다(collect_snapshot).
            upstream: Some("origin/main".into()),
            changes: vec![row("src/a.rs"), row("src/b.rs")],
            committed: vec![row("src/a.rs")],
            ..Default::default()
        }));
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut State| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                state.panel.render(ui, &catalog, "");
            },
            State { panel },
        );
        harness.run();

        harness
            .query_all_by_role_and_label(egui::accesskit::Role::Button, "src/b.rs")
            .next()
            .unwrap()
            .click();
        harness.run();
        assert_eq!(
            harness.state().panel.selected,
            Some((
                "src/b.rs".to_owned(),
                crate::ui::diff_viewer::DiffMode::Working
            ))
        );

        // 같은 경로라도 커밋 탭의 브랜치 커밋 행은 다른 선택이다.
        harness.state_mut().panel.tab = GitPanelTab::Commits;
        harness.run();
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "src/a.rs")
            .click();
        harness.run();
        assert_eq!(
            harness.state().panel.selected,
            Some((
                "src/a.rs".to_owned(),
                crate::ui::diff_viewer::DiffMode::Branch
            ))
        );
    }

    #[test]
    fn kittest_워크트리_행_클릭은_셸_열기를_올린다() {
        use egui_kittest::kittest::Queryable;

        struct State {
            panel: GitPanelUi,
            action: Option<GitPanelAction>,
        }

        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "main".into(),
            worktrees: vec![
                GitWorktreeRow {
                    path: "/repo".into(),
                    name: "repo".into(),
                    branch: Some("main".into()),
                    current: true,
                },
                GitWorktreeRow {
                    path: "/repo/wt".into(),
                    name: "wt".into(),
                    branch: Some("feat/x".into()),
                    current: false,
                },
            ],
            ..Default::default()
        }));
        panel.tab = GitPanelTab::Worktrees;

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut State| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                if let Some(action) = state.panel.render(ui, &catalog, "") {
                    state.action = Some(action);
                }
            },
            State {
                panel,
                action: None,
            },
        );
        harness.run();
        // 파일 행 테스트와 같은 질의 방식 — 접근성 이름(Role::Button + path)으로
        // 행 전체를 겨냥한다(자식 Label 클릭은 부모로 새지 않는다).
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "/repo/wt")
            .click();
        harness.run();

        assert_eq!(
            harness.state().action,
            Some(GitPanelAction::OpenWorktreeShell {
                path: "/repo/wt".to_owned()
            }),
            "워크트리 행 클릭은 OpenWorktreeShell을 내야 한다"
        );
    }

    #[test]
    fn kittest_워크트리가_하나면_탭을_숨긴다() {
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;

        // 정보가 0인 탭은 그리지 않는다(스펙 §8-4 — 예전 섹션 숨김과 같은 조건).
        let catalog = en();
        let one = GitPanelSnapshot {
            branch: "main".into(),
            worktrees: vec![worktree("/repo", true)],
            ..Default::default()
        };
        assert!(!worktree_section_visible(&one));
        let mut harness = panel_harness(one.clone(), "");
        harness.run();
        assert!(
            harness
                .query_by_role_and_label(
                    Role::Button,
                    &tab_label(&catalog, "git.section.worktrees", "1")
                )
                .is_none(),
            "워크트리가 메인 하나뿐이면 탭이 없다"
        );

        let mut two = one.clone();
        two.worktrees.push(GitWorktreeRow {
            path: "/repo/wt".into(),
            name: "wt".into(),
            branch: None,
            current: false,
        });
        assert!(worktree_section_visible(&two));
        harness.state_mut().panel.set_snapshot(Ok(two));
        harness.run();
        harness
            .get_by_role_and_label(
                Role::Button,
                &tab_label(&catalog, "git.section.worktrees", "2"),
            )
            .click();
        harness.run();
        assert_eq!(harness.state().panel.tab, GitPanelTab::Worktrees);

        // 새로고침으로 하나만 남으면 숨은 탭에 머물지 않고 변경 탭으로 돌아간다.
        harness.state_mut().panel.set_snapshot(Ok(one));
        harness.run();
        assert_eq!(harness.state().panel.tab, GitPanelTab::Changes);
    }

    #[test]
    fn 상한을_넘는_경로는_클릭_대상이_아니다() {
        let long = "/".repeat(WORKTREE_PATH_MAX_BYTES + 1);
        assert!(!worktree_path_is_spawnable(&long));
        assert!(!worktree_path_is_spawnable("/repo/\0bad"));
        assert!(worktree_path_is_spawnable("/repo/wt"));
    }

    // ── PR 5: 경로 동작(2026-10-11 계획 §3) ─────────────────────────────

    fn path_row(rel_path: &str, status: char) -> GitFileRow {
        GitFileRow {
            rel_path: rel_path.into(),
            status,
            adds: None,
            dels: None,
        }
    }

    #[test]
    fn 경로_대상은_repo_root와_rel_path로_절대_경로를_만든다() {
        let root = Path::new("/Users/me/내 저장소");
        let target = GitPathTarget::for_row(root, &path_row("src/한글 파일 (1).rs", 'M'));
        assert_eq!(
            target.abs_path,
            PathBuf::from("/Users/me/내 저장소/src/한글 파일 (1).rs")
        );
        assert!(!target.deleted);
        assert_eq!(
            target.copy_text(),
            "/Users/me/내 저장소/src/한글 파일 (1).rs"
        );
        // 폴더 없는 최상위 파일도 같은 규칙이다.
        let top = GitPathTarget::for_row(root, &path_row("Cargo.toml", '?'));
        assert_eq!(
            top.abs_path,
            PathBuf::from("/Users/me/내 저장소/Cargo.toml")
        );
    }

    #[test]
    fn 삭제된_행은_세션에_넣기와_finder를_끄고_복사만_남긴다() {
        let root = Path::new("/repo");
        let deleted = GitPathTarget::for_row(root, &path_row("gone.rs", 'D'));
        assert!(deleted.deleted);
        assert_eq!(deleted.insert_action(), None);
        assert_eq!(deleted.reveal_action(), None);
        assert_eq!(deleted.copy_text(), "/repo/gone.rs");

        let kept = GitPathTarget::for_row(root, &path_row("kept.rs", 'M'));
        assert_eq!(
            kept.insert_action(),
            Some(GitPanelAction::InsertPath {
                path: PathBuf::from("/repo/kept.rs")
            })
        );
        assert_eq!(
            kept.reveal_action(),
            Some(GitPanelAction::RevealInFinder {
                path: PathBuf::from("/repo/kept.rs")
            })
        );
    }

    #[test]
    fn 파일_트리_경로_검증을_못_넘으면_세션에_넣기와_finder를_끈다() {
        // FileTreePathPayload::try_new와 같은 검증 — 상한(32KiB) 초과·NUL 경로는
        // App 경계를 넘기지 않는다.
        for abs_path in [
            PathBuf::from(format!("/{}", "a".repeat(32 * 1024))),
            PathBuf::from("/repo/a\0b"),
        ] {
            let target = GitPathTarget {
                abs_path,
                deleted: false,
            };
            assert_eq!(target.insert_action(), None);
            assert_eq!(target.reveal_action(), None);
        }
    }

    /// 다른 repo의 스냅샷이 오면(세션 「변경 보기」로 repo 전환) 옛 repo 경로의 선택과
    /// 펼친 커밋을 비운다 — 남으면 다른 repo의 같은 경로 행이 선택으로 보였다(2026-10-11 리뷰).
    /// 같은 repo의 새로고침은 선택을 그대로 둔다.
    #[test]
    fn 다른_repo_스냅샷이_오면_선택과_펼친_커밋을_비운다() {
        use crate::ui::diff_viewer::DiffMode;

        let snapshot = |root: &str| GitPanelSnapshot {
            repo_root: PathBuf::from(root),
            branch: "main".into(),
            changes: vec![file_row("src/main.rs", 'M', 1, 0)],
            ..Default::default()
        };
        let sha = CommitSha::parse("a1b2c3d").unwrap();
        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(snapshot("/repo/a")));
        panel.selected = Some(("src/main.rs".into(), DiffMode::Working));
        panel.commit_list.toggle(sha);

        panel.set_snapshot(Ok(snapshot("/repo/a")));
        assert!(
            panel.selected.is_some(),
            "같은 repo 새로고침이 선택을 지웠다"
        );
        assert_eq!(panel.repo_root(), Some(Path::new("/repo/a")));

        panel.set_snapshot(Ok(snapshot("/repo/b")));
        assert_eq!(
            panel.selected, None,
            "다른 repo로 바뀌었는데 옛 선택이 남았다"
        );
        assert_eq!(panel.commit_list.expanded, None);
        assert_eq!(panel.repo_root(), Some(Path::new("/repo/b")));
    }

    #[test]
    fn path_target은_모드_섹션의_행으로_대상을_만든다() {
        use crate::ui::diff_viewer::DiffMode;

        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            repo_root: PathBuf::from("/repo"),
            changes: vec![path_row("a.rs", 'D')],
            committed: vec![path_row("a.rs", 'M')],
            ..Default::default()
        }));
        // 같은 경로라도 모드마다 다른 섹션의 행이다 — 워킹트리에선 지웠고 브랜치에선 고쳤다.
        assert_eq!(
            panel.path_target("a.rs", DiffMode::Working),
            Some(GitPathTarget {
                abs_path: PathBuf::from("/repo/a.rs"),
                deleted: true,
            })
        );
        assert_eq!(
            panel.path_target("a.rs", DiffMode::Branch),
            Some(GitPathTarget {
                abs_path: PathBuf::from("/repo/a.rs"),
                deleted: false,
            })
        );
        assert_eq!(panel.path_target("missing.rs", DiffMode::Working), None);
    }

    #[test]
    fn path_target은_펼친_커밋의_파일로도_대상을_만든다() {
        use crate::ui::diff_viewer::DiffMode;

        let sha = CommitSha::parse("a1b2c3d").unwrap();
        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            repo_root: PathBuf::from("/repo"),
            ..Default::default()
        }));
        // 불러오기 전에는 대상이 없다.
        assert_eq!(panel.path_target("b.rs", DiffMode::Commit(sha)), None);
        panel.commit_list.toggle(sha);
        panel.set_commit_files(
            sha,
            Ok(GitCommitFiles {
                rows: vec![path_row("b.rs", 'M')],
                truncated: false,
            }),
        );
        assert_eq!(
            panel.path_target("b.rs", DiffMode::Commit(sha)),
            Some(GitPathTarget {
                abs_path: PathBuf::from("/repo/b.rs"),
                deleted: false,
            })
        );
        let other = CommitSha::parse("ffff000").unwrap();
        assert_eq!(panel.path_target("b.rs", DiffMode::Commit(other)), None);
    }

    struct PathMenuState {
        panel: GitPanelUi,
        actions: Vec<GitPanelAction>,
        copied: Vec<String>,
    }

    fn path_menu_harness(rows: Vec<GitFileRow>) -> egui_kittest::Harness<'static, PathMenuState> {
        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            repo_root: PathBuf::from("/Users/me/my repo"),
            branch: "main".into(),
            changes: rows,
            ..Default::default()
        }));
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut PathMenuState| {
                let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
                if let Some(action) = state.panel.render(ui, &catalog, "") {
                    state.actions.push(action);
                }
                // work_history 카드 복사 테스트와 같은 관례 — 이 프레임의 복사 명령을 모은다.
                ui.ctx().output(|output| {
                    for command in &output.commands {
                        if let egui::OutputCommand::CopyText(text) = command {
                            state.copied.push(text.clone());
                        }
                    }
                });
            },
            PathMenuState {
                panel,
                actions: Vec::new(),
                copied: Vec::new(),
            },
        );
        // 생성 프레임의 첫 파일 자동 선택(2026-10-11 PR 3)은 메뉴 동작과 무관하다.
        harness.state_mut().actions.clear();
        harness
    }

    fn open_row_menu(harness: &mut egui_kittest::Harness<'static, PathMenuState>, rel_path: &str) {
        use egui_kittest::kittest::Queryable;
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, rel_path)
            .click_secondary();
        harness.run();
    }

    #[test]
    fn kittest_행_우클릭_메뉴는_경로_복사_세션에_넣기_finder를_낸다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let copy = catalog.t("git.path.copy", &[]);
        let mut harness = path_menu_harness(vec![path_row("src/한글 파일.rs", 'M')]);
        harness.run();

        // 경로 복사 — App intent 없이 leaf가 절대 경로를 클립보드에 넣고 메뉴를 닫는다.
        open_row_menu(&mut harness, "src/한글 파일.rs");
        harness.get_by_label(&copy).click();
        harness.run();
        assert_eq!(
            harness.state().copied,
            vec!["/Users/me/my repo/src/한글 파일.rs".to_owned()]
        );
        assert!(harness.state().actions.is_empty());
        assert!(
            harness.query_by_label(&copy).is_none(),
            "메뉴가 닫혀야 한다"
        );

        open_row_menu(&mut harness, "src/한글 파일.rs");
        harness
            .get_by_label(&catalog.t("git.path.insert", &[]))
            .click();
        harness.run();
        open_row_menu(&mut harness, "src/한글 파일.rs");
        harness
            .get_by_label(&catalog.t("git.path.reveal", &[]))
            .click();
        harness.run();

        let path = PathBuf::from("/Users/me/my repo/src/한글 파일.rs");
        assert_eq!(
            harness.state().actions,
            vec![
                GitPanelAction::InsertPath { path: path.clone() },
                GitPanelAction::RevealInFinder { path },
            ]
        );
    }

    #[test]
    fn kittest_삭제된_행의_메뉴는_복사만_동작한다() {
        use egui_kittest::kittest::{NodeT, Queryable};

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut harness = path_menu_harness(vec![path_row("gone.rs", 'D')]);
        harness.run();

        for key in ["git.path.insert", "git.path.reveal"] {
            // egui 메뉴는 안쪽 클릭(꺼진 항목 포함)에도 닫히므로 항목마다 다시 연다.
            open_row_menu(&mut harness, "gone.rs");
            let label = catalog.t(key, &[]);
            let item = harness.get_by_label(&label);
            assert!(
                item.accesskit_node().is_disabled(),
                "{key}는 삭제된 파일에서 꺼져 있어야 한다"
            );
            item.click();
            harness.run();
        }
        assert!(harness.state().actions.is_empty());

        open_row_menu(&mut harness, "gone.rs");
        harness
            .get_by_label(&catalog.t("git.path.copy", &[]))
            .click();
        harness.run();
        assert_eq!(
            harness.state().copied,
            vec!["/Users/me/my repo/gone.rs".to_owned()]
        );
    }

    // ── Task 2: 좌측 목록 필터(2026-08-18 계획) ─────────────────────────

    fn filter_test_rows() -> Vec<GitFileRow> {
        vec![
            GitFileRow {
                rel_path: "src/app.rs".into(),
                status: 'M',
                adds: Some(1),
                dels: None,
            },
            GitFileRow {
                rel_path: "README.md".into(),
                status: 'M',
                adds: None,
                dels: Some(2),
            },
            GitFileRow {
                rel_path: "src/ui/git_panel.rs".into(),
                status: 'A',
                adds: Some(5),
                dels: None,
            },
        ]
    }

    #[test]
    fn filter_file_rows는_경로에_대소문자_무시로_걸리는_행만_남긴다() {
        let rows = filter_test_rows();
        let filtered = filter_file_rows(&rows, "SRC");
        let paths: Vec<&str> = filtered.iter().map(|r| r.rel_path.as_str()).collect();
        assert_eq!(paths, vec!["src/app.rs", "src/ui/git_panel.rs"]);
    }

    #[test]
    fn filter_file_rows는_빈_필터면_모든_행을_순서_그대로_남긴다() {
        let rows = filter_test_rows();
        let filtered = filter_file_rows(&rows, "");
        assert_eq!(filtered.len(), rows.len());
        assert!(filtered.iter().zip(rows.iter()).all(|(a, b)| *a == b));
    }

    #[test]
    fn filter_file_rows는_걸리는_행이_없으면_빈_목록이다() {
        let rows = filter_test_rows();
        assert!(filter_file_rows(&rows, "zzz-no-match").is_empty());
    }

    // 예전 섹션 머리의 「∨ Changes 1」 개수가 이제 변경 탭 버튼의 개수다 — 거른 뒤
    // 기준이라는 의미는 그대로다(2026-10-11 PR 2).
    #[test]
    fn kittest_필터는_변경_탭_행과_탭_개수를_거른_뒤_기준으로_보여준다() {
        use egui_kittest::kittest::Queryable;

        let catalog = en();
        let mut harness = panel_harness(
            GitPanelSnapshot {
                branch: "main".into(),
                changes: vec![
                    GitFileRow {
                        rel_path: "src/app.rs".into(),
                        status: 'M',
                        adds: Some(1),
                        dels: None,
                    },
                    GitFileRow {
                        rel_path: "README.md".into(),
                        status: 'M',
                        adds: None,
                        dels: Some(2),
                    },
                ],
                ..Default::default()
            },
            "readme",
        );
        harness.run();

        harness.get_by_label("README.md");
        assert!(
            harness.query_by_label("src/app.rs").is_none(),
            "필터에 걸리지 않는 행은 보이면 안 된다"
        );
        // 탭 개수는 거른 뒤(1) 기준이어야 한다 — 원래 2행 중 1행만 남았다.
        harness.get_by_role_and_label(
            egui::accesskit::Role::Button,
            &tab_label(&catalog, "git.tab.changes", "1"),
        );
    }

    #[test]
    fn kittest_모두_보기_버튼은_거른_뒤_행_수_기준으로_나타난다() {
        use egui_kittest::kittest::Queryable;

        let mut rows: Vec<GitFileRow> = (0..(SECTION_COLLAPSED_ROWS + 1))
            .map(|i| GitFileRow {
                rel_path: format!("keep{i}.rs"),
                status: 'M',
                adds: Some(1),
                dels: None,
            })
            .collect();
        // 필터에 걸리는 행을 하나만 남겨 둔다 — 거르기 전(11) 기준이면 여전히
        // 「모두 보기」가 보일 상한(10)을 넘지만, 거른 뒤(1) 기준이면 안 보여야 한다.
        rows[0].rel_path = "only-match.rs".into();

        let mut harness = panel_harness(
            GitPanelSnapshot {
                branch: "main".into(),
                changes: rows,
                ..Default::default()
            },
            "only-match",
        );
        harness.run();

        harness.get_by_label("only-match.rs");
        assert!(
            harness.query_by_label("Show all").is_none(),
            "거른 뒤 1행뿐이면 「모두 보기」가 보이면 안 된다"
        );

        // 필터를 풀면 11행이라 상한(10)까지만 보이고, 합계 줄의 「모두 보기」가 나머지를 편다.
        harness.state_mut().filter = "";
        harness.run();
        let last = format!("keep{SECTION_COLLAPSED_ROWS}.rs");
        assert!(harness.query_by_label(&last).is_none());
        harness.get_by_label("Show all").click();
        harness.run();
        harness.get_by_label(&last);
    }

    // 예전엔 「두 섹션 모두」 0행일 때 패널 전체에 안내 한 줄이었다. 탭으로 나뉜 뒤에는
    // 지금 탭의 파일 행이 모두 걸러지면 그 탭에 안내 한 줄이 보인다(2026-10-11 PR 2).
    #[test]
    fn kittest_필터가_탭의_파일을_모두_걸러내면_안내_한_줄이_보인다() {
        use egui_kittest::kittest::Queryable;

        let catalog = en();
        let sha = CommitSha::parse("a1b2c3d").unwrap();
        let mut harness = panel_harness(
            GitPanelSnapshot {
                branch: "feat".into(),
                upstream: Some("origin/main".into()),
                changes: vec![GitFileRow {
                    rel_path: "src/a.rs".into(),
                    status: 'M',
                    adds: Some(1),
                    dels: None,
                }],
                committed: vec![file_row("src/b.rs", 'M', 1, 0)],
                commits: vec![GitCommitRow {
                    sha,
                    subject: "커밋".into(),
                    time: 0,
                }],
                ..Default::default()
            },
            "zzz-no-match",
        );
        harness.run();

        harness.get_by_label(&catalog.t("search.no_match", &[]));
        assert!(
            harness.query_by_label("src/a.rs").is_none(),
            "걸러진 행은 보이면 안 된다"
        );

        harness.state_mut().panel.tab = GitPanelTab::Commits;
        harness.run();
        harness.get_by_label(&catalog.t("search.no_match", &[]));
        assert!(
            harness.query_by_label("src/b.rs").is_none(),
            "커밋 탭의 브랜치 커밋 파일도 필터 대상이다"
        );
        // 커밋 목록은 파일 경로 필터 대상이 아니다.
        harness.get_by_role_and_label(egui::accesskit::Role::Button, "a1b2c3d 커밋");
    }

    #[test]
    fn kittest_필터는_워크트리_행을_거르지_않는다() {
        use egui_kittest::kittest::Queryable;

        let catalog = en();
        let mut harness = panel_harness(
            GitPanelSnapshot {
                branch: "main".into(),
                changes: vec![GitFileRow {
                    rel_path: "src/a.rs".into(),
                    status: 'M',
                    adds: Some(1),
                    dels: None,
                }],
                worktrees: vec![worktree("/repo", true), worktree("/repo/wt", false)],
                ..Default::default()
            },
            "zzz-no-match",
        );
        harness.run();

        // 필터가 걸려도 워크트리 탭은 필터하지 않는다(2026-08-18 스펙 §좌측 목록
        // 필터) — 경로 목록이라 검색 대상이 아니고, 통째로 사라지면 혼란스럽다.
        // 탭 개수도 거르지 않은 2다.
        harness
            .get_by_role_and_label(
                egui::accesskit::Role::Button,
                &tab_label(&catalog, "git.section.worktrees", "2"),
            )
            .click();
        harness.run();
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "/repo/wt")
            .click();
        harness.run();
        assert_eq!(
            harness.state().action,
            Some(GitPanelAction::OpenWorktreeShell {
                path: "/repo/wt".to_owned()
            })
        );
    }

    // ── 브랜치 카드와 탭(2026-10-11 PR 2) ─────────────────────────────

    fn en() -> i18n::Catalog {
        i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap()
    }

    /// 탭·카드 kittest 공용 상태 — 필터를 바꿔 가며 같은 패널을 다시 그릴 수 있다.
    struct PanelState {
        panel: GitPanelUi,
        filter: &'static str,
        action: Option<GitPanelAction>,
    }

    fn panel_harness(
        snapshot: GitPanelSnapshot,
        filter: &'static str,
    ) -> egui_kittest::Harness<'static, PanelState> {
        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(snapshot));
        egui_kittest::Harness::new_ui_state(
            |ui, state: &mut PanelState| {
                let catalog = en();
                if let Some(action) = state.panel.render(ui, &catalog, state.filter) {
                    state.action = Some(action);
                }
            },
            PanelState {
                panel,
                filter,
                action: None,
            },
        )
    }

    fn worktree(path: &str, current: bool) -> GitWorktreeRow {
        GitWorktreeRow {
            path: path.into(),
            name: path.rsplit('/').next().unwrap_or(path).into(),
            branch: Some("main".into()),
            current,
        }
    }

    /// 탭 버튼의 접근성 이름 — 「이름 개수」.
    fn tab_label(catalog: &i18n::Catalog, key: &str, count: &str) -> String {
        format!("{} {count}", catalog.t(key, &[]))
    }

    #[test]
    fn kittest_탭을_바꾸면_그_탭의_행만_보인다() {
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;

        let catalog = en();
        let sha = CommitSha::parse("a1b2c3d").unwrap();
        let mut harness = panel_harness(
            GitPanelSnapshot {
                branch: "feat".into(),
                upstream: Some("origin/main".into()),
                changes: vec![file_row("src/working.rs", 'M', 1, 0)],
                committed: vec![file_row("src/branch.rs", 'M', 2, 0)],
                commits: vec![GitCommitRow {
                    sha,
                    subject: "첫 커밋".into(),
                    time: 0,
                }],
                worktrees: vec![worktree("/repo", true), worktree("/repo/wt", false)],
                ..Default::default()
            },
            "",
        );
        harness.run();

        // 기본은 변경 탭 — 작업 중 변경 파일만 보인다.
        assert_eq!(harness.state().panel.tab, GitPanelTab::Changes);
        harness.get_by_role_and_label(Role::Button, "src/working.rs");
        assert!(
            harness
                .query_by_role_and_label(Role::Button, "src/branch.rs")
                .is_none()
        );
        assert!(
            harness
                .query_by_role_and_label(Role::Button, "a1b2c3d 첫 커밋")
                .is_none()
        );
        assert!(
            harness
                .query_by_role_and_label(Role::Button, "/repo/wt")
                .is_none()
        );

        // 커밋 탭 — 소제목(브랜치 커밋 · 기준) 아래 브랜치 커밋 파일과 커밋 목록.
        harness
            .get_by_role_and_label(Role::Button, &tab_label(&catalog, "git.tab.commits", "1"))
            .click();
        harness.run();
        assert_eq!(harness.state().panel.tab, GitPanelTab::Commits);
        harness.get_by_label(&catalog.t("git.section.committed", &[]));
        harness.get_by_label(&catalog.t("git.commits.since", &[("base", "origin/main")]));
        harness.get_by_role_and_label(Role::Button, "src/branch.rs");
        harness.get_by_role_and_label(Role::Button, "a1b2c3d 첫 커밋");
        assert!(
            harness
                .query_by_role_and_label(Role::Button, "src/working.rs")
                .is_none()
        );

        // 워크트리 탭.
        harness
            .get_by_role_and_label(
                Role::Button,
                &tab_label(&catalog, "git.section.worktrees", "2"),
            )
            .click();
        harness.run();
        assert_eq!(harness.state().panel.tab, GitPanelTab::Worktrees);
        harness.get_by_role_and_label(Role::Button, "/repo/wt");
        assert!(
            harness
                .query_by_role_and_label(Role::Button, "src/branch.rs")
                .is_none()
        );
        assert!(
            harness
                .query_by_role_and_label(Role::Button, "a1b2c3d 첫 커밋")
                .is_none()
        );

        // 변경 탭으로 돌아온다.
        harness
            .get_by_role_and_label(Role::Button, &tab_label(&catalog, "git.tab.changes", "1"))
            .click();
        harness.run();
        assert_eq!(harness.state().panel.tab, GitPanelTab::Changes);
        harness.get_by_role_and_label(Role::Button, "src/working.rs");
    }

    /// 추적 브랜치가 없어 `origin/HEAD`로 폴백한 기준은 「보낼/받을 커밋」이 아니다 — push·pull로
    /// 맞춰지지 않는다(2026-10-11 리뷰). 업스트림 없음 경고와 「기준 대비」 중립 배지를 보인다.
    #[test]
    fn kittest_폴백_기준은_보낼_받을_대신_기준_대비로_보인다() {
        use egui_kittest::kittest::Queryable;

        let catalog = en();
        let mut harness = panel_harness(
            GitPanelSnapshot {
                branch: "feat/local".into(),
                upstream: Some("origin/main".into()),
                upstream_fallback: true,
                ahead: 2,
                behind: 1,
                ..Default::default()
            },
            "",
        );
        harness.run();
        harness.get_by_label(&catalog.t("git.upstream_none", &[]));
        harness.get_by_label(&catalog.t("git.branch.ahead_base", &[("count", "2")]));
        harness.get_by_label(&catalog.t("git.branch.behind_base", &[("count", "1")]));
        assert!(
            harness
                .query_by_label(&catalog.t("git.branch.ahead", &[("count", "2")]))
                .is_none(),
            "폴백 기준에 「보낼 커밋」을 보였다"
        );
    }

    #[test]
    fn kittest_브랜치_카드는_보낼_받을_커밋과_업스트림_상태를_배지로_보인다() {
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;

        let catalog = en();
        let ahead = |count: &str| catalog.t("git.branch.ahead", &[("count", count)]);
        let behind = |count: &str| catalog.t("git.branch.behind", &[("count", count)]);
        let in_sync = catalog.t("git.branch.in_sync", &[]);
        let no_upstream = catalog.t("git.upstream_none", &[]);
        let snap = |upstream: Option<&str>, ahead: u32, behind: u32| GitPanelSnapshot {
            branch: "feat/card".into(),
            upstream: upstream.map(Into::into),
            ahead,
            behind,
            remote_https_base: Some("https://github.com/o/r".into()),
            ..Default::default()
        };

        // 보낼·받을 커밋이 둘 다 있으면 배지 둘 — 「원격과 같음」은 없다.
        let mut harness = panel_harness(snap(Some("origin/feat/card"), 3, 1), "");
        harness.run();
        harness.get_by_label("feat/card");
        harness.get_by_label("origin/feat/card");
        harness.get_by_label(&ahead("3"));
        harness.get_by_label(&behind("1"));
        assert!(harness.query_by_label(&in_sync).is_none());
        assert!(harness.query_by_label(&no_upstream).is_none());
        // GitHub remote에 upstream이 있으면 ↗가 보인다(기존 규칙).
        harness.get_by_role_and_label(Role::Button, "↗");
        harness.get_by_role_and_label(Role::Button, "⟳");

        // 받을 커밋만 있으면 그 배지만.
        let mut harness = panel_harness(snap(Some("origin/feat/card"), 0, 2), "");
        harness.run();
        harness.get_by_label(&behind("2"));
        assert!(harness.query_by_label(&ahead("0")).is_none());
        assert!(harness.query_by_label(&in_sync).is_none());

        // 둘 다 0이면 중립 배지 하나.
        let mut harness = panel_harness(snap(Some("origin/feat/card"), 0, 0), "");
        harness.run();
        harness.get_by_label(&in_sync);
        assert!(harness.query_by_label(&ahead("0")).is_none());
        assert!(harness.query_by_label(&behind("0")).is_none());

        // 업스트림 없음 — 경고 배지와 hover 이유. 보낼/받을/같음 배지와 ↗는 없다.
        let mut harness = panel_harness(snap(None, 0, 0), "");
        harness.run();
        assert!(harness.query_by_label(&in_sync).is_none());
        assert!(harness.query_by_role_and_label(Role::Button, "↗").is_none());
        harness.get_by_role_and_label(Role::Button, "⟳");
        harness.get_by_label(&no_upstream).hover();
        harness.run();
        harness.get_by_label(&catalog.t("git.branch.no_upstream_hint", &[]));
    }

    #[test]
    fn kittest_깨끗하면_카드를_보이고_보낼_커밋_링크는_커밋_탭으로_간다() {
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;

        let catalog = en();
        let view_ahead = |count: &str| catalog.t("git.clean.view_ahead", &[("count", count)]);
        let mut harness = panel_harness(
            GitPanelSnapshot {
                branch: "feat".into(),
                upstream: Some("origin/feat".into()),
                ahead: 2,
                committed: vec![file_row("src/branch.rs", 'M', 1, 0)],
                ..Default::default()
            },
            "",
        );
        harness.run();
        harness.get_by_label(&catalog.t("diff.clean", &[]));
        assert!(
            harness
                .query_by_label(&catalog.t("git.files.total", &[("count", "0")]))
                .is_none(),
            "파일이 없으면 합계 줄도 없다"
        );
        harness.get_by_label(&view_ahead("2")).click();
        harness.run();
        assert_eq!(harness.state().panel.tab, GitPanelTab::Commits);
        harness.get_by_role_and_label(Role::Button, "src/branch.rs");

        // 보낼 커밋이 없으면 링크도 없다.
        let mut harness = panel_harness(
            GitPanelSnapshot {
                branch: "main".into(),
                upstream: Some("origin/main".into()),
                ..Default::default()
            },
            "",
        );
        harness.run();
        harness.get_by_label(&catalog.t("diff.clean", &[]));
        assert!(harness.query_by_label(&view_ahead("0")).is_none());
    }

    #[test]
    fn kittest_512_상한에_잘린_목록은_배너와_개수_뒤_더하기로_알린다() {
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;

        let catalog = en();
        let truncated = catalog.t(
            "git.list.truncated",
            &[("count", &MAX_PANEL_FILES.to_string())],
        );
        let snap = GitPanelSnapshot {
            branch: "main".into(),
            upstream: Some("origin/main".into()),
            changes: vec![file_row("a.rs", 'M', 1, 0)],
            changes_truncated: true,
            committed: vec![file_row("b.rs", 'M', 1, 0)],
            ..Default::default()
        };
        let mut harness = panel_harness(snap.clone(), "");
        harness.run();
        harness.get_by_label(&truncated);
        // 버린 행이 있으니 개수는 하한이다.
        harness.get_by_role_and_label(Role::Button, &tab_label(&catalog, "git.tab.changes", "1+"));

        // 커밋 탭의 파일은 잘리지 않았다 — 배너가 없다.
        harness.state_mut().panel.tab = GitPanelTab::Commits;
        harness.run();
        assert!(harness.query_by_label(&truncated).is_none());

        harness.state_mut().panel.set_snapshot(Ok(GitPanelSnapshot {
            committed_truncated: true,
            ..snap
        }));
        harness.run();
        harness.get_by_label(&truncated);
    }

    #[test]
    fn kittest_합계_줄은_거른_뒤_파일_수와_증감을_더한다() {
        use egui_kittest::kittest::Queryable;

        let catalog = en();
        let mut harness = panel_harness(
            GitPanelSnapshot {
                branch: "main".into(),
                changes: vec![
                    file_row("src/a.rs", 'M', 3, 1),
                    file_row("src/b.rs", 'M', 4, 2),
                    file_row("docs/c.md", 'M', 100, 100),
                ],
                ..Default::default()
            },
            "src/",
        );
        harness.run();
        harness.get_by_label(&catalog.t("git.files.total", &[("count", "2")]));
        harness.get_by_label("+7");
        harness.get_by_label("−3");
    }

    // ── PR 3: 탐색 효율(2026-10-11 계획 §3) ─────────────────────────────────

    /// 목록 줄을 읽기 쉬운 글자로 — 폴더 머리는 `[폴더 개수]`(접힘이면 ` 접힘`), 파일은 경로.
    fn describe(items: &[ListItem<'_>]) -> Vec<String> {
        items
            .iter()
            .map(|item| match item {
                ListItem::Folder {
                    dir,
                    count,
                    collapsed,
                } => format!("[{dir} {count}{}]", if *collapsed { " 접힘" } else { "" }),
                ListItem::File(row) => row.rel_path.clone(),
            })
            .collect()
    }

    #[test]
    fn 폴더별_묶기는_처음_나온_폴더_순서와_폴더_안_파일_순서를_지킨다() {
        let rows = [
            file_row("src/ui/a.rs", 'M', 1, 0),
            file_row("README.md", 'M', 1, 0),
            file_row("src/b.rs", 'M', 1, 0),
            file_row("src/ui/c.rs", 'M', 1, 0),
        ];
        let refs: Vec<&GitFileRow> = rows.iter().collect();
        let groups = group_by_folder(&refs);
        let groups: Vec<(&str, Vec<&str>)> = groups
            .iter()
            .map(|(dir, files)| {
                (
                    *dir,
                    files.iter().map(|row| row.rel_path.as_str()).collect(),
                )
            })
            .collect();
        // 루트 파일은 빈 폴더("")에 묶인다.
        assert_eq!(
            groups,
            vec![
                ("src/ui", vec!["src/ui/a.rs", "src/ui/c.rs"]),
                ("", vec!["README.md"]),
                ("src", vec!["src/b.rs"]),
            ]
        );
    }

    #[test]
    fn 폴더별_목록은_접은_폴더의_파일을_숨기고_표시_상한은_파일_행만_센다() {
        // a 폴더 6개 + b 폴더 6개 = 12개. 상한(10)은 파일 행만 센다 — 머리 행은 세지 않는다.
        let rows: Vec<GitFileRow> = (0..6)
            .map(|i| file_row(&format!("a/{i}.rs"), 'M', 1, 0))
            .chain((0..6).map(|i| file_row(&format!("b/{i}.rs"), 'M', 1, 0)))
            .collect();
        let refs: Vec<&GitFileRow> = rows.iter().collect();
        let files = |dir: &str, range: std::ops::Range<usize>| -> Vec<String> {
            range.map(|i| format!("{dir}/{i}.rs")).collect()
        };
        let expected = |parts: Vec<Vec<String>>| -> Vec<String> { parts.concat() };
        let none = std::collections::HashSet::new();

        // 목록 보기 — 예전과 같이 앞 10개, 머리 행 없음.
        assert_eq!(
            describe(&list_items(&refs, GitListLayout::Flat, false, &none)),
            expected(vec![files("a", 0..6), files("b", 0..4)])
        );
        // 폴더별 — 같은 10개 파일(a 6 + b 4)이 머리 아래에 묶인다. 머리의 개수는 그
        // 폴더의(거른 뒤) 전체 파일 수다.
        assert_eq!(
            describe(&list_items(&refs, GitListLayout::Folders, false, &none)),
            expected(vec![
                vec!["[a 6]".into()],
                files("a", 0..6),
                vec!["[b 6]".into()],
                files("b", 0..4),
            ])
        );
        // a를 접으면 a의 파일 행이 사라지고 그 자리를 다음 폴더가 쓴다 — 접은 파일이 상한을
        // 차지하면 접어도 다른 폴더가 보이지 않았다(2026-10-11 리뷰).
        let collapsed: std::collections::HashSet<String> = ["a".to_owned()].into();
        assert_eq!(
            describe(&list_items(
                &refs,
                GitListLayout::Folders,
                false,
                &collapsed
            )),
            expected(vec![
                vec!["[a 6 접힘]".into(), "[b 6]".into()],
                files("b", 0..6),
            ])
        );
        // 「모두 보기」면 전부. 접힘은 그대로.
        assert_eq!(
            describe(&list_items(&refs, GitListLayout::Folders, true, &collapsed)),
            expected(vec![
                vec!["[a 6 접힘]".into(), "[b 6]".into()],
                files("b", 0..6),
            ])
        );
        // 목록 보기는 접힘을 모른다.
        assert_eq!(
            list_items(&refs, GitListLayout::Flat, false, &collapsed).len(),
            SECTION_COLLAPSED_ROWS
        );
        // 상한 안에 파일이 하나도 들지 않는 폴더는 머리째 빠진다 — a 6 + b 4가 정확히 10이면
        // 그 뒤 c는 머리도 없다.
        let c = file_row("c/0.rs", 'M', 1, 0);
        let mut exact: Vec<&GitFileRow> = refs[..10].to_vec();
        exact.push(&c);
        assert_eq!(
            describe(&list_items(&exact, GitListLayout::Folders, false, &none)),
            expected(vec![
                vec!["[a 6]".into()],
                files("a", 0..6),
                vec!["[b 4]".into()],
                files("b", 0..4),
            ])
        );
    }

    #[test]
    fn 선택_이동은_표시_순서를_따르고_양_끝에서_멈춘다() {
        use crate::ui::diff_viewer::DiffMode::{Branch, Working};

        let nav = [("a.rs", Branch), ("b.rs", Branch), ("a.rs", Working)];
        let sel = |path: &str, mode| Some((path.to_owned(), mode));
        // 선택이 없거나 이 목록 밖이면(접은 폴더·다른 탭) 방향과 상관없이 첫 행.
        assert_eq!(step_selection(&nav, None, 1), Some(0));
        assert_eq!(step_selection(&nav, None, -1), Some(0));
        assert_eq!(
            step_selection(&nav, sel("zzz.rs", Working).as_ref(), 1),
            Some(0)
        );
        // 같은 경로라도 모드가 다르면 다른 행이다.
        assert_eq!(
            step_selection(&nav, sel("a.rs", Branch).as_ref(), 1),
            Some(1)
        );
        assert_eq!(
            step_selection(&nav, sel("b.rs", Branch).as_ref(), 1),
            Some(2)
        );
        assert_eq!(
            step_selection(&nav, sel("a.rs", Working).as_ref(), -5),
            Some(0)
        );
        // 끝에서는 멈춘다 — 같은 파일을 다시 열지 않는다(None).
        assert_eq!(step_selection(&nav, sel("a.rs", Working).as_ref(), 1), None);
        assert_eq!(step_selection(&nav, sel("a.rs", Branch).as_ref(), -1), None);
        assert_eq!(step_selection(&nav, None, 0), None);
        assert_eq!(step_selection(&[], None, 1), None);
    }

    #[test]
    fn git_본문_키는_보조키나_ime_조합이_있으면_받지_않는다() {
        use egui::Key::{ArrowDown, ArrowUp, N};

        let ctx = egui::Context::default();
        let key = |key, modifiers| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let step = |events: Vec<egui::Event>| {
            let mut out = None;
            ctx.run_ui(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ui| out = git_key_step(ui.ctx(), ArrowUp, ArrowDown),
            )
            .drop_without_applying_deltas();
            out
        };
        let none = egui::Modifiers::NONE;
        assert_eq!(step(vec![]), None);
        assert_eq!(step(vec![key(ArrowDown, none)]), Some(1));
        assert_eq!(step(vec![key(ArrowUp, none)]), Some(-1));
        // 한 프레임에 여러 번 눌렸으면 모두 센다.
        assert_eq!(
            step(vec![key(ArrowDown, none), key(ArrowDown, none)]),
            Some(2)
        );
        // 다른 키는 세지 않는다.
        assert_eq!(step(vec![key(N, none)]), None);
        // ⌘·Ctrl·Alt와 함께 누른 것은 다른 단축키다.
        for modifiers in [
            egui::Modifiers::COMMAND,
            egui::Modifiers::CTRL,
            egui::Modifiers::ALT,
        ] {
            assert_eq!(step(vec![key(ArrowDown, modifiers)]), None);
        }
        // IME 조합 중이면 같은 프레임의 키는 조합 입력이다.
        let preedit = egui::Event::Ime(egui::ImeEvent::Preedit {
            text: "ㅜ".to_owned(),
            active_range_chars: None,
        });
        assert_eq!(step(vec![preedit, key(ArrowDown, none)]), None);
    }

    /// PR 3 키·자동 선택 kittest 공용 상태 — 낸 액션을 모두 모으고, 패널을 그린 뒤 ↓
    /// 눌림이 입력에 남았는지(= 패널이 소비하지 않았는지)를 누적한다. `search_focused`면
    /// 패널 앞에 보조 검색창 같은 TextEdit을 그리고 포커스를 준다.
    struct NavState {
        panel: GitPanelUi,
        filter: &'static str,
        search: String,
        search_focused: bool,
        actions: Vec<GitPanelAction>,
        down_left: bool,
    }

    fn nav_harness(snapshot: GitPanelSnapshot) -> egui_kittest::Harness<'static, NavState> {
        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(snapshot));
        egui_kittest::Harness::new_ui_state(
            |ui, state: &mut NavState| {
                let catalog = en();
                if state.search_focused {
                    ui.text_edit_singleline(&mut state.search).request_focus();
                }
                if let Some(action) = state.panel.render(ui, &catalog, state.filter) {
                    state.actions.push(action);
                }
                state.down_left |= ui.input(|input| input.key_pressed(egui::Key::ArrowDown));
            },
            NavState {
                panel,
                filter: "",
                search: String::new(),
                search_focused: false,
                actions: Vec::new(),
                down_left: false,
            },
        )
    }

    /// 교차 워크스페이스에서 붙인 터미널이 키를 쥐고 있으면 Git 목록은 ↑↓를 건드리지 않는다
    /// — 셸 history의 ↑가 Git 선택을 옮기고 diff를 요청했다(2026-10-11 리뷰).
    #[test]
    fn kittest_다른_터미널이_키를_쥐면_화살표를_건드리지_않는다() {
        let mut harness = nav_harness(GitPanelSnapshot {
            branch: "main".into(),
            changes: vec![file_row("a.rs", 'M', 1, 0), file_row("b.rs", 'M', 1, 0)],
            ..Default::default()
        });
        harness.run();
        harness.state_mut().actions.clear();
        harness.state_mut().down_left = false;
        harness.state_mut().panel.set_keys_blocked(true);
        press(&mut harness, egui::Key::ArrowDown);
        assert!(
            harness.state().actions.is_empty(),
            "막힌 키로 선택을 옮겼다"
        );
        assert!(harness.state().down_left, "막혔으면 키를 소비하지 않는다");
    }

    /// 고른 파일이 커밋 등으로 목록에서 사라지면 선택을 비운다 — 남으면 오른쪽에 사라진
    /// 파일의 diff가 남고 다음 변경도 자동으로 열리지 않았다(2026-10-11 리뷰).
    #[test]
    fn kittest_고른_파일이_사라지면_선택을_비우고_다음_파일을_연다() {
        use crate::ui::diff_viewer::DiffMode;

        let snapshot = |paths: &[&str]| GitPanelSnapshot {
            branch: "main".into(),
            changes: paths.iter().map(|path| file_row(path, 'M', 1, 0)).collect(),
            ..Default::default()
        };
        let mut harness = nav_harness(snapshot(&["a.rs", "b.rs"]));
        harness.run();
        assert_eq!(
            harness.state().panel.selected,
            Some(("a.rs".into(), DiffMode::Working))
        );
        harness.state_mut().actions.clear();

        harness
            .state_mut()
            .panel
            .set_snapshot(Ok(snapshot(&["b.rs"])));
        assert!(
            !harness.state().panel.has_selection(),
            "사라진 파일이 선택으로 남았다"
        );
        harness.run();
        assert_eq!(
            harness.state().actions,
            vec![show("b.rs", DiffMode::Working)]
        );
    }

    /// 새 스냅샷을 요청하면(⟳·탭 재활성) 기다리던 자동 선택을 거둔다 — 낡은 스냅샷 기준으로
    /// 고르면 그 파일 diff 요청이 새 스냅샷과 겹쳤다(2026-10-11 리뷰).
    #[test]
    fn 새_스냅샷을_요청하면_기다리던_자동_선택을_거둔다() {
        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot::default()));
        assert!(panel.auto_select_pending);
        panel.set_loading();
        assert!(!panel.auto_select_pending);
    }

    /// 행 글자(파일명·커밋 제목)를 눌러도 행이 눌려야 한다 — egui Label은 기본이 글자 선택
    /// 가능이라 click을 감지해 행 위에서 가로챘다(2026-10-11 리뷰: 긴 제목의 커밋 행은 71곳 중
    /// 6곳만 반응). kittest `click()`은 그 노드 가운데에 실제 포인터를 누른다.
    #[test]
    fn kittest_행_글자를_눌러도_행이_눌린다() {
        use crate::ui::diff_viewer::DiffMode;
        use egui_kittest::kittest::Queryable;

        let sha = CommitSha::parse("a1b2c3d").unwrap();
        let mut harness = nav_harness(GitPanelSnapshot {
            branch: "main".into(),
            upstream: Some("origin/main".into()),
            changes: vec![
                file_row("src/a.rs", 'M', 1, 0),
                file_row("src/b.rs", 'M', 1, 0),
            ],
            commits: vec![GitCommitRow {
                sha,
                subject: "글자를 눌러도 펼쳐지는 커밋 제목".into(),
                time: 0,
            }],
            ..Default::default()
        });
        harness.run();
        harness.state_mut().actions.clear();

        harness.get_by_label("b.rs").click();
        harness.run();
        assert_eq!(
            harness.state().actions,
            vec![show("src/b.rs", DiffMode::Working)],
            "파일명 글자를 눌렀는데 행이 반응하지 않았다"
        );

        harness.state_mut().panel.tab = GitPanelTab::Commits;
        harness.run();
        harness.state_mut().actions.clear();
        harness
            .get_by_label("글자를 눌러도 펼쳐지는 커밋 제목")
            .click();
        harness.run();
        assert_eq!(
            harness.state().actions,
            vec![GitPanelAction::LoadCommitFiles { sha }],
            "커밋 제목 글자를 눌렀는데 펼쳐지지 않았다"
        );
    }

    /// 좁은 패널(240pt)에서 긴 파일명은 말줄임되고 증감 숫자는 패널 안에 남는다 — 예전 행은
    /// 줄바꿈 없는 가로 행이라 폴더가 「+120」을 덮고 「−45」가 잘렸다(2026-10-11 리뷰).
    #[test]
    fn kittest_좁은_패널에서도_증감이_패널_안에_남는다() {
        use egui_kittest::kittest::Queryable;

        let mut panel = GitPanelUi::default();
        panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "main".into(),
            changes: vec![file_row(
                "crates/app/src/ui/workspace_session_controller.rs",
                'M',
                120,
                45,
            )],
            ..Default::default()
        }));
        let width = 240.0;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(width, 300.0))
            .build_ui_state(
                |ui, panel: &mut GitPanelUi| {
                    panel.render(ui, &en(), "");
                },
                panel,
            );
        harness.run();
        // 합계 줄에도 같은 숫자가 있다 — 행 안의 것을 고른다.
        let row = harness
            .get_by_role_and_label(
                egui::accesskit::Role::Button,
                "crates/app/src/ui/workspace_session_controller.rs",
            )
            .rect();
        let in_row = |label: &str| {
            harness
                .query_all_by_label(label)
                .map(|node| node.rect())
                .find(|rect| row.contains(rect.center()))
                .unwrap_or_else(|| panic!("행 안에 {label}가 없다"))
        };
        let adds = in_row("+120");
        let dels = in_row("−45");
        assert!(
            dels.right() <= width + 0.5,
            "「−45」가 패널 밖으로 밀렸다: {dels:?}"
        );
        let name = harness
            .get_by_label("workspace_session_controller.rs")
            .rect();
        assert!(
            name.right() <= adds.left() + 0.5,
            "파일명이 증감 숫자를 덮었다: {name:?} / {adds:?}"
        );
    }

    fn show(path: &str, mode: crate::ui::diff_viewer::DiffMode) -> GitPanelAction {
        GitPanelAction::ShowFileDiff {
            rel_path: path.to_owned(),
            mode,
        }
    }

    fn press(harness: &mut egui_kittest::Harness<'static, NavState>, key: egui::Key) {
        harness.key_press(key);
        harness.run();
    }

    #[test]
    fn kittest_폴더별은_폴더_머리_아래_파일명만_보이고_탭마다_따로_접힌다() {
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;

        let catalog = en();
        let mut harness = panel_harness(
            GitPanelSnapshot {
                branch: "feat".into(),
                upstream: Some("origin/main".into()),
                changes: vec![
                    file_row("crates/app/src/ui/a.rs", 'M', 1, 0),
                    file_row("README.md", 'M', 1, 0),
                    file_row("crates/app/src/ui/b.rs", 'M', 1, 0),
                ],
                committed: vec![file_row("crates/app/src/ui/a.rs", 'M', 1, 0)],
                ..Default::default()
            },
            "",
        );
        harness.run();

        // 목록(기본) — 행마다 줄인 폴더가 붙고 폴더 머리는 없다.
        assert_eq!(harness.query_all_by_label("…/src/ui").count(), 2);
        assert!(
            harness
                .query_by_role_and_label(Role::Button, "crates/app/src/ui 2")
                .is_none()
        );

        // 폴더별 — 처음 나온 폴더 순서로 머리(폴더 경로 · 개수) 아래에 파일명만.
        harness
            .get_by_label(&catalog.t("git.layout.folders", &[]))
            .click();
        harness.run();
        harness.get_by_role_and_label(Role::Button, "crates/app/src/ui 2");
        harness.get_by_role_and_label(Role::Button, ". 1");
        assert!(
            harness.query_by_label("…/src/ui").is_none(),
            "묶음 안의 파일 행은 폴더를 다시 보이지 않는다"
        );
        // 파일 행은 같은 file_row라 접근성 이름(rel_path)·파일명이 그대로다.
        harness.get_by_role_and_label(Role::Button, "crates/app/src/ui/a.rs");
        harness.get_by_label("a.rs");

        // 머리를 누르면 그 폴더의 파일 행만 접힌다.
        harness
            .get_by_role_and_label(Role::Button, "crates/app/src/ui 2")
            .click();
        harness.run();
        assert!(
            harness
                .query_by_role_and_label(Role::Button, "crates/app/src/ui/a.rs")
                .is_none()
        );
        harness.get_by_role_and_label(Role::Button, "README.md");
        harness.get_by_role_and_label(Role::Button, "crates/app/src/ui 2");

        // 묶기 보기는 두 탭이 같이 쓰고, 접힘은 탭마다 따로다.
        harness.state_mut().panel.tab = GitPanelTab::Commits;
        harness.run();
        harness.get_by_role_and_label(Role::Button, "crates/app/src/ui 1");
        harness.get_by_role_and_label(Role::Button, "crates/app/src/ui/a.rs");

        // 변경 탭으로 돌아와 다시 펴면 파일 행이 돌아온다.
        harness.state_mut().panel.tab = GitPanelTab::Changes;
        harness.run();
        harness
            .get_by_role_and_label(Role::Button, "crates/app/src/ui 2")
            .click();
        harness.run();
        harness.get_by_role_and_label(Role::Button, "crates/app/src/ui/b.rs");

        // 목록으로 돌아가면 폴더가 다시 행에 붙는다.
        harness
            .get_by_label(&catalog.t("git.layout.flat", &[]))
            .click();
        harness.run();
        assert_eq!(harness.query_all_by_label("…/src/ui").count(), 2);
    }

    #[test]
    fn kittest_화살표는_보이는_파일을_표시_순서로_옮기고_접은_폴더는_건너뛴다() {
        use crate::ui::diff_viewer::DiffMode::Working;
        use egui::Key::{ArrowDown, ArrowUp};
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;

        let catalog = en();
        let mut harness = nav_harness(GitPanelSnapshot {
            branch: "main".into(),
            changes: vec![
                file_row("src/a.rs", 'M', 1, 0),
                file_row("docs/c.md", 'M', 1, 0),
                file_row("src/b.rs", 'M', 1, 0),
            ],
            ..Default::default()
        });
        harness.run();
        // 첫 파일은 자동 선택된다 — 거기서 출발한다.
        assert_eq!(harness.state().actions, vec![show("src/a.rs", Working)]);

        // 목록: 입력 순서(a → c → b)로 내려가고 끝에서 멈춘다. 쓴 키는 소비한다.
        press(&mut harness, ArrowDown);
        assert!(
            !harness.state().down_left,
            "쓴 ↓는 다른 소비자에게 새지 않는다"
        );
        press(&mut harness, ArrowDown);
        press(&mut harness, ArrowDown);
        assert_eq!(
            harness.state().actions,
            vec![
                show("src/a.rs", Working),
                show("docs/c.md", Working),
                show("src/b.rs", Working),
            ]
        );
        assert_eq!(
            harness.state().panel.selected,
            Some(("src/b.rs".to_owned(), Working))
        );

        // ⌘↓ 같은 단축키는 건드리지 않는다.
        harness.key_press_modifiers(egui::Modifiers::COMMAND, ArrowDown);
        harness.run();
        assert_eq!(harness.state().actions.len(), 3);
        assert!(
            harness.state().down_left,
            "보조키가 있는 ↓는 소비하지 않는다"
        );

        // 폴더별: src(a, b) → docs(c) 순서로 묶인다 — 이동도 그 순서를 따른다.
        harness
            .get_by_label(&catalog.t("git.layout.folders", &[]))
            .click();
        harness.run();
        harness.state_mut().actions.clear();
        press(&mut harness, ArrowUp);
        press(&mut harness, ArrowDown);
        press(&mut harness, ArrowDown);
        assert_eq!(
            harness.state().actions,
            vec![
                show("src/a.rs", Working),
                show("src/b.rs", Working),
                show("docs/c.md", Working),
            ]
        );

        // src를 접으면 그 파일은 이동 대상이 아니다 — c에서 ↑는 갈 곳이 없다.
        harness.get_by_role_and_label(Role::Button, "src 2").click();
        harness.run();
        press(&mut harness, ArrowUp);
        assert_eq!(harness.state().actions.len(), 3);
        // 선택이 접힌 폴더 안이면 ↓는 보이는 첫 행으로 간다.
        harness.state_mut().panel.selected = Some(("src/a.rs".to_owned(), Working));
        press(&mut harness, ArrowDown);
        assert_eq!(
            harness.state().actions.last(),
            Some(&show("docs/c.md", Working))
        );
    }

    #[test]
    fn kittest_커밋_탭의_화살표는_브랜치_파일_다음에_펼친_커밋_파일로_간다() {
        use crate::ui::diff_viewer::DiffMode::{Branch, Commit};
        use egui::Key::ArrowDown;

        let sha = CommitSha::parse("a1b2c3d").unwrap();
        let mut harness = nav_harness(GitPanelSnapshot {
            branch: "feat".into(),
            upstream: Some("origin/main".into()),
            committed: vec![file_row("src/branch.rs", 'M', 1, 0)],
            commits: vec![GitCommitRow {
                sha,
                subject: "첫 커밋".into(),
                time: 0,
            }],
            worktrees: vec![worktree("/repo", true), worktree("/repo/wt", false)],
            ..Default::default()
        });
        {
            let panel = &mut harness.state_mut().panel;
            panel.tab = GitPanelTab::Commits;
            panel.commit_list.toggle(sha);
            panel.set_commit_files(
                sha,
                Ok(GitCommitFiles {
                    rows: vec![file_row("src/commit_only.rs", 'A', 3, 0)],
                    truncated: false,
                }),
            );
        }
        harness.run();
        assert!(
            harness.state().actions.is_empty(),
            "변경이 없으면 자동 선택도 없다"
        );

        press(&mut harness, ArrowDown);
        press(&mut harness, ArrowDown);
        press(&mut harness, ArrowDown);
        assert_eq!(
            harness.state().actions,
            vec![
                show("src/branch.rs", Branch),
                show("src/commit_only.rs", Commit(sha)),
            ]
        );

        // 워크트리 탭에는 파일 행이 없다 — ↓를 소비하지 않는다.
        harness.state_mut().panel.tab = GitPanelTab::Worktrees;
        harness.run();
        harness.state_mut().down_left = false;
        press(&mut harness, ArrowDown);
        assert!(harness.state().down_left);
        assert_eq!(harness.state().actions.len(), 2);
    }

    #[test]
    fn kittest_텍스트_입력에_포커스가_있으면_화살표를_가로채지_않는다() {
        use crate::ui::diff_viewer::DiffMode::Working;
        use egui::Key::ArrowDown;

        let mut harness = nav_harness(GitPanelSnapshot {
            branch: "main".into(),
            changes: vec![
                file_row("src/a.rs", 'M', 1, 0),
                file_row("src/b.rs", 'M', 1, 0),
            ],
            ..Default::default()
        });
        harness.run();
        assert_eq!(harness.state().actions, vec![show("src/a.rs", Working)]);

        // 보조 검색창에 포커스 — ↓는 그 입력의 키다.
        harness.state_mut().search_focused = true;
        harness.run();
        assert!(harness.ctx.text_edit_focused());
        press(&mut harness, ArrowDown);
        assert_eq!(harness.state().actions.len(), 1);
        assert_eq!(
            harness.state().panel.selected,
            Some(("src/a.rs".to_owned(), Working))
        );

        // 포커스가 빠지면 다시 목록이 받는다.
        harness.state_mut().search_focused = false;
        harness.run();
        assert!(!harness.ctx.text_edit_focused());
        press(&mut harness, ArrowDown);
        assert_eq!(
            harness.state().actions,
            vec![show("src/a.rs", Working), show("src/b.rs", Working)]
        );
    }

    #[test]
    fn kittest_스냅샷이_오면_변경_탭의_첫_파일을_한_번만_자동_선택한다() {
        use crate::ui::diff_viewer::DiffMode::Working;
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;

        let snapshot = || GitPanelSnapshot {
            branch: "feat".into(),
            upstream: Some("origin/main".into()),
            changes: vec![
                file_row("src/a.rs", 'M', 1, 0),
                file_row("src/b.rs", 'M', 1, 0),
            ],
            committed: vec![file_row("src/c.rs", 'M', 1, 0)],
            ..Default::default()
        };
        let mut harness = nav_harness(snapshot());
        harness.run();
        assert_eq!(harness.state().actions, vec![show("src/a.rs", Working)]);
        assert_eq!(
            harness.state().panel.selected,
            Some(("src/a.rs".to_owned(), Working))
        );

        // 프레임이 더 돌아도, 선택을 지키는 새로고침이 와도 다시 내지 않는다.
        harness.run();
        harness.state_mut().panel.set_snapshot(Ok(snapshot()));
        harness.run();
        assert_eq!(harness.state().actions.len(), 1);

        // 사용자가 고른 파일은 새로고침이 덮지 않는다.
        harness
            .get_by_role_and_label(Role::Button, "src/b.rs")
            .click();
        harness.run();
        harness.state_mut().panel.set_snapshot(Ok(snapshot()));
        harness.run();
        assert_eq!(
            harness.state().actions,
            vec![show("src/a.rs", Working), show("src/b.rs", Working)]
        );

        // reset_git_surfaces처럼 패널을 새로 만들면 다음 스냅샷에서 다시 한 번 고른다.
        harness.state_mut().panel = GitPanelUi::default();
        harness.state_mut().panel.set_snapshot(Ok(snapshot()));
        harness.run();
        harness.run();
        assert_eq!(
            harness.state().actions,
            vec![
                show("src/a.rs", Working),
                show("src/b.rs", Working),
                show("src/a.rs", Working),
            ]
        );
    }

    #[test]
    fn kittest_자동_선택은_필터를_거쳐_보이는_첫_파일을_고른다() {
        use crate::ui::diff_viewer::DiffMode::Working;

        // 필터가 걸린 채 스냅샷이 온다(하네스는 만들 때 한 프레임을 그린다).
        let mut harness = nav_harness(GitPanelSnapshot {
            branch: "main".into(),
            ..Default::default()
        });
        harness.state_mut().filter = "docs";
        harness.state_mut().panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "main".into(),
            changes: vec![
                file_row("src/a.rs", 'M', 1, 0),
                file_row("docs/b.md", 'M', 1, 0),
            ],
            ..Default::default()
        }));
        harness.run();
        assert_eq!(harness.state().actions, vec![show("docs/b.md", Working)]);
    }

    #[test]
    fn kittest_변경이_없으면_자동_선택하지_않고_커밋_탭으로_넘어가지_않는다() {
        use crate::ui::diff_viewer::DiffMode::Working;

        let clean = GitPanelSnapshot {
            branch: "feat".into(),
            upstream: Some("origin/main".into()),
            ahead: 1,
            committed: vec![file_row("src/c.rs", 'M', 1, 0)],
            ..Default::default()
        };
        let mut harness = nav_harness(clean.clone());
        harness.run();
        assert!(harness.state().actions.is_empty());
        assert_eq!(harness.state().panel.tab, GitPanelTab::Changes);
        assert_eq!(harness.state().panel.selected, None);

        // 아무것도 고르지 않은 채 변경이 생기면 그 스냅샷에서 고른다 — 덮을 사용자 선택이 없다.
        harness.state_mut().panel.set_snapshot(Ok(GitPanelSnapshot {
            changes: vec![file_row("src/new.rs", 'M', 1, 0)],
            ..clean
        }));
        harness.run();
        assert_eq!(harness.state().actions, vec![show("src/new.rs", Working)]);
    }

    #[test]
    fn 깨끗함_요약은_작업_트리가_깨끗한_스냅샷에서만_있다() {
        let mut panel = GitPanelUi::default();
        assert_eq!(panel.clean_summary(), None, "스냅샷 전");
        panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "feat".into(),
            upstream: Some("origin/main".into()),
            ahead: 3,
            behind: 1,
            committed: vec![file_row("src/c.rs", 'M', 1, 0)],
            ..Default::default()
        }));
        assert_eq!(
            panel.clean_summary(),
            Some(GitCleanSummary {
                upstream: Some("origin/main".into()),
                ahead: 3,
                behind: 1,
            })
        );
        panel.set_snapshot(Ok(GitPanelSnapshot {
            branch: "feat".into(),
            changes: vec![file_row("src/a.rs", 'M', 1, 0)],
            ..Default::default()
        }));
        assert_eq!(panel.clean_summary(), None, "변경이 있으면 요약이 아니다");
        panel.set_snapshot(Err(GitPanelErrorCode::NoRepo));
        assert_eq!(panel.clean_summary(), None);
    }
}
