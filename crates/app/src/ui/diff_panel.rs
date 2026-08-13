//! 세션 cwd 레포의 git 변경분(diff) 리뷰 패널 (PR-D).
//!
//! 사이드바 세션 우클릭 「변경 보기」 → 독립 egui Window. leaf는 immutable snapshot을
//! 렌더하고 capacity-1 intent만 반환한다. App host가 Git/metadata I/O를 수행한 뒤
//! operation/generation completion을 돌려주며, 닫힌 창은 모든 상태를 즉시 버린다.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// git 수집 타임아웃 — repo_root/run_git 각 호출에 적용.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// diff 섹션당 상한 (agent_session::limit_text와 같은 접근 — 대형 diff가 UI에
/// 수 MB로 상주하지 않게 줄 경계에서 자른다).
const MAX_DIFF_BYTES: usize = 200 * 1024;
const MAX_DIFF_LINES: usize = 4000;
/// tracked diff 한 섹션에서 metadata/stat까지 수행할 파일 상한. 기존 줄 상한이 허용한
/// 최악의 4,000개보다 낮춰 pathological tiny-file 저장소의 syscall burst를 막는다.
const MAX_DIFF_FILES: usize = 512;
/// cwd/repo-root 한 경로의 UTF-8 표시 바이트 상한.
const MAX_PATH_BYTES: usize = 32 * 1024;
/// status + unstaged + staged + untracked의 기존 섹션별 상한을 합친 최종 snapshot 상한.
/// 기존 구현이 보유할 수 있던 양보다 커지지 않게 명시적으로 고정한다.
const MAX_SNAPSHOT_BYTES: usize = MAX_DIFF_BYTES * 4;
const MAX_SNAPSHOT_ROWS: usize = MAX_DIFF_LINES * 4 + 16;
/// untracked 새 파일 diff를 수집할 최대 파일 수 — 넘치면 개수 라벨로 접는다.
const MAX_UNTRACKED_FILES: usize = 50;
/// git 설정 무력화 — 사용자/레포 설정(color.ui=always, diff.external, pager)이
/// 파서를 깨거나 외부 앱을 띄우지 않게 모든 수집 호출 앞에 강제한다 (codex 리뷰).
const GIT_CONFIG_OVERRIDES: &[&str] = &[
    "-c",
    "color.ui=false",
    "-c",
    "diff.external=",
    "-c",
    "core.pager=cat",
];

/// unified diff 한 줄의 의미 분류.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffLineKind {
    /// `diff --git …` — 파일 구분 (굵게).
    FileHeader,
    /// index/`---`/`+++`/mode/rename/Binary 등 헤더 부속 (약하게).
    Meta,
    /// `@@ -a,b +c,d @@` (약하게).
    HunkHeader,
    /// `+` 추가 줄 (초록 계열).
    Addition,
    /// `-` 삭제 줄 (빨강 계열).
    Removal,
    /// 문맥·그 외 (기본색).
    Context,
}

/// 줄 단위 상태 분류기 — hunk 내부 플래그가 `diff --git`/`@@` 경계에서 뒤집혀,
/// 헤더 구역의 `---`/`+++`와 hunk 안의 ± 줄이 절대 충돌하지 않는다.
fn parse_unified_diff(text: &str) -> Vec<(DiffLineKind, &str)> {
    let mut rows = Vec::new();
    let mut in_hunk = false;
    for line in text.lines() {
        let kind = if line.starts_with("diff --git ") {
            in_hunk = false;
            DiffLineKind::FileHeader
        } else if line.starts_with("@@") {
            in_hunk = true;
            DiffLineKind::HunkHeader
        } else if in_hunk {
            match line.as_bytes().first() {
                Some(b'+') => DiffLineKind::Addition,
                Some(b'-') => DiffLineKind::Removal,
                // "\ No newline at end of file"
                Some(b'\\') => DiffLineKind::Meta,
                _ => DiffLineKind::Context,
            }
        } else {
            DiffLineKind::Meta
        };
        rows.push((kind, line));
    }
    rows
}

/// 상한을 넘는 diff를 줄 경계에서 자른다 — (잘린 텍스트, 잘림 여부).
/// 개행 offset에서만 슬라이스하므로 UTF-8 경계에 안전하다.
fn clip_diff(text: &str) -> (&str, bool) {
    let mut lines = 0usize;
    let mut last_newline = 0usize;
    for (offset, byte) in text.bytes().enumerate() {
        if offset >= MAX_DIFF_BYTES {
            return (&text[..last_newline], true);
        }
        if byte == b'\n' {
            lines += 1;
            if lines >= MAX_DIFF_LINES {
                return (&text[..offset], true);
            }
            last_newline = offset;
        }
    }
    (text, false)
}

/// 패널 렌더 행 — 수집 완료 시 한 번 만들어 프레임마다 재사용한다.
#[derive(Clone, PartialEq, Eq)]
enum DiffRow {
    /// 섹션 제목 (i18n key).
    Section(&'static str),
    /// `status --short` 한 줄.
    Status(String),
    /// 파일 하나의 시작 — raw `diff --git a/X b/X` 줄 대신 파일명 + 변경 통계 +
    /// 수정 시각으로 보여준다(2026-07-18 사용자: "파일명이나 날짜를 좀더 가독성
    /// 좋게"). mtime은 diff에 실려오지 않아 수집 시점에 파일시스템에서 stat한다 —
    /// 삭제된 파일 등 stat 실패는 None(생략, 에러 아님).
    FileHeader {
        path: String,
        additions: usize,
        removals: usize,
        mtime: Option<String>,
    },
    /// diff 본문 한 줄.
    Line(DiffLineKind, String),
    /// 상한 잘림 안내 (i18n `diff.truncated`).
    Truncated,
    /// 표시 상한을 넘어 생략된 untracked 파일 수 (i18n `diff.untracked_more`).
    UntrackedMore(usize),
}

impl std::fmt::Debug for DiffRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Section(key) => f.debug_tuple("Section").field(key).finish(),
            Self::Status(value) => f
                .debug_struct("Status")
                .field("bytes", &value.len())
                .finish(),
            Self::FileHeader {
                path,
                additions,
                removals,
                mtime,
            } => f
                .debug_struct("FileHeader")
                .field("path", &"REDACTED")
                .field("path_bytes", &path.len())
                .field("additions", additions)
                .field("removals", removals)
                .field("mtime", &mtime.is_some())
                .finish(),
            Self::Line(kind, value) => f
                .debug_struct("Line")
                .field("kind", kind)
                .field("bytes", &value.len())
                .finish(),
            Self::Truncated => f.write_str("Truncated"),
            Self::UntrackedMore(count) => f.debug_tuple("UntrackedMore").field(count).finish(),
        }
    }
}

/// App host가 만든 immutable 최신 snapshot. rows가 비면 작업 트리가 깨끗하다.
pub struct DiffSnapshot {
    repo_root: String,
    rows: Arc<[DiffRow]>,
    retained_bytes: usize,
}

impl std::fmt::Debug for DiffSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffSnapshot")
            .field("repo_root", &"REDACTED")
            .field("repo_root_bytes", &self.repo_root.len())
            .field("rows", &self.rows.len())
            .field("retained_bytes", &self.retained_bytes)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffIoOperation(u64);

/// Root host로만 이동하는 cwd. raw path는 Debug에 절대 노출하지 않는다.
pub struct DiffPathPayload {
    path: String,
    bytes: usize,
}

impl DiffPathPayload {
    fn try_new(path: String) -> Result<Self, DiffIoErrorCode> {
        let bytes = path.len();
        if bytes == 0 || path.as_bytes().contains(&0) {
            return Err(DiffIoErrorCode::InvalidPath);
        }
        if bytes > MAX_PATH_BYTES {
            return Err(DiffIoErrorCode::PathTooLarge);
        }
        Ok(Self { path, bytes })
    }

    fn as_path(&self) -> &Path {
        Path::new(&self.path)
    }
}

impl std::fmt::Debug for DiffPathPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffPathPayload")
            .field("path", &"REDACTED")
            .field("bytes", &self.bytes)
            .finish()
    }
}

pub struct DiffIoIntent {
    pub operation: DiffIoOperation,
    pub generation: u64,
    cwd: DiffPathPayload,
}

impl std::fmt::Debug for DiffIoIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffIoIntent")
            .field("operation", &self.operation)
            .field("generation", &self.generation)
            .field("cwd", &self.cwd)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffIoErrorCode {
    Busy,
    InvalidPath,
    PathTooLarge,
    CollectionFailed,
    SnapshotTooLarge,
}

pub struct DiffIoCompletion {
    pub operation: DiffIoOperation,
    pub generation: u64,
    pub result: Result<DiffSnapshot, DiffIoErrorCode>,
}

impl std::fmt::Debug for DiffIoCompletion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffIoCompletion")
            .field("operation", &self.operation)
            .field("generation", &self.generation)
            .field("result", &self.result)
            .finish()
    }
}

/// `status --porcelain -z` 항목 — XY 상태와 경로 (rename/copy는 원경로 포함).
struct StatusEntry {
    xy: String,
    path: String,
    orig: Option<String>,
}

/// porcelain v1 `-z` 파싱 — NUL 구분이라 경로 인용/이스케이프가 없고, XY에 R/C가
/// 있으면 다음 필드가 원경로다. 잘린 출력이면 마지막 부분 항목을 버린다
/// (`-z`는 항상 NUL로 끝나므로 비어 있지 않은 마지막 조각 = 불완전 항목).
fn parse_status_z(raw: &str, truncated: bool) -> Vec<StatusEntry> {
    let mut fields: Vec<&str> = raw.split('\0').collect();
    if truncated {
        fields.pop();
    }
    let mut fields = fields.into_iter().filter(|field| !field.is_empty());
    let mut entries = Vec::new();
    while let Some(field) = fields.next() {
        // "XY path" 최소형 — XY 2자 + 공백 1 + 경로. 형식 미달은 방어적으로 버린다.
        if field.len() < 4 || !field.is_char_boundary(2) || !field.is_char_boundary(3) {
            continue;
        }
        let xy = &field[..2];
        let path = &field[3..];
        let orig = (xy.contains('R') || xy.contains('C'))
            .then(|| fields.next())
            .flatten()
            .map(str::to_owned);
        entries.push(StatusEntry {
            xy: xy.to_owned(),
            path: path.to_owned(),
            orig,
        });
    }
    entries
}

/// status 섹션 표시용 한 줄 — `--short`가 보여주던 `XY old -> new` 모양을 유지한다.
fn status_display(entry: &StatusEntry) -> String {
    match &entry.orig {
        Some(orig) => format!("{} {} -> {}", entry.xy, orig, entry.path),
        None => format!("{} {}", entry.xy, entry.path),
    }
}

fn valid_path_text(path: &str) -> bool {
    !path.is_empty() && path.len() <= MAX_PATH_BYTES && !path.as_bytes().contains(&0)
}

/// 설정 무력화 접두어를 붙여 상한부 git 실행 — 수집 호출은 전부 이 경로를 쓴다.
fn run_limited(root: &Path, tail: &[&str], max_bytes: usize) -> anyhow::Result<(String, bool)> {
    let mut args: Vec<&str> = GIT_CONFIG_OVERRIDES.to_vec();
    args.extend_from_slice(tail);
    crate::git_cli::run_git_limited(root, &args, GIT_TIMEOUT, max_bytes)
}

/// cwd 레포의 status/diff(unstaged+staged+untracked)를 수집해 렌더 행으로 만든다.
/// 블로킹 — 백그라운드 스레드에서만 호출한다 (git_cli 규칙).
fn collect_diff(cwd: &Path) -> Result<DiffSnapshot, DiffIoErrorCode> {
    let root = crate::git_cli::repo_root(cwd, GIT_TIMEOUT)
        .map_err(|_| DiffIoErrorCode::CollectionFailed)?;
    // -uall: 디렉터리 접힘 없이 untracked를 파일 단위로 나열 (신규 파일 diff 대상).
    let (status_raw, status_truncated) = run_limited(
        &root,
        &["status", "--porcelain", "-z", "-uall"],
        MAX_DIFF_BYTES,
    )
    .map_err(|_| DiffIoErrorCode::CollectionFailed)?;
    let entries = parse_status_z(&status_raw, status_truncated);
    if entries.iter().any(|entry| {
        !valid_path_text(&entry.path)
            || entry
                .orig
                .as_deref()
                .is_some_and(|path| !valid_path_text(path))
    }) {
        return Err(DiffIoErrorCode::SnapshotTooLarge);
    }
    let (unstaged, unstaged_truncated) =
        run_limited(&root, &["diff", "--no-ext-diff"], MAX_DIFF_BYTES)
            .map_err(|_| DiffIoErrorCode::CollectionFailed)?;
    let (staged, staged_truncated) = run_limited(
        &root,
        &["diff", "--no-ext-diff", "--cached"],
        MAX_DIFF_BYTES,
    )
    .map_err(|_| DiffIoErrorCode::CollectionFailed)?;

    let mut rows = Vec::new();
    if !entries.is_empty() {
        rows.push(DiffRow::Section("diff.section.status"));
        rows.extend(
            entries
                .iter()
                .take(MAX_DIFF_LINES)
                .map(|entry| DiffRow::Status(status_display(entry))),
        );
        if status_truncated || entries.len() > MAX_DIFF_LINES {
            rows.push(DiffRow::Truncated);
        }
    }
    append_diff_section(
        &mut rows,
        "diff.section.unstaged",
        &unstaged,
        unstaged_truncated,
        &root,
    )?;
    append_diff_section(
        &mut rows,
        "diff.section.staged",
        &staged,
        staged_truncated,
        &root,
    )?;
    append_untracked_section(&mut rows, &root, &entries)?;
    let repo_root = root.display().to_string();
    if repo_root.len() > MAX_PATH_BYTES || repo_root.as_bytes().contains(&0) {
        return Err(DiffIoErrorCode::SnapshotTooLarge);
    }
    bound_snapshot_rows(&mut rows);
    let retained_bytes = rows.iter().map(diff_row_retained_bytes).sum();
    Ok(DiffSnapshot {
        repo_root,
        rows: rows.into(),
        retained_bytes,
    })
}

fn diff_row_retained_bytes(row: &DiffRow) -> usize {
    match row {
        DiffRow::Section(key) => key.len(),
        DiffRow::Status(value) | DiffRow::Line(_, value) => value.len(),
        DiffRow::FileHeader { path, mtime, .. } => {
            path.len() + mtime.as_ref().map_or(0, String::len)
        }
        DiffRow::Truncated | DiffRow::UntrackedMore(_) => 0,
    }
}

/// 최종 immutable snapshot에 행/바이트 상한을 한 번 더 적용한다. 수집 primitive의
/// 섹션별 상한이 바뀌더라도 UI 보유량이 증가하지 않는다.
fn bound_snapshot_rows(rows: &mut Vec<DiffRow>) {
    let mut retained_bytes = 0usize;
    let mut keep = 0usize;
    for row in rows.iter() {
        let row_bytes = diff_row_retained_bytes(row);
        if keep >= MAX_SNAPSHOT_ROWS
            || retained_bytes.saturating_add(row_bytes) > MAX_SNAPSHOT_BYTES
        {
            break;
        }
        retained_bytes += row_bytes;
        keep += 1;
    }
    if keep < rows.len() {
        rows.truncate(keep.saturating_sub(1));
        rows.push(DiffRow::Truncated);
    }
}

/// App-owned bounded worker가 호출하는 유일한 host 실행 진입점. leaf render는 이 함수를
/// 호출하지 않으며, raw 오류/경로를 completion이나 Debug로 반환하지 않는다.
pub fn execute_io(intent: DiffIoIntent) -> DiffIoCompletion {
    let operation = intent.operation;
    let generation = intent.generation;
    let result = collect_diff(intent.cwd.as_path());
    DiffIoCompletion {
        operation,
        generation,
        result,
    }
}

/// untracked(`??`) 새 파일 내용 — 스테이징 전 신규 파일은 "에이전트가 뭘 바꿨나"의
/// 흔한 핵심인데 `git diff`에는 나오지 않는다 (codex 리뷰 P1). 파일별
/// `diff --no-index /dev/null`로 수집하고(차이 있으면 종료코드 1 —
/// run_git_limited가 성공으로 본다), 섹션 전체에 같은 바이트 상한과 파일 수 상한을
/// 적용한다. 바이너리는 git이 주는 "Binary files …" 한 줄이 그대로 표시된다.
fn append_untracked_section(
    rows: &mut Vec<DiffRow>,
    root: &Path,
    entries: &[StatusEntry],
) -> Result<(), DiffIoErrorCode> {
    let untracked: Vec<&str> = entries
        .iter()
        .filter(|entry| entry.xy == "??")
        .map(|entry| entry.path.as_str())
        .collect();
    if untracked.is_empty() {
        return Ok(());
    }
    let mut diff = String::new();
    let mut truncated = false;
    let mut shown = 0usize;
    for path in untracked.iter().take(MAX_UNTRACKED_FILES) {
        let budget = MAX_DIFF_BYTES.saturating_sub(diff.len());
        if budget == 0 {
            truncated = true;
            break;
        }
        let (out, out_truncated) = run_limited(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-index",
                "--",
                "/dev/null",
                path,
            ],
            budget,
        )
        .map_err(|_| DiffIoErrorCode::CollectionFailed)?;
        diff.push_str(&out);
        truncated |= out_truncated;
        shown += 1;
    }
    append_diff_section(rows, "diff.section.untracked", &diff, truncated, root)?;
    if untracked.len() > shown {
        rows.push(DiffRow::UntrackedMore(untracked.len() - shown));
    }
    Ok(())
}

fn append_diff_section(
    rows: &mut Vec<DiffRow>,
    key: &'static str,
    diff: &str,
    collector_truncated: bool,
    root: &Path,
) -> Result<(), DiffIoErrorCode> {
    if diff.trim().is_empty() {
        return Ok(());
    }
    let (clipped, clip_truncated) = clip_diff(diff);
    rows.push(DiffRow::Section(key));
    let (file_rows, files_truncated) = build_file_rows(clipped, root)?;
    rows.extend(file_rows);
    if clip_truncated || collector_truncated || files_truncated {
        rows.push(DiffRow::Truncated);
    }
    Ok(())
}

/// `diff --git a/X b/Y` 줄에서 표시할 경로를 뽑는다 — b측(새/현재 경로, rename도
/// 최신명)을 우선한다. 경로 자체에 " b/"가 들어간 극단적 케이스는 놓칠 수 있으나
/// (git 자체도 헤더 줄만으로는 완전히 무손실 파싱이 안 되는 형식), 실사용 경로에서는
/// 항상 맞는다.
fn diff_git_header_path(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("diff --git ")?;
    let b_at = rest.rfind(" b/")?;
    Some(&rest[b_at + 3..])
}

/// 헤더 직후의 `index …`/`--- …`/`+++ …`는 파일명 헤더가 이미 보여주는 정보라 그대로
/// 두면 중복만 늘린다 — 생략한다. mode/rename/binary/개행누락 등 **새 정보를 담은**
/// 줄은 그대로 보여준다(가독성 개선이 정보 손실이 되면 안 된다).
fn is_redundant_file_meta(line: &str) -> bool {
    line.starts_with("index ") || line.starts_with("--- ") || line.starts_with("+++ ")
}

/// 파일 대상 상대경로의 최근 수정 시각을 사람이 읽는 상대 표기로 — "언제 바뀌었는지"
/// (2026-07-18 사용자). git diff 자체엔 시각 정보가 없어(작업 트리 변경이라 커밋
/// 없음) 파일시스템 mtime으로 답한다. 이 워크스페이스엔 시간대 변환 없이 std만
/// 쓰는 관례가 있어(PR-W slug — chrono 미의존) 상대 표기를 택했다: 절대시각은
/// 타임존 변환이 필요하지만 "N분 전"은 지금과의 차이만 있으면 된다.
fn file_mtime_label(root: &Path, rel_path: &str) -> Option<String> {
    let modified = std::fs::metadata(root.join(rel_path))
        .ok()?
        .modified()
        .ok()?;
    let elapsed = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    Some(relative_time_label(elapsed))
}

fn relative_time_label(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        "방금 전".to_owned()
    } else if secs < 3600 {
        format!("{}분 전", secs / 60)
    } else if secs < 86_400 {
        format!("{}시간 전", secs / 3600)
    } else {
        format!("{}일 전", secs / 86_400)
    }
}

/// 줄 단위 분류(`parse_unified_diff`)를 파일 블록으로 묶어 렌더 행을 만든다 —
/// 각 파일의 시작에 raw 헤더 대신 `FileHeader`(경로+±통계+mtime)를 놓고, 중복
/// 메타 줄은 걸러낸다.
fn build_file_rows(clipped: &str, root: &Path) -> Result<(Vec<DiffRow>, bool), DiffIoErrorCode> {
    let classified = parse_unified_diff(clipped);
    let mut rows = Vec::new();
    let mut i = 0;
    let mut files = 0usize;
    let mut truncated = false;
    while i < classified.len() {
        let (kind, line) = classified[i];
        if kind != DiffLineKind::FileHeader {
            rows.push(DiffRow::Line(kind, line.to_owned()));
            i += 1;
            continue;
        }
        if files >= MAX_DIFF_FILES {
            truncated = true;
            break;
        }
        files += 1;
        // 다음 FileHeader 전까지가 이 파일의 블록 — 그 안의 +/- 줄을 세어 통계로.
        let mut j = i + 1;
        let mut additions = 0usize;
        let mut removals = 0usize;
        while j < classified.len() && classified[j].0 != DiffLineKind::FileHeader {
            match classified[j].0 {
                DiffLineKind::Addition => additions += 1,
                DiffLineKind::Removal => removals += 1,
                _ => {}
            }
            j += 1;
        }
        let path = diff_git_header_path(line).unwrap_or(line).to_owned();
        if !valid_path_text(&path) {
            return Err(DiffIoErrorCode::SnapshotTooLarge);
        }
        let mtime = file_mtime_label(root, &path);
        rows.push(DiffRow::FileHeader {
            path,
            additions,
            removals,
            mtime,
        });
        for &(k, l) in &classified[i + 1..j] {
            if is_redundant_file_meta(l) {
                continue;
            }
            rows.push(DiffRow::Line(k, l.to_owned()));
        }
        i = j;
    }
    Ok((rows, truncated))
}

/// diff 패널 창의 고정 Id — workspace가 이 창을 터미널 입력을 막지 않는 비모달로
/// 분류한다 (agent_sessions::agents_window_id와 같은 관례).
pub(crate) fn diff_window_id() -> egui::Id {
    egui::Id::new("deppy_diff_window")
}

/// diff 의미색 — 다크/라이트 상수, 나머지는 visuals에서 파생.
fn diff_line_color(visuals: &egui::Visuals, kind: DiffLineKind) -> egui::Color32 {
    match kind {
        DiffLineKind::Addition => {
            if visuals.dark_mode {
                egui::Color32::from_rgb(0x3f, 0xb9, 0x50)
            } else {
                egui::Color32::from_rgb(0x1a, 0x7f, 0x37)
            }
        }
        DiffLineKind::Removal => {
            if visuals.dark_mode {
                egui::Color32::from_rgb(0xf8, 0x51, 0x49)
            } else {
                egui::Color32::from_rgb(0xcf, 0x22, 0x2e)
            }
        }
        DiffLineKind::FileHeader => visuals.strong_text_color(),
        DiffLineKind::Meta | DiffLineKind::HunkHeader => visuals.weak_text_color(),
        DiffLineKind::Context => visuals.text_color(),
    }
}

/// 한 행 렌더 — 모노스페이스, 가로는 truncate(줄바꿈 금지 — 코드다).
fn render_diff_row(ui: &mut egui::Ui, catalog: &i18n::Catalog, row: &DiffRow) {
    if let DiffRow::FileHeader {
        path,
        additions,
        removals,
        mtime,
    } = row
    {
        // raw "diff --git a/X b/X" 한 줄 대신 파일명·±통계·수정시각을 한 줄에
        // (2026-07-18 사용자: 가독성 개선 요청). **한 줄 고정** — 위 ScrollArea가
        // `show_rows`(고정 행높이 가상화)라 이 행만 커지면 스크롤 위치가 어긋난다.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.add(
                egui::Label::new(
                    egui::RichText::new(path)
                        .monospace()
                        .strong()
                        .color(ui.visuals().hyperlink_color),
                )
                .truncate(),
            );
            if *additions > 0 {
                ui.label(
                    egui::RichText::new(format!("+{additions}"))
                        .monospace()
                        .color(diff_line_color(ui.visuals(), DiffLineKind::Addition)),
                );
            }
            if *removals > 0 {
                ui.label(
                    egui::RichText::new(format!("−{removals}"))
                        .monospace()
                        .color(diff_line_color(ui.visuals(), DiffLineKind::Removal)),
                );
            }
            if let Some(mtime) = mtime {
                ui.label(egui::RichText::new(format!("· {mtime}")).monospace().weak());
            }
        });
        return;
    }
    let text = match row {
        DiffRow::FileHeader { .. } => unreachable!("above early-return handles this"),
        DiffRow::Section(key) => egui::RichText::new(catalog.t(key, &[]))
            .monospace()
            .strong(),
        DiffRow::Status(line) => egui::RichText::new(line).monospace(),
        DiffRow::Line(kind, line) => egui::RichText::new(line)
            .monospace()
            .color(diff_line_color(ui.visuals(), *kind)),
        DiffRow::Truncated => egui::RichText::new(catalog.t("diff.truncated", &[]))
            .monospace()
            .weak(),
        DiffRow::UntrackedMore(count) => {
            egui::RichText::new(catalog.t("diff.untracked_more", &[("count", &count.to_string())]))
                .monospace()
                .weak()
        }
    };
    ui.add(egui::Label::new(text).truncate());
}

/// 「변경 보기」 독립 창 — App이 소유하고 매 프레임 show를 호출한다.
pub struct DiffPanelUi {
    open: bool,
    /// 대상 — 사이드바 「변경 보기」 dispatch가 채운다.
    workspace_id: String,
    session: Option<runtime::SessionId>,
    cwd: Option<String>,
    /// 표시용 제목 "워크스페이스 · 세션" — "세션 #2" 같은 내부 id 대신 사람이 읽는
    /// 이름(2026-07-18 사용자: 가독성 개선 요청). App이 인박스와 같은 방식으로
    /// 해석해 넘긴다(workspace_display_name + display_pane_title) — 이 모듈은
    /// App을 몰라 직접 해석할 수 없다.
    title: String,
    /// target/open이 바뀔 때 증가한다. 이전 worker 결과는 정확 일치하지 않으면 버린다.
    generation: u64,
    next_operation: u64,
    /// leaf가 보유하는 큐는 정확히 한 칸이다. App이 take한 뒤에도 pending identity만
    /// 남고 thread/channel/process는 leaf에 존재하지 않는다.
    queued_intent: Option<DiffIoIntent>,
    pending: Option<(DiffIoOperation, u64)>,
    snapshot: Option<DiffSnapshot>,
    error: Option<DiffIoErrorCode>,
}

impl DiffPanelUi {
    pub fn new() -> Self {
        Self {
            open: false,
            workspace_id: String::new(),
            session: None,
            cwd: None,
            title: String::new(),
            generation: 1,
            next_operation: 1,
            queued_intent: None,
            pending: None,
            snapshot: None,
            error: None,
        }
    }

    /// 사이드바 「변경 보기」 진입점 — 대상 세팅 + 창 열기 + 자동 1회 조회.
    /// cwd 미확인이면 조회 없이 열어 패널이 안내를 표시한다.
    pub fn open_for(
        &mut self,
        ctx: &egui::Context,
        workspace_id: String,
        session: runtime::SessionId,
        cwd: Option<String>,
        title: String,
    ) {
        self.open_for_target(ctx, workspace_id, Some(session), cwd, title);
    }

    /// 저장된 작업 이력의 cwd 진입점. 세션 lifecycle과 무관하지만 기존 diff 수집의
    /// generation/capacity-one/경로 검증을 그대로 공유한다.
    pub fn open_for_path(
        &mut self,
        ctx: &egui::Context,
        workspace_id: String,
        cwd: String,
        title: String,
    ) {
        self.open_for_target(ctx, workspace_id, None, Some(cwd), title);
    }

    fn open_for_target(
        &mut self,
        _ctx: &egui::Context,
        workspace_id: String,
        session: Option<runtime::SessionId>,
        cwd: Option<String>,
        title: String,
    ) {
        self.invalidate_target();
        self.open = true;
        self.workspace_id = workspace_id;
        self.session = session;
        self.title = title;
        match cwd {
            Some(path) => match DiffPathPayload::try_new(path) {
                Ok(payload) => self.cwd = Some(payload.path),
                Err(code) => {
                    self.cwd = None;
                    self.error = Some(code);
                }
            },
            None => {
                self.cwd = None;
            }
        }
        if self.cwd.is_some() {
            let _ = self.queue_collect();
        }
    }

    fn next_operation(&mut self) -> DiffIoOperation {
        let operation = DiffIoOperation(self.next_operation);
        self.next_operation = self.next_operation.wrapping_add(1).max(1);
        operation
    }

    fn queue_collect(&mut self) -> Result<(), DiffIoErrorCode> {
        if self.queued_intent.is_some() || self.pending.is_some() {
            return Err(DiffIoErrorCode::Busy);
        }
        let Some(cwd) = self.cwd.clone() else {
            return Err(DiffIoErrorCode::InvalidPath);
        };
        let cwd = DiffPathPayload::try_new(cwd)?;
        let operation = self.next_operation();
        let generation = self.generation;
        self.queued_intent = Some(DiffIoIntent {
            operation,
            generation,
            cwd,
        });
        self.pending = Some((operation, generation));
        Ok(())
    }

    fn invalidate_target(&mut self) {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.queued_intent = None;
        self.pending = None;
        self.snapshot = None;
        self.error = None;
    }

    fn clear_closed_state(&mut self) {
        self.open = false;
        self.invalidate_target();
        self.workspace_id.clear();
        self.session = None;
        self.cwd = None;
        self.title.clear();
    }

    /// Root의 capacity-1 AppHost executor가 가져갈 다음 intent. 안정된 render frame과
    /// 닫힌/idle 상태에서는 항상 None이다.
    pub fn take_io_intent(&mut self) -> Option<DiffIoIntent> {
        self.queued_intent.take()
    }

    /// Root worker 결과를 적용한다. 현재 open target의 exact operation/generation과
    /// 다르면 아무 상태도 바꾸지 않아 late result가 새 target을 덮지 못한다.
    pub fn complete_io(&mut self, completion: DiffIoCompletion) {
        if !self.open
            || self.pending != Some((completion.operation, completion.generation))
            || completion.generation != self.generation
        {
            return;
        }
        self.pending = None;
        match completion.result {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                self.error = None;
            }
            Err(code) => {
                self.snapshot = None;
                self.error = Some(code);
            }
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, catalog: &i18n::Catalog) {
        if !self.open {
            return;
        }
        let mut window_open = self.open;
        egui::Window::new(catalog.t("diff.title", &[]))
            .id(diff_window_id())
            .open(&mut window_open)
            .default_width(760.0)
            .default_height(520.0)
            .min_width(420.0)
            .resizable(true)
            .show(ctx, |ui| self.render_body(ui, catalog));
        self.open = window_open;
        if !self.open {
            // 명시적 조회형 — 닫으면 대상/결과를 모두 버린다.
            self.clear_closed_state();
        }
    }

    fn render_body(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        ui.horizontal(|ui| {
            if !self.title.is_empty() {
                // "세션 #2" 같은 내부 id 대신 App이 해석한 표시명 — hover에 워크스페이스
                // id(디버그용)만 보조로 남긴다(2026-07-18 사용자: 가독성 개선 요청).
                ui.strong(&self.title).on_hover_text(&self.workspace_id);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let can_refresh = self.pending.is_none() && self.cwd.is_some();
                if ui
                    .add_enabled(
                        can_refresh,
                        egui::Button::new(catalog.t("diff.refresh", &[])),
                    )
                    .clicked()
                {
                    self.snapshot = None;
                    self.error = None;
                    if let Err(code) = self.queue_collect() {
                        self.error = Some(code);
                    }
                }
                if self.pending.is_some() {
                    ui.spinner();
                    ui.weak(catalog.t("diff.loading", &[]));
                }
            });
        });
        // 경로: 수집 후엔 레포 루트, 그 전엔 세션 cwd. 둘 다 없으면 cwd 미확인 안내.
        let path = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.repo_root.as_str())
            .or(self.cwd.as_deref());
        match path {
            Some(path) => {
                ui.add(egui::Label::new(egui::RichText::new(path).monospace().weak()).truncate());
            }
            None => {
                ui.label(catalog.t("diff.no_cwd", &[]));
            }
        }
        if let Some(error) = &self.error {
            ui.colored_label(
                egui::Color32::from_rgb(0xff, 0x7b, 0x72),
                format!("{}: {error:?}", catalog.t("diff.error", &[])),
            );
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        crate::ui::hairline(ui);
        if snapshot.rows.is_empty() {
            ui.weak(catalog.t("diff.clean", &[]));
            return;
        }
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
        let rows = snapshot.rows.as_ref();
        egui::ScrollArea::vertical()
            .id_salt("diff-panel-rows")
            .auto_shrink([false, false])
            .show_rows(ui, row_height, rows.len(), |ui, range| {
                for row in &rows[range] {
                    render_diff_row(ui, catalog, row);
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(repo_root: &str, rows: Vec<DiffRow>) -> DiffSnapshot {
        let rows: Arc<[DiffRow]> = rows.into();
        DiffSnapshot {
            repo_root: repo_root.to_owned(),
            retained_bytes: rows.iter().map(diff_row_retained_bytes).sum(),
            rows,
        }
    }

    // ── 파서 유닛 ──

    #[test]
    fn 파서는_파일_hunk_플러스마이너스_문맥을_분류한다() {
        let diff = "diff --git a/f.txt b/f.txt\n\
                    index 1234567..89abcde 100644\n\
                    --- a/f.txt\n\
                    +++ b/f.txt\n\
                    @@ -1,2 +1,2 @@\n \
                    context\n\
                    -old\n\
                    +new\n";
        let rows = parse_unified_diff(diff);
        let kinds: Vec<DiffLineKind> = rows.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiffLineKind::FileHeader,
                DiffLineKind::Meta,
                DiffLineKind::Meta,
                DiffLineKind::Meta,
                DiffLineKind::HunkHeader,
                DiffLineKind::Context,
                DiffLineKind::Removal,
                DiffLineKind::Addition,
            ]
        );
        assert_eq!(rows[6].1, "-old");
        assert_eq!(rows[7].1, "+new");
    }

    #[test]
    fn 파서는_hunk_안의_트리플대시를_삭제줄로_본다() {
        // "-- foo" 내용의 삭제 줄은 "--- foo"로 나타난다 — 헤더 `--- a/f`와 달리
        // hunk 내부이므로 Removal이어야 한다 (상태 플래그가 이 충돌을 푼다).
        let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n--- foo\n+++ bar\n";
        let rows = parse_unified_diff(diff);
        assert_eq!(rows[1].0, DiffLineKind::Meta, "{:?}", rows[1]);
        assert_eq!(rows[4].0, DiffLineKind::Removal, "{:?}", rows[4]);
        assert_eq!(rows[5].0, DiffLineKind::Addition, "{:?}", rows[5]);
    }

    #[test]
    fn 파서는_빈_diff에서_빈_행을_낸다() {
        assert!(parse_unified_diff("").is_empty());
    }

    #[test]
    fn 클립은_거대_diff를_줄_상한에서_자른다() {
        let huge = "+line\n".repeat(MAX_DIFF_LINES + 100);
        let (clipped, truncated) = clip_diff(&huge);
        assert!(truncated);
        assert_eq!(clipped.lines().count(), MAX_DIFF_LINES);
        let (kept, untruncated) = clip_diff("+a\n+b\n");
        assert!(!untruncated);
        assert_eq!(kept, "+a\n+b\n");
    }

    #[test]
    fn 클립은_바이트_상한을_멀티바이트_경계에서_안전하게_자른다() {
        // 줄당 ~40바이트 한글 줄로 바이트 상한을 먼저 넘긴다 — 개행 경계 슬라이스라
        // char boundary panic이 없어야 한다.
        let huge = "+한글변경줄한글변경줄\n".repeat(MAX_DIFF_BYTES / 30);
        let (clipped, truncated) = clip_diff(&huge);
        assert!(truncated);
        assert!(clipped.len() <= MAX_DIFF_BYTES);
        assert!(clipped.lines().all(|line| line == "+한글변경줄한글변경줄"));
    }

    #[test]
    fn status_z_파싱은_rename의_원경로를_함께_담는다() {
        let raw = "RM new.txt\0old.txt\0?? add.txt\0 M mod.txt\0";
        let entries = parse_status_z(raw, false);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].xy, "RM");
        assert_eq!(entries[0].path, "new.txt");
        assert_eq!(entries[0].orig.as_deref(), Some("old.txt"));
        assert_eq!(status_display(&entries[0]), "RM old.txt -> new.txt");
        assert_eq!(entries[1].xy, "??");
        assert_eq!(entries[2].xy, " M");
        // 잘린 출력이면 NUL로 끝나지 않은 마지막 부분 항목을 버린다.
        let clipped = parse_status_z("?? a.txt\0?? b.tx", true);
        assert_eq!(clipped.len(), 1);
        assert_eq!(clipped[0].path, "a.txt");
    }

    #[test]
    fn open은_capacity_one_intent만_만들고_completion은_generation에_묶인다() {
        let context = egui::Context::default();
        let mut panel = DiffPanelUi::new();
        panel.open_for(
            &context,
            "workspace-a".to_owned(),
            runtime::SessionId(1),
            Some("/private/repo-a".to_owned()),
            "A".to_owned(),
        );
        let old = panel.take_io_intent().expect("initial intent");
        assert!(panel.take_io_intent().is_none(), "capacity must be one");

        panel.open_for(
            &context,
            "workspace-b".to_owned(),
            runtime::SessionId(2),
            Some("/private/repo-b".to_owned()),
            "B".to_owned(),
        );
        let current = panel.take_io_intent().expect("replacement intent");
        assert_ne!(old.generation, current.generation);

        panel.complete_io(DiffIoCompletion {
            operation: old.operation,
            generation: old.generation,
            result: Ok(snapshot("/private/repo-a", Vec::new())),
        });
        assert!(panel.snapshot.is_none(), "stale completion must be ignored");
        assert_eq!(
            panel.pending,
            Some((current.operation, current.generation)),
            "stale completion must not clear the current operation"
        );

        panel.complete_io(DiffIoCompletion {
            operation: current.operation,
            generation: current.generation,
            result: Ok(snapshot("/private/repo-b", Vec::new())),
        });
        assert!(panel.pending.is_none());
        assert_eq!(
            panel
                .snapshot
                .as_ref()
                .map(|value| value.repo_root.as_str()),
            Some("/private/repo-b")
        );
    }

    #[test]
    fn path_target은_session없이_같은_bounded_intent를_만든다() {
        let context = egui::Context::default();
        let mut panel = DiffPanelUi::new();
        panel.open_for_path(
            &context,
            "workspace-a".to_owned(),
            "/private/repo-a".to_owned(),
            "History task".to_owned(),
        );

        let intent = panel.take_io_intent().expect("path target intent");
        assert!(panel.take_io_intent().is_none(), "capacity must remain one");
        assert_eq!(panel.session, None);
        assert_eq!(panel.cwd.as_deref(), Some("/private/repo-a"));
        assert_eq!(panel.title, "History task");

        panel.complete_io(DiffIoCompletion {
            operation: intent.operation,
            generation: intent.generation,
            result: Ok(snapshot("/private/repo-a", Vec::new())),
        });
        assert!(panel.pending.is_none());
        assert!(panel.snapshot.is_some());
    }

    #[test]
    fn diff_path는_32kib와_nul을_거부하고_debug를_redact한다() {
        let context = egui::Context::default();
        let mut panel = DiffPanelUi::new();
        panel.open_for(
            &context,
            "workspace".to_owned(),
            runtime::SessionId(1),
            Some(format!("/private/{}", "x".repeat(MAX_PATH_BYTES))),
            "title".to_owned(),
        );
        assert!(panel.take_io_intent().is_none());
        assert_eq!(panel.error, Some(DiffIoErrorCode::PathTooLarge));

        panel.open_for(
            &context,
            "workspace".to_owned(),
            runtime::SessionId(1),
            Some("/private/repo\0escape".to_owned()),
            "title".to_owned(),
        );
        assert!(panel.take_io_intent().is_none());
        assert_eq!(panel.error, Some(DiffIoErrorCode::InvalidPath));

        let payload = DiffPathPayload::try_new("/private/secret-repo".to_owned()).unwrap();
        let debug = format!("{payload:?}");
        assert!(debug.contains("REDACTED"), "{debug}");
        assert!(!debug.contains("secret-repo"), "{debug}");
        let row = DiffRow::FileHeader {
            path: "secret-name.txt".to_owned(),
            additions: 1,
            removals: 0,
            mtime: None,
        };
        assert!(!format!("{row:?}").contains("secret-name"));
    }

    #[test]
    fn immutable_snapshot은_최종_line과_byte상한을_초과하지_않는다() {
        let mut rows = (0..MAX_SNAPSHOT_ROWS + 100)
            .map(|_| DiffRow::Line(DiffLineKind::Context, "x".repeat(128)))
            .collect::<Vec<_>>();
        bound_snapshot_rows(&mut rows);
        assert!(rows.len() <= MAX_SNAPSHOT_ROWS);
        assert!(rows.iter().map(diff_row_retained_bytes).sum::<usize>() <= MAX_SNAPSHOT_BYTES);
        assert!(matches!(rows.last(), Some(DiffRow::Truncated)));
    }

    #[test]
    fn tracked_diff는_섹션당_file상한에서_잘린다() {
        let diff = (0..MAX_DIFF_FILES + 1)
            .map(|index| format!("diff --git a/f{index} b/f{index}\n"))
            .collect::<String>();
        let (rows, truncated) = build_file_rows(&diff, Path::new("/nonexistent")).unwrap();
        assert!(truncated);
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, DiffRow::FileHeader { .. }))
                .count(),
            MAX_DIFF_FILES
        );
    }

    #[test]
    fn stable_snapshot_300_frames는_host_intent를_만들지_않는다() {
        let context = egui::Context::default();
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut panel = DiffPanelUi::new();
        panel.open = true;
        panel.workspace_id = "workspace".to_owned();
        panel.session = Some(runtime::SessionId(1));
        panel.cwd = Some("/private/repo".to_owned());
        panel.title = "Workspace · Agent".to_owned();
        panel.snapshot = Some(snapshot("/private/repo", Vec::new()));
        for _ in 0..300 {
            let _ = context.run_ui(egui::RawInput::default(), |ui| {
                panel.show(ui.ctx(), &catalog)
            });
            assert!(panel.take_io_intent().is_none());
        }
        assert!(panel.pending.is_none());
    }

    #[test]
    fn closed_panel은_intent_pending_snapshot_target을_모두_버린다() {
        let context = egui::Context::default();
        let mut panel = DiffPanelUi::new();
        panel.open_for(
            &context,
            "workspace".to_owned(),
            runtime::SessionId(1),
            Some("/private/repo".to_owned()),
            "Workspace · Agent".to_owned(),
        );
        panel.snapshot = Some(snapshot("/private/repo", Vec::new()));
        panel.clear_closed_state();
        assert!(!panel.open);
        assert!(panel.queued_intent.is_none());
        assert!(panel.pending.is_none());
        assert!(panel.snapshot.is_none());
        assert!(panel.workspace_id.is_empty());
        assert!(panel.session.is_none());
        assert!(panel.cwd.is_none());
        assert!(panel.title.is_empty());
    }

    #[test]
    fn show_render_source에는_host_io_polling_spawn이_없다() {
        let source = include_str!("diff_panel.rs");
        let render = source
            .split_once("    pub fn show(&mut self")
            .expect("show marker")
            .1
            .split_once("\n}\n\n#[cfg(test)]")
            .expect("test marker")
            .0;
        for forbidden in [
            "std::fs::",
            "crate::git_cli",
            "std::process",
            "std::thread",
            "std::sync::mpsc",
            "try_recv",
            "request_repaint",
            "request_repaint_after",
        ] {
            assert!(
                !render.contains(forbidden),
                "render source contains forbidden host edge {forbidden}"
            );
        }
    }

    // ── 통합: 임시 git repo (git_cli::tests::temp_repo 패턴) ──

    fn temp_repo() -> std::path::PathBuf {
        static NEXT_REPO: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let dir = std::env::temp_dir().join(format!(
            "deppy-diffpanel-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_REPO.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        crate::git_cli::run_git(&dir, &["init", "-q"], GIT_TIMEOUT).unwrap();
        dir
    }

    fn commit_all(repo: &Path, message: &str) {
        crate::git_cli::run_git(repo, &["add", "."], GIT_TIMEOUT).unwrap();
        crate::git_cli::run_git(
            repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.t",
                "commit",
                "-q",
                "-m",
                message,
            ],
            GIT_TIMEOUT,
        )
        .unwrap();
    }

    fn section_index(rows: &[DiffRow], key: &str) -> usize {
        rows.iter()
            .position(|row| matches!(row, DiffRow::Section(k) if *k == key))
            .unwrap_or_else(|| panic!("{key} 섹션 없음: {rows:?}"))
    }

    #[test]
    fn collect는_수정과_스테이징을_각_섹션의_플러스마이너스로_잡는다() {
        let repo = temp_repo();
        std::fs::write(repo.join("a.txt"), "old line\n").unwrap();
        commit_all(&repo, "init");
        // unstaged 수정 + 별도 파일 staged 추가
        std::fs::write(repo.join("a.txt"), "new line\n").unwrap();
        std::fs::write(repo.join("b.txt"), "staged line\n").unwrap();
        crate::git_cli::run_git(&repo, &["add", "b.txt"], GIT_TIMEOUT).unwrap();

        let data = collect_diff(&repo).unwrap();
        let rows = data.rows.as_ref();
        let unstaged = section_index(rows, "diff.section.unstaged");
        let staged = section_index(rows, "diff.section.staged");
        assert!(section_index(rows, "diff.section.status") < unstaged);
        assert!(unstaged < staged);
        let removal = rows
            .iter()
            .position(
                |row| matches!(row, DiffRow::Line(DiffLineKind::Removal, l) if l == "-old line"),
            )
            .expect("unstaged 삭제 줄");
        let addition = rows
            .iter()
            .position(
                |row| matches!(row, DiffRow::Line(DiffLineKind::Addition, l) if l == "+new line"),
            )
            .expect("unstaged 추가 줄");
        assert!(unstaged < removal && removal < staged);
        assert!(unstaged < addition && addition < staged);
        assert!(
            rows.iter().skip(staged).any(|row| matches!(
                row,
                DiffRow::Line(DiffLineKind::Addition, l) if l == "+staged line"
            )),
            "staged 섹션의 추가 줄: {rows:?}"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn collect는_깨끗한_트리에서_빈_행이다() {
        let repo = temp_repo();
        std::fs::write(repo.join("a.txt"), "line\n").unwrap();
        commit_all(&repo, "init");
        let data = collect_diff(&repo).unwrap();
        assert!(data.rows.is_empty(), "{:?}", data.rows);
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn app_host_execute_io는_identity를_보존하고_bounded_snapshot을_돌려준다() {
        let repo = temp_repo();
        std::fs::write(repo.join("a.txt"), "line\n").unwrap();
        commit_all(&repo, "init");
        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        let context = egui::Context::default();
        let mut panel = DiffPanelUi::new();
        panel.open_for(
            &context,
            "workspace".to_owned(),
            runtime::SessionId(1),
            Some(repo.display().to_string()),
            "title".to_owned(),
        );
        let intent = panel.take_io_intent().unwrap();
        let identity = (intent.operation, intent.generation);
        let completion = execute_io(intent);
        assert_eq!((completion.operation, completion.generation), identity);
        let snapshot = completion.result.unwrap();
        assert!(snapshot.rows.len() <= MAX_SNAPSHOT_ROWS);
        assert!(snapshot.retained_bytes <= MAX_SNAPSHOT_BYTES);
        panel.complete_io(DiffIoCompletion {
            operation: identity.0,
            generation: identity.1,
            result: Ok(snapshot),
        });
        assert!(panel.pending.is_none());
        assert!(panel.snapshot.is_some());
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn collect는_untracked_새_파일_내용을_잡는다() {
        // codex 리뷰 P1 회귀 — 스테이징 전 신규 파일은 status에만 뜨고 diff 본문이
        // 없었다. untracked 섹션이 + 줄로 내용을 보여줘야 한다.
        let repo = temp_repo();
        std::fs::write(repo.join("a.txt"), "line\n").unwrap();
        commit_all(&repo, "init");
        std::fs::write(repo.join("fresh.txt"), "untracked content\n").unwrap();

        let data = collect_diff(&repo).unwrap();
        let rows = data.rows.as_ref();
        let untracked = section_index(rows, "diff.section.untracked");
        assert!(
            rows.iter().skip(untracked).any(|row| matches!(
                row,
                DiffRow::Line(DiffLineKind::Addition, l) if l == "+untracked content"
            )),
            "untracked 내용의 + 줄: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| matches!(row, DiffRow::Status(l) if l == "?? fresh.txt")),
            "status의 ?? 항목: {rows:?}"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn collect는_color_와_external_diff_설정이_있어도_정상_출력을_받는다() {
        // codex 리뷰 P2 회귀 — color.ui=always는 파이프에도 ANSI를 섞고
        // diff.external은 외부 명령을 띄운다. -c 무력화 + --no-ext-diff로 차단한다.
        let repo = temp_repo();
        std::fs::write(repo.join("a.txt"), "old\n").unwrap();
        commit_all(&repo, "init");
        crate::git_cli::run_git(&repo, &["config", "color.ui", "always"], GIT_TIMEOUT).unwrap();
        crate::git_cli::run_git(
            &repo,
            &["config", "diff.external", "/nonexistent-external-diff"],
            GIT_TIMEOUT,
        )
        .unwrap();
        std::fs::write(repo.join("a.txt"), "new\n").unwrap();
        std::fs::write(repo.join("fresh.txt"), "u\n").unwrap();

        let data = collect_diff(&repo).unwrap();
        for row in data.rows.iter() {
            let text = match row {
                DiffRow::Status(line) | DiffRow::Line(_, line) => line.as_str(),
                _ => continue,
            };
            assert!(!text.contains('\u{1b}'), "ANSI 이스케이프 발견: {text:?}");
        }
        // 색이 섞였다면 ±가 Context로 빠진다 — 분류까지 확인.
        assert!(
            data.rows.iter().any(|row| matches!(
                row,
                DiffRow::Line(DiffLineKind::Addition, l) if l == "+new"
            )),
            "{:?}",
            data.rows
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    /// 2026-07-18 사용자 회귀: raw "diff --git a/X b/X" + "index …"/"---"/"+++" 4줄이
    /// 파일 하나당 그대로 노출돼 "파일명이나 변경 이력이 가독성이 나쁘다"는 지적을
    /// 받았다. FileHeader 행이 경로·±통계를 담고, 중복 메타 줄은 걸러져야 한다.
    #[test]
    fn build_file_rows는_raw_헤더_대신_경로와_통계를_담는다() {
        let repo = temp_repo();
        std::fs::write(repo.join("a.txt"), "1\n2\n3\n").unwrap();
        commit_all(&repo, "init");
        std::fs::write(repo.join("a.txt"), "1\nX\n3\n4\n").unwrap();
        let (diff, _) = crate::git_cli::run_git_limited(
            &repo,
            &["diff", "--no-ext-diff"],
            GIT_TIMEOUT,
            MAX_DIFF_BYTES,
        )
        .unwrap();

        let (rows, truncated) = build_file_rows(&diff, &repo).unwrap();
        assert!(!truncated);
        let header = rows
            .iter()
            .find_map(|row| match row {
                DiffRow::FileHeader {
                    path,
                    additions,
                    removals,
                    mtime,
                } => Some((path.clone(), *additions, *removals, mtime.clone())),
                _ => None,
            })
            .expect("FileHeader 행이 있어야 한다");
        assert_eq!(header.0, "a.txt");
        assert_eq!(header.1, 2, "+X +4 두 줄"); // +X, +4
        assert_eq!(header.2, 1, "-2 한 줄");
        assert!(
            header.3.is_some(),
            "방금 고친 파일이라 mtime이 있어야 한다: {rows:?}"
        );
        // index/---/+++ 는 파일명 헤더가 이미 보여주는 정보라 생략돼야 한다.
        assert!(
            !rows.iter().any(|row| matches!(
                row,
                DiffRow::Line(_, l) if l.starts_with("index ")
                    || l.starts_with("--- ")
                    || l.starts_with("+++ ")
            )),
            "중복 메타 줄이 남아 있다: {rows:?}"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    /// 파일 여러 개가 섞인 diff에서 각 파일의 ±통계가 서로 새지 않는지.
    #[test]
    fn build_file_rows는_파일별_통계를_분리한다() {
        let repo = temp_repo();
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        std::fs::write(repo.join("b.txt"), "b1\nb2\n").unwrap();
        commit_all(&repo, "init");
        std::fs::write(repo.join("a.txt"), "a\na2\na3\n").unwrap(); // +2
        std::fs::write(repo.join("b.txt"), "b1\n").unwrap(); // -1
        let (diff, _) = crate::git_cli::run_git_limited(
            &repo,
            &["diff", "--no-ext-diff"],
            GIT_TIMEOUT,
            MAX_DIFF_BYTES,
        )
        .unwrap();

        let (rows, truncated) = build_file_rows(&diff, &repo).unwrap();
        assert!(!truncated);
        let headers: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                DiffRow::FileHeader {
                    path,
                    additions,
                    removals,
                    ..
                } => Some((path.as_str(), *additions, *removals)),
                _ => None,
            })
            .collect();
        assert_eq!(headers, vec![("a.txt", 2, 0), ("b.txt", 0, 1)], "{rows:?}");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn relative_time_label은_구간별로_다른_문구를_낸다() {
        assert_eq!(relative_time_label(Duration::from_secs(5)), "방금 전");
        assert_eq!(relative_time_label(Duration::from_secs(90)), "1분 전");
        assert_eq!(relative_time_label(Duration::from_secs(3661)), "1시간 전");
        assert_eq!(relative_time_label(Duration::from_secs(90_000)), "1일 전");
    }

    #[test]
    fn collect는_레포가_아니면_에러다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-diffpanel-norepo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(collect_diff(&dir).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── kittest 렌더 스모크 ──

    #[test]
    fn kittest_수집_결과가_라벨로_렌더된다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut panel = DiffPanelUi::new();
        panel.open = true;
        panel.workspace_id = "ws-1".to_owned();
        panel.session = Some(runtime::SessionId(3));
        panel.cwd = Some("/tmp/repo".to_owned());
        panel.title = "SKRT · Claude".to_owned();
        let rows: Arc<[DiffRow]> = vec![
            DiffRow::Section("diff.section.unstaged"),
            DiffRow::FileHeader {
                path: "f.txt".to_owned(),
                additions: 1,
                removals: 1,
                mtime: Some("3분 전".to_owned()),
            },
            DiffRow::Line(DiffLineKind::Removal, "-old line".to_owned()),
            DiffRow::Line(DiffLineKind::Addition, "+new line".to_owned()),
        ]
        .into();
        panel.snapshot = Some(DiffSnapshot {
            repo_root: "/tmp/repo".to_owned(),
            retained_bytes: rows.iter().map(diff_row_retained_bytes).sum(),
            rows,
        });
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, panel: &mut DiffPanelUi| panel.show(ui.ctx(), &catalog),
            panel,
        );
        harness.run();
        // "세션 #3" 같은 내부 id 대신 App이 넘긴 표시명이 보여야 한다.
        harness.get_by_label("SKRT · Claude");
        harness.get_by_label("Unstaged changes");
        // 파일 헤더 — raw "diff --git a/f b/f" 대신 파일명·±통계·mtime 각각 라벨로.
        harness.get_by_label("f.txt");
        harness.get_by_label("+1");
        harness.get_by_label("−1");
        harness.get_by_label("· 3분 전");
        harness.get_by_label("-old line");
        harness.get_by_label("+new line");
        harness.get_by_label("Refresh");
    }

    #[test]
    fn kittest_cwd_미확인이면_안내를_표시한다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut panel = DiffPanelUi::new();
        panel.open = true;
        panel.session = Some(runtime::SessionId(7));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, panel: &mut DiffPanelUi| panel.show(ui.ctx(), &catalog),
            panel,
        );
        harness.run();
        harness.get_by_label("Session working folder not detected yet");
    }
}
