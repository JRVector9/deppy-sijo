//! 폴더 트리 사이드바 (docs/file-tree-design.md).
//!
//! 리소스 3원칙(§3): lazy listing(펼친 노드만), flat 평탄화 + `show_rows`
//! 가상화, immutable snapshot render. Leaf는 filesystem/watcher/thread/channel을 소유하지
//! 않고 bounded maintenance intent만 App host로 올린다.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 사이드바 세션 목록 항목 (§6 확장 — 좌측 패널은 트리+세션의 workspace 사이드바다,
/// 2026-07-05). App이 WorkspaceUi 스냅샷에서 조립해 넘긴다.
pub struct SessionEntry {
    pub tab: runtime::MuxTabId,
    pub pane: runtime::MuxPaneId,
    /// pane의 세션 id — App의 alert(주목) 추적 키.
    pub session: Option<runtime::SessionId>,
    pub title: String,
    /// agent 감지 상태 (Running/NeedsApproval/Done/Error/Idle). 셸은 항상 None —
    /// status 감지는 agent만(§PR-12). 세션 행 좌측 상태 레일 색으로 그린다.
    pub status: Option<runtime::SessionStatus>,
    /// 최신 화면 요약 (마지막 비어있지 않은 행 — 2026-07-05)
    pub summary: String,
    pub focused: bool,
    /// 미확인 완료/입력대기 — 레일을 6px로 굵힌다. 해당 pane 포커스 시 해제.
    pub attention: bool,
    /// 알림 도착 시 이미 포커스 중이던 pane의 1회 펄스 — (진행 0..1, 알림 색).
    pub pulse: Option<(f32, egui::Color32)>,
    /// 에이전트 2행: "Codex · gpt-5.5 · xhigh" (에이전트일 때만 Some → 3줄 렌더).
    pub agent_line: Option<String>,
    /// 에이전트 제목 옆 상태: "실행 중"/"유휴"/"승인 필요" 등.
    pub status_label: Option<String>,
    /// 저장된 에이전트 세션이 있고 지금 실행 중이 아님 — 컨텍스트 메뉴 '이어가기' 노출.
    pub resumable: bool,
    /// 세션 cwd가 감지 캐시에 있음 — 워크트리 메뉴 노출 조건. App이 채운다(PR-W).
    pub has_cwd: bool,
    /// 세션 cwd가 `.deppy/worktrees/` 하위 — 「워크트리 삭제」 메뉴 노출 조건.
    /// App이 채운다(2026-07-18).
    pub in_worktree: bool,
    /// 에이전트 3행: 최신 에이전트 응답/진행 메시지, 없으면 터미널 현재 줄/일반 활동 설명.
    pub status_line: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarWorkspaceState {
    Active,
    Warm,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarWorkspaceEntry {
    pub id: String,
    pub name: String,
    pub repo: Option<String>,
    pub state: SidebarWorkspaceState,
    pub summary: SidebarSessionSummary,
}

/// 접힌 워크스페이스 행에 표시할 세션 상태 총합. 한 세션은 정확히 한 상태에만 들어간다.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SidebarSessionSummary {
    pub running: usize,
    pub waiting: usize,
    pub done: usize,
    pub error: usize,
    pub idle: usize,
    /// 런타임이 내려가 현재 상태를 관측할 수 없는 복원 세션.
    pub inactive: usize,
    /// 런타임도 저장된 복원 세션도 없는 비활성 워크스페이스.
    pub no_sessions: bool,
}

impl SidebarSessionSummary {
    pub fn add(&mut self, status: Option<runtime::SessionStatus>, waiting: bool) {
        use runtime::SessionStatus as S;
        if waiting || matches!(status, Some(S::Waiting | S::NeedsApproval)) {
            self.waiting += 1;
        } else {
            match status {
                Some(S::Done) => self.done += 1,
                Some(S::Error) => self.error += 1,
                Some(S::Idle) => self.idle += 1,
                Some(S::Running) | None => self.running += 1,
                Some(S::Waiting | S::NeedsApproval) => unreachable!("handled above"),
            }
        }
    }

    pub fn inactive(count: usize) -> Self {
        Self {
            inactive: count,
            no_sessions: count == 0,
            ..Self::default()
        }
    }
}

pub struct SidebarSnapshot<'a> {
    pub active_workspace_id: &'a str,
    pub workspaces: &'a [SidebarWorkspaceEntry],
    pub view: super::agent_terminal::AgentTerminalView,
    /// 마지막 Home 열람 뒤 새로 도착한 공지 수 — Home 행에 작업함과 같은 배지로 표시.
    pub home_notice_count: usize,
    pub inbox_count: usize,
    /// fleet nav 배지 — 주목 필요한 에이전트 수(대기+오류). 0이면 숨김.
    pub fleet_count: usize,
    /// Agents 창 열림 여부 — 하단 nav 「에이전트」 행의 선택 상태 (2026-07-18).
    pub agents_open: bool,
}

/// 사이드바에서 App으로 올라가는 액션.
pub enum SidebarAction {
    SwitchWorkspace(String),
    ShowHome,
    /// 멀티에이전트 fleet 그리드로 전환(하단 nav). 재클릭 토글은 App이 현재 view로 결정.
    ShowFleet,
    /// 「작업함」 전체 페이지로 전환 (하단 nav, 2026-07-18). 재클릭 토글(터미널 복귀)은
    /// App이 현재 view를 보고 결정한다 — 이 모듈은 view를 바꾸지 않는다.
    ShowInbox,
    OpenAgents,
    /// root folder를 macOS가 거부한 상태에서 「개인정보 보호 및 보안 → 파일 및 폴더」를 연다.
    OpenMacosFileAccessSettings,
    /// 경로를 포커스된 터미널에 삽입 (FT-3)
    InsertPath(FileTreePathPayload),
    /// 포커스된 터미널에서 이 폴더로 cd 실행 (디렉터리 컨텍스트 메뉴, 2026-07-08)
    CdPath(FileTreePathPayload),
    /// 세션 목록에서 선택 — 해당 tab/pane으로 전환
    FocusSession {
        workspace_id: String,
        tab: runtime::MuxTabId,
        pane: runtime::MuxPaneId,
    },
    /// 세션 이름 변경 — pane 제목을 갱신한다(더블클릭/메뉴 인라인 편집).
    RenameSession {
        pane: runtime::MuxPaneId,
        title: String,
    },
    /// 세션의 현재 작업 폴더를 Finder(OS 기본)로 연다.
    OpenSessionFolder {
        session: runtime::SessionId,
    },
    /// 세션의 현재 작업 폴더 경로를 클립보드에 복사한다.
    CopySessionPath {
        session: runtime::SessionId,
    },
    /// 이 세션과 같은 작업 폴더에서 새 셸을 연다 (tmux식 복제).
    NewShellSameFolder {
        session: runtime::SessionId,
    },
    /// 저장된 에이전트 세션을 이 pane 셸에서 resume한다 (수동 이어가기).
    ResumeAgent {
        pane: runtime::MuxPaneId,
        session: runtime::SessionId,
        title: String,
    },
    /// pane 닫기 — 실행 중 세션이면 기존 확인 모달을 거친다.
    ClosePane {
        pane: runtime::MuxPaneId,
    },
    /// 이 세션 cwd 레포의 변경분(diff)을 본다 (「변경 보기」 메뉴).
    ShowDiff {
        session: runtime::SessionId,
    },
    /// 이 세션 레포의 새 git worktree를 만들고 그 폴더에서 셸을 연다 (PR-W).
    NewWorktreeCell {
        session: runtime::SessionId,
    },
    /// 이 세션 cwd가 속한 워크트리를 지운다(작업 디렉터리만 — 브랜치는 남긴다).
    /// 성공하면 같은 cwd를 쓰던 pane을 전부 닫는다(같은 폴더에서 새 셀로 만든
    /// 형제 pane 포함) — cwd가 사라진 셸을 남기지 않기 위함(2026-07-18).
    RemoveWorktree {
        session: runtime::SessionId,
    },
    /// 워크스페이스의 세션(pane)을 전부 닫는다 — 워크스페이스 자체(경로·설정·DB
    /// 기록)는 보존한다(설정의 「프로젝트 삭제」와 구분). 실행 중 에이전트를 죽일 수
    /// 있어 App이 확인 다이얼로그를 거친 뒤 수행한다.
    CloseWorkspace(String),
    /// 워크스페이스 표시명(별칭) 편집 모달을 연다 — 실제 폴더/경로는 불변.
    /// 편집 자체는 App 소유 모달이 하고(현재 별칭 원본은 App만 안다), 여기서는
    /// 대상 id만 올린다.
    RenameWorkspace(String),
    /// 워크스페이스가 하나도 없을 때의 빈 상태 CTA(큰 + 버튼) — 폴더 선택으로 새
    /// 워크스페이스를 만들어 전환한다. rfd 다이얼로그는 UI leaf가 아니라 App이 연다
    /// (기존 ws_create 관례, 2026-07-18).
    CreateWorkspaceFromPicker,
}

const FILE_TREE_IO_QUEUE_CAP: usize = 1;
const FILE_TREE_PATH_MAX_BYTES: usize = 32 * 1024;
const FILE_TREE_PATH_LIST_MAX_ITEMS: usize = 16;
const FILE_TREE_PATH_LIST_MAX_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileTreeIoOperation(u64);

/// App host 경계를 지나는 경로. raw path는 Debug에서 항상 숨기고 Clone/Serialize하지
/// 않는다. 생성 시 NUL/개별 byte 상한을 검증한다.
pub struct FileTreePathPayload {
    path: PathBuf,
    bytes: usize,
}

impl FileTreePathPayload {
    pub fn try_new(path: PathBuf) -> Result<Self, FileTreeIoErrorCode> {
        let display = path.to_string_lossy();
        if display.as_bytes().contains(&0) {
            return Err(FileTreeIoErrorCode::InvalidPath);
        }
        let bytes = display.len();
        if bytes == 0 || bytes > FILE_TREE_PATH_MAX_BYTES {
            return Err(FileTreeIoErrorCode::PathTooLarge);
        }
        Ok(Self { path, bytes })
    }

    pub fn as_path(&self) -> &Path {
        &self.path
    }

    pub fn into_path(self) -> PathBuf {
        self.path
    }
}

impl std::fmt::Debug for FileTreePathPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTreePathPayload")
            .field("path", &"REDACTED")
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl std::ops::Deref for FileTreePathPayload {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.as_path()
    }
}

pub struct FileTreePathListPayload {
    paths: Vec<PathBuf>,
    bytes: usize,
}

impl FileTreePathListPayload {
    pub fn try_new(paths: Vec<PathBuf>) -> Result<Self, FileTreeIoErrorCode> {
        if paths.is_empty() || paths.len() > FILE_TREE_PATH_LIST_MAX_ITEMS {
            return Err(FileTreeIoErrorCode::PathListTooLarge);
        }
        let mut bytes = 0usize;
        for path in &paths {
            let display = path.to_string_lossy();
            if display.as_bytes().contains(&0) || display.len() > FILE_TREE_PATH_MAX_BYTES {
                return Err(FileTreeIoErrorCode::InvalidPath);
            }
            bytes = bytes
                .checked_add(display.len())
                .ok_or(FileTreeIoErrorCode::PathListTooLarge)?;
            if bytes > FILE_TREE_PATH_LIST_MAX_BYTES {
                return Err(FileTreeIoErrorCode::PathListTooLarge);
            }
        }
        Ok(Self { paths, bytes })
    }

    pub fn into_paths(self) -> Vec<PathBuf> {
        self.paths
    }
}

impl std::fmt::Debug for FileTreePathListPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTreePathListPayload")
            .field("items", &self.paths.len())
            .field("bytes", &self.bytes)
            .finish()
    }
}

pub enum FileTreeIoRequest {
    Rename {
        source: FileTreePathPayload,
        name: String,
    },
    CreateDirectory {
        parent: FileTreePathPayload,
        name: String,
    },
    CreateFile {
        parent: FileTreePathPayload,
        name: String,
    },
    Move {
        root: FileTreePathPayload,
        source: FileTreePathPayload,
        destination: FileTreePathPayload,
    },
    CopyInto {
        sources: FileTreePathListPayload,
        destination: FileTreePathPayload,
    },
    PasteFromClipboard {
        destination: FileTreePathPayload,
    },
    Trash {
        target: FileTreePathPayload,
    },
    DeletePermanently {
        target: FileTreePathPayload,
    },
    CopyFileUrls {
        paths: FileTreePathListPayload,
    },
    OpenPath {
        target: FileTreePathPayload,
        require_openable_file: bool,
    },
}

impl std::fmt::Debug for FileTreeIoRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::Rename { .. } => "rename",
            Self::CreateDirectory { .. } => "create_directory",
            Self::CreateFile { .. } => "create_file",
            Self::Move { .. } => "move",
            Self::CopyInto { .. } => "copy_into",
            Self::PasteFromClipboard { .. } => "paste_from_clipboard",
            Self::Trash { .. } => "trash",
            Self::DeletePermanently { .. } => "delete_permanently",
            Self::CopyFileUrls { .. } => "copy_file_urls",
            Self::OpenPath { .. } => "open_path",
        };
        f.debug_struct("FileTreeIoRequest")
            .field("kind", &kind)
            .finish_non_exhaustive()
    }
}

pub struct FileTreeIoIntent {
    pub operation: FileTreeIoOperation,
    pub generation: u64,
    pub request: FileTreeIoRequest,
}

impl std::fmt::Debug for FileTreeIoIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTreeIoIntent")
            .field("operation", &self.operation)
            .field("generation", &self.generation)
            .field("request", &self.request)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileTreeIoErrorCode {
    Busy,
    InvalidPath,
    PathTooLarge,
    PathListTooLarge,
    InvalidName,
    Conflict,
    OutsideRoot,
    TrashUnavailable,
    NativeFailure,
}

pub struct FileTreeIoCompletion {
    pub operation: FileTreeIoOperation,
    pub generation: u64,
    pub result: Result<(), FileTreeIoErrorCode>,
}

const FILE_TREE_MAINTENANCE_QUEUE_CAP: usize = 1;
pub const FILE_TREE_LISTING_MAX_ITEMS: usize = 4_096;
pub const FILE_TREE_LISTING_MAX_BYTES: usize = 4 * 1024 * 1024;
const FILE_TREE_RETAINED_MAX_ITEMS: usize = 16_384;
const FILE_TREE_RETAINED_MAX_BYTES: usize = 16 * 1024 * 1024;
pub const FILE_TREE_WATCH_MAX_DIRECTORIES: usize = 256;
pub const FILE_TREE_WATCH_MAX_EVENTS: usize = 64;
const FILE_TREE_WATCH_MAX_BYTES: usize = 4 * 1024 * 1024;
const FILE_TREE_REFRESH_BACKLOG_CAP: usize = 64;
const FILE_TREE_WATCH_IGNORE_MAX_ITEMS: usize = 16;
const FILE_TREE_WATCH_IGNORE_MAX_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileTreeMaintenanceOperation(u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTreeListingItem {
    name: Arc<str>,
    is_dir: bool,
}

impl FileTreeListingItem {
    pub fn try_new(name: String, is_dir: bool) -> Result<Self, FileTreeMaintenanceErrorCode> {
        if name.is_empty() || name.as_bytes().contains(&0) || name.len() > FILE_TREE_PATH_MAX_BYTES
        {
            return Err(FileTreeMaintenanceErrorCode::InvalidSnapshot);
        }
        Ok(Self {
            name: Arc::from(name),
            is_dir,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_dir(&self) -> bool {
        self.is_dir
    }
}

/// Host가 한 디렉터리를 나열한 immutable 결과. 생성 시 item/byte 상한을 강제하므로
/// UI가 unbounded storage row나 `read_dir` iterator를 받지 않는다.
#[derive(Clone)]
pub struct FileTreeListingSnapshot {
    items: Arc<[FileTreeListingItem]>,
    bytes: usize,
}

impl FileTreeListingSnapshot {
    pub fn try_new(
        mut items: Vec<FileTreeListingItem>,
    ) -> Result<Self, FileTreeMaintenanceErrorCode> {
        if items.len() > FILE_TREE_LISTING_MAX_ITEMS {
            return Err(FileTreeMaintenanceErrorCode::ListingTooLarge);
        }
        let bytes = items.iter().try_fold(0usize, |total, item| {
            total
                .checked_add(item.name.len())
                .filter(|total| *total <= FILE_TREE_LISTING_MAX_BYTES)
                .ok_or(FileTreeMaintenanceErrorCode::ListingTooLarge)
        })?;
        items.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        Ok(Self {
            items: Arc::from(items),
            bytes,
        })
    }

    pub fn items(&self) -> &[FileTreeListingItem] {
        &self.items
    }

    fn bytes(&self) -> usize {
        self.bytes
    }
}

impl std::fmt::Debug for FileTreeListingSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTreeListingSnapshot")
            .field("items", &self.items.len())
            .field("bytes", &self.bytes)
            .finish()
    }
}

/// App-owned watcher가 유지할 비재귀 디렉터리 집합. raw path는 Debug에 노출하지 않으며
/// Clone/Serialize하지 않는다.
pub struct FileTreeWatchPlan {
    directories: Vec<PathBuf>,
    ignored_prefixes: Vec<PathBuf>,
    show_hidden: bool,
    bytes: usize,
}

impl FileTreeWatchPlan {
    fn try_new(
        directories: Vec<PathBuf>,
        ignored_prefixes: Vec<PathBuf>,
        show_hidden: bool,
    ) -> Result<Self, FileTreeMaintenanceErrorCode> {
        if directories.len() > FILE_TREE_WATCH_MAX_DIRECTORIES
            || ignored_prefixes.len() > FILE_TREE_WATCH_IGNORE_MAX_ITEMS
        {
            return Err(FileTreeMaintenanceErrorCode::WatchPlanTooLarge);
        }
        let bytes =
            directories
                .iter()
                .chain(&ignored_prefixes)
                .try_fold(0usize, |total, path| {
                    let display = path.to_string_lossy();
                    if display.is_empty()
                        || display.as_bytes().contains(&0)
                        || display.len() > FILE_TREE_PATH_MAX_BYTES
                    {
                        return Err(FileTreeMaintenanceErrorCode::InvalidSnapshot);
                    }
                    total
                        .checked_add(display.len())
                        .filter(|total| *total <= FILE_TREE_WATCH_MAX_BYTES)
                        .ok_or(FileTreeMaintenanceErrorCode::WatchPlanTooLarge)
                })?;
        Ok(Self {
            directories,
            ignored_prefixes,
            show_hidden,
            bytes,
        })
    }

    pub fn into_parts(self) -> (Vec<PathBuf>, Vec<PathBuf>, bool) {
        (self.directories, self.ignored_prefixes, self.show_hidden)
    }
}

impl std::fmt::Debug for FileTreeWatchPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTreeWatchPlan")
            .field("directories", &self.directories.len())
            .field("ignored_prefixes", &self.ignored_prefixes.len())
            .field("show_hidden", &self.show_hidden)
            .field("bytes", &self.bytes)
            .finish()
    }
}

pub enum FileTreeMaintenanceRequest {
    ListDirectory {
        root: FileTreePathPayload,
        directory: FileTreePathPayload,
        max_items: usize,
        max_bytes: usize,
    },
    ReplaceWatchSet(FileTreeWatchPlan),
}

impl std::fmt::Debug for FileTreeMaintenanceRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ListDirectory {
                max_items,
                max_bytes,
                ..
            } => f
                .debug_struct("ListDirectory")
                .field("root", &"REDACTED")
                .field("directory", &"REDACTED")
                .field("max_items", max_items)
                .field("max_bytes", max_bytes)
                .finish(),
            Self::ReplaceWatchSet(plan) => f.debug_tuple("ReplaceWatchSet").field(plan).finish(),
        }
    }
}

pub struct FileTreeMaintenanceIntent {
    pub operation: FileTreeMaintenanceOperation,
    pub generation: u64,
    pub request: FileTreeMaintenanceRequest,
}

impl std::fmt::Debug for FileTreeMaintenanceIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTreeMaintenanceIntent")
            .field("operation", &self.operation)
            .field("generation", &self.generation)
            .field("request", &self.request)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileTreeMaintenanceErrorCode {
    PermissionDenied,
    ListingTooLarge,
    WatchPlanTooLarge,
    InvalidSnapshot,
    WatchUnavailable,
    NativeFailure,
}

pub enum FileTreeMaintenanceResult {
    Listing(FileTreeListingSnapshot),
    WatchSetApplied,
}

pub struct FileTreeMaintenanceCompletion {
    pub operation: FileTreeMaintenanceOperation,
    pub generation: u64,
    pub result: Result<FileTreeMaintenanceResult, FileTreeMaintenanceErrorCode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileTreeWatchEventKind {
    DirtyDirectory,
    EnvFileChanged,
}

pub struct FileTreeWatchEvent {
    kind: FileTreeWatchEventKind,
    path: PathBuf,
    bytes: usize,
}

impl FileTreeWatchEvent {
    pub fn try_new(
        kind: FileTreeWatchEventKind,
        path: PathBuf,
    ) -> Result<Self, FileTreeMaintenanceErrorCode> {
        let display = path.to_string_lossy();
        if display.is_empty()
            || display.as_bytes().contains(&0)
            || display.len() > FILE_TREE_PATH_MAX_BYTES
        {
            return Err(FileTreeMaintenanceErrorCode::InvalidSnapshot);
        }
        let bytes = display.len();
        Ok(Self { kind, path, bytes })
    }
}

impl std::fmt::Debug for FileTreeWatchEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTreeWatchEvent")
            .field("kind", &self.kind)
            .field("path", &"REDACTED")
            .field("bytes", &self.bytes)
            .finish()
    }
}

/// Watcher callback backlog의 latest-only immutable snapshot. 이벤트 수와 path bytes는
/// 생성 시 고정 상한을 검증한다.
pub struct FileTreeWatchSnapshot {
    generation: u64,
    revision: u64,
    overflowed: bool,
    events: Arc<[FileTreeWatchEvent]>,
    bytes: usize,
}

impl FileTreeWatchSnapshot {
    pub fn try_new(
        generation: u64,
        revision: u64,
        overflowed: bool,
        events: Vec<FileTreeWatchEvent>,
    ) -> Result<Self, FileTreeMaintenanceErrorCode> {
        if events.len() > FILE_TREE_WATCH_MAX_EVENTS {
            return Err(FileTreeMaintenanceErrorCode::WatchPlanTooLarge);
        }
        let bytes = events.iter().try_fold(0usize, |total, event| {
            total
                .checked_add(event.bytes)
                .filter(|total| *total <= FILE_TREE_WATCH_MAX_BYTES)
                .ok_or(FileTreeMaintenanceErrorCode::WatchPlanTooLarge)
        })?;
        Ok(Self {
            generation,
            revision,
            overflowed,
            events: Arc::from(events),
            bytes,
        })
    }
}

impl std::fmt::Debug for FileTreeWatchSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTreeWatchSnapshot")
            .field("generation", &self.generation)
            .field("revision", &self.revision)
            .field("overflowed", &self.overflowed)
            .field("events", &self.events.len())
            .field("bytes", &self.bytes)
            .finish()
    }
}

enum PendingFileTreeMaintenance {
    Listing {
        operation: FileTreeMaintenanceOperation,
        generation: u64,
        path: PathBuf,
        preserve_expanded: Arc<HashSet<PathBuf>>,
    },
    WatchSet {
        operation: FileTreeMaintenanceOperation,
        generation: u64,
    },
}

struct PendingFileTreeIo {
    operation: FileTreeIoOperation,
    generation: u64,
    refresh: Vec<PathBuf>,
    trash_target: Option<PathBuf>,
    retry_edit: Option<EditState>,
}

/// 트리 노드. `children == None`은 아직 나열 안 됨(lazy).
/// 접으면 children을 버려 캐시는 항상 "펼친 노드"만 유지한다(§3 메모리 상한).
struct TreeNode {
    name: String,
    is_dir: bool,
    expanded: bool,
    children: Option<Vec<TreeNode>>,
}

impl TreeNode {
    fn new(name: String, is_dir: bool) -> Self {
        Self {
            name,
            is_dir,
            expanded: false,
            children: None,
        }
    }
}

/// 평탄화된 가시 행 (§3 가상화 — `show_rows`로 보이는 행만 렌더).
#[derive(Debug, Clone, PartialEq, Eq)]
struct FlatRow {
    path: PathBuf,
    name: String,
    depth: usize,
    is_dir: bool,
    expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RootListingError {
    PermissionDenied,
}

pub struct FileTreeUi {
    /// workspace 루트. None = path 미설정 → 안내 표시(§9-2).
    root: Option<PathBuf>,
    /// 루트 나열 실패 사유 (invalid root — 에러 라벨 + 트리 비활성, §9-2).
    root_error: Option<RootListingError>,
    /// 루트 디렉터리의 자식들. 루트 자체는 행으로 그리지 않는다.
    children: Option<Vec<TreeNode>>,
    /// 가시 행 평탄화 캐시 — 펼침/접힘/조작 시에만 재계산(§3).
    flat: Vec<FlatRow>,
    show_hidden: bool,
    file_search_open: bool,
    file_search: String,
    /// 사이드바 접힘 (Panel 폭만 줄인다 — 상태/캐시는 유지).
    collapsed: bool,
    /// 내장 SidePanel 리사이저 대신 사용하는 폭. 내장 리사이저는 드래그 가이드선을
    /// 하단 상태바까지 그리므로, 상태바 위에서 끝나는 전용 핸들로 직접 조절한다.
    sidebar_width: f32,
    /// 사이드바 최하단 내비게이션 뷰포트 높이. 위 경계선을 드래그해 조절하며,
    /// 작게 접었을 때는 내부 ScrollArea로 홈/작업함/플릿/에이전트를 탐색한다.
    navigation_section_height: f32,
    /// 마지막 조작 에러 (하단 빨간 라벨, §4).
    error: Option<String>,
    /// macOS/TCC 등에서 나열 권한이 거부된 디렉터리. 전역 오류로 승격하지 않고
    /// 해당 행만 비활성화해 상위 탐색과 나머지 트리를 계속 사용할 수 있게 한다.
    inaccessible_paths: HashSet<PathBuf>,
    /// Native mutation은 leaf에서 실행하지 않고 capacity-1 intent로 App host에 넘긴다.
    io_generation: u64,
    next_io_operation: u64,
    io_intent: Option<FileTreeIoIntent>,
    pending_io: Option<PendingFileTreeIo>,
    /// Listing/watch는 leaf가 실행하지 않고 capacity-1 maintenance intent로 올린다.
    maintenance_generation: u64,
    next_maintenance_operation: u64,
    maintenance_intent: Option<FileTreeMaintenanceIntent>,
    pending_maintenance: Option<PendingFileTreeMaintenance>,
    /// Watch burst와 mutation refresh를 조상 경로 축약하는 bounded backlog.
    pending_refresh_dirs: BTreeSet<PathBuf>,
    watch_plan_dirty: bool,
    last_watch_revision: u64,
    watch_ignore: Vec<PathBuf>,
    /// 진행 중인 백그라운드 조작 수 (>0이면 스피너 표시).
    in_flight: usize,
    /// 백그라운드 완료 시 UI를 깨우기 위한 컨텍스트.
    /// 인라인 편집 상태 (이름 변경/새 폴더, FT-3).
    edit: Option<EditState>,
    /// 휴지통 이동 실패 → 영구삭제 확인 대기 중인 경로 (§9-7).
    confirm_delete: Option<PathBuf>,
    /// 실측 행높이 (show_rows 자기보정). show_rows는 "모든 행 = 선언 높이" 계약인데
    /// 실제 행높이는 폰트 메트릭(한글 폰트 라인높이 등)에 따라 선언값과 어긋날 수 있고,
    /// 어긋나면 스크롤 위치·가시 범위가 리빌드마다 밀려 클릭이 다른 행에 떨어진다
    /// (2026-07-05 사용자 보고: 펼침 간헐 실패/재클릭 접힘 안 됨/위치 점프). 첫 프레임에
    /// 실제 그린 행높이를 재서 다음 프레임부터 그 값을 쓴다.
    measured_row_height: Option<f32>,
    /// `.env*` 파일 변경 후보. 숨김 파일 필터와 무관하게 기록해 env-warning 후보로 쓸 수 있다.
    env_warning_candidates: BTreeSet<PathBuf>,
    /// 세션 목록 이름 인라인 편집 중 (pane, 편집 버퍼). 우클릭/더블클릭으로 시작.
    session_name_edit: Option<(runtime::MuxPaneId, String)>,
    /// 워크스페이스별 세션 트리 펼침 상태. 포커스 전환과 독립적이어서 다른 workspace를
    /// 선택해도 기존 트리는 사용자가 직접 접기 전까지 유지된다.
    workspace_sessions_expanded: HashMap<String, bool>,
    /// 활성 변경을 감지해 이전 활성의 기본-open 상태를 map에 고정한다. 이 기록이 없으면
    /// 명시값이 없던 이전 workspace가 inactive가 되는 순간 default false로 닫힌다.
    last_sidebar_active_workspace: Option<String>,
    /// 마지막 외부 파일 붙여넣기(⌘V) 처리 시각 — 같은 제스처의 press(native)와
    /// release(egui fallback)가 두 번 복사하는 것을 막는다(터미널 PASTE_GESTURE 관례).
    last_external_paste: Option<std::time::Instant>,
    /// 이번 프레임 트리가 ⌘V를 소비했는지 — App이 터미널의 같은 제스처 붙여넣기를 누른다.
    consumed_paste_shortcut: bool,
    /// 이번 프레임 트리가 ⌘C를 소비했는지 — App이 터미널 선택 복사의 덮어쓰기를 누른다.
    consumed_copy_shortcut: bool,
    /// 사이드바 내 "워크스페이스·세션" 블록 높이(px) — 하단 폴더 트리와의 경계선을
    /// 드래그해 사용자가 직접 조절한다(2026-07-24 사용자 요청). 매 프레임 가용 높이
    /// 기준으로 재클램프하므로 창 크기가 바뀌어도 두 섹션 모두 최소 높이를 유지한다.
    workspace_section_height: f32,
}

/// 디렉터리 listing worker 결과. 큰 디렉터리 apply 비용도 쪼개기 위해 chunk로 전달한다.
/// 인라인 편집 (FT-3). focus는 첫 프레임에 TextEdit에 포커스를 1회 요청하는 플래그 —
/// 편집 중 키 입력이 터미널로 새지 않게 한다(§9-8: 터미널은 자기 response가
/// 포커스를 가질 때만 입력을 소비한다).
#[derive(Clone)]
enum EditState {
    Rename {
        path: PathBuf,
        buffer: String,
        focus: bool,
    },
    NewFolder {
        parent: PathBuf,
        buffer: String,
        focus: bool,
    },
    NewFile {
        parent: PathBuf,
        buffer: String,
        focus: bool,
    },
}

impl FileTreeUi {
    pub fn new(_egui_ctx: egui::Context) -> Self {
        Self {
            root: None,
            root_error: None,
            children: None,
            flat: Vec::new(),
            show_hidden: false,
            file_search_open: false,
            file_search: String::new(),
            collapsed: false,
            sidebar_width: 360.0,
            navigation_section_height: SIDEBAR_NAV_DEFAULT_HEIGHT,
            error: None,
            inaccessible_paths: HashSet::new(),
            io_generation: 1,
            next_io_operation: 1,
            io_intent: None,
            pending_io: None,
            maintenance_generation: 1,
            next_maintenance_operation: 1,
            maintenance_intent: None,
            pending_maintenance: None,
            pending_refresh_dirs: BTreeSet::new(),
            watch_plan_dirty: false,
            last_watch_revision: 0,
            watch_ignore: Vec::new(),
            in_flight: 0,
            edit: None,
            confirm_delete: None,
            measured_row_height: None,
            env_warning_candidates: BTreeSet::new(),
            session_name_edit: None,
            workspace_sessions_expanded: HashMap::new(),
            last_sidebar_active_workspace: None,
            last_external_paste: None,
            consumed_paste_shortcut: false,
            consumed_copy_shortcut: false,
            workspace_section_height: 270.0,
        }
    }

    /// 이번 프레임 트리가 소비한 (⌘V, ⌘C). App이 같은 프레임 터미널 이중 처리
    /// (경로 삽입 붙여넣기/선택 복사 pasteboard 덮어쓰기)를 누르는 데 쓴다. 읽으면 리셋.
    pub fn take_clipboard_shortcut_consumption(&mut self) -> (bool, bool) {
        (
            std::mem::take(&mut self.consumed_paste_shortcut),
            std::mem::take(&mut self.consumed_copy_shortcut),
        )
    }

    fn queue_io(
        &mut self,
        request: FileTreeIoRequest,
        refresh: Vec<PathBuf>,
        trash_target: Option<PathBuf>,
        retry_edit: Option<EditState>,
    ) -> Result<(), FileTreeIoErrorCode> {
        debug_assert_eq!(FILE_TREE_IO_QUEUE_CAP, 1);
        if self.io_intent.is_some() || self.pending_io.is_some() {
            return Err(FileTreeIoErrorCode::Busy);
        }
        let operation = FileTreeIoOperation(self.next_io_operation);
        self.next_io_operation = self.next_io_operation.wrapping_add(1).max(1);
        let generation = self.io_generation;
        self.io_intent = Some(FileTreeIoIntent {
            operation,
            generation,
            request,
        });
        self.pending_io = Some(PendingFileTreeIo {
            operation,
            generation,
            refresh,
            trash_target,
            retry_edit,
        });
        self.in_flight = 1;
        Ok(())
    }

    fn reject_io(&mut self, code: FileTreeIoErrorCode) {
        self.error = Some(file_tree_io_error_message(code).to_owned());
    }

    /// App host가 수행할 capacity-1 native mutation. leaf는 실행하지 않고 snapshot을
    /// 렌더한 뒤 이 intent만 반환한다.
    pub fn take_io_intent(&mut self) -> Option<FileTreeIoIntent> {
        self.io_intent.take()
    }

    /// App host 결과를 exact operation/generation으로 적용한다. stale 결과는 현재
    /// pending을 건드리지 않고 폐기한다.
    pub fn complete_io(&mut self, completion: FileTreeIoCompletion) {
        let Some(pending) = self.pending_io.as_ref() else {
            return;
        };
        if pending.operation != completion.operation
            || pending.generation != completion.generation
            || completion.generation != self.io_generation
        {
            return;
        }
        let pending = self.pending_io.take().expect("exact pending checked");
        self.in_flight = 0;
        match completion.result {
            Ok(()) => {
                self.error = None;
                for dir in pending.refresh {
                    self.reload_dir(&dir);
                }
            }
            Err(FileTreeIoErrorCode::TrashUnavailable) => {
                self.error = Some(
                    file_tree_io_error_message(FileTreeIoErrorCode::TrashUnavailable).to_owned(),
                );
                self.confirm_delete = pending.trash_target;
                self.edit = pending.retry_edit;
            }
            Err(code) => {
                self.edit = pending.retry_edit;
                self.reject_io(code);
            }
        }
    }

    /// App host가 수행할 capacity-1 listing/watch maintenance. Constructor와 render는
    /// thread/channel/watcher를 만들지 않고 이 의도만 반환한다.
    pub fn take_maintenance_intent(&mut self) -> Option<FileTreeMaintenanceIntent> {
        self.maintenance_intent.take()
    }

    /// Exact operation/generation으로만 immutable host snapshot을 적용한다. 루트
    /// 전환/접힘 후 도착한 stale completion은 현재 트리를 건드리지 않는다.
    pub fn complete_maintenance(&mut self, completion: FileTreeMaintenanceCompletion) {
        let Some(pending) = self.pending_maintenance.as_ref() else {
            return;
        };
        let exact = match pending {
            PendingFileTreeMaintenance::Listing {
                operation,
                generation,
                ..
            }
            | PendingFileTreeMaintenance::WatchSet {
                operation,
                generation,
            } => {
                *operation == completion.operation
                    && *generation == completion.generation
                    && *generation == self.maintenance_generation
            }
        };
        if !exact {
            return;
        }
        let pending = self
            .pending_maintenance
            .take()
            .expect("exact maintenance checked");
        match (pending, completion.result) {
            (
                PendingFileTreeMaintenance::Listing {
                    path,
                    preserve_expanded,
                    ..
                },
                Ok(FileTreeMaintenanceResult::Listing(snapshot)),
            ) => self.apply_listing_snapshot(path, snapshot, &preserve_expanded),
            (
                PendingFileTreeMaintenance::WatchSet { .. },
                Ok(FileTreeMaintenanceResult::WatchSetApplied),
            ) => {}
            (PendingFileTreeMaintenance::Listing { path, .. }, Err(code)) => {
                self.apply_maintenance_error(&path, code);
            }
            (PendingFileTreeMaintenance::WatchSet { .. }, Err(code)) => {
                self.error = Some(file_tree_maintenance_error_message(code).to_owned());
            }
            _ => {
                self.error = Some("파일 트리 host 결과 종류가 요청과 일치하지 않습니다".to_owned());
            }
        }
        self.drive_maintenance();
    }

    /// App-owned watcher가 event burst를 coalesce한 latest snapshot을 적용한다.
    /// 외부 event wake 시에만 호출되며 polling/repaint timer를 만들지 않는다.
    pub fn apply_watch_snapshot(&mut self, snapshot: FileTreeWatchSnapshot) {
        if snapshot.generation != self.maintenance_generation
            || snapshot.revision <= self.last_watch_revision
        {
            return;
        }
        self.last_watch_revision = snapshot.revision;
        let Some(root) = self.root.clone() else {
            return;
        };
        if snapshot.overflowed {
            self.pending_refresh_dirs.clear();
            self.enqueue_refresh_dir(root.clone());
        }
        for event in snapshot.events.iter() {
            if !event.path.starts_with(&root)
                || self
                    .watch_ignore
                    .iter()
                    .any(|prefix| event.path.starts_with(prefix))
            {
                continue;
            }
            match event.kind {
                FileTreeWatchEventKind::DirtyDirectory => {
                    if self.show_hidden || !has_hidden_component(&root, &event.path) {
                        self.enqueue_refresh_dir(event.path.clone());
                    }
                }
                FileTreeWatchEventKind::EnvFileChanged => {
                    if self.env_warning_candidates.len() < FILE_TREE_WATCH_MAX_EVENTS
                        || self.env_warning_candidates.contains(&event.path)
                    {
                        self.env_warning_candidates.insert(event.path.clone());
                    }
                }
            }
        }
        self.drive_maintenance();
    }

    fn enqueue_refresh_dir(&mut self, path: PathBuf) {
        let Some(root) = self.root.as_ref() else {
            return;
        };
        if !path.starts_with(root) {
            return;
        }
        insert_pending_watch_dir(&mut self.pending_refresh_dirs, path);
        if self.pending_refresh_dirs.len() > FILE_TREE_REFRESH_BACKLOG_CAP {
            self.pending_refresh_dirs.clear();
            self.pending_refresh_dirs.insert(root.clone());
        }
    }

    fn remove_refresh_subtree(&mut self, path: &Path) {
        self.pending_refresh_dirs
            .retain(|pending| !pending.starts_with(path));
        let cancel_current = matches!(
            self.pending_maintenance.as_ref(),
            Some(PendingFileTreeMaintenance::Listing { path: pending, .. }) if pending.starts_with(path)
        );
        if cancel_current {
            self.maintenance_generation = self.maintenance_generation.wrapping_add(1).max(1);
            self.maintenance_intent = None;
            self.pending_maintenance = None;
            if let Some(root) = self.root.clone() {
                self.pending_refresh_dirs.insert(root);
            }
        }
    }

    fn drive_maintenance(&mut self) {
        debug_assert_eq!(FILE_TREE_MAINTENANCE_QUEUE_CAP, 1);
        if self.maintenance_intent.is_some() || self.pending_maintenance.is_some() {
            return;
        }
        while let Some(path) = self.pending_refresh_dirs.pop_first() {
            let Some(root) = self.root.clone() else {
                continue;
            };
            if !self.should_list_directory(&path) {
                continue;
            }
            let Ok(root_payload) = FileTreePathPayload::try_new(root) else {
                self.error = Some("파일 트리 루트 경로가 허용된 크기를 초과했습니다".to_owned());
                return;
            };
            let Ok(directory) = FileTreePathPayload::try_new(path.clone()) else {
                self.error = Some("파일 트리 경로가 허용된 크기를 초과했습니다".to_owned());
                continue;
            };
            let operation = FileTreeMaintenanceOperation(self.next_maintenance_operation);
            self.next_maintenance_operation =
                self.next_maintenance_operation.wrapping_add(1).max(1);
            let generation = self.maintenance_generation;
            self.maintenance_intent = Some(FileTreeMaintenanceIntent {
                operation,
                generation,
                request: FileTreeMaintenanceRequest::ListDirectory {
                    root: root_payload,
                    directory,
                    max_items: FILE_TREE_LISTING_MAX_ITEMS,
                    max_bytes: FILE_TREE_LISTING_MAX_BYTES,
                },
            });
            self.pending_maintenance = Some(PendingFileTreeMaintenance::Listing {
                operation,
                generation,
                path,
                preserve_expanded: Arc::new(self.collect_expanded_paths()),
            });
            return;
        }
        if !self.watch_plan_dirty {
            return;
        }
        let mut directories = Vec::new();
        if let Some(root) = &self.root {
            directories.push(root.clone());
            directories.extend(
                self.flat
                    .iter()
                    .filter(|row| row.is_dir && row.expanded)
                    .map(|row| row.path.clone()),
            );
        }
        let plan = match FileTreeWatchPlan::try_new(
            directories,
            self.watch_ignore.clone(),
            self.show_hidden,
        ) {
            Ok(plan) => plan,
            Err(code) => {
                self.watch_plan_dirty = false;
                self.error = Some(file_tree_maintenance_error_message(code).to_owned());
                return;
            }
        };
        let operation = FileTreeMaintenanceOperation(self.next_maintenance_operation);
        self.next_maintenance_operation = self.next_maintenance_operation.wrapping_add(1).max(1);
        let generation = self.maintenance_generation;
        self.watch_plan_dirty = false;
        self.maintenance_intent = Some(FileTreeMaintenanceIntent {
            operation,
            generation,
            request: FileTreeMaintenanceRequest::ReplaceWatchSet(plan),
        });
        self.pending_maintenance = Some(PendingFileTreeMaintenance::WatchSet {
            operation,
            generation,
        });
    }

    fn should_list_directory(&self, path: &Path) -> bool {
        let Some(root) = self.root.as_ref() else {
            return false;
        };
        if path == root {
            return true;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            return false;
        };
        self.children
            .as_ref()
            .and_then(|children| node_ref(children, rel))
            .is_some_and(|node| node.is_dir && node.expanded)
    }

    fn apply_listing_snapshot(
        &mut self,
        path: PathBuf,
        snapshot: FileTreeListingSnapshot,
        preserve_expanded: &HashSet<PathBuf>,
    ) {
        let (retained_items, retained_bytes) = self.retained_usage_without(&path);
        if retained_items.saturating_add(snapshot.items().len()) > FILE_TREE_RETAINED_MAX_ITEMS
            || retained_bytes.saturating_add(snapshot.bytes()) > FILE_TREE_RETAINED_MAX_BYTES
        {
            self.error =
                Some("파일 트리 메모리 상한을 초과해 추가 항목을 보관하지 않습니다".to_owned());
            return;
        }
        self.inaccessible_paths.remove(&path);
        let nodes = snapshot
            .items()
            .iter()
            .map(|item| TreeNode::new(item.name().to_owned(), item.is_dir()))
            .collect();
        let prepared = prepare_listing_nodes(&path, nodes, preserve_expanded);
        if !self.replace_listing_children(&path, prepared) {
            return;
        }
        self.root_error = None;
        self.rebuild_flat();
        for child in self.expanded_direct_child_paths(&path) {
            self.enqueue_refresh_dir(child);
        }
    }

    fn retained_usage_without(&self, path: &Path) -> (usize, usize) {
        let Some(root) = self.root.as_ref() else {
            return (0, 0);
        };
        let Some(children) = self.children.as_ref() else {
            return (0, 0);
        };
        let total = tree_node_usage(children);
        if path == root {
            return (0, 0);
        }
        let Ok(rel) = path.strip_prefix(root) else {
            return total;
        };
        let replaced = children
            .iter()
            .find_map(|node| node_ref(std::slice::from_ref(node), rel))
            .and_then(|node| node.children.as_deref())
            .map(tree_node_usage)
            .unwrap_or((0, 0));
        (
            total.0.saturating_sub(replaced.0),
            total.1.saturating_sub(replaced.1),
        )
    }

    fn apply_maintenance_error(&mut self, path: &Path, code: FileTreeMaintenanceErrorCode) {
        if code == FileTreeMaintenanceErrorCode::PermissionDenied {
            if self.root.as_deref() == Some(path) {
                self.children = None;
                self.root_error = Some(RootListingError::PermissionDenied);
            } else {
                self.inaccessible_paths.insert(path.to_path_buf());
                self.collapse_directory(path);
            }
            self.rebuild_flat();
            return;
        }
        self.error = Some(file_tree_maintenance_error_message(code).to_owned());
    }

    fn collapse_directory(&mut self, path: &Path) {
        let Some(root) = self.root.as_ref() else {
            return;
        };
        let Ok(rel) = path.strip_prefix(root) else {
            return;
        };
        if let Some(node) = self
            .children
            .as_mut()
            .and_then(|children| node_mut(children, rel))
        {
            node.expanded = false;
            node.children = None;
        }
    }

    /// 루트 교체 (workspace 전환/경로 변경). 캐시를 버리고 루트만 다시 나열한다.
    /// 루트는 canonicalize해 보관한다 — 트리의 모든 행 경로가 canonical 기준이 되어
    /// 이동 가드(§9-4)·부분 재나열의 경로 비교가 일관된다.
    /// 워처 무시 prefix 설정 (앱 data dir 등). set_root 이전에 호출.
    pub fn set_watch_ignore(&mut self, prefixes: Vec<PathBuf>) {
        let bytes = prefixes.iter().try_fold(0usize, |total, path| {
            let display = path.to_string_lossy();
            if display.is_empty()
                || display.as_bytes().contains(&0)
                || display.len() > FILE_TREE_PATH_MAX_BYTES
            {
                return None;
            }
            total.checked_add(display.len())
        });
        if prefixes.len() > FILE_TREE_WATCH_IGNORE_MAX_ITEMS
            || bytes.is_none_or(|bytes| bytes > FILE_TREE_WATCH_IGNORE_MAX_BYTES)
        {
            self.error = Some("파일 감시 제외 경로가 허용된 크기를 초과했습니다".to_owned());
            return;
        }
        self.watch_ignore = prefixes;
        self.watch_plan_dirty = true;
        self.drive_maintenance();
    }

    /// 워처가 감지한 `.env*` 변경 후보를 꺼낸다. App이 매 프레임 소비해 .env 변경/삭제
    /// 시 dotenv 재동기화를 트리거한다(2026-07-08 — stale secret 주입 방지, codex High).
    pub fn take_env_warning_candidates(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.env_warning_candidates)
            .into_iter()
            .collect()
    }

    pub fn set_root(&mut self, root: Option<PathBuf>) {
        self.io_generation = self.io_generation.wrapping_add(1).max(1);
        self.io_intent = None;
        self.pending_io = None;
        self.in_flight = 0;
        self.maintenance_generation = self.maintenance_generation.wrapping_add(1).max(1);
        self.maintenance_intent = None;
        self.pending_maintenance = None;
        self.pending_refresh_dirs.clear();
        self.last_watch_revision = 0;
        // App snapshot의 bounded root를 그대로 사용한다. canonicalize/metadata는 host
        // repository가 listing 결과를 만들 때 수행하며 render leaf는 filesystem을 읽지 않는다.
        self.root = root;
        self.root_error = None;
        self.children = None;
        self.flat.clear();
        self.file_search.clear();
        self.file_search_open = false;
        self.error = None;
        self.inaccessible_paths.clear();
        self.edit = None;
        self.confirm_delete = None;
        self.env_warning_candidates.clear();
        self.watch_plan_dirty = true;
        if let Some(root) = self.root.clone() {
            self.enqueue_refresh_dir(root);
        }
        self.drive_maintenance();
    }

    /// 펼친 노드 전체를 재나열한다 (수동 새로고침 — 펼침 상태는 이월).
    fn refresh(&mut self) {
        let Some(root) = self.root.clone() else {
            return;
        };
        self.root_error = None;
        self.enqueue_refresh_dir(root);
        self.drive_maintenance();
    }

    /// flat 캐시 재계산 (펼침/접힘/숨김 토글/조작 후에만 호출).
    fn rebuild_flat(&mut self) {
        self.flat.clear();
        if let (Some(root), Some(children)) = (&self.root, &self.children) {
            flatten(children, root, 0, self.show_hidden, &mut self.flat);
        }
        self.watch_plan_dirty = true;
        self.drive_maintenance();
    }

    /// 디렉터리 행 클릭: 펼침 ↔ 접힘. 펼칠 때만 host listing, 접으면 캐시 해제.
    fn toggle_dir(&mut self, path: &Path) {
        if self.inaccessible_paths.contains(path) {
            return;
        }
        let Some(root) = self.root.clone() else {
            return;
        };
        let Ok(rel) = path.strip_prefix(&root) else {
            return;
        };
        let mut expand = false;
        let mut collapse = false;
        {
            let Some(node) = self.children.as_mut().and_then(|c| node_mut(c, rel)) else {
                return;
            };
            if node.expanded {
                node.expanded = false;
                node.children = None; // 접힌 노드 캐시 해제 (§3 메모리 상한)
                collapse = true;
            } else {
                node.expanded = true;
                node.children = Some(Vec::new());
                expand = true;
            }
        }
        if collapse {
            self.remove_refresh_subtree(path);
        }
        if expand {
            self.enqueue_refresh_dir(path.to_path_buf());
        }
        self.rebuild_flat();
    }

    /// 좌측 사이드바 렌더 (§6 — `egui::Panel::left`, CentralPanel 앞에서 호출할 것 §9-1).
    /// 반환: "터미널에 경로 삽입" 요청 경로 (호출측 App이 WriteInput으로 전달 — §6
    /// 유일한 runtime 접점을 App에 남긴다).
    pub fn panel(
        &mut self,
        ui: &mut egui::Ui,
        sessions_by_workspace: &HashMap<String, Vec<SessionEntry>>,
        sidebar: &SidebarSnapshot<'_>,
        catalog: &i18n::Catalog,
    ) -> Option<SidebarAction> {
        let status_bar_top = ui.ctx().content_rect().bottom() - 26.0;
        let paint_separator = |ui: &egui::Ui, rect: egui::Rect, stroke: egui::Stroke| {
            let bottom = rect.bottom().min(status_bar_top);
            if bottom > rect.top() {
                ui.painter().vline(
                    rect.right(),
                    egui::Rangef::new(rect.top(), bottom),
                    stroke,
                );
            }
        };
        if self.collapsed {
            let panel = egui::Panel::left("file_tree_panel_collapsed")
                  .resizable(false)
                  .exact_size(22.0)
                  .show_separator_line(false)
                  .show(ui, |ui| {
                    crate::fonts::apply_sidebar_text_styles(ui);
                    if ui
                        .small_button("▸")
                        .on_hover_text(catalog.t("file_tree.expand_sidebar", &[]))
                        .clicked()
                    {
                        self.collapsed = false;
                      }
                  });
            paint_separator(
                ui,
                panel.response.rect,
                ui.visuals().widgets.noninteractive.bg_stroke,
            );
            return None;
        }
        self.sidebar_width = self.sidebar_width.clamp(40.0, 680.0);
        let panel = egui::Panel::left("file_tree_panel")
              .resizable(false)
              .exact_size(self.sidebar_width)
              .show_separator_line(false)
            // 패널 기본 inner_margin 제거 — 첫 워크스페이스가 상단 라인에 붙게
            // (2026-07-18 사용자). 각 행이 자체 좌측 인셋을 그리므로 여백 0이 안전.
            .frame(
                egui::Frame::side_top_panel(&ui.ctx().global_style())
                    .inner_margin(egui::Margin::ZERO)
                    .fill(SIDEBAR_BACKGROUND),
            )
            .show(ui, |ui| {
                crate::fonts::apply_sidebar_text_styles(ui);
                let available_height = ui.available_height();
                let (body_h, navigation_h) = sidebar_vertical_section_heights(
                    available_height,
                    self.navigation_section_height,
                );
                self.navigation_section_height = navigation_h;
                let body = ui
                    .allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), body_h),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| self.contents(ui, sessions_by_workspace, sidebar, catalog),
                    )
                    .inner;
                self.navigation_split_handle(ui, available_height);
                let navigation = ui
                    .allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), navigation_h),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("sidebar_navigation_scroll")
                                .auto_shrink([false, false])
                                .show(ui, |ui| self.navigation(ui, sidebar, catalog))
                                .inner
                        },
                    )
                    .inner;
                body.or(navigation)
              });
        let panel_rect = panel.response.rect;
        let resize_bottom = panel_rect.bottom().min(status_bar_top);
        let resize_rect = egui::Rect::from_min_max(
            egui::pos2(panel_rect.right() - 3.0, panel_rect.top()),
            egui::pos2(panel_rect.right() + 3.0, resize_bottom),
        );
        let resize_response = ui
            .interact(
                resize_rect,
                egui::Id::new("file_tree_sidebar_resize"),
                egui::Sense::drag(),
            )
            .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        if resize_response.dragged() {
            let delta_x = ui.input(|input| input.pointer.delta().x);
            self.sidebar_width = (self.sidebar_width + delta_x).clamp(40.0, 680.0);
            ui.ctx().request_repaint();
        }
        let separator_stroke = if resize_response.dragged() {
            ui.visuals().widgets.active.bg_stroke
        } else if resize_response.hovered() {
            ui.visuals().widgets.hovered.bg_stroke
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke
        };
        paint_separator(ui, panel_rect, separator_stroke);
        panel.inner
    }

    fn navigation_split_handle(&mut self, ui: &mut egui::Ui, available_height: f32) {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), SIDEBAR_NAV_SPLIT_HANDLE_HEIGHT),
            egui::Sense::hover(),
        );
        let response = ui
            .interact(
                rect.expand2(egui::vec2(0.0, 2.0)),
                ui.id().with("file_tree_navigation_split_handle"),
                egui::Sense::drag(),
            )
            .on_hover_cursor(egui::CursorIcon::ResizeVertical);
        if response.dragged() {
            let delta_y = ui.input(|input| input.pointer.delta().y);
            let (_, navigation_height) = sidebar_vertical_section_heights(
                available_height,
                self.navigation_section_height - delta_y,
            );
            self.navigation_section_height = navigation_height;
            ui.ctx().request_repaint();
        }
        let color = if response.hovered() || response.dragged() {
            ui.visuals().selection.bg_fill
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke.color
        };
        let y = ui.painter().round_to_pixel_center(rect.center().y);
        ui.painter()
            .hline(rect.x_range(), y, egui::Stroke::new(1.0, color));
    }

    /// 「워크스페이스·세션」 블록과 폴더 트리 사이 경계선 — 위아래로 끌면
    /// `workspace_section_height`가 바뀌어 두 섹션의 높이 비중을 조절한다
    /// (터미널 pane split 핸들과 동일한 hover/drag 스타일, workspace.rs 참고).
    /// 반환값 = 이번 프레임에 드래그 중인지 — 드래그로 아래 폴더 트리 행이 밀려
    /// 포인터 밑에 오면 hover 판정만으로 클릭 가능한 것처럼 보이는 오작동을
    /// 막기 위해 호출측이 행 상호작용을 잠시 꺼야 한다(2026-07-24 사용자 보고).
    fn workspace_split_handle(&mut self, ui: &mut egui::Ui) -> bool {
        let gap = 6.0;
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), gap), egui::Sense::hover());
        let hit_rect = rect.expand2(egui::vec2(0.0, 2.0));
        let id = ui.id().with("file_tree_workspace_split_handle");
        let resp = ui
            .interact(hit_rect, id, egui::Sense::drag())
            .on_hover_cursor(egui::CursorIcon::ResizeVertical);
        if resp.dragged() {
            self.workspace_section_height += resp.drag_delta().y;
        }
        let color = if resp.hovered() || resp.dragged() {
            ui.visuals().selection.bg_fill
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke.color
        };
        let painter = ui.painter();
        let y = painter.round_to_pixel_center(rect.center().y);
        painter.hline(ui.clip_rect().x_range(), y, egui::Stroke::new(1.0, color));
        resp.dragged()
    }

    fn contents(
        &mut self,
        ui: &mut egui::Ui,
        sessions_by_workspace: &HashMap<String, Vec<SessionEntry>>,
        sidebar: &SidebarSnapshot<'_>,
        catalog: &i18n::Catalog,
    ) -> Option<SidebarAction> {
        // (워처/백그라운드 채널 수거는 panel()이 접힘 여부와 무관하게 이미 수행했다)
        let mut action: Option<SidebarAction> = None;

        // ── 통합 워크스페이스·세션 계층 ──
        let compact_sidebar = ui.available_width() < 120.0;
        self.workspace_sessions_expanded.retain(|workspace_id, _| {
            sidebar
                .workspaces
                .iter()
                .any(|workspace| workspace.id == *workspace_id)
        });
        if self.last_sidebar_active_workspace.as_deref() != Some(sidebar.active_workspace_id) {
            if let Some(previous) = self.last_sidebar_active_workspace.as_ref() {
                self.workspace_sessions_expanded
                    .entry(previous.clone())
                    .or_insert(true);
            }
            self.workspace_sessions_expanded
                .entry(sidebar.active_workspace_id.to_owned())
                .or_insert(true);
            self.last_sidebar_active_workspace = Some(sidebar.active_workspace_id.to_owned());
        }
        if sidebar.workspaces.is_empty() {
            ui.add_space(3.0);
            // 빈 상태 — 워크스페이스가 하나도 없으면(종료 숨김 반영) 헤더/목록 대신
            // 가운데 큰 + 버튼과 안내문을 보여준다. 클릭 = 폴더 선택(App이 rfd로 열고
            // 기존 ws_create 흐름으로 생성·전환, 2026-07-18 사용자 요구).
            ui.add_space(18.0);
            ui.vertical_centered(|ui| {
                let side = 44.0_f32.min((ui.available_width() - 8.0).max(24.0));
                let plus = egui::Button::new(egui::RichText::new("+").size(24.0))
                    .min_size(egui::vec2(side, side));
                if ui
                    .add(plus)
                    .on_hover_text(catalog.t("sidebar.empty.start_workspace", &[]))
                    .clicked()
                {
                    action = Some(SidebarAction::CreateWorkspaceFromPicker);
                }
                if !compact_sidebar {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(catalog.t("sidebar.empty.start_workspace", &[])).weak(),
                    );
                }
            });
            ui.add_space(14.0);
        } else {
            // 헤더 바("워크스페이스 & 세션 +") 제거 — 첫 워크스페이스가 여백 없이
            // 상단에 붙는다(2026-07-18 사용자). 워크스페이스 추가(+)는 목록 아래로
            // 옮기고, 새 세션은 워크스페이스 우클릭 메뉴가 담당한다.
            // DB list_workspaces가 보장하는 created_at 순서를 그대로 그린다. 이전 구현은
            // 활성 workspace를 먼저 뽑아 맨 위에 렌더해 선택할 때마다 행이 이동했다.
            let (before_active, active, after_active) =
                workspace_creation_order_partition(sidebar.workspaces, sidebar.active_workspace_id);
            // 세션 블록 상한은 스크롤 진입 **전** 실제 패널 높이로 계산한다 — ScrollArea
            // 내부의 available_height는 사실상 무한이라 비례 계산이 무의미해진다.
            let session_max_h = (ui.available_height() * 0.34).clamp(70.0, 230.0);
            let active_sessions = sessions_by_workspace
                .get(sidebar.active_workspace_id)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let any_sessions_visible = sidebar.workspaces.iter().any(|workspace| {
                self.workspace_sessions_expanded
                    .get(&workspace.id)
                    .copied()
                    .unwrap_or(false)
                    && sessions_by_workspace
                        .get(&workspace.id)
                        .is_some_and(|sessions| !sessions.is_empty())
            });
            // 활성 워크스페이스가 생성순 뒤쪽이면 before_active 행들이 스크롤 밖에 그려져
            // 46px씩 사이드바 고정 높이를 잠식했다 (codex P2 — 세션·파일 트리가 클립 밖으로
            // 밀리는데 스크롤할 방법이 없었다). 전체 순서 목록(before + 활성 + 세션 + after)을
            // 하나의 bounded 스크롤 영역이 공유한다. 상한은 기존 워크스페이스 목록 예산에
            // 활성 행(46px)과 세션 블록 예산을 더한 값 — before가 없던 기존 화면과 동일한
            // 최악 높이를 유지하면서 before 행들만 스크롤로 흡수한다.
            let session_block_h = if any_sessions_visible {
                session_max_h
            } else {
                0.0
            };
            // 사용자가 아래 경계선(workspace_split_handle)을 드래그해 조절한 높이 —
            // 창 크기가 바뀌어도 안전하도록 매 프레임 가용 높이 기준으로 재클램프한다.
            let min_list_h = 118.0_f32;
            let max_list_h = (ui.available_height() - 160.0).max(min_list_h);
            self.workspace_section_height =
                self.workspace_section_height.clamp(min_list_h, max_list_h);
            let list_max_h = self.workspace_section_height + session_block_h;
            egui::ScrollArea::vertical()
                .id_salt("workspace_list_scroll")
                  .max_height(list_max_h)
                  .auto_shrink([false, true])
                  .show(ui, |ui| {
                      ui.painter()
                          .add(workspace_list_background_gradient(ui.clip_rect()));
                      ui.spacing_mut().item_spacing.y = 3.0;
                      ui.add_space(4.0);
                      for workspace in before_active {
                        // 워크스페이스 헤더 + 그 세션 목록을 한 카드(#0f171d 배경·
                        // #131c23 테두리)로 묶는다 — paint_workspace_group_wrap 주석 참고.
                        // 세션 구간만 살짝 다른 톤(#121a20)을 더 얹는다(inset_reserve) —
                        // paint_workspace_session_inset 참고.
                        let reserve = ui.painter().add(egui::Shape::Noop);
                        let inset_reserve = ui.painter().add(egui::Shape::Noop);
                        let inner = ui.scope(|ui| {
                            let color = workspace_accent(sidebar.workspaces, &workspace.id);
                            let expanded = self
                                .workspace_sessions_expanded
                                .get(&workspace.id)
                                .copied()
                                .unwrap_or(false);
                            let resp =
                                workspace_row(ui, workspace, color, false, Some(expanded), catalog);
                            let header_bottom = resp.rect.bottom();
                            workspace_context_menu(&resp, workspace, catalog, &mut action);
                            if resp.clicked() {
                                self.workspace_sessions_expanded
                                    .insert(workspace.id.clone(), true);
                                action = Some(SidebarAction::SwitchWorkspace(workspace.id.clone()));
                            }
                            let mut session_rows_rect = None;
                            if expanded
                                && let Some(sessions) = sessions_by_workspace.get(&workspace.id)
                            {
                                 let (session_action, rect) = inactive_workspace_sessions(
                                     ui,
                                     &workspace.id,
                                     sessions,
                                     session_max_h,
                                     color,
                                 );
                                if let Some(session_action) = session_action {
                                    action = Some(session_action);
                                }
                                session_rows_rect = rect;
                            }
                            if session_rows_rect.is_some() {
                                ui.add_space(2.5);
                            }
                            (header_bottom, session_rows_rect)
                        });
                        let group_rect = inner.response.rect;
                        let (header_bottom, session_rows_rect) = inner.inner;
                        paint_workspace_group_wrap(ui, reserve, group_rect);
                          paint_workspace_session_inset(
                              ui,
                              inset_reserve,
                            group_rect,
                            header_bottom,
                            session_rows_rect,
                        );
                    }
                    // 활성 워크스페이스 헤더 + 그 세션 목록을 하나의 카드 배경(#0f171d)·
                    // 테두리(#131c23)로 묶는다(2026-07-25 사용자). 배경 자리를 먼저
                    // 예약(add)해 두고, 아래 두 블록을 ui.scope로 감싸 실제 점유 rect를
                    // 얻은 뒤 그 자리에 칠한다(paint_workspace_group_wrap 참고) — 두
                    // 블록의 기존 조건(if let Some(active)/if active_sessions_visible)은
                    // 그대로 두어 동작을 바꾸지 않는다.
                     let active_group_reserve = ui.painter().add(egui::Shape::Noop);
                     let active_inset_reserve = ui.painter().add(egui::Shape::Noop);
                     let active_color =
                         workspace_accent(sidebar.workspaces, sidebar.active_workspace_id);
                     let active_inner = ui.scope(|ui| {
                         let mut header_bottom = None;
                         if let Some(active) = active {
                             let expanded = self
                                .workspace_sessions_expanded
                                .get(&active.id)
                                .copied()
                                .unwrap_or(true);
                             let resp =
                                 workspace_row(ui, active, active_color, true, Some(expanded), catalog);
                            header_bottom = Some(resp.rect.bottom());
                            workspace_context_menu(&resp, active, catalog, &mut action);
                            if resp.clicked() {
                                self.workspace_sessions_expanded
                                    .insert(active.id.clone(), !expanded);
                                // Home/Inbox/Agents에서 현재 활성 워크스페이스를 다시 눌러도
                                // App dispatch가 Terminal view로 복귀할 수 있게 명시적 전환을
                                // 방출한다. 같은 id의 runtime 전환은 App에서 no-op이다.
                                action = Some(SidebarAction::SwitchWorkspace(active.id.clone()));
                            }
                        }

                        // 현재 workspace의 셸/에이전트를 활성 워크스페이스 아래에 들여써 나열한다.
                        let mut session_rows_rect = None;
                        let active_sessions_visible = self
                            .workspace_sessions_expanded
                            .get(sidebar.active_workspace_id)
                            .copied()
                            .unwrap_or(true)
                            && !active_sessions.is_empty();
                        if active_sessions_visible {
                            // 세션이 많으면 목록이 패널을 다 먹고 아래로 넘쳐 잘렸다 (2026-07-05
                            // 사용자 보고). 세션 목록은 자기 상한 안에서만 스크롤하고, 나머지는
                            // 파일 트리가 갖는다. auto_shrink[_, true]로 세션이 적으면 줄어든다.
                            egui::ScrollArea::vertical()
                                .id_salt("session_list_scroll")
                                        .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    // 헤더-세션 사이 여백 없음(2026-07-25 사용자) — 첫 행이
                                    // 인셋 상단에 바로 붙는다.
                                    ui.spacing_mut().item_spacing.y = 0.0;
                                    for (index, entry) in active_sessions.iter().enumerate() {
                                        let is_last = index + 1 == active_sessions.len();
                                        ui.horizontal(|ui| {
                                            ui.add_space(16.0);
                                            ui.vertical(|ui| {
                                                let editing = matches!(
                                                    &self.session_name_edit,
                                                    Some((p, _)) if *p == entry.pane
                                                );
                                                if editing {
                                                    // 인라인 이름 편집 — Enter 확정(RenameSession), Esc 취소.
                                                    // 행(레일/상태줄) 레이아웃은 유지하고 제목 자리만 편집기로.
                                                    let buf = &mut self
                                                        .session_name_edit
                                                        .as_mut()
                                                        .unwrap()
                                                        .1;
                                                     let resp = session_row_editing(
                                                         ui,
                                                         entry,
                                                         buf,
                                                         is_last,
                                                         active_color,
                                                     );
                                                    {
                                                        let row_rect = resp.rect;
                                                        session_rows_rect = Some(
                                                            session_rows_rect.map_or(row_rect, |rect: egui::Rect| rect.union(row_rect)),
                                                        );
                                                    }
                                                    let (enter, esc) = ui.input(|i| {
                                                        (
                                                            i.key_pressed(egui::Key::Enter),
                                                            i.key_pressed(egui::Key::Escape),
                                                        )
                                                    });
                                                    if enter {
                                                        if let Some((pane, title)) =
                                                            self.session_name_edit.take()
                                                        {
                                                            let title = title.trim().to_owned();
                                                            if !title.is_empty() {
                                                                action = Some(
                                                                    SidebarAction::RenameSession {
                                                                        pane,
                                                                        title,
                                                                    },
                                                                );
                                                            }
                                                        }
                                                    } else if esc {
                                                        self.session_name_edit = None;
                                                    }
                                                } else {
                                                    // 세션 행 자체에는 hover tooltip을 띄우지 않는다.
                                                    // 상태 감지 출처/신뢰도 같은 내부 진단과 이름 변경
                                                    // 안내가 터미널 위를 가리는 문제(2026-07-19 사용자).
                                                 let resp =
                                                     session_row(ui, entry, is_last, active_color);
                                                    {
                                                        let row_rect = resp.rect;
                                                        session_rows_rect = Some(
                                                            session_rows_rect.map_or(row_rect, |rect: egui::Rect| rect.union(row_rect)),
                                                        );
                                                    }
                                                    // 우클릭 → 컨텍스트 메뉴(이름 변경/폴더/새 셸/이어가기/닫기).
                                                    // 더블클릭 → 이름 편집. 단순 클릭 → 세션 전환.
                                                    // (수동 상태 지정 U17b는 hook 감지 정착으로 제거 — 2026-07-17 사용자.)
                                                    if let Some(session) = entry.session {
                                                        resp.context_menu(|ui| {
                                                            if ui
                                                                .button(catalog.t(
                                                                    "workspace.rename_menu",
                                                                    &[],
                                                                ))
                                                                .clicked()
                                                            {
                                                                self.session_name_edit = Some((
                                                                    entry.pane.clone(),
                                                                    entry.title.clone(),
                                                                ));
                                                                ui.close();
                                                            }
                                                            ui.separator();
                                                            if ui
                                                                .button(catalog.t(
                                                                    "sidebar.menu.open_folder",
                                                                    &[],
                                                                ))
                                                                .clicked()
                                                            {
                                                                action = Some(
                                                                SidebarAction::OpenSessionFolder {
                                                                    session,
                                                                },
                                                            );
                                                                ui.close();
                                                            }
                                                            if ui
                                                                .button(catalog.t(
                                                                    "sidebar.menu.copy_path",
                                                                    &[],
                                                                ))
                                                                .clicked()
                                                            {
                                                                action = Some(
                                                                SidebarAction::CopySessionPath {
                                                                    session,
                                                                },
                                                            );
                                                                ui.close();
                                                            }
                                                            if ui
                                                                .button(catalog.t(
                                                                    "sidebar.menu.new_shell_here",
                                                                    &[],
                                                                ))
                                                                .clicked()
                                                            {
                                                                action = Some(
                                                                SidebarAction::NewShellSameFolder {
                                                                    session,
                                                                },
                                                            );
                                                                ui.close();
                                                            }
                                                            // 변경 보기 — 세션 cwd 레포의 git diff 패널 (PR-D).
                                                            if ui
                                                                .button(catalog.t(
                                                                    "sidebar.menu.show_diff",
                                                                    &[],
                                                                ))
                                                                .clicked()
                                                            {
                                                                action =
                                                                    Some(SidebarAction::ShowDiff {
                                                                        session,
                                                                    });
                                                                ui.close();
                                                            }
                                                            // 새 워크트리에서 셸 — cwd를 아는 세션만 (레포 판정은
                                                            // dispatch의 백그라운드 repo_root가 한다, PR-W).
                                                            if entry.has_cwd
                                                        && ui
                                                            .button(catalog.t(
                                                                "sidebar.menu.new_worktree_cell",
                                                                &[],
                                                            ))
                                                            .clicked()
                                                    {
                                                        action =
                                                            Some(SidebarAction::NewWorktreeCell {
                                                                session,
                                                            });
                                                        ui.close();
                                                    }
                                                            // 워크트리 삭제 — 이 세션 cwd가 `.deppy/worktrees/`
                                                            // 하위일 때만 노출(2026-07-18 사용자 제안).
                                                            if entry.in_worktree
                                                            && ui
                                                                .button(catalog.t(
                                                                    "sidebar.menu.remove_worktree",
                                                                    &[],
                                                                ))
                                                                .clicked()
                                                        {
                                                            action = Some(
                                                                SidebarAction::RemoveWorktree {
                                                                    session,
                                                                },
                                                            );
                                                            ui.close();
                                                        }
                                                            if entry.resumable
                                                                && ui
                                                                    .button(catalog.t(
                                                                        "sidebar.menu.resume_agent",
                                                                        &[],
                                                                    ))
                                                                    .clicked()
                                                            {
                                                                action = Some(
                                                                    SidebarAction::ResumeAgent {
                                                                        pane: entry.pane.clone(),
                                                                        session,
                                                                        title: entry.title.clone(),
                                                                    },
                                                                );
                                                                ui.close();
                                                            }
                                                            ui.separator();
                                                            if ui
                                                                .button(catalog.t(
                                                                    "sidebar.menu.close_pane",
                                                                    &[],
                                                                ))
                                                                .clicked()
                                                            {
                                                                action = Some(
                                                                    SidebarAction::ClosePane {
                                                                        pane: entry.pane.clone(),
                                                                    },
                                                                );
                                                                ui.close();
                                                            }
                                                        });
                                                    }
                                                    if resp.double_clicked() {
                                                        self.session_name_edit = Some((
                                                            entry.pane.clone(),
                                                            entry.title.clone(),
                                                        ));
                                                    } else if resp.clicked() && !entry.focused {
                                                        action =
                                                            Some(SidebarAction::FocusSession {
                                                                workspace_id: sidebar
                                                                    .active_workspace_id
                                                                    .to_owned(),
                                                                tab: entry.tab.clone(),
                                                                pane: entry.pane.clone(),
                                                            });
                                                    }
                                                }
                                            });
                                        });
                                    }
                                });
                        }
                        if session_rows_rect.is_some() {
                            ui.add_space(2.5);
                        }
                        (header_bottom, session_rows_rect)
                    });
                    let active_group_rect = active_inner.response.rect;
                    let (active_header_bottom, active_last_row_rect) = active_inner.inner;
                    paint_workspace_group_wrap(ui, active_group_reserve, active_group_rect);
                    // 헤더가 없으면(활성 workspace를 못 찾은 예외적 상태) 카드 전체를
                    // 세션 구간으로 본다.
                    paint_workspace_session_inset(
                        ui,
                        active_inset_reserve,
                        active_group_rect,
                        active_header_bottom.unwrap_or(active_group_rect.top()),
                        active_last_row_rect,
                    );
                    for workspace in after_active {
                        // before_active와 동일한 카드 배경/테두리 + 세션 인셋 묶음.
                        let reserve = ui.painter().add(egui::Shape::Noop);
                        let inset_reserve = ui.painter().add(egui::Shape::Noop);
                        let inner = ui.scope(|ui| {
                            let color = workspace_accent(sidebar.workspaces, &workspace.id);
                            let expanded = self
                                .workspace_sessions_expanded
                                .get(&workspace.id)
                                .copied()
                                .unwrap_or(false);
                            let resp =
                                workspace_row(ui, workspace, color, false, Some(expanded), catalog);
                            let header_bottom = resp.rect.bottom();
                            workspace_context_menu(&resp, workspace, catalog, &mut action);
                            if resp.clicked() {
                                self.workspace_sessions_expanded
                                    .insert(workspace.id.clone(), true);
                                action = Some(SidebarAction::SwitchWorkspace(workspace.id.clone()));
                            }
                            let mut session_rows_rect = None;
                            if expanded
                                && let Some(sessions) = sessions_by_workspace.get(&workspace.id)
                            {
                                let (session_action, rect) = inactive_workspace_sessions(
                                    ui,
                                    &workspace.id,
                                    sessions,
                                    session_max_h,
                                    color,
                                );
                                if let Some(session_action) = session_action {
                                    action = Some(session_action);
                                }
                                session_rows_rect = rect;
                            }
                            if session_rows_rect.is_some() {
                                ui.add_space(2.5);
                            }
                            (header_bottom, session_rows_rect)
                        });
                        let group_rect = inner.response.rect;
                        let (header_bottom, session_rows_rect) = inner.inner;
                        paint_workspace_group_wrap(ui, reserve, group_rect);
                        paint_workspace_session_inset(
                            ui,
                            inset_reserve,
                            group_rect,
                            header_bottom,
                              session_rows_rect,
                          );
                      }
                  });
        }
        ui.add_space(4.0);
        let resizing_workspace_split = self.workspace_split_handle(ui);

        // 독립 「파일」 제목행은 제거하고 현재 경로와 핵심 도구를 한 행에 합친다.
        // 패널이 극단적으로 좁아지면 검색 → 새 폴더 → 숨김 순으로 도구를 남겨
        // 40pt까지 실제로 축소할 수 있게 한다.
        let mut create_folder = false;
        let mut create_file = false;
        let (header_rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 38.0), egui::Sense::hover());
        let visible_tools = (((header_rect.width() - 4.0).max(0.0) / 20.0).floor() as usize).min(4);
        let mut tool_right = header_rect.right() - 4.0;
        if visible_tools >= 2 {
            let rect = egui::Rect::from_min_size(
                egui::pos2(tool_right - 20.0, header_rect.top() + 9.0),
                egui::vec2(20.0, 20.0),
            );
            create_folder =
                file_toolbar_icon_at(ui, rect, "new_folder", FileToolbarIcon::Folder, false)
                    .on_hover_text(catalog.t("file_tree.new_folder_root", &[]))
                    .clicked();
            tool_right -= 20.0;
        }
        if visible_tools >= 1 {
            let rect = egui::Rect::from_min_size(
                egui::pos2(tool_right - 20.0, header_rect.top() + 9.0),
                egui::vec2(20.0, 20.0),
            );
            let search = file_toolbar_icon_at(
                ui,
                rect,
                "search",
                FileToolbarIcon::Search,
                self.file_search_open,
            )
            .on_hover_text(catalog.t("file_tree.search", &[]));
            if search.clicked() {
                self.file_search_open = !self.file_search_open;
                if !self.file_search_open {
                    self.file_search.clear();
                }
            }
            tool_right -= 20.0;
        }
        if visible_tools >= 3 {
            let rect = egui::Rect::from_min_size(
                egui::pos2(tool_right - 20.0, header_rect.top() + 9.0),
                egui::vec2(20.0, 20.0),
            );
            let hidden = file_toolbar_icon_at(
                ui,
                rect,
                "hidden",
                FileToolbarIcon::Hidden,
                self.show_hidden,
            )
            .on_hover_text(if self.show_hidden {
                "숨김 파일 감추기"
            } else {
                "숨김 파일 표시"
            });
            if hidden.clicked() {
                self.show_hidden = !self.show_hidden;
                self.rebuild_flat();
            }
            tool_right -= 20.0;
        }
        // 새 파일 — 툴바 리팩토링(2026-07-18)에서 빠졌던 버튼 복원. EditState::NewFile
        // 소비 흐름(인라인 편집·커밋)은 그대로 살아 있어 생성 지점만 다시 잇는다.
        if visible_tools >= 4 {
            let rect = egui::Rect::from_min_size(
                egui::pos2(tool_right - 20.0, header_rect.top() + 9.0),
                egui::vec2(20.0, 20.0),
            );
            create_file = file_toolbar_icon_at(ui, rect, "new_file", FileToolbarIcon::File, false)
                .on_hover_text(catalog.t("file_tree.new_file_root", &[]))
                .clicked();
            tool_right -= 20.0;
        }

        let path_left = header_rect.left() + 10.0;
        if tool_right - path_left >= 20.0 {
            let icon_center = egui::pos2(path_left + 8.0, header_rect.center().y);
            // 최상단은 "현재 열린 폴더" 헤더 — 트리의 닫힌 폴더와 구분해 열린 폴더로
            // (고정 앵커 아님, 2026-07-19 사용자).
            paint_folder_open(
                ui.painter(),
                icon_center,
                ui.visuals().text_color(),
                egui::vec2(12.825, 11.875),
            );
            let text_left = path_left + 27.0;
            let text_width = (tool_right - text_left - 5.0).max(0.0);
            if text_width > 8.0
                && let Some(root) = &self.root
            {
                let name = root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root.display().to_string());
                let label = format!("{name}  {}", compact_root_path(root));
                let galley = clipped_line(
                    ui,
                    &label,
                    crate::fonts::sidebar_font(11.5),
                    text_width,
                    None,
                );
                ui.painter().galley(
                    egui::pos2(text_left, header_rect.center().y - galley.size().y / 2.0),
                    galley,
                    ui.visuals().text_color(),
                );
            }
        }

        if self.file_search_open {
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.file_search)
                    .hint_text(catalog.t("file_tree.search_hint", &[]))
                    .desired_width(f32::INFINITY)
                    .margin(egui::Margin::symmetric(8, 5)),
            );
            response.request_focus();
            if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                self.file_search_open = false;
                self.file_search.clear();
            }
        }

        let header_drop = ui.interact(
            header_rect,
            egui::Id::new("file_tree_root_drop"),
            egui::Sense::hover(),
        );
        if header_drop.dnd_hover_payload::<PathBuf>().is_some() {
            ui.painter().rect_stroke(
                header_rect,
                2.0,
                ui.visuals().widgets.active.bg_stroke,
                egui::StrokeKind::Inside,
            );
        }
        if let (Some(payload), Some(root)) = (
            header_drop.dnd_release_payload::<PathBuf>(),
            self.root.clone(),
        ) {
            self.start_move((*payload).clone(), root);
        }

        // 현재 표시 루트의 상위 폴더로 이동한다. 이는 파일 도크의 탐색 루트만
        // 바꾸며 workspace 경로와 터미널 cwd는 그대로 유지한다.
        let parent_root = self
            .root
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        let parent_sense = if parent_root.is_some() {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        };
        let (parent_rect, parent_response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 25.0), parent_sense);
        if parent_response.hovered() && parent_root.is_some() {
            ui.painter()
                .rect_filled(parent_rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
        }
        let parent_color = if parent_root.is_some() {
            ui.visuals().text_color()
        } else {
            ui.visuals().weak_text_color()
        };
        let parent_icon_center = egui::pos2(parent_rect.left() + 28.5, parent_rect.center().y);
        paint_folder(
            ui.painter(),
            parent_icon_center,
            parent_color,
            egui::vec2(12.825, 11.875),
        );
        ui.painter().text(
            egui::pos2(parent_rect.left() + 47.0, parent_rect.center().y),
            egui::Align2::LEFT_CENTER,
            "..",
            crate::fonts::sidebar_font(12.5),
            parent_color,
        );
        if parent_response.clicked()
            && let Some(parent) = parent_root
        {
            self.set_root(Some(parent));
            return action;
        }
        if let Some(root) = self.root.clone() {
            if create_file {
                self.edit = Some(EditState::NewFile {
                    parent: root,
                    buffer: String::new(),
                    focus: true,
                });
            } else if create_folder {
                self.edit = Some(EditState::NewFolder {
                    parent: root,
                    buffer: String::new(),
                    focus: true,
                });
            }
        }

        if self.root.is_none() {
            // path 미설정 + HOME도 없음(극히 드묾) — 트리 대신 안내(환경 메뉴로 유도).
            ui.weak(catalog.t("file_tree.set_project_path", &[]));
            ui.weak(catalog.t("file_tree.edit_workspace_path_hint", &[]));
            return action;
        }
        if let Some(error) = &self.root_error {
            match error {
                RootListingError::PermissionDenied => {
                    let area = ui.available_rect_before_wrap();
                    let button_rect = permission_denied_button_rect(area, ui.max_rect().center());
                    let accent = ui.visuals().selection.bg_fill;
                    let text_color = if ui.visuals().dark_mode {
                        egui::Color32::from_rgb(0x0f, 0x11, 0x17)
                    } else {
                        egui::Color32::WHITE
                    };
                    let open_settings = ui
                        .put(
                            button_rect,
                            egui::Button::new(
                                egui::RichText::new(
                                    catalog.t("file_tree.macos_access_denied", &[]),
                                )
                                .color(text_color)
                                .strong(),
                            )
                            .fill(accent)
                            .stroke(egui::Stroke::new(1.0, accent))
                            .corner_radius(2.0)
                            .wrap(),
                        )
                        .clicked();
                    if open_settings {
                        action = Some(SidebarAction::OpenMacosFileAccessSettings);
                    }
                }
            }
            return action;
        }

        // 인라인 편집 상태를 로컬로 꺼낸다 (flat 순회와 동시 &mut 회피, FT-3)
        let mut edit = self.edit.take();
        let mut edit_done: Option<bool> = None; // Some(true)=커밋, Some(false)=취소
        let mut menu_action: Option<MenuAction> = None;

        // 새 폴더 인라인 편집기 (헤더 아래 고정 행 — 가상화 행높이를 흔들지 않는다)
        if let Some(EditState::NewFolder {
            parent,
            buffer,
            focus,
        }) = &mut edit
        {
            ui.horizontal(|ui| {
                ui.label(catalog.t("file_tree.new_folder_label", &[]));
                let resp = ui.add(
                    egui::TextEdit::singleline(buffer)
                        .hint_text(catalog.t("common.name", &[]))
                        .desired_width(120.0),
                );
                if *focus {
                    resp.request_focus(); // §9-8 — 키가 터미널로 새지 않게 즉시 포커스
                    *focus = false;
                }
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.small_button(catalog.t("action.new", &[])).clicked() || enter {
                    edit_done = Some(true);
                } else if ui.small_button(catalog.t("action.cancel", &[])).clicked()
                    || ui.input(|i| i.key_pressed(egui::Key::Escape))
                {
                    edit_done = Some(false);
                }
            });
            ui.weak(catalog.t(
                "file_tree.location",
                &[("path", &parent.display().to_string())],
            ));
        }
        if let Some(EditState::NewFile {
            parent,
            buffer,
            focus,
        }) = &mut edit
        {
            ui.horizontal(|ui| {
                ui.label(catalog.t("file_tree.new_file_label", &[]));
                let resp = ui.add(
                    egui::TextEdit::singleline(buffer)
                        .hint_text(catalog.t("common.name", &[]))
                        .desired_width(150.0),
                );
                if *focus {
                    resp.request_focus();
                    *focus = false;
                }
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.small_button(catalog.t("action.new", &[])).clicked() || enter {
                    edit_done = Some(true);
                } else if ui.small_button(catalog.t("action.cancel", &[])).clicked()
                    || ui.input(|i| i.key_pressed(egui::Key::Escape))
                {
                    edit_done = Some(false);
                }
            });
            ui.weak(catalog.t(
                "file_tree.location",
                &[("path", &parent.display().to_string())],
            ));
        }

        // 헤더 우클릭: 루트에 새 폴더 (FT-3)
        if let Some(root) = self.root.clone() {
            header_drop.context_menu(|ui| {
                if ui
                    .button(catalog.t("file_tree.new_folder_root", &[]))
                    .clicked()
                {
                    menu_action = Some(MenuAction::NewFolder(root.clone()));
                    ui.close();
                }
            });
        }

        // 가상화: 고정 행높이 + path 기반 explicit Id (§9-6).
        // 행높이는 실측 자기보정 — 선언값과 실제가 어긋나면 클릭 대상이 밀린다(필드 주석).
        let row_height = self.measured_row_height.unwrap_or(25.0);
        let query = self.file_search.trim().to_lowercase();
        let visible_rows: Vec<usize> = self
            .flat
            .iter()
            .enumerate()
            .filter(|(_, row)| query.is_empty() || row.name.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect();
        let total = visible_rows.len();
        let mut toggle: Option<PathBuf> = None;
        let mut navigate_root: Option<PathBuf> = None;
        let mut open_file: Option<PathBuf> = None; // 파일 더블클릭 → 연결 프로그램 열기
        let mut drop_action: Option<(PathBuf, PathBuf)> = None; // (src, dst_dir)
        let mut observed_row_height: Option<f32> = None;
        // ── OS 파일 반입 상태 (Finder → 트리, §드롭·⌘V) ──
        // winit 0.30은 macOS draggingUpdated:를 구현하지 않아 드래그 중 포인터 이벤트가
        // 오지 않는다 — 대상 행 판정은 AppKit 마우스 위치를 창 좌표로 환산해 쓰고,
        // 실패(viewport 미상 — kittest 등)면 egui 포인터로 폴백한다.
        let os_drag_active = ui.input(|i| !i.raw.hovered_files.is_empty());
        let os_dropped: Vec<PathBuf> = ui.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect()
        });
        let drag_pos = (os_drag_active || !os_dropped.is_empty())
            .then(|| os_drag_pointer_pos(ui.ctx()).or_else(|| ui.input(|i| i.pointer.latest_pos())))
            .flatten();
        // 포인터 밑 행 기준 반입 대상(폴더 행=자신, 파일 행=부모) — 드롭(①)/⌘V(②) 공유.
        let mut hover_target_dir: Option<PathBuf> = None;
        let mut hover_row_path: Option<PathBuf> = None;
        let mut drop_target_dir: Option<PathBuf> = None;
        let mut drag_row_highlighted = false;
        let scroll_output = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, row_height, total, |ui, range| {
                for index in &visible_rows[range] {
                    let row = &self.flat[*index];
                    let inaccessible = self.inaccessible_paths.contains(&row.path);
                    // 이름 변경 중인 행은 인라인 TextEdit로 대체 (FT-3, §9-8)
                    if let Some(EditState::Rename {
                        path,
                        buffer,
                        focus,
                    }) = &mut edit
                        && path == &row.path
                    {
                        ui.horizontal(|ui| {
                            ui.add_space(12.0 + row.depth as f32 * 18.0);
                            let resp = ui.add(
                                egui::TextEdit::singleline(buffer)
                                    .margin(egui::Margin::ZERO) // 고정 행높이 유지 (§9-6)
                                    .desired_width(f32::INFINITY),
                            );
                            if *focus {
                                resp.request_focus(); // §9-8 — 편집 키가 터미널로 새지 않게
                                *focus = false;
                            }
                            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                edit_done = Some(true);
                            } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                edit_done = Some(false);
                            }
                        });
                        continue;
                    }

                    // hover 하이라이트 — 위젯을 그리기 전에 예정 행 rect로 판정해
                    // 텍스트 아래 배경으로 깐다 (위젯 등록 없이 포인터 포함 검사만).
                    let row_top = ui.cursor().min.y;
                    let hover_rect = egui::Rect::from_min_max(
                        egui::pos2(ui.max_rect().left(), row_top),
                        egui::pos2(ui.max_rect().right(), row_top + row_height),
                    );
                    // 워크스페이스·폴더 트리 경계선 드래그 중엔 hover 판정을 끈다 —
                    // 리사이즈로 행이 포인터 밑에 밀려 들어오면 클릭 가능한 것처럼
                    // 하이라이트되어 오클릭처럼 보였다(2026-07-24 사용자 보고).
                    if !resizing_workspace_split && ui.rect_contains_pointer(hover_rect) {
                        ui.painter().rect_filled(
                            hover_rect,
                            1.0,
                            ui.visuals().widgets.hovered.weak_bg_fill,
                        );
                        // ⌘V 대상 폴더/⌘C 대상 행 — hover 판정을 그대로 재사용(§과제②③).
                        if !inaccessible {
                            hover_target_dir = Some(row_target_dir(row, self.root.as_deref()));
                            hover_row_path = Some(row.path.clone());
                        }
                    }
                    // Finder 드래그 대상: 폴더 행 하이라이트 + 드롭 대상 기록 (§과제①).
                    // 드래그 중엔 egui 포인터가 멎으므로 drag_pos(AppKit 위치)로 판정한다.
                    if !inaccessible
                        && let Some(pos) = drag_pos
                        && hover_rect.contains(pos)
                    {
                        drop_target_dir = Some(row_target_dir(row, self.root.as_deref()));
                        if row.is_dir && os_drag_active {
                            ui.painter().rect_stroke(
                                hover_rect,
                                2.0,
                                ui.visuals().widgets.active.bg_stroke,
                                egui::StrokeKind::Inside,
                            );
                            drag_row_highlighted = true;
                        }
                    }

                    // 행 전체 = 드래그 소스 (payload = 절대 경로, §4). Id는 path 기반(§9-6).
                    let drag_id = egui::Id::new(("file_tree_row", &row.path));
                    let egui::InnerResponse {
                        inner: label_resp,
                        response,
                    } = ui.dnd_drag_source(drag_id, row.path.clone(), |ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width(), row_height),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.spacing_mut().item_spacing.x = 5.0;
                                ui.add_space(10.0 + row.depth as f32 * 18.0);
                                // 캐럿+폴더/파일 아이콘을 도형으로 (이모지 □ 깨짐 회피, 목업 §트리)
                                let caret_col = ui.visuals().weak_text_color();
                                let entry_color = file_entry_color(
                                    &row.name,
                                    row.is_dir,
                                    egui::Color32::from_rgb(0xc8, 0xcc, 0xd2),
                                );
                                let folder_col = if inaccessible {
                                    ui.visuals().weak_text_color()
                                } else {
                                    entry_color
                                };
                                let carve = ui.visuals().extreme_bg_color;
                                let (cr, _) = ui.allocate_exact_size(
                                    egui::vec2(10.0, 16.0),
                                    egui::Sense::hover(),
                                );
                                if row.is_dir && !inaccessible {
                                    paint_caret(ui.painter(), cr.center(), row.expanded, caret_col);
                                }
                                let (ir, _) = ui.allocate_exact_size(
                                    egui::vec2(17.0, 16.0),
                                    egui::Sense::hover(),
                                );
                                if row.is_dir {
                                    paint_folder(
                                        ui.painter(),
                                        ir.center(),
                                        folder_col,
                                        egui::vec2(12.825, 11.875),
                                    );
                                } else {
                                    let file_color = if inaccessible {
                                        ui.visuals().weak_text_color()
                                    } else if row.name.starts_with('.') {
                                        entry_color.gamma_multiply(0.62)
                                    } else {
                                        entry_color
                                    };
                                    paint_file(
                                        ui.painter(),
                                        ir.center(),
                                        file_color,
                                        carve,
                                        egui::vec2(9.5, 11.97),
                                    );
                                }
                                let text_color = if inaccessible {
                                    ui.visuals().weak_text_color()
                                } else if row.name.starts_with('.') {
                                    entry_color.gamma_multiply(0.62)
                                } else {
                                    entry_color
                                };
                                let rich = egui::RichText::new(&row.name)
                                    .family(egui::FontFamily::Name(
                                        crate::fonts::SIDEBAR_FONT_FAMILY.into(),
                                    ))
                                    .size(12.5)
                                    .color(text_color);
                                ui.add(
                                    egui::Label::new(rich)
                                        .sense(egui::Sense::click())
                                        .truncate(),
                                )
                            },
                        )
                        .inner
                    });
                    // 행 전체(패널 폭)를 클릭/드롭/메뉴 대상으로 — 텍스트만 클릭 가능하면
                    // 오클릭이 잦다 (2026-07-05 사용자 보고). 라벨보다 나중에 등록되므로
                    // 클릭은 이 위젯이 받고, label_resp.clicked()와 OR로 합친다.
                    let row_rect = egui::Rect::from_min_max(
                        egui::pos2(ui.max_rect().left(), response.rect.min.y),
                        egui::pos2(ui.max_rect().right(), response.rect.max.y),
                    );
                    // 경계선 리사이즈 중엔 행 전체를 hover만 받게 낮춰 클릭/드래그를
                    // 아예 못 일으키게 한다(위 hover 판정 차단과 같은 이유).
                    let row_sense = if resizing_workspace_split {
                        egui::Sense::hover()
                    } else {
                        egui::Sense::click_and_drag()
                    };
                    let row_resp = ui.interact(row_rect, drag_id.with("row"), row_sense);
                    let row_resp = if inaccessible {
                        row_resp.on_hover_text(catalog.t("file_tree.macos_access_denied", &[]))
                    } else {
                        row_resp
                    };
                    if !inaccessible && row_resp.drag_started() {
                        row_resp.dnd_set_drag_payload(row.path.clone());
                    }
                    // 행높이 실측 (드래그 중엔 행이 tooltip 레이어로 빠져 rect가 다름 — 제외)
                    if observed_row_height.is_none()
                        && !egui::DragAndDrop::has_any_payload(ui.ctx())
                    {
                        observed_row_height = Some(response.rect.height());
                    }
                    if row.is_dir && !inaccessible {
                        // 폴더 행 = 드롭 대상: hover 하이라이트 + release 처리 (§4)
                        if let Some(hover) = row_resp.dnd_hover_payload::<PathBuf>()
                            && hover.as_ref() != &row.path
                        {
                            ui.painter().rect_stroke(
                                row_rect,
                                2.0,
                                ui.visuals().widgets.active.bg_stroke,
                                egui::StrokeKind::Inside,
                            );
                        }
                        if let Some(payload) = row_resp.dnd_release_payload::<PathBuf>() {
                            drop_action = Some(((*payload).clone(), row.path.clone()));
                        }
                        if row_resp.double_clicked() || label_resp.double_clicked() {
                            navigate_root = Some(row.path.clone());
                        } else if row_resp.clicked() || label_resp.clicked() {
                            toggle = Some(row.path.clone());
                        }
                    } else if row_resp.double_clicked() || label_resp.double_clicked() {
                        // host가 실존 regular file + 원본/realpath 허용 확장자를 다시 검증한
                        // 뒤에만 연다. leaf는 filesystem metadata를 읽지 않는다.
                        open_file = Some(row.path.clone());
                    }
                    // 우클릭 컨텍스트 메뉴 (FT-3) — 행 전체에서 열리게 row_resp에 단다
                    if !inaccessible {
                        row_resp.context_menu(|ui| {
                            let new_folder_parent = if row.is_dir {
                                Some(row.path.clone())
                            } else {
                                row.path.parent().map(Path::to_path_buf)
                            };
                            if let Some(parent) = new_folder_parent {
                                let label = if row.is_dir {
                                    catalog.t("file_tree.new_folder_inside", &[])
                                } else {
                                    catalog.t("file_tree.new_folder_alongside", &[])
                                };
                                if ui.button(label).clicked() {
                                    menu_action = Some(MenuAction::NewFolder(parent));
                                    ui.close();
                                }
                            }
                            if ui.button(catalog.t("file_tree.rename", &[])).clicked() {
                                menu_action = Some(MenuAction::Rename(row.path.clone()));
                                ui.close();
                            }
                            if ui
                                .button(catalog.t("file_tree.move_to_trash", &[]))
                                .clicked()
                            {
                                menu_action = Some(MenuAction::Delete(row.path.clone()));
                                ui.close();
                            }
                            ui.separator();
                            // 파일 복사(③) — pasteboard 파일 URL로 써서 Finder ⌘V 대상.
                            if ui.button(catalog.t("file_tree.copy_file", &[])).clicked() {
                                menu_action = Some(MenuAction::CopyFile(row.path.clone()));
                                ui.close();
                            }
                            if ui.button(catalog.t("file_tree.copy_path", &[])).clicked() {
                                menu_action = Some(MenuAction::CopyPath(row.path.clone()));
                                ui.close();
                            }
                            if ui
                                .button(catalog.t("file_tree.insert_path_terminal", &[]))
                                .clicked()
                            {
                                menu_action = Some(MenuAction::InsertPath(row.path.clone()));
                                ui.close();
                            }
                            // 디렉터리만 — 포커스된 터미널에서 이 폴더로 cd (2026-07-08).
                            if row.is_dir
                                && ui.button(catalog.t("file_tree.cd_here", &[])).clicked()
                            {
                                menu_action = Some(MenuAction::CdPath(row.path.clone()));
                                ui.close();
                            }
                        });
                    }
                }
            });
        // 행높이 자기보정: 실측이 사용값과 어긋나면 저장하고 즉시 한 프레임 재그리기
        // (다음 프레임부터 스크롤 계산이 실제와 일치 — 클릭 밀림/위치 점프 방지)
        if let Some(observed) = observed_row_height
            && observed > 0.0
            && (observed - row_height).abs() > 0.1
        {
            self.measured_row_height = Some(observed);
            ui.ctx().request_repaint();
        }
        if let Some(path) = navigate_root {
            self.set_root(Some(path));
            return action;
        }
        if let Some(path) = toggle {
            self.toggle_dir(&path);
        }
        if let Some(path) = open_file {
            let request =
                FileTreePathPayload::try_new(path).map(|target| FileTreeIoRequest::OpenPath {
                    target,
                    require_openable_file: true,
                });
            if let Err(code) =
                request.and_then(|request| self.queue_io(request, Vec::new(), None, None))
            {
                self.reject_io(code);
            }
        }
        if let Some((src, dst_dir)) = drop_action {
            self.start_move(src, dst_dir);
        }

        // ── Finder → 트리 반입: OS 드롭(①)·클립보드 ⌘V(②) — 원본 보존 복사 ──
        // 반입 영역 = 파일 헤더 + 행 목록 (워크스페이스 목록/하단 nav 제외).
        let tree_area = header_rect.union(scroll_output.inner_rect);
        if os_drag_active && drag_pos.is_some_and(|pos| tree_area.contains(pos)) {
            if !drag_row_highlighted {
                // 특정 폴더 행 위가 아니면 루트 반입 — 트리 영역 전체 테두리로 표시.
                ui.painter().rect_stroke(
                    tree_area,
                    2.0,
                    ui.visuals().widgets.active.bg_stroke,
                    egui::StrokeKind::Inside,
                );
            }
            // 드래그 중엔 winit 이벤트가 없어 즉시 다음 frame을 요청해야 하이라이트가
            // 포인터를 따라온다. hovered_files가 비면 요청도 즉시 끝나며 timer는 남지 않는다.
            ui.ctx().request_repaint();
        }
        if !os_dropped.is_empty()
            && drag_pos.is_some_and(|pos| tree_area.contains(pos))
            && let Some(root) = self.root.clone()
        {
            let dst_dir = drop_target_dir.unwrap_or(root);
            self.start_copy_into(os_dropped, dst_dir);
        }
        self.handle_clipboard_shortcuts(ui, tree_area, hover_target_dir, hover_row_path);

        // 인라인 편집은 검증 후 native mutation intent만 만든다. 실패 completion이면
        // PendingFileTreeIo가 보관한 편집 snapshot을 복원한다.
        match edit_done {
            Some(false) => edit = None,
            Some(true) => match edit {
                Some(EditState::Rename { path, buffer, .. }) => {
                    let validated = validate_name(&buffer)
                        .map_err(|_| FileTreeIoErrorCode::InvalidName)
                        .and_then(|name| {
                            FileTreePathPayload::try_new(path.clone())
                                .map(|source| FileTreeIoRequest::Rename { source, name })
                        });
                    let refresh = path.parent().map(Path::to_path_buf).into_iter().collect();
                    let retry = EditState::Rename {
                        path,
                        buffer,
                        focus: true,
                    };
                    match validated.and_then(|request| {
                        self.queue_io(request, refresh, None, Some(retry.clone()))
                    }) {
                        Ok(()) => edit = None,
                        Err(code) => {
                            self.reject_io(code);
                            edit = Some(retry);
                        }
                    }
                }
                Some(EditState::NewFolder { parent, buffer, .. }) => {
                    let validated = validate_name(&buffer)
                        .map_err(|_| FileTreeIoErrorCode::InvalidName)
                        .and_then(|name| {
                            FileTreePathPayload::try_new(parent.clone())
                                .map(|parent| FileTreeIoRequest::CreateDirectory { parent, name })
                        });
                    let refresh = vec![parent.clone()];
                    let retry = EditState::NewFolder {
                        parent,
                        buffer,
                        focus: true,
                    };
                    match validated.and_then(|request| {
                        self.queue_io(request, refresh, None, Some(retry.clone()))
                    }) {
                        Ok(()) => edit = None,
                        Err(code) => {
                            self.reject_io(code);
                            edit = Some(retry);
                        }
                    }
                }
                Some(EditState::NewFile { parent, buffer, .. }) => {
                    let validated = validate_name(&buffer)
                        .map_err(|_| FileTreeIoErrorCode::InvalidName)
                        .and_then(|name| {
                            FileTreePathPayload::try_new(parent.clone())
                                .map(|parent| FileTreeIoRequest::CreateFile { parent, name })
                        });
                    let refresh = vec![parent.clone()];
                    let retry = EditState::NewFile {
                        parent,
                        buffer,
                        focus: true,
                    };
                    match validated.and_then(|request| {
                        self.queue_io(request, refresh, None, Some(retry.clone()))
                    }) {
                        Ok(()) => edit = None,
                        Err(code) => {
                            self.reject_io(code);
                            edit = Some(retry);
                        }
                    }
                }
                None => {}
            },
            None => {}
        }
        // 메뉴 동작 처리 (flat 순회 밖 — &mut self 필요 동작들)

        match menu_action {
            Some(MenuAction::NewFolder(parent)) => {
                edit = Some(EditState::NewFolder {
                    parent,
                    buffer: String::new(),
                    focus: true,
                });
            }
            Some(MenuAction::Rename(path)) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                edit = Some(EditState::Rename {
                    path,
                    buffer: name,
                    focus: true,
                });
            }
            Some(MenuAction::Delete(path)) => self.spawn_trash(path),
            Some(MenuAction::CopyFile(path)) => self.copy_files_to_clipboard(&[path]),
            Some(MenuAction::CopyPath(path)) => ui.ctx().copy_text(path.display().to_string()),
            Some(MenuAction::InsertPath(path)) => match FileTreePathPayload::try_new(path) {
                Ok(path) => action = Some(SidebarAction::InsertPath(path)),
                Err(code) => self.reject_io(code),
            },
            Some(MenuAction::CdPath(path)) => match FileTreePathPayload::try_new(path) {
                Ok(path) => action = Some(SidebarAction::CdPath(path)),
                Err(code) => self.reject_io(code),
            },
            None => {}
        }
        self.edit = edit;

        // 휴지통 실패 → 영구삭제 확인 (§9-7 — 조용한 영구삭제 금지)
        if let Some(path) = self.confirm_delete.clone() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            ui.colored_label(
                ui.visuals().warn_fg_color,
                catalog.t("file_tree.permanent_delete_prompt", &[("name", &name)]),
            );
            ui.horizontal(|ui| {
                if ui
                    .button(catalog.t("file_tree.permanent_delete", &[]))
                    .clicked()
                {
                    self.confirm_delete = None;
                    let refresh: Vec<PathBuf> =
                        path.parent().map(Path::to_path_buf).into_iter().collect();
                    let request = FileTreePathPayload::try_new(path.clone())
                        .map(|target| FileTreeIoRequest::DeletePermanently { target });
                    if let Err(code) =
                        request.and_then(|request| self.queue_io(request, refresh, None, None))
                    {
                        self.reject_io(code);
                    }
                }
                if ui.button(catalog.t("action.cancel", &[])).clicked() {
                    self.confirm_delete = None;
                }
            });
        }

        if self.in_flight > 0 {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                ui.weak(catalog.t("file_tree.file_operation_running", &[]));
            });
        }
        if self.pending_maintenance.is_some() || self.maintenance_intent.is_some() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                ui.weak(catalog.t("file_tree.listing_folders", &[]));
            });
        }
        if let Some(err) = self.error.clone() {
            ui.horizontal(|ui| {
                ui.colored_label(ui.visuals().error_fg_color, err);
                if ui.small_button("×").clicked() {
                    self.error = None;
                }
            });
        }
        action
    }

    /// 사이드바 최하단 nav — 홈 / 작업함 / 플릿 / 에이전트. 각 행은 painter 아이콘 +
    /// 라벨의 둥근 필(pill)이고,
    /// 작업함 행 우측에 대기+안읽음 카운트 배지가 붙는다(0이면 숨김).
    /// 홈/작업함 재클릭 시 터미널 복귀 토글은 App이 처리한다(view 소유자).
    fn navigation(
        &mut self,
        ui: &mut egui::Ui,
        sidebar: &SidebarSnapshot<'_>,
        catalog: &i18n::Catalog,
    ) -> Option<SidebarAction> {
        let mut action = None;
        ui.spacing_mut().item_spacing.y = SIDEBAR_NAV_ITEM_SPACING;
        ui.add_space(2.0);
        if nav_row(
            ui,
            NavIcon::Home,
            &catalog.t("sidebar.nav.home", &[]),
            sidebar.view == super::agent_terminal::AgentTerminalView::Home,
            nav_badge_text(sidebar.home_notice_count).as_deref(),
        )
        .clicked()
        {
            action = Some(SidebarAction::ShowHome);
        }
        if nav_row(
            ui,
            NavIcon::Inbox,
            &catalog.t("sidebar.nav.inbox", &[]),
            sidebar.view == super::agent_terminal::AgentTerminalView::Inbox,
            nav_badge_text(sidebar.inbox_count).as_deref(),
        )
        .clicked()
        {
            action = Some(SidebarAction::ShowInbox);
        }
        if nav_row(
            ui,
            NavIcon::Fleet,
            &catalog.t("sidebar.nav.fleet", &[]),
            sidebar.view == super::agent_terminal::AgentTerminalView::Fleet,
            nav_badge_text(sidebar.fleet_count).as_deref(),
        )
        .clicked()
        {
            action = Some(SidebarAction::ShowFleet);
        }
        if nav_row(
            ui,
            NavIcon::Agents,
            &catalog.t("sidebar.nav.agents", &[]),
            sidebar.agents_open,
            None,
        )
        .clicked()
        {
            action = Some(SidebarAction::OpenAgents);
        }
        ui.add_space(2.0);
        action
    }

    /// 드롭 → 이동 intent. canonicalize/root guard/rename/cross-volume 처리는 App host가
    /// 수행하고 leaf는 bounded 경로와 성공 후 refresh 대상만 보관한다.
    fn start_move(&mut self, src: PathBuf, dst_dir: PathBuf) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let refresh = parent_dirs(&src, &dst_dir);
        let request = (|| {
            Ok(FileTreeIoRequest::Move {
                root: FileTreePathPayload::try_new(root)?,
                source: FileTreePathPayload::try_new(src)?,
                destination: FileTreePathPayload::try_new(dst_dir)?,
            })
        })();
        match request.and_then(|request| self.queue_io(request, refresh, None, None)) {
            Ok(()) => {}
            Err(code) => self.reject_io(code),
        }
    }

    /// Finder 드롭(①)/⌘V(②) 반입 — 외부 원본을 대상 폴더로 **복사**한다(원본 보존).
    /// 이동(start_move)과 달리 루트 밖 원본을 허용하고 원본을 지우지 않는다.
    /// 실제 IO는 백그라운드(§9-3), 실패는 하단 에러 라벨로 표면화.
    fn start_copy_into(&mut self, sources: Vec<PathBuf>, dst_dir: PathBuf) {
        if sources.is_empty() {
            return;
        }
        let refresh = vec![dst_dir.clone()];
        let request = (|| {
            Ok(FileTreeIoRequest::CopyInto {
                sources: FileTreePathListPayload::try_new(sources)?,
                destination: FileTreePathPayload::try_new(dst_dir)?,
            })
        })();
        match request.and_then(|request| self.queue_io(request, refresh, None, None)) {
            Ok(()) => {}
            Err(code) => self.reject_io(code),
        }
    }

    /// 파일 트리 위 ⌘C/⌘V — Finder와의 파일 전송(§과제②③).
    ///
    /// 게이트: 포인터가 트리 영역 위 + 텍스트에딧 포커스 없음 + 팝업 없음. 터미널(기본
    /// 키보드 소유자)/컴포저와의 이중 처리는 소비 플래그(take_clipboard_shortcut_
    /// consumption)를 App이 WorkspaceUi에 전달해 같은 프레임에 누른다.
    fn handle_clipboard_shortcuts(
        &mut self,
        ui: &egui::Ui,
        tree_area: egui::Rect,
        target_dir: Option<PathBuf>,
        row_path: Option<PathBuf>,
    ) {
        let Some(root) = self.root.clone() else {
            return;
        };
        if ui.ctx().text_edit_focused() || ui.ctx().any_popup_open() {
            return;
        }
        let pointer_over = ui
            .input(|i| i.pointer.latest_pos())
            .is_some_and(|pos| tree_area.contains(pos));
        if !pointer_over {
            return;
        }
        // ⌘C(③): 포인터 밑 행을 파일 URL로 pasteboard에 — Finder에서 ⌘V 가능.
        if let Some(path) = row_path
            && ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)))
        {
            self.consumed_copy_shortcut = true;
            self.copy_files_to_clipboard(std::slice::from_ref(&path));
        }
        // ⌘V(②): 클립보드 파일 목록을 대상 폴더로 복사. macOS는 press가 native
        // key-down(peek)으로, 텍스트 표현이 있으면 Event::Paste로, release가 V key-up
        // fallback으로 온다(터미널 관례) — 어느 쪽이든 한 제스처는 한 번만 처리한다.
        let paste_signal = ui.input(|i| i.events.iter().any(is_tree_paste_signal))
            || crate::native_key_monitor::peek_clipboard_paste();
        if !paste_signal {
            return;
        }
        if self
            .last_external_paste
            .is_some_and(|at| at.elapsed() < EXTERNAL_PASTE_GESTURE_WINDOW)
        {
            // 같은 ⌘V 제스처의 후속 신호(press→release) — 재복사 없이 터미널 이중
            // 처리만 계속 누른다.
            self.consumed_paste_shortcut = true;
            return;
        }
        let destination = target_dir.unwrap_or(root);
        let refresh = vec![destination.clone()];
        let request = FileTreePathPayload::try_new(destination)
            .map(|destination| FileTreeIoRequest::PasteFromClipboard { destination });
        if let Err(code) = request.and_then(|request| self.queue_io(request, refresh, None, None)) {
            self.reject_io(code);
            return;
        }
        self.consumed_paste_shortcut = true;
        self.last_external_paste = Some(std::time::Instant::now());
    }

    /// 파일 URL pasteboard 쓰기 — 실패는 하단 에러 라벨로 표면화(조용한 실패 금지).
    fn copy_files_to_clipboard(&mut self, paths: &[PathBuf]) {
        let request = FileTreePathListPayload::try_new(paths.to_vec())
            .map(|paths| FileTreeIoRequest::CopyFileUrls { paths });
        match request.and_then(|request| self.queue_io(request, Vec::new(), None, None)) {
            Ok(()) => {}
            Err(code) => self.reject_io(code),
        }
    }

    /// 휴지통 이동 intent. 실패 completion만 영구삭제 확인으로 승격한다.
    fn spawn_trash(&mut self, path: PathBuf) {
        let refresh: Vec<PathBuf> = path.parent().map(Path::to_path_buf).into_iter().collect();
        let target = path.clone();
        let request =
            FileTreePathPayload::try_new(path).map(|target| FileTreeIoRequest::Trash { target });
        match request.and_then(|request| self.queue_io(request, refresh, Some(target), None)) {
            Ok(()) => {}
            Err(code) => self.reject_io(code),
        }
    }

    /// 한 디렉터리만 재나열한다 (조작/워처 후 부분 갱신). 루트면 루트 children을,
    /// 아니면 해당 노드가 펼쳐져 있을 때만 그 children을 다시 읽는다.
    fn reload_dir(&mut self, dir: &Path) {
        let Some(root) = self.root.clone() else {
            return;
        };
        if dir == root {
            self.refresh();
            return;
        }
        let Ok(rel) = dir.strip_prefix(&root) else {
            return;
        };
        let should_reload = self
            .children
            .as_ref()
            .and_then(|c| node_ref(c, rel))
            .map(|node| node.expanded)
            .unwrap_or(false);
        if should_reload {
            self.enqueue_refresh_dir(dir.to_path_buf());
            self.drive_maintenance();
        }
    }

    fn replace_listing_children(&mut self, path: &Path, nodes: Vec<TreeNode>) -> bool {
        let Some(root) = self.root.clone() else {
            return false;
        };
        if path == root {
            self.children = Some(nodes);
            return true;
        }
        let Ok(rel) = path.strip_prefix(&root) else {
            return false;
        };
        let Some(node) = self.children.as_mut().and_then(|c| node_mut(c, rel)) else {
            return false;
        };
        if !node.expanded {
            return false;
        }
        node.children = Some(nodes);
        true
    }

    fn collect_expanded_paths(&self) -> HashSet<PathBuf> {
        let mut expanded = HashSet::new();
        if let (Some(root), Some(children)) = (&self.root, &self.children) {
            collect_expanded_paths(children, root, &mut expanded);
        }
        expanded
    }

    fn expanded_direct_child_paths(&self, path: &Path) -> Vec<PathBuf> {
        let Some(root) = self.root.as_ref() else {
            return Vec::new();
        };
        let children = if path == root {
            self.children.as_ref()
        } else {
            let Ok(rel) = path.strip_prefix(root) else {
                return Vec::new();
            };
            self.children
                .as_ref()
                .and_then(|c| node_ref(c, rel))
                .and_then(|node| node.children.as_ref())
        };
        children
            .into_iter()
            .flat_map(|children| children.iter())
            .filter(|node| node.is_dir && node.expanded)
            .map(|node| path.join(&node.name))
            .collect()
    }
}

fn permission_denied_button_rect(area: egui::Rect, screen_center: egui::Pos2) -> egui::Rect {
    let button_width = (area.width() - 24.0).clamp(24.0, 360.0);
    let button_height = area.height().clamp(0.0, 44.0);
    let half_height = button_height * 0.5;
    let center = egui::pos2(
        area.center().x,
        screen_center
            .y
            .clamp(area.top() + half_height, area.bottom() - half_height),
    );
    egui::Rect::from_center_size(center, egui::vec2(button_width, button_height))
}

/// src의 옛 부모와 dst의 새 부모 (중복 제거) — 조작 후 재나열 대상.
fn parent_dirs(src: &Path, dst: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for p in [src.parent(), dst.parent()].into_iter().flatten() {
        if !dirs.contains(&p.to_path_buf()) {
            dirs.push(p.to_path_buf());
        }
    }
    dirs
}

fn insert_pending_watch_dir(pending: &mut BTreeSet<PathBuf>, dir: PathBuf) {
    if pending.iter().any(|existing| dir.starts_with(existing)) {
        return;
    }
    let descendants: Vec<PathBuf> = pending
        .iter()
        .filter(|existing| existing.starts_with(&dir))
        .cloned()
        .collect();
    for descendant in descendants {
        pending.remove(&descendant);
    }
    pending.insert(dir);
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum WatchEvent {
    DirtyDir(PathBuf),
    EnvFileChanged(PathBuf),
}

#[cfg(test)]
fn watch_events_for_path(
    root: &Path,
    path: &Path,
    show_hidden: bool,
    ignore_prefixes: &[PathBuf],
) -> Vec<WatchEvent> {
    if ignore_prefixes
        .iter()
        .any(|prefix| path.starts_with(prefix))
    {
        return Vec::new();
    }
    let env_file = is_env_file_candidate(path);
    if !show_hidden && has_hidden_component(root, path) && !env_file {
        return Vec::new();
    }

    let mut events = Vec::new();
    if env_file {
        events.push(WatchEvent::EnvFileChanged(path.to_path_buf()));
    }
    if let Some(parent) = path.parent() {
        events.push(WatchEvent::DirtyDir(parent.to_path_buf()));
    }
    events
}

#[cfg(test)]
fn is_env_file_candidate(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name == ".env" || name == ".envrc" || name.starts_with(".env.") || name.starts_with(".env-")
}

/// 트리에 영향을 주는 fs 이벤트인지 (FT-4). Access(읽기 등) 이벤트는 잡음이라 무시.
/// root 기준 상대 경로에 숨김(`.`) 컴포넌트가 있는가 — 표시되지 않는 서브트리의 이벤트 판별.
fn has_hidden_component(root: &std::path::Path, path: &std::path::Path) -> bool {
    let rel = match path.strip_prefix(root) {
        Ok(rel) => rel,
        Err(_) => return false, // 루트 밖(이상 케이스)은 거르지 않음 — 상위에서 ignore로 처리
    };
    rel.components().any(|c| {
        matches!(c, std::path::Component::Normal(name) if name.to_string_lossy().starts_with('.'))
    })
}

#[cfg(test)]
fn relevant_fs_event(kind: &notify::EventKind) -> bool {
    !matches!(kind, notify::EventKind::Access(_))
}

/// 우클릭 컨텍스트 메뉴 동작 (FT-3) — flat 순회 밖에서 처리한다.
enum MenuAction {
    NewFolder(PathBuf),
    Rename(PathBuf),
    Delete(PathBuf),
    /// 파일/폴더를 pasteboard에 파일 URL로 복사 — Finder ⌘V 대상(§과제③).
    CopyFile(PathBuf),
    CopyPath(PathBuf),
    InsertPath(PathBuf),
    CdPath(PathBuf),
}

/// 이름 검증 (§5): 빈 이름·경로 구분자·'.'/'..' 거부. Ok = 트림된 이름.
fn validate_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("이름이 비어 있습니다".to_owned());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("이름에 경로 구분자를 쓸 수 없습니다".to_owned());
    }
    if name == "." || name == ".." {
        return Err("사용할 수 없는 이름입니다".to_owned());
    }
    Ok(name.to_owned())
}

/// 이름 변경 (덮어쓰기 금지 §9-5 공유). 성공 시 새 경로.
#[cfg(test)]
fn apply_rename(path: &Path, new_name: &str) -> Result<PathBuf, String> {
    let name = validate_name(new_name)?;
    let parent = path
        .parent()
        .ok_or_else(|| "이름을 바꿀 수 없는 경로입니다".to_owned())?;
    let dst = parent.join(&name);
    if dst == path {
        return Ok(dst); // 이름 그대로 — no-op
    }
    match rename_no_replace(path, &dst) {
        Ok(()) => Ok(dst),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(format!("같은 이름이 이미 있습니다: {name}"))
        }
        Err(e) => Err(format!("이름 변경 실패: {e}")),
    }
}

/// 새 폴더 생성 (이미 있으면 거부). 성공 시 생성 경로.
#[cfg(test)]
fn apply_new_folder(parent: &Path, name: &str) -> Result<PathBuf, String> {
    let name = validate_name(name)?;
    let dst = parent.join(&name);
    match std::fs::create_dir(&dst) {
        Ok(()) => Ok(dst),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(format!("같은 이름이 이미 있습니다: {name}"))
        }
        Err(e) => Err(format!("폴더 생성 실패: {e}")),
    }
}

/// 새 빈 파일 생성 (덮어쓰기 금지). 성공 시 생성 경로.
#[cfg(test)]
fn apply_new_file(parent: &Path, name: &str) -> Result<PathBuf, String> {
    let name = validate_name(name)?;
    let dst = parent.join(&name);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dst)
    {
        Ok(_) => Ok(dst),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(format!("같은 이름이 이미 있습니다: {name}"))
        }
        Err(e) => Err(format!("파일 생성 실패: {e}")),
    }
}

/// 터미널 경로 삽입 대상 셸 계열. 세션별 셸 metadata 배선은 후속 PR 범위이므로,
/// 현재 call site는 `shell_quote`/`shell_path_insert_bytes` 기본 wrapper를 쓴다.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// POSIX sh/bash/zsh 계열 single-quote escaping.
    Posix,
    /// fish single-quote escaping.
    Fish,
    /// PowerShell single-quote escaping.
    PowerShell,
    /// cmd.exe double-quote grouping.
    Cmd,
}

fn paint_git_branch_icon(ui: &egui::Ui, rect: egui::Rect, color: egui::Color32) {
    ui.painter().rect_filled(rect, 0.0, color);
}

/// 세션 행을 painter로 직접 그린다 (2026-07-06 목업 반영). 상태를 이모지 글리프로
/// 쓰면 폰트(AppleGothic)에 ⏳/✋/▸/◆ 글리프가 없어 □(두부)로 깨진다 — 색 점·삼각형·
/// 마름모를 도형으로 그려 회피한다. 선택 시 액센트 배경 + 좌측 레일, agent는 레일 표시,
/// 요약 한 줄(dim/Apple SD Gothic). 반환 Response로 클릭을 처리한다.
fn workspace_row(
    ui: &mut egui::Ui,
    workspace: &SidebarWorkspaceEntry,
    color: egui::Color32,
    active: bool,
    expanded: Option<bool>,
    catalog: &i18n::Catalog,
) -> egui::Response {
    // 2026-07-26 사용자: 워크스페이스 헤더와 아바타를 다시 10% 축소한다.
    let has_repo = workspace.repo.as_deref().is_some_and(|repo| !repo.is_empty());
    let row_height = if has_repo { 34.0 } else { 29.19 };
    let (full_rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), row_height),
        egui::Sense::click(),
    );
    // 이름은 painter galley라 행 Response에 명시적으로 연결해야 키보드/스크린리더가
    // workspace 선택 대상을 식별할 수 있다(세션 행과 같은 접근성 계약).
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            ui.is_enabled(),
            workspace.name.as_str(),
        )
    });
    if !ui.is_rect_visible(full_rect) {
        return response;
    }
    // 좌우 여백(2026-07-19 사용자) — 패널 좌우 margin이 0이라 pill이 가장자리에
    // 붙었다. 그리기 rect만 좌우 8px 안으로 들여 pill·내용에 숨 공간을 준다
    // (클릭 판정은 full_rect라 가장자리도 눌린다).
    // 카드 시각 경계 안에 별도 좌우 padding을 둔다. 배경만 inset하고 콘텐츠 rect는
    // 예전 8px 기준을 유지하면 아바타와 chevron이 카드 양끝에 붙어 잘려 보인다.
    let rect = egui::Rect::from_min_max(
        egui::pos2(full_rect.left() + 4.0, full_rect.top()),
        egui::pos2(
            full_rect.right() - WORKSPACE_CARD_HORIZONTAL_INSET - 1.0,
            full_rect.bottom(),
        ),
    );
    // 접힘/펼침 여부와 무관하게 헤더 자체를 가리킬 때만 워크스페이스 고유색으로
    // hover를 표시한다. 세션 행과 같은 0.16 강도를 사용해 상호작용 규칙을 통일한다.
    if response.hovered() {
        let hover_rect = egui::Rect::from_min_max(
            rect.min,
            egui::pos2((rect.right() + 1.0).min(full_rect.right()), rect.bottom()),
        );
        ui.painter()
            .rect_filled(hover_rect, 1.0, color.gamma_multiply(0.16));
    }
    // 선택/실행 상태와 무관한 프로젝트 고유색. 목록 전체에서 같은 계열이 겹치지 않게
    // 미리 배정된 색을 받아 비활성 행과 40pt 아이콘 레일에서도 그대로 유지한다.
    let avatar = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 5.8 + 10.5, rect.center().y),
        egui::vec2(21.0, 21.0),
    );
    // 워크스페이스 마크는 별도 테두리 없이 상태색을 채운다(HTML 목업과 같은 규칙).
    ui.painter().rect_filled(
        avatar,
        1.0,
        color.gamma_multiply(if active { 0.48 } else { 0.36 }),
    );
    // 아바타는 빠른 식별용 마크라 첫 글자를 항상 대문자로 고정한다. 반대로 실제
    // 워크스페이스 이름은 사용자가 지정한 대소문자를 그대로 보존한다.
    let initial = workspace_initial(&workspace.name);
    ui.painter().text(
        avatar.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        crate::fonts::sidebar_font(10.8),
        egui::Color32::WHITE,
    );
    let summary_mode = workspace_summary_mode(rect.width());
    let show_summary = summary_mode != WorkspaceSummaryMode::IconOnly;
    let show_disclosure = expanded.is_some() && rect.width() >= 56.0;
    let total_sessions = workspace_total_sessions(workspace.summary);
    let (_, badge_color) = workspace_primary_summary_segment(workspace.summary, catalog);
    if rect.width() >= 64.0 {
        // 요약 배지 자리를 **실제 폭**만큼만 예약한다 — 고정 198px는 "유휴 5"처럼
        // 짧은 요약에도 이름을 훨씬 일찍 잘라 옆 여백이 남았다(2026-07-18 사용자).
        // 우측 여백 8 + 이름/요약 간격 16 + disclosure 폭(있으면 14)을 더한다.
        let reserved_right = if show_summary {
            let disclosure = if show_disclosure { 12.0 } else { 0.0 };
            workspace_status_badge_width(ui, total_sessions) + 22.0 + disclosure
        } else {
            6.8
        };
        let name_width = (rect.right() - reserved_right - avatar.right() - 7.65).max(0.0);
        if name_width > 4.0 {
            let name = clipped_line(
                ui,
                workspace_label(&workspace.name),
                // 워크스페이스명은 좌측 사이드바 전용 Apple SD Gothic 가족을 사용해
                // 원래 대소문자와 자연스러운 자폭을 보존한다.
                crate::fonts::sidebar_font(14.0),
                name_width,
                None,
            );
            let text_x = avatar.right() + 7.65;
            let name_center_y = if has_repo {
                rect.center().y - 7.2
            } else {
                rect.center().y
            };
            ui.painter().galley(
                egui::pos2(text_x, name_center_y - name.size().y / 2.0),
                name,
                ui.visuals().text_color(),
            );
            if let Some(repo) = workspace.repo.as_deref().filter(|repo| !repo.is_empty()) {
                let branch_icon_size = 3.0;
                let branch_gap = 3.0;
                let branch_text_x = text_x + branch_icon_size + branch_gap;
                let repo = clipped_line(
                    ui,
                    repo,
                    crate::fonts::sidebar_font(10.5),
                    (name_width - branch_icon_size - branch_gap).max(4.0),
                    None,
                );
                let repo_y = rect.center().y + 1.8;
                let branch_icon_center_y = repo_y + repo.size().y / 2.0;
                paint_git_branch_icon(
                    ui,
                    egui::Rect::from_center_size(
                        egui::pos2(text_x + branch_icon_size / 2.0, branch_icon_center_y),
                        egui::vec2(branch_icon_size, branch_icon_size),
                    ),
                    ui.visuals().weak_text_color(),
                );
                ui.painter().galley(
                    egui::pos2(branch_text_x, repo_y),
                    repo,
                    ui.visuals().weak_text_color(),
                );
            }
        }
    }
    if show_summary {
        // 텍스트 요약("유휴"/"비활성" 등) 대신 상태색 dot + 세션 수 배지 — 활성/닫힌
        // 행 공용(2026-07-25 사용자: "비활성" 문구 제거, 색으로 상태 표기).
        // chevron과의 간격 22→14(너무 붙음)→16으로 재조정(2026-07-25 사용자:
        // 숫자·화살표 사이 여백 2 추가).
        let right = if show_disclosure {
            rect.right() - 22.0
        } else {
            rect.right() - 10.0
        };
        paint_workspace_status_badge(ui, right, rect.center().y, badge_color, total_sessions);
    } else {
        // 40pt 아이콘 레일까지 줄였을 때는 배지 자리가 없으므로 아바타 우하단의
        // 작은 점으로 primary state를 계속 표시한다. 이름이 보이는 폭부터는 반드시
        // 위의 dot+카운트 배지로 바뀐다.
        ui.painter().circle_filled(
            egui::pos2(avatar.right() - 2.5, avatar.bottom() - 2.5),
            2.5,
            badge_color,
        );
    }
    if show_disclosure && let Some(expanded) = expanded {
        let center = egui::pos2(rect.right() - 12.0, rect.center().y);
        let points = disclosure_chevron_points(center, expanded);
        ui.painter().add(egui::Shape::line(
            points.to_vec(),
            egui::Stroke::new(1.0, ui.visuals().weak_text_color().gamma_multiply(0.82)),
        ));
    }
    response
}

const SIDEBAR_NAV_MIN_HEIGHT: f32 = 50.0;
const SIDEBAR_NAV_DEFAULT_HEIGHT: f32 = 108.0;
const SIDEBAR_NAV_SPLIT_HANDLE_HEIGHT: f32 = 6.0;
const SIDEBAR_BODY_MIN_HEIGHT: f32 = 180.0;
const SIDEBAR_NAV_ROW_HEIGHT: f32 = 24.0;
const SIDEBAR_NAV_ITEM_SPACING: f32 = 0.8;
const SIDEBAR_BACKGROUND: egui::Color32 = egui::Color32::from_rgb(0x17, 0x17, 0x17);
const WORKSPACE_CARD_HORIZONTAL_INSET: f32 = 6.0;
const WORKSPACE_LIST_BACKGROUND_TOP: egui::Color32 = SIDEBAR_BACKGROUND;
const WORKSPACE_LIST_BACKGROUND_BOTTOM: egui::Color32 = SIDEBAR_BACKGROUND;
const WORKSPACE_GROUP_FILL: egui::Color32 = egui::Color32::from_rgb(0x17, 0x17, 0x17);
const WORKSPACE_GROUP_TOP_FILL: egui::Color32 = egui::Color32::from_rgb(0x17, 0x17, 0x17);
const WORKSPACE_GROUP_BORDER: egui::Color32 = egui::Color32::from_rgb(0x17, 0x17, 0x17);
const WORKSPACE_GROUP_SHADOW: egui::Color32 = egui::Color32::from_black_alpha(54);

fn vertical_gradient_rect(
    rect: egui::Rect,
    top: egui::Color32,
    bottom: egui::Color32,
) -> egui::Shape {
    let mut mesh = egui::epaint::Mesh::default();
    let first = mesh.vertices.len() as u32;
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.indices
        .extend_from_slice(&[first, first + 1, first + 2, first, first + 2, first + 3]);
    egui::Shape::mesh(mesh)
}

fn workspace_list_background_gradient(rect: egui::Rect) -> egui::Shape {
    vertical_gradient_rect(
        rect,
        WORKSPACE_LIST_BACKGROUND_TOP,
        WORKSPACE_LIST_BACKGROUND_BOTTOM,
    )
}

fn workspace_group_gradient(rect: egui::Rect) -> egui::Shape {
    vertical_gradient_rect(rect, WORKSPACE_GROUP_TOP_FILL, WORKSPACE_GROUP_FILL)
}

/// 워크스페이스 헤더 + (펼쳐졌으면) 그 세션 목록을 배경(#0f171d)·테두리(#131c23)로
/// 하나의 카드처럼 묶어 그린다(2026-07-25 사용자: 여백 없이 이어지는 카드).
///
/// 실제 행 크기는 렌더 전에 알 수 없으므로(에이전트 유무로 세션 행 높이가
/// 38/52px로 갈리고, 세션 목록 자체도 자기 상한 안에서 스크롤될 수 있다) 배경을
/// 먼저 계산하지 않는다. 대신 `ui.painter().add(Shape::Noop)`로 그리기 순서상의
/// 자리만 예약해 두고(`reserve`), 실제 행들을 `ui.scope`로 감싸 그 결과 rect(=
/// 스크롤 클리핑까지 반영된 실제 점유 영역)를 얻은 뒤 `set`으로 그 자리에 채워
/// 넣는다 — 순서는 예약 시점 그대로라 배경이 행 콘텐츠보다 항상 아래에 그려진다.
fn paint_workspace_group_wrap(ui: &egui::Ui, reserve: egui::layers::ShapeIdx, rect: egui::Rect) {
    let rect = egui::Rect::from_min_max(
        rect.left_top(),
        egui::pos2(rect.right() - WORKSPACE_CARD_HORIZONTAL_INSET, rect.bottom()),
    );
    if rect.height() <= 0.0 || rect.width() <= 0.0 {
        return;
    }
    let rounding = 6.0;
    let shadow_rect = rect.translate(egui::vec2(0.0, 2.0)).expand(1.0);
    ui.painter().set(
        reserve,
        egui::Shape::Vec(vec![
            egui::Shape::rect_filled(shadow_rect, rounding, WORKSPACE_GROUP_SHADOW),
            egui::Shape::rect_filled(rect, rounding, WORKSPACE_GROUP_FILL),
            workspace_group_gradient(rect.shrink(1.0)),
            egui::Shape::rect_stroke(
                rect,
                rounding,
                egui::Stroke::new(1.0, WORKSPACE_GROUP_BORDER),
                egui::StrokeKind::Inside,
            ),
        ]),
    );
}

const WORKSPACE_SESSION_INSET_FILL: egui::Color32 = egui::Color32::from_rgb(0x17, 0x17, 0x17);
const WORKSPACE_SESSION_INSET_BORDER: egui::Color32 = egui::Color32::from_rgb(0x17, 0x17, 0x17);

// 좌측 인셋 8px = 세션 행 자체의 hover 좌측 경계와 같은 값(20px 들여쓰기 -
// SESSION_HIGHLIGHT_LEFT_EXTEND 12px, 아래 session_highlight_rect 참고) —
// workspace_row의 아바타 영역과도 같은 여백 관례라 우연히 같은 8이다.
const WORKSPACE_SESSION_INSET_LEFT: f32 = 24.0;

/// 카드 안에서 세션 목록 구간만 살짝 다른 톤(#121a20 채우기 + #19222a 테두리)을
/// 얹어 헤더와 분리해 보이게 한다(2026-07-25 사용자) — group_rect(헤더+세션 전체)
/// 에서 헤더가 이미 차지한 위쪽을 뺀 나머지에만 칠한다. 헤더와 맞닿는 위쪽까지
/// 포함해 네 모서리 모두 1px로 통일한다(2026-07-25 사용자: "위쪽도 1px만").
///
/// 네 경계는 group_rect가 아니라 **세션 행 전체의 실제 합집합 rect**로 계산한다
/// (2026-07-25 사용자: "인셋이 hover보다 커서 빈 공간 생기는거"). group_rect의
/// 우측은 세션 ScrollArea 밖에 있는 헤더 full_rect까지 합친 값이라, 세션 행이
/// (스크롤바 유무 등으로) 헤더보다 좁아지면 인셋이 hover 영역보다 넓게 그려져
/// 빈 공간이 남았다. 세션 행 합집합에 포커스 배경이 쓰는 `session_inset_fill_rect`를 직접
/// 적용해 상단/우측/하단 경계를 정확히 맞춘다. 좌측은 헤더 폭에 영향받지 않아
/// group_rect 기준 8px 인셋을 유지한다.
fn paint_workspace_session_inset(
    ui: &egui::Ui,
    reserve: egui::layers::ShapeIdx,
    group_rect: egui::Rect,
    header_bottom: f32,
    session_rows_rect: Option<egui::Rect>,
) {
    let Some(session_rows_rect) = session_rows_rect else {
        return;
    };
    let rect = workspace_session_inset_rect(group_rect, header_bottom, session_rows_rect);
    if rect.height() <= 0.0 || rect.width() <= 0.0 {
        return;
    }
    ui.painter().set(
        reserve,
        egui::Shape::Vec(vec![
            egui::Shape::rect_filled(rect, 0.0, WORKSPACE_SESSION_INSET_FILL),
            egui::Shape::rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(1.0, WORKSPACE_SESSION_INSET_BORDER),
                egui::StrokeKind::Inside,
            ),
        ]),
    );
}

fn workspace_initial(name: &str) -> char {
    name.chars()
        .next()
        .and_then(|character| character.to_uppercase().next())
        .unwrap_or('W')
}

fn workspace_label(name: &str) -> &str {
    name
}

/// 첨부 시안의 얇은 선형 chevron. 접힘은 `>`이고 펼침은 `⌄` 방향이다.
fn disclosure_chevron_points(center: egui::Pos2, expanded: bool) -> [egui::Pos2; 3] {
    if expanded {
        [
            egui::pos2(center.x - 3.5, center.y - 2.0),
            egui::pos2(center.x, center.y + 2.0),
            egui::pos2(center.x + 3.5, center.y - 2.0),
        ]
    } else {
        [
            egui::pos2(center.x - 2.0, center.y - 3.5),
            egui::pos2(center.x + 2.0, center.y),
            egui::pos2(center.x - 2.0, center.y + 3.5),
        ]
    }
}

/// 워크스페이스 행 우클릭 메뉴 — 「이름 바꾸기」(별칭 편집)는 세션이 없어도 항상,
/// 「워크스페이스 종료」(세션 일괄 닫기, 확인은 App)는 닫을 세션이 있는 비 Idle만.
fn workspace_context_menu(
    resp: &egui::Response,
    workspace: &SidebarWorkspaceEntry,
    catalog: &i18n::Catalog,
    action: &mut Option<SidebarAction>,
) {
    resp.context_menu(|ui| workspace_context_menu_items(ui, workspace, catalog, action));
}

/// 메뉴 본문 — 팝업 없이 렌더할 수 있게 분리해 kittest 대상으로 삼는다
/// (workspace.rs last_output_menu_items 관례 — kittest는 press/release를 다른
/// 프레임에 재생해 실제 팝업 안 버튼의 clicked를 관측하지 못한다).
fn workspace_context_menu_items(
    ui: &mut egui::Ui,
    workspace: &SidebarWorkspaceEntry,
    catalog: &i18n::Catalog,
    action: &mut Option<SidebarAction>,
) {
    let rename_label = catalog.t("sidebar.menu.rename_workspace", &[]);
    let close_label = catalog.t("sidebar.menu.close_workspace", &[]);
    // 메뉴 폭이 좁으면 「워크스페이스 종료」가 두 줄로 접혀 잘렸다(2026-07-18 사용자
    // 스샷). 가장 긴 항목의 no-wrap 폭으로 최소 폭을 강제해(max_rect까지 확장된다)
    // 어느 로케일에서도 모든 항목이 항상 한 줄로 그려지게 한다. Idle이라 종료 항목이
    // 빠져도 두 항목 모두 재서 메뉴 폭이 상태에 따라 널뛰지 않게 한다.
    let font = egui::TextStyle::Button.resolve(ui.style());
    let widest = [rename_label.as_str(), close_label.as_str()]
        .into_iter()
        .map(|label| {
            ui.painter()
                .layout_no_wrap(label.to_owned(), font.clone(), egui::Color32::WHITE)
                .size()
                .x
        })
        .fold(0.0_f32, f32::max);
    ui.set_min_width(widest + ui.spacing().button_padding.x * 2.0 + 2.0);
    if ui.button(rename_label).clicked() {
        *action = Some(SidebarAction::RenameWorkspace(workspace.id.clone()));
        ui.close();
    }
    if workspace.state != SidebarWorkspaceState::Idle && ui.button(close_label).clicked() {
        *action = Some(SidebarAction::CloseWorkspace(workspace.id.clone()));
        ui.close();
    }
}

fn workspace_creation_order_partition<'a>(
    workspaces: &'a [SidebarWorkspaceEntry],
    active_id: &str,
) -> (
    &'a [SidebarWorkspaceEntry],
    Option<&'a SidebarWorkspaceEntry>,
    &'a [SidebarWorkspaceEntry],
) {
    match workspaces
        .iter()
        .position(|workspace| workspace.id == active_id)
    {
        Some(index) => (
            &workspaces[..index],
            Some(&workspaces[index]),
            &workspaces[index + 1..],
        ),
        None => (workspaces, None, &[]),
    }
}

/// 요약 배지의 렌더 폭 — workspace_row가 이름 자리를 이 폭만큼만 비워 두게 한다.
/// paint_workspace_summary와 같은 세그먼트·폰트를 써야 실제 렌더와 어긋나지 않는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceSummaryMode {
    IconOnly,
    Compact,
    Full,
}

fn workspace_summary_mode(width: f32) -> WorkspaceSummaryMode {
    if width >= 270.0 {
        WorkspaceSummaryMode::Full
    } else if width >= 104.0 {
        WorkspaceSummaryMode::Compact
    } else {
        WorkspaceSummaryMode::IconOnly
    }
}

/// 접힌/펼친 워크스페이스 행 공용 총 세션 수(우측 dot+카운트 배지에 쓴다,
/// 2026-07-25 사용자: 텍스트 요약 대신 색+숫자로 상태 표기).
fn workspace_total_sessions(summary: SidebarSessionSummary) -> usize {
    summary.running + summary.waiting + summary.done + summary.error + summary.idle + summary.inactive
}

const WORKSPACE_STATUS_DOT_DIAMETER: f32 = 6.0;
// dot↔숫자 간격 — 6→4(너무 넓음)→5→6으로 재조정(2026-07-25 사용자: 여백 1 추가).
const WORKSPACE_STATUS_DOT_GAP: f32 = 4.5;

/// 세션 수 자리의 고정 슬롯 폭(dot+간격+숫자 전체) — 실제 글리프 폭(자릿수마다
/// 다름)으로 dot 위치를 정하면 0→1→10처럼 자릿수가 바뀔 때마다 dot이 옆으로
/// 밀린다(2026-07-25 사용자: 버튼 정렬 안 맞음). 두 자리(예 "99")까지 넉넉한
/// 고정폭이라 dot의 x 위치가 행마다 항상 같다.
const WORKSPACE_STATUS_COUNT_SLOT_WIDTH: f32 = 22.0;

/// 상태색 dot + 세션 수 배지의 그리기 폭 — 이름 자리 예약 계산에 쓴다.
fn workspace_status_badge_width(_ui: &egui::Ui, _count: usize) -> f32 {
    WORKSPACE_STATUS_COUNT_SLOT_WIDTH
}

/// 상태색 dot + 세션 수 — `right`를 오른쪽 끝으로 왼쪽으로 그린다. dot은 고정
/// 슬롯의 좌측 경계에 앵커링해 자릿수가 바뀌어도 흔들리지 않고, 숫자는 dot
/// 바로 옆(고정 간격)에 좌측 정렬해 실제 자폭과 무관하게 여백이 일정하다.
fn paint_workspace_status_badge(
    ui: &egui::Ui,
    right: f32,
    center_y: f32,
    color: egui::Color32,
    count: usize,
) {
    let dot_x = right - WORKSPACE_STATUS_COUNT_SLOT_WIDTH + WORKSPACE_STATUS_DOT_DIAMETER / 2.0;
    ui.painter().circle_filled(
        egui::pos2(dot_x, center_y),
        WORKSPACE_STATUS_DOT_DIAMETER / 2.0,
        color,
    );
    let text_x = dot_x + WORKSPACE_STATUS_DOT_DIAMETER / 2.0 + WORKSPACE_STATUS_DOT_GAP;
    // Align2::LEFT_CENTER — 아바타 이니셜(CENTER_CENTER)과 같은 방식으로 egui가
    // 직접 세로 중앙을 잡게 한다. 수동으로 size().y/2를 빼는 방식은 폰트 라인하이트
    // 여백 때문에 dot과 시각적으로 어긋나 보였다(2026-07-25 사용자). +1px는 그
    // 위에 얹은 미세 보정(2026-07-25 사용자: 숫자를 아래로 1).
    ui.painter().text(
        egui::pos2(text_x, center_y + 1.0),
        egui::Align2::LEFT_CENTER,
        count.to_string(),
        crate::fonts::sidebar_font(11.5),
        ui.visuals().weak_text_color().gamma_multiply(0.9),
    );
}

// workspace_row는 이제 상태색 dot + 세션 수 배지만 그려 이 세그먼트 목록을 쓰지
// 않지만(2026-07-25 사용자), 세그먼트별 텍스트·색 우선순위 로직은 테스트가 여전히
// 검증한다 — 프로덕션 미사용이라 cfg(test)로 경고만 제거한다.
#[cfg(test)]
fn workspace_summary_segments_for_mode(
    summary: SidebarSessionSummary,
    weak: egui::Color32,
    mode: WorkspaceSummaryMode,
    catalog: &i18n::Catalog,
) -> Vec<(String, egui::Color32)> {
    match mode {
        WorkspaceSummaryMode::IconOnly => Vec::new(),
        WorkspaceSummaryMode::Compact => {
            vec![workspace_primary_summary_segment(summary, catalog)]
        }
        WorkspaceSummaryMode::Full => workspace_summary_segments(summary, weak, catalog),
    }
}

fn workspace_primary_summary_segment(
    summary: SidebarSessionSummary,
    catalog: &i18n::Catalog,
) -> (String, egui::Color32) {
    use crate::agent_surface::AgentVisualState as VisualState;

    if summary.no_sessions {
        return (
            catalog.t("workspace.summary.no_sessions", &[]),
            crate::ui::agent_visuals::status_color(VisualState::Off),
        );
    }
    if summary.error > 0 {
        return (
            catalog.t(
                "workspace.summary.error",
                &[("count", &summary.error.to_string())],
            ),
            crate::ui::agent_visuals::status_color(VisualState::Error),
        );
    }
    if summary.waiting > 0 {
        return (
            catalog.t(
                "workspace.summary.waiting",
                &[("count", &summary.waiting.to_string())],
            ),
            crate::ui::agent_visuals::status_color(VisualState::Waiting),
        );
    }
    if summary.running > 0 {
        return (
            catalog.t(
                "workspace.summary.running",
                &[("count", &summary.running.to_string())],
            ),
            crate::ui::agent_visuals::status_color(VisualState::Active),
        );
    }
    if summary.done > 0 {
        return (
            catalog.t(
                "workspace.summary.done",
                &[("count", &summary.done.to_string())],
            ),
            crate::ui::agent_visuals::status_color(VisualState::Complete),
        );
    }
    if summary.idle > 0 {
        return (
            if summary.idle == 1 {
                catalog.t("workspace.summary.idle", &[])
            } else {
                catalog.t(
                    "workspace.summary.idle_count",
                    &[("count", &summary.idle.to_string())],
                )
            },
            crate::ui::agent_visuals::status_color(VisualState::Idle),
        );
    }
    if summary.inactive > 0 {
        return (
            catalog.t("workspace.summary.inactive", &[]),
            crate::ui::agent_visuals::status_color(VisualState::Off),
        );
    }
    (
        catalog.t("workspace.summary.idle", &[]),
        crate::ui::agent_visuals::status_color(VisualState::Idle),
    )
}

#[cfg(test)]
fn workspace_summary_segments(
    summary: SidebarSessionSummary,
    weak: egui::Color32,
    catalog: &i18n::Catalog,
) -> Vec<(String, egui::Color32)> {
    use crate::agent_surface::AgentVisualState as VisualState;

    // 세션 행·Agents·알림과 같은 단일 팔레트를 사용한다. 워크스페이스 요약만 별도
    // RGB를 가지면 같은 상태가 표면마다 다른 색으로 보여 상태 의미가 흐려진다.
    let running = crate::ui::agent_visuals::status_color(VisualState::Active);
    let waiting = crate::ui::agent_visuals::status_color(VisualState::Waiting);
    let done = crate::ui::agent_visuals::status_color(VisualState::Complete);
    let error = crate::ui::agent_visuals::status_color(VisualState::Error);
    let idle = crate::ui::agent_visuals::status_color(VisualState::Idle);
    let inactive = crate::ui::agent_visuals::status_color(VisualState::Off);
    let mut parts = Vec::new();
    let push = |parts: &mut Vec<(String, egui::Color32)>, label: String, color| {
        if !parts.is_empty() {
            parts.push((" · ".to_owned(), weak));
        }
        parts.push((label, color));
    };
    if summary.running > 0 {
        push(
            &mut parts,
            catalog.t(
                "workspace.summary.running",
                &[("count", &summary.running.to_string())],
            ),
            running,
        );
    }
    if summary.waiting > 0 {
        push(
            &mut parts,
            catalog.t(
                "workspace.summary.waiting",
                &[("count", &summary.waiting.to_string())],
            ),
            waiting,
        );
    }
    if summary.done > 0 {
        push(
            &mut parts,
            catalog.t(
                "workspace.summary.done",
                &[("count", &summary.done.to_string())],
            ),
            done,
        );
    }
    if summary.error > 0 {
        push(
            &mut parts,
            catalog.t(
                "workspace.summary.error",
                &[("count", &summary.error.to_string())],
            ),
            error,
        );
    }
    if summary.idle > 0 {
        if summary.idle == 1 && parts.is_empty() {
            push(&mut parts, catalog.t("workspace.summary.idle", &[]), idle);
        } else {
            push(
                &mut parts,
                catalog.t(
                    "workspace.summary.idle_count",
                    &[("count", &summary.idle.to_string())],
                ),
                idle,
            );
        }
    }
    if summary.inactive > 0 {
        if parts.is_empty() {
            push(
                &mut parts,
                catalog.t("workspace.summary.inactive", &[]),
                inactive,
            );
        } else {
            push(
                &mut parts,
                catalog.t(
                    "workspace.summary.inactive_count",
                    &[("count", &summary.inactive.to_string())],
                ),
                inactive,
            );
        }
    }
    if summary.no_sessions && parts.is_empty() {
        parts.push((catalog.t("workspace.summary.no_sessions", &[]), inactive));
    }
    // 세션이 아직 생성되지 않은 활성/warm 워크스페이스도 상태 영역을 비워 두지 않는다.
    if parts.is_empty() {
        parts.push((catalog.t("workspace.summary.idle", &[]), idle));
    }
    parts
}

/// 비활성(warm) workspace의 마지막 세션 스냅샷. 편집/컨텍스트 작업은 활성 runtime을
/// 전제로 하므로 노출하지 않고, 클릭만 workspace 전환 + 정확한 tab/pane focus로 보낸다.
/// 반환값 두 번째 필드는 세션 행 전체의 합집합 rect — 인셋 배경의 네 경계를
/// 이걸로 맞춰야 hover 영역과 정확히 일치한다(아래 paint_workspace_session_inset
/// 참고, 2026-07-25 사용자: "인셋이 hover보다 커서 빈 공간 생기는거"). 헤더의
/// full_rect는 세션 ScrollArea 밖이라 스크롤바 유무로 폭이 안 흔들리지만, 세션
/// 행은 ScrollArea 안이라 실제 폭이 다를 수 있어 group_rect로 대체할 수 없다.
fn inactive_workspace_sessions(
    ui: &mut egui::Ui,
    workspace_id: &str,
    sessions: &[SessionEntry],
    _max_height: f32,
    accent_color: egui::Color32,
) -> (Option<SidebarAction>, Option<egui::Rect>) {
    if sessions.is_empty() {
        return (None, None);
    }
    let mut action = None;
    let mut session_rows_rect = None;
    egui::ScrollArea::vertical()
        .id_salt(("inactive_session_list_scroll", workspace_id))
        .auto_shrink([false, true])
        .show(ui, |ui| {
            // 헤더-세션 사이 여백 없음(2026-07-25 사용자) — 첫 행이 인셋 상단에
            // 바로 붙는다.
            ui.spacing_mut().item_spacing.y = 0.0;
            for (index, entry) in sessions.iter().enumerate() {
                let is_last = index + 1 == sessions.len();
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.vertical(|ui| {
                    let response = session_row(ui, entry, is_last, accent_color);
                        session_rows_rect = Some(
                            session_rows_rect.map_or(response.rect, |rect: egui::Rect| rect.union(response.rect)),
                        );
                        if response.clicked() {
                            action = Some(SidebarAction::FocusSession {
                                workspace_id: workspace_id.to_owned(),
                                tab: entry.tab.clone(),
                                pane: entry.pane.clone(),
                            });
                        }
                    });
                });
            }
        });
    (action, session_rows_rect)
}

fn session_row(
    ui: &mut egui::Ui,
    entry: &SessionEntry,
    is_last: bool,
    accent_color: egui::Color32,
) -> egui::Response {
    session_row_impl(ui, entry, None, is_last, accent_color)
}

/// 이름 인라인 편집 중인 행 — 레일/보조 행(2·3행)은 그대로 유지하고 **제목 자리만**
/// TextEdit로 바꾼다. 행 전체를 편집기로 대체하면 편집 중 레이아웃이 무너진다
/// (2026-07-16 사용자).
fn session_row_editing(
    ui: &mut egui::Ui,
    entry: &SessionEntry,
    buf: &mut String,
    is_last: bool,
    accent_color: egui::Color32,
) -> egui::Response {
    session_row_impl(ui, entry, Some(buf), is_last, accent_color)
}

// 워크스페이스 헤더의 우측 인셋(workspace_row 내부 rect =
// full_rect.shrink2((WORKSPACE_CARD_HORIZONTAL_INSET + 8, 0)))
// 과 같은 6px — 8px일 땐 세션 카드 배경이 위 워크스페이스 카드보다 우측 여백이
// 2px 더 넓어 보였다(2026-07-25 사용자).
const SESSION_HIGHLIGHT_RIGHT_INSET: f32 = WORKSPACE_CARD_HORIZONTAL_INSET;
const SESSION_RAIL_LEFT_INSET: f32 = 0.0;
const SESSION_RAIL_MAX_WIDTH: f32 = 4.5;
const SESSION_RAIL_HEIGHT: f32 = 35.0;
fn session_text_inset(rail_width: f32) -> f32 {
    SESSION_RAIL_LEFT_INSET + rail_width
}
// 폰트 기본 줄높이(CJK 포함이라 여유 있게 잡힘) 대신 폰트 크기에 곱하는 비율로
// 세션 정보의 2~3개 행 사이에 참고 이미지 수준의 여유를 둔다.
// 절대 px(예전엔 15.0/12.0 고정값)는 폰트 크기가 바뀌면 그대로 깨진다 —
// cmux/Warp 조사 후 Warp의 DEFAULT_UI_LINE_HEIGHT_RATIO 패턴을 따라 비율로
// 바꿨다(2026-07-25 사용자). 호출부는 자기 폰트 크기 × 이 비율을 쓴다.
const SESSION_LINE_HEIGHT_RATIO: f32 = 1.0;
const SESSION_CONTENT_RIGHT_INSET: f32 = 24.0;
/// 세션 행은 호출부(session_list_scroll/inactive_workspace_sessions)가
    /// `ui.add_space(16.0)`으로 들여쓰는데, 워크스페이스 헤더는 같은 원점 기준 8px만
/// 들여쓴다(위 SESSION_HIGHLIGHT_RIGHT_INSET 주석 참고). 배경을 그대로
/// rect.left()에서 시작하면 워크스페이스 카드보다 12px(20-8) 더 안쪽에서
/// 시작해 레일 왼쪽에 배경이 안 칠해진 틈이 생긴다(2026-07-25 사용자) — 그만큼
/// 왼쪽으로 더 그린다.
const SESSION_HIGHLIGHT_LEFT_EXTEND: f32 = 20.0 - WORKSPACE_SESSION_INSET_LEFT;

fn session_highlight_rect(rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(rect.left() - SESSION_HIGHLIGHT_LEFT_EXTEND, rect.top()),
        egui::pos2(
            (rect.right() - SESSION_HIGHLIGHT_RIGHT_INSET).max(rect.left()),
            rect.bottom(),
        ),
    )
}

fn session_inset_fill_rect(rect: egui::Rect) -> egui::Rect {
    let highlight = session_highlight_rect(rect);
    egui::Rect::from_min_max(
        egui::pos2(highlight.left(), highlight.top() - 1.0),
        egui::pos2(highlight.right(), highlight.bottom() - 1.0),
    )
}

fn session_focus_fill_rect(rect: egui::Rect) -> egui::Rect {
    let highlight = session_highlight_rect(rect);
    egui::Rect::from_min_max(
        highlight.min,
        egui::pos2(highlight.right(), highlight.bottom() - 1.0),
    )
}

fn workspace_session_inset_rect(
    group_rect: egui::Rect,
    header_bottom: f32,
    session_rows_rect: egui::Rect,
) -> egui::Rect {
    let hover = session_inset_fill_rect(session_rows_rect);
    egui::Rect::from_min_max(
        egui::pos2(
            group_rect.left() + WORKSPACE_SESSION_INSET_LEFT,
            hover.top().max(header_bottom),
        ),
        hover.max,
    )
}

fn session_title_lines(
    ui: &egui::Ui,
    entry: &SessionEntry,
    status_color: egui::Color32,
    separator_color: egui::Color32,
    max_width: f32,
) -> (
    std::sync::Arc<egui::Galley>,
    Option<std::sync::Arc<egui::Galley>>,
) {
    let title_size = 13.0;
    let title_font_id = crate::fonts::sidebar_font(title_size);
    // 제목보다 상태를 1pt 작게 두어 `폴더명 · 상태`의 시각적 위계를 분리한다.
    let status_size = 12.0;
    let status_font_id = crate::fonts::sidebar_font(status_size);
    let status_line_height = status_size * SESSION_LINE_HEIGHT_RATIO;
    let status_galley = entry
        .status_label
        .as_deref()
        .filter(|status| !status.is_empty())
        .map(|status| {
            let mut job = egui::text::LayoutJob::default();
            job.append(
                " · ",
                0.0,
                egui::TextFormat {
                    font_id: status_font_id.clone(),
                    color: separator_color,
                    line_height: Some(status_line_height),
                    ..Default::default()
                },
            );
            job.append(
                status,
                0.0,
                egui::TextFormat {
                    font_id: status_font_id,
                    color: status_color,
                    line_height: Some(status_line_height),
                    ..Default::default()
                },
            );
            ui.painter().layout_job(job)
        });
    let status_width = status_galley.as_ref().map_or(0.0, |galley| galley.size().x);
    let title_width = (max_width - status_width).max(10.0);
    let title_galley = clipped_line(
        ui,
        &entry.title,
        title_font_id,
        title_width,
        Some(title_size * SESSION_LINE_HEIGHT_RATIO),
    );
    (title_galley, status_galley)
}

fn session_row_impl(
    ui: &mut egui::Ui,
    entry: &SessionEntry,
    edit_buf: Option<&mut String>,
    is_last: bool,
    accent_color: egui::Color32,
) -> egui::Response {
    // 에이전트면 3줄(제목/에이전트·모델·effort/상태·ctx%), 아니면 2줄(제목/요약).
    // 요약이 없어도(유휴/시작 직후) 2행에 '~'를 표시해 행 높이를 유지한다(2026-07-07).
    let agent = entry.agent_line.is_some();
    let summary_text: &str = if entry.summary.is_empty() {
        "~"
    } else {
        &entry.summary
    };
    // 46/34에서 레일·텍스트가 바닥 밖으로 삐져나와 51/39로 5px씩 늘렸다(2026-07-25
    // 사용자 스샷). row_h는 여전히 고정값이라 폰트/언어별 실제 렌더 높이가 이 값을
    // 넘으면 같은 문제가 재발할 수 있다 — 근본 해결은 행 높이를 실측 갤리 높이로
    // 동적 계산하는 것이지만, 그러려면 지금 화면 밖 행에서 건너뛰는 텍스트
    // 레이아웃(is_rect_visible 조기 리턴, 위 참고)을 모든 행에서 항상 해야 해서
    // 스크롤 목록 성능과 맞바꿔야 한다(사용자 확인 대기).
    // 줄 사이 간격을 1px씩 더 좁혀서(아래 gap 계산) 남는 줄 수만큼 그대로
    // 줄인다(2026-07-25 사용자: "행간 간격을 1px 줄여도 돼") — 안 그러면 위/아래
    // 여백 대칭(SESSION_TEXT_MARGIN)이 깨진다.
    let line_count = if agent { 3.0 } else { 2.0 };
    let row_h = if agent {
        51.0 - (line_count - 1.0)
    } else {
        36.0
    };
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), row_h),
        egui::Sense::click(),
    );
    // 제목은 painter galley로 그리므로 별도 접근성 라벨이 없으면 키보드/스크린리더와
    // kittest가 세션 행을 식별할 수 없다. 클릭 행 자체를 제목이 있는 버튼으로 노출한다.
    resp.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            ui.is_enabled(),
            entry.title.as_str(),
        )
    });
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    // 색을 먼저 복사(Copy)해 visuals 차용을 끝낸 뒤 ui.fonts로 galley를 만든다.
    let dot = session_entry_status_color(entry);
    // 텍스트는 최대 레일 폭을 기준으로 고정해 attention/pulse 중에도 좌우로 흔들리지
    // 않게 한다. 포커스/hover 배경 시작점만 아래에서 실제 레일 폭에 맞춘다.
    let (rail_w, rail_color) = if let Some((t, color)) = entry.pulse {
        (2.0 + 2.5 * (t * std::f32::consts::PI).sin(), color)
    } else if entry.attention {
        (4.5, dot)
    } else {
        (2.0, dot)
    };
    let text_inset = session_text_inset(SESSION_RAIL_MAX_WIDTH);
    // 2·3행(보조 정보): 다크는 기존 weak 톤, 라이트는 weak가 패널 위에서 너무 옅어
    // textSecondary(#444444) 수준으로 진하게 (라이트 테마 회색 흐림, 2026-07-10).
    let sub_color = if ui.visuals().dark_mode {
        ui.visuals().weak_text_color().gamma_multiply(0.9)
    } else {
        egui::Color32::from_rgb(0x44, 0x44, 0x44)
    };
    let title_color = ui.visuals().text_color();
    // 텍스트는 행 폭(좌 11 + 우 여백 16) 안으로 잘라 '…' 처리 — 고정 글자수 truncate는
    // 좁은 사이드바에서 박스 밖으로 삐져나갔다(#91 사용자).
    let max_w = (rect.width() - text_inset - SESSION_CONTENT_RIGHT_INSET).max(10.0);
    let (title_galley, status_galley) = session_title_lines(ui, entry, dot, sub_color, max_w);
    // 2행/3행: 에이전트면 agent_line/status_line, 아니면 요약(2행)만.
    let (line2, line3) = if agent {
        (entry.agent_line.as_deref(), entry.status_line.as_deref())
    } else {
        (Some(summary_text), None)
    };
    let subline_size = 10.5;
    let subline_line_height = Some(subline_size * SESSION_LINE_HEIGHT_RATIO);
    let line2_galley = line2.map(|t| {
        clipped_line(
            ui,
            t,
            crate::fonts::sidebar_font(subline_size),
            max_w,
            subline_line_height,
        )
    });
    let line3_galley = line3.map(|t| {
        clipped_line(
            ui,
            t,
            crate::fonts::sidebar_font(subline_size),
            max_w,
            subline_line_height,
        )
    });

    let painter = ui.painter();
    let highlight_rect = session_highlight_rect(rect);
    // 행 자체의 상시 배경(구 #2a2a33)은 걷어냈다 — 워크스페이스 헤더와 한 카드로
    // 감싸는 배경(paint_workspace_group_wrap, #0f171d)이 호출부에서 먼저 깔린다.
    // selected가 hover보다 우선한다. 두 상태 모두 같은 행 영역을 사용해
    // 포인터 이동 시 크기나 좌표가 달라지지 않는다.
    let state_fill = if entry.focused {
        Some(accent_color.gamma_multiply(0.24))
    } else if resp.hovered() {
        Some(accent_color.gamma_multiply(0.16))
    } else {
        None
    };
    if let Some(fill) = state_fill {
        let focus_rect = session_focus_fill_rect(rect);
        let focus_rect = egui::Rect::from_min_max(
            egui::pos2(rect.left() + SESSION_RAIL_LEFT_INSET + rail_w, focus_rect.top()),
            focus_rect.max,
        );
        painter.rect_filled(focus_rect, 1.0, fill);
    }
    // 세션이 둘 이상일 때 행 사이를 구분선으로 나눈다(2026-07-25 사용자) — 마지막
    // 행은 그리지 않는다(카드/인셋 바닥과 겹쳐 이중선으로 보이는 것 방지).
    if !is_last {
        painter.hline(
            highlight_rect.x_range(),
            highlight_rect.bottom(),
            egui::Stroke::new(1.0, WORKSPACE_SESSION_INSET_BORDER),
        );
    }
    // 좌측 상태 레일 — 항상 표시, 상태 색으로 세로로 훑어 파악 (목업 §세션).
    // 평시 2px, 미확인 완료/입력대기(attention)는 4.5px로 굵힌다. 알림 도착 시 이미
    // 보고 있던 pane은 1회 펄스(2→4.5→2px). 자리는 최대 폭 기준으로 상시 예약한다.
    // 레일 높이는 행 수와 분리해 35px로 고정하고 행의 세로 중앙에 배치한다.
    let rail = egui::Rect::from_min_size(
        egui::pos2(
            rect.left() + SESSION_RAIL_LEFT_INSET,
            rect.center().y - SESSION_RAIL_HEIGHT / 2.0,
        ),
        egui::vec2(rail_w, SESSION_RAIL_HEIGHT),
    );
    painter.rect_filled(rail, 0.0, rail_color);
    // 제목(1행) + 2행 + 3행 — 세로 위치는 행 수에 맞춰.
    // 위/아래 여백을 2px로 대칭 맞춘다(2026-07-25 사용자: 텍스트 내리고, 아래
    // 여백 2, 위아래 대칭). 실제 렌더된 줄 높이(galley.size().y)로 계산해야
    // 고정 오프셋(9/23/37 등)처럼 가정한 줄 높이가 틀려서 어긋나는 일이 없다.
    // 남는 공간은 줄 사이에 균등 배분한다.
const SESSION_TEXT_MARGIN: f32 = 2.25;
    let line_heights = [
        Some(title_galley.size().y),
        line2_galley.as_ref().map(|g| g.size().y),
        line3_galley.as_ref().map(|g| g.size().y),
    ];
    let heights: Vec<f32> = line_heights.into_iter().flatten().collect();
    let content_h: f32 = heights.iter().sum();
    let available = (row_h - 2.0 * SESSION_TEXT_MARGIN).max(0.0);
    let gap = if heights.len() > 1 {
        ((available - content_h) / (heights.len() as f32 - 1.0)).max(0.0)
    } else {
        0.0
    };
    let mut y = rect.top() + SESSION_TEXT_MARGIN;
    let title_center = y + title_galley.size().y / 2.0;
    y += title_galley.size().y + gap;
    let line2_center = line2_galley.as_ref().map(|g| {
        let c = y + g.size().y / 2.0;
        y += g.size().y + gap;
        c
    });
    let line3_center = line3_galley.as_ref().map(|g| y + g.size().y / 2.0);

    // 편집 중에는 제목 갤리 대신 같은 자리에 TextEdit를 얹는다 (아래 edit_buf 분기).
    if edit_buf.is_none() {
        let title_pos = egui::pos2(
            rect.left() + text_inset,
            title_center - title_galley.size().y / 2.0,
        );
        painter.galley(title_pos, title_galley.clone(), title_color);
        if let Some(status_galley) = status_galley {
            painter.galley(
                egui::pos2(
                    title_pos.x + title_galley.size().x,
                    title_center - status_galley.size().y / 2.0,
                ),
                status_galley,
                egui::Color32::WHITE,
            );
        }
    }
    // line2_center/line3_center는 line2_galley/line3_galley와 같은 Option에서
    // 나왔으므로(위 계산부) 항상 함께 Some/None이다 — 튜플 매치로 그 관계를 드러낸다.
    if let (Some(g), Some(center)) = (line2_galley, line2_center) {
        painter.galley(
            egui::pos2(rect.left() + text_inset, center - g.size().y / 2.0),
            g,
            sub_color,
        );
    }
    if let (Some(g), Some(center)) = (line3_galley, line3_center) {
        painter.galley(
            egui::pos2(rect.left() + text_inset, center - g.size().y / 2.0),
            g,
            sub_color,
        );
    }
    if let Some(buf) = edit_buf {
        // 제목 1행 자리에 프레임 없는 TextEdit — 글꼴/x 위치를 제목 갤리와 맞춘다.
        // 세로는 title_center를 감싸는 title_galley 높이만큼의 박스로.
        let half_h = title_galley.size().y / 2.0;
        let title_rect = egui::Rect::from_min_max(
            egui::pos2(rect.left() + text_inset, title_center - half_h),
            egui::pos2(
                rect.right() - SESSION_CONTENT_RIGHT_INSET,
                title_center + half_h,
            ),
        );
        let edit_resp = ui.put(
            title_rect,
            egui::TextEdit::singleline(buf)
                .font(crate::fonts::sidebar_font(13.0))
                .frame(egui::Frame::NONE)
                .margin(egui::Margin::ZERO)
                .vertical_align(egui::Align::Center),
        );
        edit_resp.request_focus();
    }
    resp
}

/// 한 줄 텍스트를 max_width 안으로 잘라 '…'로 끝내는 galley (박스 밖 삐짐 방지, #91).
/// `line_height`를 주면 폰트 기본 줄높이 대신 그 값을 쓴다 — 세션 행처럼 여러 줄을
/// 촘촘히 쌓아야 할 때만 좁혀 쓰고, 나머지 호출부는 None으로 기존 그대로 둔다.
fn clipped_line(
    ui: &egui::Ui,
    text: &str,
    font_id: egui::FontId,
    max_width: f32,
    line_height: Option<f32>,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat {
            font_id,
            // PLACEHOLDER여야 painter.galley의 fallback 색이 적용된다 — 기본값
            // Color32::GRAY는 fallback을 무시하고 항상 회색으로 그려졌다(라이트 흐림 원인).
            color: egui::Color32::PLACEHOLDER,
            line_height,
            ..Default::default()
        },
    );
    job.wrap = egui::text::TextWrapping {
        max_width,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.painter().layout_job(job)
}

/// 트리 확장 캐럿 (▸ 접힘 / ▾ 펼침) — 작은 삼각형 (이모지 □ 깨짐 회피).
fn paint_caret(p: &egui::Painter, c: egui::Pos2, expanded: bool, col: egui::Color32) {
    let d = 3.0;
    let pts = if expanded {
        vec![
            egui::pos2(c.x - d, c.y - d * 0.6),
            egui::pos2(c.x + d, c.y - d * 0.6),
            egui::pos2(c.x, c.y + d * 0.8),
        ]
    } else {
        vec![
            egui::pos2(c.x - d * 0.6, c.y - d),
            egui::pos2(c.x - d * 0.6, c.y + d),
            egui::pos2(c.x + d * 0.8, c.y),
        ]
    };
    p.add(egui::Shape::convex_polygon(pts, col, egui::Stroke::NONE));
}

enum FileToolbarIcon {
    Hidden,
    Folder,
    File,
    Search,
}

fn file_toolbar_icon_at(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    id: &'static str,
    icon: FileToolbarIcon,
    active: bool,
) -> egui::Response {
    let response = ui.interact(
        rect,
        ui.id().with(("file_toolbar_icon", id)),
        egui::Sense::click(),
    );
    let color = if active {
        ui.visuals().selection.stroke.color
    } else if response.hovered() {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    match icon {
        FileToolbarIcon::Hidden => {
            let center = rect.center();
            let stroke = egui::Stroke::new(1.2, color);
            // 원본(±7, ±4, 동공 r2)의 14.5% 축소 (2026-07-18 사용자).
            let upper = vec![
                egui::pos2(center.x - 5.985, center.y),
                egui::pos2(center.x - 2.9925, center.y - 2.736),
                egui::pos2(center.x, center.y - 3.42),
                egui::pos2(center.x + 2.9925, center.y - 2.736),
                egui::pos2(center.x + 5.985, center.y),
            ];
            let lower = vec![
                egui::pos2(center.x - 5.985, center.y),
                egui::pos2(center.x - 2.9925, center.y + 2.736),
                egui::pos2(center.x, center.y + 3.42),
                egui::pos2(center.x + 2.9925, center.y + 2.736),
                egui::pos2(center.x + 5.985, center.y),
            ];
            ui.painter().add(egui::Shape::line(upper, stroke));
            ui.painter().add(egui::Shape::line(lower, stroke));
            ui.painter().circle_filled(center, 1.71, color);
        }
        FileToolbarIcon::Folder => {
            // 원본 15×14의 14.5% 축소 (2026-07-18 사용자).
            paint_folder(
                ui.painter(),
                rect.center(),
                color,
                egui::vec2(12.825, 11.875),
            )
        }
        FileToolbarIcon::File => paint_file(
            ui.painter(),
            rect.center(),
            color,
            ui.visuals().panel_fill,
            // 원본 11×14의 23% 축소 (툴바 파일만 추가 10%, 2026-07-18 사용자).
            egui::vec2(8.4645, 10.773),
        ),
        FileToolbarIcon::Search => {
            // 터미널 pane 헤더 검색(workspace.rs paint_terminal_toolbar_icon)과 동일
            // 디자인 — 렌즈 r3.2·얇은 스트로크·짧은 핸들. 크기만 툴바에 맞춰 1.3배
            // (스트로크는 헤더의 1.25 유지, 2026-07-18 사용자).
            let lens = rect.center() + egui::vec2(-1.17, -1.17);
            let stroke = egui::Stroke::new(1.25, color);
            ui.painter().circle_stroke(lens, 4.16, stroke);
            ui.painter().line_segment(
                [lens + egui::vec2(2.99, 2.99), lens + egui::vec2(5.85, 5.85)],
                stroke,
            );
        }
    }
    response
}

/// 현재 위치(파일 도크 루트) 표식 — 열린 폴더. 아래 트리의 닫힌 폴더와 구분해
/// "지금 이 폴더가 열려 있다"를 나타낸다(고정 앵커 아님 — `..`로 자유 이동,
/// 2026-07-19 사용자). `size`는 전체 (폭, 높이).
fn paint_folder_open(p: &egui::Painter, c: egui::Pos2, col: egui::Color32, size: egui::Vec2) {
    let w = size.x;
    let stroke = egui::Stroke::new(1.2, col);
    let rise = (size.y * 0.24).round().max(2.0);
    // 뒤판(탭 달린 몸통) — paint_folder와 같은 비율.
    let body = egui::Rect::from_min_size(
        egui::pos2(c.x - w / 2.0, c.y - size.y / 2.0 + rise),
        egui::vec2(w, size.y - rise),
    );
    let tab = egui::Rect::from_min_size(
        egui::pos2(body.left(), body.top() - rise),
        egui::vec2(w * 0.45, rise + 1.0),
    );
    p.rect_stroke(tab, 1.0, stroke, egui::StrokeKind::Inside);
    p.rect_stroke(body, 1.0, stroke, egui::StrokeKind::Inside);
    // 앞면(열린 덮개) — 몸통 안쪽에서 오른쪽으로 벌어진 사다리꼴로 "열림"을 표현.
    let inset = 1.5;
    let flap = vec![
        egui::pos2(body.left() + inset, body.bottom() - inset),
        egui::pos2(body.right() - inset, body.bottom() - inset),
        egui::pos2(
            body.right() - inset - w * 0.14,
            body.top() + body.height() * 0.42,
        ),
        egui::pos2(
            body.left() + inset + w * 0.14,
            body.top() + body.height() * 0.42,
        ),
    ];
    p.add(egui::Shape::closed_line(flap, stroke));
}

fn compact_root_path(root: &Path) -> String {
    if let Some(home) = crate::paths::home_dir()
        && let Ok(relative) = root.strip_prefix(home)
    {
        return format!("~/{}", relative.display());
    }
    root.display().to_string()
}

/// 파일 트리와 셸 `LS_COLORS`가 공유하는 어두운 배경용 유형 팔레트.
fn file_entry_color(name: &str, is_dir: bool, fallback: egui::Color32) -> egui::Color32 {
    if is_dir {
        return egui::Color32::from_rgb(0x4c, 0xa8, 0xdf);
    }
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "cargo.toml"
            | "cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | ".gitignore"
            | ".gitattributes"
            | ".env"
    ) {
        return egui::Color32::from_rgb(0xd7, 0xa6, 0x5f);
    }
    match Path::new(&lower)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
    {
        "rs" => egui::Color32::from_rgb(0xe5, 0x8c, 0x55),
        "js" | "jsx" | "mjs" | "cjs" => egui::Color32::from_rgb(0xe5, 0xc0, 0x7b),
        "ts" | "tsx" => egui::Color32::from_rgb(0x61, 0xaf, 0xef),
        "html" | "htm" | "css" | "scss" | "sass" | "less" => {
            egui::Color32::from_rgb(0x56, 0xb6, 0xc2)
        }
        "py" | "rb" | "go" | "sh" | "bash" | "zsh" | "fish" => {
            egui::Color32::from_rgb(0x98, 0xc3, 0x79)
        }
        "toml" | "json" | "jsonc" | "yaml" | "yml" | "xml" | "ini" | "conf" | "config" => {
            egui::Color32::from_rgb(0xd7, 0xa6, 0x5f)
        }
        "md" | "mdx" | "txt" | "rst" | "pdf" | "doc" | "docx" => {
            egui::Color32::from_rgb(0x7e, 0xc6, 0x99)
        }
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "heic" | "avif" | "bmp" | "tif"
        | "tiff" => egui::Color32::from_rgb(0xc6, 0x78, 0xdd),
        "mp3" | "wav" | "flac" | "aac" | "m4a" | "mp4" | "mov" | "mkv" | "webm" => {
            egui::Color32::from_rgb(0xe0, 0x6c, 0x75)
        }
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" => {
            egui::Color32::from_rgb(0xa8, 0x78, 0xd4)
        }
        _ => fallback,
    }
}

const WORKSPACE_ACCENT_PALETTE: [(u8, u8, u8); 8] = [
    (0x55, 0xc8, 0x79), // emerald
    (0xe7, 0x9a, 0x3b), // orange
    (0x9a, 0x78, 0xe8), // violet
    (0xe0, 0x5d, 0x69), // crimson
    (0xc8, 0xaa, 0x35), // gold
    (0x4c, 0x84, 0xdf), // blue
    (0xcc, 0x65, 0xae), // pink
    (0x3b, 0xa3, 0xa0), // teal
];

/// 안정 ID가 DB 생성순 목록에서 차지하는 slot으로 색을 배정한다. palette 앞쪽 여섯
/// 계열은 녹색·주황·보라·빨강·금색·파랑 순으로 의도적으로 떨어뜨렸다. 따라서 선택/
/// 접힘으로 렌더 순서가 바뀌어도 색은 유지되고, 같은 이니셜도 서로 다른 계열을 갖는다.
/// 생성순 목록 끝에 새 워크스페이스를 추가해도 기존 배정은 변하지 않는다.
fn workspace_accent(workspaces: &[SidebarWorkspaceEntry], workspace_id: &str) -> egui::Color32 {
    let slot = workspaces
        .iter()
        .position(|workspace| workspace.id == workspace_id)
        .map(|ordinal| ordinal % WORKSPACE_ACCENT_PALETTE.len())
        .unwrap_or_else(|| {
            workspace_id.bytes().fold(0usize, |acc, byte| {
                acc.wrapping_mul(31).wrapping_add(byte as usize)
            }) % WORKSPACE_ACCENT_PALETTE.len()
        });
    let (red, green, blue) = WORKSPACE_ACCENT_PALETTE[slot];
    egui::Color32::from_rgb(red, green, blue)
}

/// 폴더 아이콘 — 참고 시안처럼 탭 + 본체의 얇은 윤곽선.
/// 폴더 아이콘 — `size`는 탭 돌출까지 포함한 전체 (폭, 높이). 헤더/트리 행은
/// 13.5×12.5, 툴바는 10×10 (2026-07-18 사용자 확정 수치).
fn paint_folder(p: &egui::Painter, c: egui::Pos2, col: egui::Color32, size: egui::Vec2) {
    let w = size.x;
    let rise = (size.y * 0.24).round().max(2.0); // 12.5 기준 3px 탭 돌출 비례
    let body_h = size.y - rise;
    let body = egui::Rect::from_min_size(
        egui::pos2(c.x - w / 2.0, c.y - size.y / 2.0 + rise),
        egui::vec2(w, body_h),
    );
    let tab = egui::Rect::from_min_size(
        egui::pos2(body.left(), body.top() - rise),
        egui::vec2(w * 0.45, rise + 1.0),
    );
    let stroke = egui::Stroke::new(1.2, col);
    p.rect_stroke(tab, 1.0, stroke, egui::StrokeKind::Inside);
    p.rect_stroke(body, 1.0, stroke, egui::StrokeKind::Inside);
}

/// 파일 아이콘 — 문서(접힌 모서리). `carve`는 접힌 모서리를 파낼 배경색.
/// `size` = (폭, 높이) — 트리 행 10×12.6, 툴바 7.9×10 (2026-07-18 사용자 확정).
fn paint_file(
    p: &egui::Painter,
    c: egui::Pos2,
    col: egui::Color32,
    carve: egui::Color32,
    size: egui::Vec2,
) {
    let w = size.x;
    let h = size.y;
    let fold = (h * 0.29).round(); // 14 기준 4px 접힘 비례
    let l = c.x - w / 2.0;
    let r = c.x + w / 2.0;
    let top = c.y - h / 2.0;
    let bot = c.y + h / 2.0;
    let body = vec![
        egui::pos2(l, top),
        egui::pos2(r - fold, top),
        egui::pos2(r, top + fold),
        egui::pos2(r, bot),
        egui::pos2(l, bot),
    ];
    p.add(egui::Shape::convex_polygon(body, col, egui::Stroke::NONE));
    let corner = vec![
        egui::pos2(r - fold, top),
        egui::pos2(r - fold, top + fold),
        egui::pos2(r, top + fold),
    ];
    p.add(egui::Shape::convex_polygon(
        corner,
        carve,
        egui::Stroke::NONE,
    ));
}

/// 하단 nav 아이콘 종류 (2026-07-18 확정 디자인).
enum NavIcon {
    Home,
    Inbox,
    Fleet,
    Agents,
}

/// 작업함 배지 문구 — 0이면 숨김(None).
fn nav_badge_text(count: usize) -> Option<String> {
    (count > 0).then(|| count.to_string())
}

fn sidebar_vertical_section_heights(available: f32, requested_navigation: f32) -> (f32, f32) {
    let max_navigation = (available - SIDEBAR_NAV_SPLIT_HANDLE_HEIGHT - SIDEBAR_BODY_MIN_HEIGHT)
        .max(SIDEBAR_NAV_MIN_HEIGHT);
    let navigation = requested_navigation.clamp(SIDEBAR_NAV_MIN_HEIGHT, max_navigation);
    let body = (available - SIDEBAR_NAV_SPLIT_HANDLE_HEIGHT - navigation).max(0.0);
    (body, navigation)
}

/// 하단 nav 행 하나 — 외곽선 아이콘 + 라벨, hover/선택 시 둥근 필(pill) 배경
/// (workspace_row와 같은 색 계열). painter 텍스트라 접근성 라벨은 widget_info로 단다.
fn nav_row(
    ui: &mut egui::Ui,
    icon: NavIcon,
    label: &str,
    selected: bool,
    badge: Option<&str>,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), SIDEBAR_NAV_ROW_HEIGHT),
        egui::Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let pill = rect.shrink2(egui::vec2(6.0, 0.8));
    if selected {
        ui.painter().rect_filled(
            pill,
            6.0,
            ui.visuals().selection.bg_fill.gamma_multiply(0.16),
        );
    } else if response.hovered() {
        ui.painter()
            .rect_filled(pill, 6.0, ui.visuals().widgets.hovered.bg_fill);
    }
    let color = if selected || response.hovered() {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    // 아이콘 레일(좁은 폭)에서는 아이콘만 중앙에 — workspace_row의 폭 단계 규칙과 동일.
    let show_label = rect.width() >= 64.0;
    let icon_center = if show_label {
        egui::pos2(pill.left() + 16.0, rect.center().y)
    } else {
        egui::pos2(rect.center().x, rect.center().y)
    };
    paint_nav_icon(ui.painter(), icon_center, icon, color);
    if show_label {
        ui.painter().text(
            egui::pos2(pill.left() + 32.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            crate::fonts::sidebar_font(13.0),
            color,
        );
        if let Some(badge) = badge {
            paint_nav_badge(ui, pill, badge);
        }
    }
    response
}

/// 작업함 카운트 배지 — 빨간 원형(두 자리부터는 알약꼴), 흰 숫자.
fn paint_nav_badge(ui: &egui::Ui, pill: egui::Rect, text: &str) {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        crate::fonts::sidebar_font(10.0),
        egui::Color32::WHITE,
    );
    let h = 16.0;
    let w = (galley.size().x + 8.0).max(h);
    let center = egui::pos2(pill.right() - 8.0 - w / 2.0, pill.center().y);
    let rect = egui::Rect::from_center_size(center, egui::vec2(w, h));
    ui.painter()
        .rect_filled(rect, h / 2.0, egui::Color32::from_rgb(0xed, 0x5b, 0x61));
    ui.painter()
        .galley(center - galley.size() / 2.0, galley, egui::Color32::WHITE);
}

/// 하단 nav 아이콘 — 이모지는 폰트 글리프가 없어 □로 깨진다(레포 관례: painter 직접
/// 드로잉 — paint_folder/file_toolbar_icon_at 참고). 1.3px 스트로크로 기존 톤과 맞춘다.
fn paint_nav_icon(p: &egui::Painter, c: egui::Pos2, icon: NavIcon, col: egui::Color32) {
    let stroke = egui::Stroke::new(1.3, col);
    match icon {
        // 집 — 지붕(꺾은선) + 몸통(사각).
        NavIcon::Home => {
            let roof = vec![
                egui::pos2(c.x - 6.5, c.y - 0.5),
                egui::pos2(c.x, c.y - 6.0),
                egui::pos2(c.x + 6.5, c.y - 0.5),
            ];
            p.add(egui::Shape::line(roof, stroke));
            let body = egui::Rect::from_min_max(
                egui::pos2(c.x - 4.5, c.y - 0.5),
                egui::pos2(c.x + 4.5, c.y + 6.0),
            );
            p.rect_stroke(body, 0.0, stroke, egui::StrokeKind::Inside);
        }
        // 서류함 — 상자 + 투입구 슬롯.
        NavIcon::Inbox => {
            let body = egui::Rect::from_center_size(c, egui::vec2(13.0, 11.0));
            p.rect_stroke(body, 1.5, stroke, egui::StrokeKind::Inside);
            p.line_segment(
                [
                    egui::pos2(c.x - 3.5, c.y - 2.0),
                    egui::pos2(c.x + 3.5, c.y - 2.0),
                ],
                stroke,
            );
        }
        // fleet — 2×2 격자(여러 에이전트를 한 화면에).
        NavIcon::Fleet => {
            for (dx, dy) in [(-3.0, -3.0), (3.0, -3.0), (-3.0, 3.0), (3.0, 3.0)] {
                let cell = egui::Rect::from_center_size(
                    egui::pos2(c.x + dx, c.y + dy),
                    egui::vec2(5.0, 5.0),
                );
                p.rect_stroke(cell, 1.0, stroke, egui::StrokeKind::Inside);
            }
        }
        // 봇 — 머리(사각) + 눈 2점 + 안테나.
        NavIcon::Agents => {
            let head =
                egui::Rect::from_center_size(egui::pos2(c.x, c.y + 1.0), egui::vec2(12.0, 9.0));
            p.rect_stroke(head, 1.5, stroke, egui::StrokeKind::Inside);
            p.line_segment(
                [
                    egui::pos2(c.x, head.top()),
                    egui::pos2(c.x, head.top() - 2.5),
                ],
                stroke,
            );
            p.circle_filled(egui::pos2(c.x, head.top() - 3.5), 1.2, col);
            p.circle_filled(egui::pos2(c.x - 2.5, c.y + 1.0), 1.2, col);
            p.circle_filled(egui::pos2(c.x + 2.5, c.y + 1.0), 1.2, col);
        }
    }
}

/// PTY 세션 상태 → 공통 에이전트 상태 색. 기존 호출부(App의 pulse, pane glyph)가
/// 같은 팔레트를 공유하도록 이 wrapper를 유지한다.
pub(crate) fn session_status_color(
    status: Option<runtime::SessionStatus>,
    _visuals: &egui::Visuals,
) -> egui::Color32 {
    crate::ui::agent_visuals::status_color(crate::agent_surface::AgentVisualState::from_pty(status))
}

/// 세션 행의 상태 점 색 — 에이전트 감지 여부까지 반영한다(from_pty_with_agent).
/// fleet 카드와 같은 규칙을 써야 같은 세션이 두 표면에서 다른 색으로 보이지 않는다.
pub(crate) fn session_entry_status_color(entry: &SessionEntry) -> egui::Color32 {
    let state = crate::agent_surface::AgentVisualState::from_pty_with_agent(
        entry.status,
        entry.agent_line.is_some(),
    );
    crate::ui::agent_visuals::status_color(state)
}

/// 현재 플랫폼/환경에서 새 shell session이 사용할 것으로 예상되는 기본 shell kind.
/// Runtime이 per-session shell metadata를 노출하기 전까지 실제 path-insert call site의
/// 보수적 기본값으로 쓴다.
pub fn default_shell_kind() -> ShellKind {
    #[cfg(windows)]
    {
        ShellKind::PowerShell
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_default();
        let name = Path::new(&shell)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if name == "fish" {
            ShellKind::Fish
        } else {
            ShellKind::Posix
        }
    }
}

/// 터미널 삽입용 기본 셸 인용. 기존 call site는 shell kind를 모르므로 POSIX 동작을
/// 유지한다. 세션 shell metadata가 생기면 `shell_quote_for`로 분기한다.
#[allow(dead_code)]
pub fn shell_quote(path: &Path) -> String {
    shell_quote_for(path, ShellKind::Posix)
}

/// 터미널 경로 삽입용 byte payload: quoted path + trailing space, no Enter.
#[allow(dead_code)]
pub fn shell_path_insert_bytes(path: &Path) -> Vec<u8> {
    shell_path_insert_bytes_for(path, ShellKind::Posix)
}

/// shell-specific 터미널 경로 삽입용 byte payload: quoted path + trailing space, no Enter.
pub fn shell_path_insert_bytes_for(path: &Path, shell: ShellKind) -> Vec<u8> {
    let mut bytes = shell_quote_for(path, shell).into_bytes();
    bytes.push(b' ');
    bytes
}

/// shell-specific 터미널 삽입용 최소 셸 인용.
pub fn shell_quote_for(path: &Path, shell: ShellKind) -> String {
    let s = path.display().to_string();
    match shell {
        ShellKind::Posix => quote_posix(&s),
        ShellKind::Fish => quote_fish(&s),
        ShellKind::PowerShell => quote_powershell(&s),
        ShellKind::Cmd => quote_cmd(&s),
    }
}

fn quote_posix(s: &str) -> String {
    if shell_safe(s, "/._-~") {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn quote_fish(s: &str) -> String {
    if shell_safe(s, "/._-~") {
        s.to_owned()
    } else {
        let escaped = s.replace('\\', r"\\").replace('\'', r"\'");
        format!("'{escaped}'")
    }
}

fn quote_powershell(s: &str) -> String {
    if shell_safe(s, r"/._-~:\") {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

fn quote_cmd(s: &str) -> String {
    if shell_safe(s, r"/._-~:\") {
        s.to_owned()
    } else {
        format!("\"{}\"", s.replace('"', r#"\""#))
    }
}

fn shell_safe(s: &str, extra_safe: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || extra_safe.contains(c))
}

/// 이동 계획 (§9-4 가드 통과 결과).
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
enum MovePlan {
    /// 실제 이동: canonicalize된 src(링크면 링크 자체)와 최종 목적지.
    Move { src: PathBuf, dst: PathBuf },
    /// 같은 부모로의 드롭 — 조용한 no-op.
    Noop,
}

/// 드롭 가드 (§9-4): root·src 부모·dst_dir을 canonicalize한 뒤
/// `dst_dir ⊂ root`(루트 탈출 차단, 심볼릭 링크 경유 포함) && `¬(dst_dir ⊂ src)`(자기
/// 자신/자손 금지)를 검사한다. src 자체는 canonicalize하지 않는다 — symlink는 따라가지
/// 않고 링크 자체를 이동한다(정책 확정).
#[cfg(test)]
fn plan_move(root: &Path, src: &Path, dst_dir: &Path) -> Result<MovePlan, String> {
    let root_c = root
        .canonicalize()
        .map_err(|e| format!("루트 확인 실패: {e}"))?;
    let dst_dir_c = dst_dir
        .canonicalize()
        .map_err(|e| format!("대상 폴더 확인 실패: {e}"))?;
    let name = src
        .file_name()
        .ok_or_else(|| "이동할 수 없는 경로입니다".to_owned())?;
    let src_parent_c = src
        .parent()
        .ok_or_else(|| "이동할 수 없는 경로입니다".to_owned())?
        .canonicalize()
        .map_err(|e| format!("원본 위치 확인 실패: {e}"))?;
    let src_c = src_parent_c.join(name);

    if !dst_dir_c.starts_with(&root_c) {
        return Err("워크스페이스 루트 밖으로는 이동할 수 없습니다".to_owned());
    }
    if dst_dir_c.starts_with(&src_c) {
        return Err("자기 자신/하위 폴더로는 이동할 수 없습니다".to_owned());
    }
    if dst_dir_c == src_parent_c {
        return Ok(MovePlan::Noop);
    }
    let dst = dst_dir_c.join(name);
    Ok(MovePlan::Move { src: src_c, dst })
}

/// 덮어쓰기 금지 rename (§9-5). macOS(주 타깃)는 `renamex_np(RENAME_EXCL)`로 원자적 —
/// TOCTOU 없음. 미지원 파일시스템(ENOTSUP)·그 외 OS는 사전검사+rename 폴백
/// (전제: 단일 사용자 로컬 조작 — 외부 동시 변경과의 경합은 비전제).
#[cfg(test)]
fn rename_no_replace(src: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let to_cstr = |p: &Path| {
            std::ffi::CString::new(p.as_os_str().as_bytes())
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
        };
        let (s, d) = (to_cstr(src)?, to_cstr(dst)?);
        let ret = unsafe { libc::renamex_np(s.as_ptr(), d.as_ptr(), libc::RENAME_EXCL) };
        if ret == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ENOTSUP) {
            return Err(err);
        }
        // RENAME_EXCL 미지원 볼륨(SMB 등) — 사전검사 폴백으로 계속
        rename_precheck(src, dst)
    }
    #[cfg(not(target_os = "macos"))]
    {
        rename_precheck(src, dst)
    }
}

/// 사전검사+rename 폴백 (§9-5 명시 전제: 단일 사용자 로컬 조작).
#[cfg(test)]
fn rename_precheck(src: &Path, dst: &Path) -> std::io::Result<()> {
    // symlink 자체도 "존재"로 취급 — try_exists는 링크를 따라가므로 symlink_metadata로 검사
    if std::fs::symlink_metadata(dst).is_ok() {
        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    }
    std::fs::rename(src, dst)
}

/// 크로스 볼륨 이동 (§9-4 확정 순서): `dst_dir/.tmp-<uuid>`에 전체 copy → 최종 이름으로
/// rename → 성공 후에만 원본 delete. 부분 실패 시 tmp 정리, 원본 보존.
#[cfg(test)]
fn move_cross_volume(src: &Path, dst_dir: &Path, dst: &Path) -> Result<(), String> {
    let tmp = dst_dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    if let Err(e) = copy_recursive(src, &tmp) {
        let _ = remove_all(&tmp);
        return Err(format!("복사 실패(원본 보존됨): {e}"));
    }
    if let Err(e) = rename_no_replace(&tmp, dst) {
        let _ = remove_all(&tmp);
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(format!(
                "같은 이름이 이미 있습니다 — 덮어쓰지 않습니다: {}",
                dst.display()
            ));
        }
        return Err(format!("이동 마무리 실패(원본 보존됨): {e}"));
    }
    remove_all(src).map_err(|e| format!("원본 삭제 실패(복사본은 생성됨): {e}"))
}

/// 재귀 복사. symlink는 따라가지 않고 링크 자체를 재현한다(§9-4 정책과 일관).
#[cfg(test)]
fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    let file_type = std::fs::symlink_metadata(src)?.file_type();
    if file_type.is_symlink() {
        let target = std::fs::read_link(src)?;
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, dst);
        #[cfg(not(unix))]
        {
            let _ = target;
            return Err(std::io::Error::other("symlink 복사 미지원 플랫폼"));
        }
    }
    if file_type.is_dir() {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        return Ok(());
    }
    std::fs::copy(src, dst).map(|_| ())
}

/// 한 원본을 dst_dir/<이름>으로 복사한다 — move_cross_volume과 같은 관례(tmp 스테이징
/// → rename_no_replace, 덮어쓰기 금지 §9-5)에서 원본 삭제만 없다. 자기 자신/자손으로의
/// 복사는 무한 재귀라 사전 차단한다(호출부가 dst_dir을 canonicalize해 비교 기준 일치).
#[cfg(test)]
fn copy_into_dir(src: &Path, dst_dir: &Path) -> Result<(), String> {
    let name = src
        .file_name()
        .ok_or_else(|| format!("복사할 수 없는 경로입니다: {}", src.display()))?;
    if let Ok(src_c) = src.canonicalize()
        && dst_dir.starts_with(&src_c)
    {
        return Err(format!(
            "자기 자신/하위 폴더로는 복사할 수 없습니다: {}",
            src.display()
        ));
    }
    let dst = dst_dir.join(name);
    let tmp = dst_dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    if let Err(e) = copy_recursive(src, &tmp) {
        let _ = remove_all(&tmp);
        return Err(format!("복사 실패: {e}"));
    }
    match rename_no_replace(&tmp, &dst) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = remove_all(&tmp);
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Err(format!(
                    "같은 이름이 이미 있습니다 — 덮어쓰지 않습니다: {}",
                    dst.display()
                ))
            } else {
                Err(format!("복사 마무리 실패: {e}"))
            }
        }
    }
}

/// 행 기준 반입 대상 폴더 — 폴더 행이면 자신, 파일 행이면 부모(§과제 판정 규칙).
fn row_target_dir(row: &FlatRow, root: Option<&Path>) -> PathBuf {
    if row.is_dir {
        row.path.clone()
    } else {
        row.path
            .parent()
            .map(Path::to_path_buf)
            .or_else(|| root.map(Path::to_path_buf))
            .unwrap_or_else(|| row.path.clone())
    }
}

/// 같은 ⌘V 제스처(press+release) 이중 처리 방지 창 — 터미널 PASTE_GESTURE_WINDOW 관례.
const EXTERNAL_PASTE_GESTURE_WINDOW: std::time::Duration = std::time::Duration::from_millis(600);

/// 트리 ⌘V 신호(egui 이벤트 기반). macOS는 press가 Event::Paste(클립보드에 텍스트
/// 표현이 있을 때만)로 오고 파일-only pasteboard면 press 이벤트가 없다 — release
/// (V key-up)가 fallback이다(터미널 is_clipboard_paste_shortcut 관례). native
/// key-down은 peek_clipboard_paste로 별도 감지한다.
fn is_tree_paste_signal(event: &egui::Event) -> bool {
    match event {
        egui::Event::Paste(_) => true,
        egui::Event::Key {
            key: egui::Key::V,
            pressed,
            modifiers,
            ..
        } => cfg!(target_os = "macos") && !*pressed && modifiers.command && !modifiers.ctrl,
        _ => false,
    }
}

/// OS 파일 드래그/드롭 중 포인터 위치(egui 창 좌표). winit 0.30은 macOS
/// `draggingUpdated:`를 구현하지 않아 드래그 중 CursorMoved가 오지 않는다 — AppKit
/// 전역 마우스 위치(bottom-left 스크린 좌표)를 primary 스크린 기준으로 뒤집고
/// viewport inner_rect(모니터 공간 egui points)를 빼서 환산한다. viewport 미상
/// (kittest 등)이면 None → 호출부가 egui 포인터로 폴백한다.
#[cfg(target_os = "macos")]
fn os_drag_pointer_pos(ctx: &egui::Context) -> Option<egui::Pos2> {
    let inner = ctx.input(|i| i.viewport().inner_rect)?;
    let mtm = objc2::MainThreadMarker::new()?;
    let location = objc2_app_kit::NSEvent::mouseLocation();
    let primary = objc2_app_kit::NSScreen::screens(mtm).firstObject()?;
    let primary_height = primary.frame().size.height;
    Some(screen_to_window_pos(
        (location.x as f32, location.y as f32),
        primary_height as f32,
        ctx.zoom_factor(),
        inner.min,
    ))
}

#[cfg(not(target_os = "macos"))]
fn os_drag_pointer_pos(_ctx: &egui::Context) -> Option<egui::Pos2> {
    None
}

/// AppKit 스크린 좌표(bottom-left, points) → egui 창 좌표(points). zoom_factor로
/// egui points 스케일(native ppp × zoom)을 맞춘 뒤 창 내용 원점(inner_min)을 뺀다.
fn screen_to_window_pos(
    mouse: (f32, f32),
    primary_height: f32,
    zoom_factor: f32,
    inner_min: egui::Pos2,
) -> egui::Pos2 {
    egui::pos2(
        mouse.0 / zoom_factor - inner_min.x,
        (primary_height - mouse.1) / zoom_factor - inner_min.y,
    )
}

/// 파일/링크/디렉터리를 삭제한다 (링크는 링크 자체만).
#[cfg(test)]
fn remove_all(path: &Path) -> std::io::Result<()> {
    let file_type = std::fs::symlink_metadata(path)?.file_type();
    if file_type.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

fn file_tree_io_error_message(code: FileTreeIoErrorCode) -> &'static str {
    match code {
        FileTreeIoErrorCode::Busy => "파일 작업이 진행 중입니다 — 완료 후 다시 시도해 주세요",
        FileTreeIoErrorCode::InvalidPath => "유효하지 않은 파일 경로입니다",
        FileTreeIoErrorCode::PathTooLarge | FileTreeIoErrorCode::PathListTooLarge => {
            "파일 경로 입력이 허용된 크기를 초과했습니다"
        }
        FileTreeIoErrorCode::InvalidName => "사용할 수 없는 파일 이름입니다",
        FileTreeIoErrorCode::Conflict => "같은 이름이 이미 있습니다 — 덮어쓰지 않습니다",
        FileTreeIoErrorCode::OutsideRoot => "워크스페이스 루트 밖으로 이동할 수 없습니다",
        FileTreeIoErrorCode::TrashUnavailable => "휴지통으로 이동하지 못했습니다",
        FileTreeIoErrorCode::NativeFailure => "파일 작업을 완료하지 못했습니다",
    }
}

fn file_tree_maintenance_error_message(code: FileTreeMaintenanceErrorCode) -> &'static str {
    match code {
        FileTreeMaintenanceErrorCode::PermissionDenied => "폴더 접근 권한이 없습니다",
        FileTreeMaintenanceErrorCode::ListingTooLarge => {
            "폴더 항목이 파일 트리 자원 상한을 초과했습니다"
        }
        FileTreeMaintenanceErrorCode::WatchPlanTooLarge => {
            "파일 감시 범위가 자원 상한을 초과했습니다"
        }
        FileTreeMaintenanceErrorCode::InvalidSnapshot => "파일 트리 결과가 유효하지 않습니다",
        FileTreeMaintenanceErrorCode::WatchUnavailable => "파일 감시를 시작하지 못했습니다",
        FileTreeMaintenanceErrorCode::NativeFailure => "파일 트리를 갱신하지 못했습니다",
    }
}

#[cfg(test)]
fn read_children(path: &Path, _root: Option<&Path>) -> std::io::Result<Vec<TreeNode>> {
    let mut nodes = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        nodes.push(TreeNode::new(
            entry.file_name().to_string_lossy().into_owned(),
            is_dir,
        ));
    }
    sort_nodes(&mut nodes);
    Ok(nodes)
}

fn prepare_listing_nodes(
    parent: &Path,
    mut nodes: Vec<TreeNode>,
    expanded_paths: &HashSet<PathBuf>,
) -> Vec<TreeNode> {
    for node in &mut nodes {
        if !node.is_dir {
            continue;
        }
        if expanded_paths.contains(&parent.join(&node.name)) {
            node.expanded = true;
            node.children = Some(Vec::new());
        }
    }
    nodes
}

fn tree_node_usage(nodes: &[TreeNode]) -> (usize, usize) {
    nodes.iter().fold((0usize, 0usize), |usage, node| {
        let child_usage = node
            .children
            .as_deref()
            .map(tree_node_usage)
            .unwrap_or((0, 0));
        (
            usage.0.saturating_add(1).saturating_add(child_usage.0),
            usage
                .1
                .saturating_add(node.name.len())
                .saturating_add(child_usage.1),
        )
    })
}

/// 정렬: 디렉터리 우선 + 이름 (단순 유니코드 순 — §3, 로케일 비교는 비목표).
#[cfg(test)]
fn sort_nodes(nodes: &mut [TreeNode]) {
    nodes.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
}

/// 펼친 트리를 가시 행 목록으로 평탄화한다 (숨김 필터 포함). 순수 함수 — 단위 테스트 대상.
fn flatten(
    nodes: &[TreeNode],
    base: &Path,
    depth: usize,
    show_hidden: bool,
    out: &mut Vec<FlatRow>,
) {
    for node in nodes {
        if !show_hidden && node.name.starts_with('.') {
            continue;
        }
        let path = base.join(&node.name);
        out.push(FlatRow {
            path: path.clone(),
            name: node.name.clone(),
            depth,
            is_dir: node.is_dir,
            expanded: node.expanded,
        });
        if node.expanded
            && let Some(children) = &node.children
        {
            flatten(children, &path, depth + 1, show_hidden, out);
        }
    }
}

/// 루트 기준 상대 경로로 노드를 찾는다 (조작 대상 탐색).
fn node_mut<'a>(mut nodes: &'a mut Vec<TreeNode>, rel: &Path) -> Option<&'a mut TreeNode> {
    let mut comps = rel.components().peekable();
    while let Some(comp) = comps.next() {
        let name = comp.as_os_str().to_string_lossy();
        let idx = nodes.iter().position(|n| n.name == name)?;
        if comps.peek().is_none() {
            return Some(&mut nodes[idx]);
        }
        nodes = nodes[idx].children.as_mut()?;
    }
    None
}

fn node_ref<'a>(mut nodes: &'a [TreeNode], rel: &Path) -> Option<&'a TreeNode> {
    let mut comps = rel.components().peekable();
    while let Some(comp) = comps.next() {
        let name = comp.as_os_str().to_string_lossy();
        let node = nodes.iter().find(|n| n.name == name)?;
        if comps.peek().is_none() {
            return Some(node);
        }
        nodes = node.children.as_deref()?;
    }
    None
}

fn collect_expanded_paths(nodes: &[TreeNode], base: &Path, out: &mut HashSet<PathBuf>) {
    for node in nodes {
        if !node.is_dir || !node.expanded {
            continue;
        }
        let path = base.join(&node.name);
        out.insert(path.clone());
        if let Some(children) = &node.children {
            collect_expanded_paths(children, &path, out);
        }
    }
}

/// 디렉터리를 다시 나열하되, 이전 트리의 펼침 상태를 이월한다 (펼친 하위만 재귀 —
/// 접근 불가/사라진 하위는 접는다). 새로고침·부분 재나열의 공통 코어.
#[cfg(test)]
fn reread(base: &Path, old: &[TreeNode]) -> std::io::Result<Vec<TreeNode>> {
    let mut fresh = read_children(base, None)?;
    for node in fresh.iter_mut() {
        if !node.is_dir {
            continue;
        }
        let was = old.iter().find(|o| o.name == node.name && o.is_dir);
        if let Some(was) = was
            && was.expanded
        {
            let old_children = was.children.as_deref().unwrap_or(&[]);
            match reread(&base.join(&node.name), old_children) {
                Ok(children) => {
                    node.expanded = true;
                    node.children = Some(children);
                }
                Err(_) => {
                    // 사라졌거나 접근 불가 — 접힌 상태로 계속 (치명 아님)
                    node.expanded = false;
                    node.children = None;
                }
            }
        }
    }
    Ok(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_constructor는_thread_channel_watcher와_intent가_없다() {
        let tree = FileTreeUi::new(egui::Context::default());
        assert!(tree.maintenance_intent.is_none());
        assert!(tree.pending_maintenance.is_none());
        assert!(tree.pending_refresh_dirs.is_empty());
        assert!(tree.root.is_none());
    }

    #[test]
    fn maintenance_listing_intent는_capacity_one_상한과_redacted_debug를_강제한다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(PathBuf::from("/private/secret-workspace")));
        let intent = tree.take_maintenance_intent().expect("lazy listing intent");
        let debug = format!("{intent:?}");
        assert!(!debug.contains("secret-workspace"));
        match intent.request {
            FileTreeMaintenanceRequest::ListDirectory {
                root,
                directory,
                max_items,
                max_bytes,
            } => {
                assert_eq!(root.as_path(), Path::new("/private/secret-workspace"));
                assert_eq!(directory.as_path(), root.as_path());
                assert_eq!(max_items, FILE_TREE_LISTING_MAX_ITEMS);
                assert_eq!(max_bytes, FILE_TREE_LISTING_MAX_BYTES);
            }
            other => panic!("unexpected intent: {other:?}"),
        }
        assert!(tree.take_maintenance_intent().is_none());
        assert!(tree.pending_maintenance.is_some());
    }

    #[test]
    fn file_tree_impl과_render는_native_io_poll_timer를_만지지_않는다() {
        let source = include_str!("file_tree.rs");
        assert!(
            !source.contains(concat!("cfg(", "any", "())")),
            "비활성 legacy 구현을 production source에 보관하지 않는다"
        );
        let implementation = source
            .split_once("impl FileTreeUi {")
            .expect("FileTreeUi impl start")
            .1
            .split_once("fn permission_denied_button_rect(")
            .expect("FileTreeUi impl end")
            .0;
        for forbidden in [
            "std::fs::",
            "notify::",
            "std::thread::",
            "std::sync::mpsc",
            "mpsc::",
            "channel(",
            "sync_channel(",
            ".try_recv(",
            ".try_iter(",
            ".recv(",
            ".poll(",
            "poll_",
            "read_dir(",
            ".metadata(",
            ".canonicalize(",
            "recommended_watcher",
            ".watch(",
            ".unwatch(",
            "request_repaint_after(",
        ] {
            assert!(
                !implementation.contains(forbidden),
                "FileTreeUi impl source contains {forbidden}"
            );
        }
    }

    #[test]
    fn native_intent는_capacity_one이고_stale_completion을_버린다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        let request = FileTreeIoRequest::OpenPath {
            target: FileTreePathPayload::try_new(PathBuf::from("/private/example.pdf")).unwrap(),
            require_openable_file: true,
        };
        tree.queue_io(request, Vec::new(), None, None).unwrap();
        assert_eq!(tree.in_flight, 1);
        let intent = tree.take_io_intent().unwrap();
        assert!(!format!("{intent:?}").contains("/private/example.pdf"));

        tree.complete_io(FileTreeIoCompletion {
            operation: intent.operation,
            generation: intent.generation.wrapping_add(1),
            result: Ok(()),
        });
        assert!(
            tree.pending_io.is_some(),
            "stale result removed current pending"
        );
        tree.complete_io(FileTreeIoCompletion {
            operation: intent.operation,
            generation: intent.generation,
            result: Ok(()),
        });
        assert!(tree.pending_io.is_none());
        assert_eq!(tree.in_flight, 0);
    }

    #[test]
    fn native_path_payload는_item_byte상한과_redacted_debug를_강제한다() {
        let payload = FileTreePathListPayload::try_new(vec![PathBuf::from("/secret/a.txt")])
            .expect("bounded path");
        let debug = format!("{payload:?}");
        assert!(debug.contains("items: 1"));
        assert!(!debug.contains("/secret/a.txt"));
        assert!(matches!(
            FileTreePathListPayload::try_new(vec![
                PathBuf::from("a");
                FILE_TREE_PATH_LIST_MAX_ITEMS + 1
            ]),
            Err(FileTreeIoErrorCode::PathListTooLarge)
        ));
    }

    /// App 시작 경로는 fonts::install_cjk_fallback에서 named family를 등록한다. 파일 트리
    /// 단위 harness는 그 초기화를 거치지 않으므로 기본 Proportional face를 같은 이름에
    /// 연결해 레이아웃만 가볍게 검증한다(55MB 시스템 TTC를 test context마다 파싱하지 않음).
    fn install_sidebar_test_fonts(ctx: &egui::Context) {
        let mut fonts = egui::FontDefinitions::default();
        let fallback = fonts
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();
        fonts.families.insert(
            egui::FontFamily::Name(crate::fonts::SIDEBAR_FONT_FAMILY.into()),
            fallback,
        );
        ctx.set_fonts(fonts);
    }

    fn catalog() -> i18n::Catalog {
        i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap()
    }

    #[test]
    fn 워크스페이스_아바타만_대문자이고_이름표기는_보존한다() {
        assert_eq!(workspace_initial("arteawiki"), 'A');
        assert_eq!(workspace_initial("VisionAI"), 'V');
        assert_eq!(workspace_initial(""), 'W');
        assert_eq!(workspace_label("arteawiki"), "arteawiki");
        assert_eq!(workspace_label("VisionAI"), "VisionAI");
    }

    #[test]
    fn 같은_이니셜의_워크스페이스도_서로_다른_색상_계열을_쓴다() {
        let workspaces = (0..6)
            .map(|index| SidebarWorkspaceEntry {
                id: format!("stable-id-{index}"),
                name: format!("same-{index}"),
                repo: None,
                state: SidebarWorkspaceState::Idle,
                summary: SidebarSessionSummary::default(),
            })
            .collect::<Vec<_>>();
        let colors = workspaces
            .iter()
            .map(|workspace| workspace_accent(&workspaces, &workspace.id).to_array())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(colors.len(), workspaces.len());

        let original = workspace_accent(&workspaces, &workspaces[0].id);
        let mut selected_elsewhere = workspaces.clone();
        selected_elsewhere[4].state = SidebarWorkspaceState::Active;
        assert_eq!(
            workspace_accent(&selected_elsewhere, &workspaces[0].id),
            original,
            "선택 상태는 아바타 색상 배정에 영향을 주지 않는다"
        );
    }

    #[test]
    fn 워크스페이스_chevron은_얇은_접힘과_펼침_방향을_가진다() {
        let center = egui::pos2(10.0, 20.0);
        let collapsed = disclosure_chevron_points(center, false);
        assert!(collapsed[1].x > collapsed[0].x);
        assert!(collapsed[1].x > collapsed[2].x);
        assert!(collapsed[0].y < collapsed[1].y);
        assert!(collapsed[2].y > collapsed[1].y);

        let expanded = disclosure_chevron_points(center, true);
        assert!(expanded[1].y > expanded[0].y);
        assert!(expanded[1].y > expanded[2].y);
        assert!(expanded[0].x < expanded[1].x);
        assert!(expanded[2].x > expanded[1].x);
    }

    #[test]
    fn 워크스페이스_선택이_바뀌어도_생성순서가_고정된다() {
        let workspaces = ["first", "second", "third"].map(|id| SidebarWorkspaceEntry {
            id: id.to_owned(),
            name: id.to_owned(),
            repo: None,
            state: SidebarWorkspaceState::Idle,
            summary: SidebarSessionSummary::default(),
        });
        for selected in ["first", "second", "third"] {
            let (before, active, after) = workspace_creation_order_partition(&workspaces, selected);
            let rendered = before
                .iter()
                .chain(active)
                .chain(after)
                .map(|workspace| workspace.id.as_str())
                .collect::<Vec<_>>();
            assert_eq!(rendered, ["first", "second", "third"]);
        }
    }

    #[test]
    fn 접힌_워크스페이스_요약은_세션상태를_한번씩_집계한다() {
        use runtime::SessionStatus as S;
        let mut summary = SidebarSessionSummary::default();
        summary.add(Some(S::Running), false);
        summary.add(Some(S::NeedsApproval), false);
        summary.add(Some(S::Done), false);
        summary.add(Some(S::Error), false);
        summary.add(Some(S::Idle), false);
        summary.add(None, true);
        assert_eq!(summary.running, 1);
        assert_eq!(summary.waiting, 2);
        assert_eq!(summary.done, 1);
        assert_eq!(summary.error, 1);
        assert_eq!(summary.idle, 1);

        let catalog = catalog();
        let segments = workspace_summary_segments(summary, egui::Color32::GRAY, &catalog);
        use crate::agent_surface::AgentVisualState as VisualState;
        let status_segments = segments
            .iter()
            .filter(|(text, _)| text != " · ")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            status_segments,
            vec![
                (
                    "1 running".to_owned(),
                    crate::ui::agent_visuals::status_color(VisualState::Active),
                ),
                (
                    "2 waiting for input".to_owned(),
                    crate::ui::agent_visuals::status_color(VisualState::Waiting),
                ),
                (
                    "1 completed".to_owned(),
                    crate::ui::agent_visuals::status_color(VisualState::Complete),
                ),
                (
                    "1 errors".to_owned(),
                    crate::ui::agent_visuals::status_color(VisualState::Error),
                ),
                (
                    "1 idle".to_owned(),
                    crate::ui::agent_visuals::status_color(VisualState::Idle),
                ),
            ]
        );
        let text = segments
            .into_iter()
            .map(|(text, _)| text)
            .collect::<String>();
        assert_eq!(
            text,
            "1 running · 2 waiting for input · 1 completed · 1 errors · 1 idle"
        );
    }

    #[test]
    fn 상태없는_활성은_유휴_복원세션은_비활성_빈비활성은_세션없음으로_표시한다() {
        use crate::agent_surface::AgentVisualState as VisualState;
        let weak = egui::Color32::GRAY;
        let catalog = catalog();
        let idle = workspace_summary_segments(SidebarSessionSummary::default(), weak, &catalog);
        assert_eq!(idle[0].0, "Idle");
        assert_eq!(
            idle[0].1,
            crate::ui::agent_visuals::status_color(VisualState::Idle)
        );
        let inactive =
            workspace_summary_segments(SidebarSessionSummary::inactive(3), weak, &catalog);
        assert_eq!(inactive[0].0, "Inactive");
        assert_eq!(
            inactive[0].1,
            crate::ui::agent_visuals::status_color(VisualState::Off)
        );
        let no_sessions =
            workspace_summary_segments(SidebarSessionSummary::inactive(0), weak, &catalog);
        assert_eq!(no_sessions[0].0, "No sessions");
        assert_eq!(
            no_sessions[0].1,
            crate::ui::agent_visuals::status_color(VisualState::Off)
        );

        let primary = workspace_primary_summary_segment(SidebarSessionSummary::default(), &catalog);
        assert_eq!(primary, idle[0]);
        assert_eq!(
            workspace_primary_summary_segment(SidebarSessionSummary::inactive(0), &catalog),
            no_sessions[0]
        );
    }

    #[test]
    fn 세션_하이라이트와_인셋은_같은_경계를_쓰고_레일텍스트간격은_절반이다() {
        let full = egui::Rect::from_min_max(egui::pos2(20.0, 10.0), egui::pos2(500.0, 62.0));
        let highlight = session_highlight_rect(full);
        // 좌측은 호출부의 20px 들여쓰기를 걷어내 워크스페이스 헤더와 같은 8px
        // 인셋으로 맞춘다(SESSION_HIGHLIGHT_LEFT_EXTEND 주석 참고).
        assert_eq!(highlight.left(), full.left() - SESSION_HIGHLIGHT_LEFT_EXTEND);
        assert_eq!(highlight.right(), full.right() - SESSION_HIGHLIGHT_RIGHT_INSET);
        let last = egui::Rect::from_min_max(egui::pos2(20.0, 62.0), egui::pos2(500.0, 100.0));
        let inset = workspace_session_inset_rect(
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(506.0, 100.0)),
            8.0,
            full.union(last),
        );
        assert_eq!(inset.left(), session_inset_fill_rect(full).left());
        assert_eq!(inset.top(), session_inset_fill_rect(full).top());
        assert_eq!(inset.right(), session_inset_fill_rect(last).right());
        assert_eq!(inset.bottom(), session_inset_fill_rect(last).bottom());
        let focus = session_focus_fill_rect(last);
        assert_eq!(focus.top(), last.top(), "포커스 배경은 이전 행을 침범하지 않음");
        assert_eq!(focus.bottom(), last.bottom() - 1.0);
        assert_eq!(
            session_text_inset(SESSION_RAIL_MAX_WIDTH)
                - SESSION_RAIL_LEFT_INSET
                - SESSION_RAIL_MAX_WIDTH,
            0.0,
            "레일의 실제 폭 바로 뒤에서 텍스트 시작"
        );
    }

    #[test]
    fn 워크스페이스_상태는_이름폭부터_문구로_항상_표시한다() {
        assert_eq!(
            workspace_summary_mode(103.9),
            WorkspaceSummaryMode::IconOnly
        );
        assert_eq!(workspace_summary_mode(104.0), WorkspaceSummaryMode::Compact);
        assert_eq!(workspace_summary_mode(269.9), WorkspaceSummaryMode::Compact);
        assert_eq!(workspace_summary_mode(270.0), WorkspaceSummaryMode::Full);

        let compact = workspace_summary_segments_for_mode(
            SidebarSessionSummary::default(),
            egui::Color32::GRAY,
            WorkspaceSummaryMode::Compact,
            &catalog(),
        );
        assert_eq!(compact[0].0, "Idle");
    }

    #[test]
    fn stale_epoch_청크는_송신전에_중단된다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(PathBuf::from("/tmp/old")));
        let stale = tree.take_maintenance_intent().expect("old intent");
        tree.set_root(Some(PathBuf::from("/tmp/current")));
        tree.complete_maintenance(FileTreeMaintenanceCompletion {
            operation: stale.operation,
            generation: stale.generation,
            result: Ok(FileTreeMaintenanceResult::Listing(
                FileTreeListingSnapshot::try_new(vec![
                    FileTreeListingItem::try_new("stale.txt".to_owned(), false).unwrap(),
                ])
                .unwrap(),
            )),
        });
        assert!(tree.children.is_none(), "stale snapshot applied");
    }

    /// codex High(2026-07-08): 같은 경로 재요청(reload_dir token 교체) 시 구 요청의
    /// cancel 플래그가 서면 같은 epoch이어도 송신 전에 중단된다.
    #[test]
    fn cancel_플래그는_같은_epoch에서도_송신을_중단한다() {
        let items = (0..=FILE_TREE_LISTING_MAX_ITEMS)
            .map(|i| FileTreeListingItem::try_new(format!("f{i}"), false).unwrap())
            .collect();
        assert!(matches!(
            FileTreeListingSnapshot::try_new(items),
            Err(FileTreeMaintenanceErrorCode::ListingTooLarge)
        ));
    }

    fn dir(name: &str) -> TreeNode {
        TreeNode::new(name.into(), true)
    }

    fn file(name: &str) -> TreeNode {
        TreeNode::new(name.into(), false)
    }

    fn names(nodes: &[TreeNode]) -> Vec<&str> {
        nodes.iter().map(|n| n.name.as_str()).collect()
    }

    #[test]
    fn 정렬은_디렉터리_우선_이름순() {
        let mut nodes = vec![
            file("b.txt"),
            dir("zz"),
            file("a.txt"),
            dir("aa"),
            dir("mm"),
        ];
        sort_nodes(&mut nodes);
        assert_eq!(names(&nodes), vec!["aa", "mm", "zz", "a.txt", "b.txt"]);
    }

    #[test]
    fn 평탄화는_펼친_노드만_내려간다() {
        // src(펼침, [main.rs]) / docs(접힘, 캐시 없음) / a.txt
        let mut src = dir("src");
        src.expanded = true;
        src.children = Some(vec![file("main.rs")]);
        let nodes = vec![src, dir("docs"), file("a.txt")];

        let mut out = Vec::new();
        flatten(&nodes, Path::new("/root"), 0, false, &mut out);

        let got: Vec<(String, usize, bool)> = out
            .iter()
            .map(|r| (r.name.clone(), r.depth, r.is_dir))
            .collect();
        assert_eq!(
            got,
            vec![
                ("src".into(), 0, true),
                ("main.rs".into(), 1, false),
                ("docs".into(), 0, true),
                ("a.txt".into(), 0, false),
            ]
        );
        // 경로는 base + 이름 누적
        assert_eq!(out[1].path, Path::new("/root/src/main.rs"));
    }

    #[test]
    fn 평탄화_숨김_필터와_토글() {
        let mut secret_dir = dir(".git");
        secret_dir.expanded = true;
        secret_dir.children = Some(vec![file("config")]);
        let nodes = vec![secret_dir, file(".env"), file("visible.txt")];

        let mut hidden_off = Vec::new();
        flatten(&nodes, Path::new("/r"), 0, false, &mut hidden_off);
        assert_eq!(hidden_off.len(), 1);
        assert_eq!(hidden_off[0].name, "visible.txt");

        let mut hidden_on = Vec::new();
        flatten(&nodes, Path::new("/r"), 0, true, &mut hidden_on);
        // .git + .git/config + .env + visible.txt
        assert_eq!(hidden_on.len(), 4);
    }

    #[test]
    fn node_mut은_중첩_경로를_찾는다() {
        let mut inner = dir("inner");
        inner.children = Some(vec![file("deep.txt")]);
        let mut outer = dir("outer");
        outer.children = Some(vec![inner]);
        let mut nodes = vec![outer, file("top.txt")];

        assert!(node_mut(&mut nodes, Path::new("outer/inner/deep.txt")).is_some());
        assert_eq!(
            node_mut(&mut nodes, Path::new("outer/inner")).unwrap().name,
            "inner"
        );
        assert!(node_mut(&mut nodes, Path::new("outer/none")).is_none());
        // 미로딩(children=None) 하위로는 내려가지 않는다
        assert!(node_mut(&mut nodes, Path::new("top.txt/x")).is_none());
    }

    /// 테스트용 tempdir (canonicalize — macOS /tmp→/private/tmp 대칭성 확보).
    fn temp_root(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("deppy-ft-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base.canonicalize().unwrap()
    }

    fn drain_listings(tree: &mut FileTreeUi) {
        for _ in 0..=FILE_TREE_REFRESH_BACKLOG_CAP + 2 {
            let Some(intent) = tree.take_maintenance_intent() else {
                assert!(tree.pending_maintenance.is_none());
                return;
            };
            let result = match intent.request {
                FileTreeMaintenanceRequest::ListDirectory { directory, .. } => {
                    let items = read_children(directory.as_path(), None)
                        .unwrap()
                        .into_iter()
                        .map(|node| FileTreeListingItem::try_new(node.name, node.is_dir).unwrap())
                        .collect();
                    Ok(FileTreeMaintenanceResult::Listing(
                        FileTreeListingSnapshot::try_new(items).unwrap(),
                    ))
                }
                FileTreeMaintenanceRequest::ReplaceWatchSet(_) => {
                    Ok(FileTreeMaintenanceResult::WatchSetApplied)
                }
            };
            tree.complete_maintenance(FileTreeMaintenanceCompletion {
                operation: intent.operation,
                generation: intent.generation,
                result,
            });
        }
        panic!("bounded maintenance did not quiesce");
    }

    fn pump_listings_for(tree: &mut FileTreeUi, _duration: std::time::Duration) {
        drain_listings(tree);
    }

    #[test]
    fn plan_move_가드_4종() {
        let base = temp_root("guard");
        let root = base.join("root");
        std::fs::create_dir_all(root.join("a/sub")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/f.txt"), b"x").unwrap();
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        // 정상 이동: a/f.txt → b
        assert_eq!(
            plan_move(&root, &root.join("a/f.txt"), &root.join("b")).unwrap(),
            MovePlan::Move {
                src: root.join("a/f.txt"),
                dst: root.join("b/f.txt"),
            }
        );
        // 같은 부모 → no-op
        assert_eq!(
            plan_move(&root, &root.join("a/f.txt"), &root.join("a")).unwrap(),
            MovePlan::Noop
        );
        // 자기 자손으로 금지 (a → a/sub)
        assert!(plan_move(&root, &root.join("a"), &root.join("a/sub")).is_err());
        // 자기 자신으로 금지 (a → a)
        assert!(plan_move(&root, &root.join("a"), &root.join("a")).is_err());
        // 루트 밖 금지
        assert!(plan_move(&root, &root.join("a/f.txt"), &outside).is_err());

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn plan_move_symlink_루트탈출_차단과_링크_자체_이동() {
        let base = temp_root("symlink");
        let root = base.join("root");
        std::fs::create_dir_all(root.join("b")).unwrap();
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        // 루트 안의 링크가 루트 밖 디렉터리를 가리킨다
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        // 링크를 드롭 대상으로 쓰면 canonicalize가 루트 밖을 드러내 차단된다 (§9-4)
        assert!(plan_move(&root, &root.join("b"), &root.join("escape")).is_err());
        // 링크 자체를 옮기는 것은 허용 — src는 resolve되지 않는다 (링크 자체 이동 정책)
        let plan = plan_move(&root, &root.join("escape"), &root.join("b")).unwrap();
        assert_eq!(
            plan,
            MovePlan::Move {
                src: root.join("escape"),
                dst: root.join("b/escape"),
            }
        );
        // 실제 이동해도 링크가 링크로 남는다
        if let MovePlan::Move { src, dst } = plan {
            rename_no_replace(&src, &dst).unwrap();
            assert!(
                std::fs::symlink_metadata(&dst)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn rename_no_replace는_덮어쓰지_않는다() {
        let base = temp_root("excl");
        std::fs::write(base.join("src.txt"), b"src").unwrap();
        std::fs::write(base.join("dst.txt"), b"dst").unwrap();

        // 충돌: AlreadyExists (macOS는 RENAME_EXCL의 EEXIST → AlreadyExists 매핑)
        let err = rename_no_replace(&base.join("src.txt"), &base.join("dst.txt")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        // 원본·기존 대상 모두 보존
        assert_eq!(std::fs::read(base.join("dst.txt")).unwrap(), b"dst");
        assert_eq!(std::fs::read(base.join("src.txt")).unwrap(), b"src");

        // 충돌 없으면 정상 이동
        rename_no_replace(&base.join("src.txt"), &base.join("moved.txt")).unwrap();
        assert!(!base.join("src.txt").exists());
        assert_eq!(std::fs::read(base.join("moved.txt")).unwrap(), b"src");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn move_cross_volume_전체복사_후_원본삭제_tmp잔재없음() {
        // 같은 볼륨에서도 로직은 동일하게 동작한다 (copy → rename → delete)
        let base = temp_root("exdev");
        let src = base.join("proj");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("nested/deep.txt"), b"deep").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("nested/deep.txt", src.join("link")).unwrap();
        let dst_dir = base.join("target");
        std::fs::create_dir_all(&dst_dir).unwrap();
        let dst = dst_dir.join("proj");

        move_cross_volume(&src, &dst_dir, &dst).unwrap();

        assert!(!src.exists(), "원본은 삭제");
        assert_eq!(std::fs::read(dst.join("nested/deep.txt")).unwrap(), b"deep");
        #[cfg(unix)]
        assert!(
            std::fs::symlink_metadata(dst.join("link"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "symlink는 링크로 복사"
        );
        // tmp 잔재 없음
        let leftovers: Vec<_> = std::fs::read_dir(&dst_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn move_cross_volume_이름충돌은_원본보존_tmp정리() {
        let base = temp_root("exdev-conflict");
        let src = base.join("item");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("f.txt"), b"keep").unwrap();
        let dst_dir = base.join("target");
        std::fs::create_dir_all(dst_dir.join("item")).unwrap(); // 같은 이름 선점
        let dst = dst_dir.join("item");

        let err = move_cross_volume(&src, &dst_dir, &dst).unwrap_err();
        assert!(err.contains("덮어쓰지 않습니다"), "err={err}");
        // 원본 보존
        assert_eq!(std::fs::read(src.join("f.txt")).unwrap(), b"keep");
        // tmp 정리됨
        let leftovers: Vec<_> = std::fs::read_dir(&dst_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn validate_name_거부_케이스() {
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\\b").is_err());
        assert!(validate_name(".").is_err());
        assert!(validate_name("..").is_err());
        assert_eq!(validate_name(" 새 폴더 ").unwrap(), "새 폴더");
        assert_eq!(validate_name(".env").unwrap(), ".env"); // 숨김 이름은 허용
    }

    #[test]
    fn apply_rename_성공과_충돌() {
        let base = temp_root("rename");
        std::fs::write(base.join("old.txt"), b"x").unwrap();
        std::fs::write(base.join("taken.txt"), b"y").unwrap();

        // 충돌: 덮어쓰기 금지, 원본 유지
        let err = apply_rename(&base.join("old.txt"), "taken.txt").unwrap_err();
        assert!(err.contains("이미 있습니다"), "err={err}");
        assert!(base.join("old.txt").exists());
        // 유효하지 않은 이름
        assert!(apply_rename(&base.join("old.txt"), "a/b").is_err());
        // 같은 이름 → no-op 성공
        assert_eq!(
            apply_rename(&base.join("old.txt"), "old.txt").unwrap(),
            base.join("old.txt")
        );
        // 정상 변경
        let new_path = apply_rename(&base.join("old.txt"), "new.txt").unwrap();
        assert_eq!(new_path, base.join("new.txt"));
        assert!(!base.join("old.txt").exists());
        assert_eq!(std::fs::read(new_path).unwrap(), b"x");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn apply_new_folder_성공과_충돌() {
        let base = temp_root("newdir");
        let created = apply_new_folder(&base, " 새 폴더 ").unwrap();
        assert_eq!(created, base.join("새 폴더"));
        assert!(created.is_dir());
        // 같은 이름 재생성은 거부
        let err = apply_new_folder(&base, "새 폴더").unwrap_err();
        assert!(err.contains("이미 있습니다"), "err={err}");
        // 구분자 거부
        assert!(apply_new_folder(&base, "a/b").is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn apply_new_file_성공과_덮어쓰기_거부() {
        let base = temp_root("newfile");
        let created = apply_new_file(&base, " note.md ").unwrap();
        assert_eq!(created, base.join("note.md"));
        assert!(created.is_file());
        std::fs::write(&created, b"keep").unwrap();
        let err = apply_new_file(&base, "note.md").unwrap_err();
        assert!(err.contains("이미 있습니다"), "err={err}");
        assert_eq!(std::fs::read(&created).unwrap(), b"keep");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn shell_quote_안전문자는_그대로_특수문자는_인용() {
        assert_eq!(shell_quote(Path::new("/a/b_c-d.txt")), "/a/b_c-d.txt");
        assert_eq!(shell_quote(Path::new("/a/b c")), "'/a/b c'");
        assert_eq!(shell_quote(Path::new("/한글/경로")), "'/한글/경로'");
        // 작은따옴표 이스케이프
        assert_eq!(shell_quote(Path::new("/a/it's")), r"'/a/it'\''s'");
    }

    #[test]
    fn shell_quote_for_shell별_table() {
        let cases = [
            (
                "posix/bash/zsh spaces",
                ShellKind::Posix,
                "/tmp/My File.txt",
                "'/tmp/My File.txt'",
            ),
            (
                "posix/bash/zsh single quote",
                ShellKind::Posix,
                "/tmp/Bob's File.txt",
                r"'/tmp/Bob'\''s File.txt'",
            ),
            (
                "posix/bash/zsh backslash drive colon",
                ShellKind::Posix,
                r"C:\Users\me\file.txt",
                r"'C:\Users\me\file.txt'",
            ),
            (
                "posix/bash/zsh Japanese",
                ShellKind::Posix,
                "/tmp/プロジェクト/設定.rs",
                "'/tmp/プロジェクト/設定.rs'",
            ),
            (
                "posix/bash/zsh Chinese",
                ShellKind::Posix,
                "/tmp/项目/配置.rs",
                "'/tmp/项目/配置.rs'",
            ),
            (
                "posix/bash/zsh Korean",
                ShellKind::Posix,
                "/tmp/프로젝트/설정.rs",
                "'/tmp/프로젝트/설정.rs'",
            ),
            (
                "posix/bash/zsh emoji",
                ShellKind::Posix,
                "/tmp/project/🚀-deploy/config.json",
                "'/tmp/project/🚀-deploy/config.json'",
            ),
            (
                "fish spaces",
                ShellKind::Fish,
                "/tmp/My File.txt",
                "'/tmp/My File.txt'",
            ),
            (
                "fish single quote",
                ShellKind::Fish,
                "/tmp/Bob's File.txt",
                r"'/tmp/Bob\'s File.txt'",
            ),
            (
                "fish backslash drive colon",
                ShellKind::Fish,
                r"C:\Users\me\file.txt",
                r"'C:\\Users\\me\\file.txt'",
            ),
            (
                "fish Japanese",
                ShellKind::Fish,
                "/tmp/プロジェクト/設定.rs",
                "'/tmp/プロジェクト/設定.rs'",
            ),
            (
                "fish Chinese",
                ShellKind::Fish,
                "/tmp/项目/配置.rs",
                "'/tmp/项目/配置.rs'",
            ),
            (
                "fish Korean",
                ShellKind::Fish,
                "/tmp/프로젝트/설정.rs",
                "'/tmp/프로젝트/설정.rs'",
            ),
            (
                "fish emoji",
                ShellKind::Fish,
                "/tmp/project/🚀-deploy/config.json",
                "'/tmp/project/🚀-deploy/config.json'",
            ),
            (
                "PowerShell spaces",
                ShellKind::PowerShell,
                r"C:\Users\me\My File.txt",
                r"'C:\Users\me\My File.txt'",
            ),
            (
                "PowerShell single quote",
                ShellKind::PowerShell,
                r"C:\Users\me\Bob's File.txt",
                r"'C:\Users\me\Bob''s File.txt'",
            ),
            (
                "PowerShell backslash drive colon",
                ShellKind::PowerShell,
                r"C:\Users\me\file.txt",
                r"C:\Users\me\file.txt",
            ),
            (
                "PowerShell Japanese",
                ShellKind::PowerShell,
                r"C:\work\プロジェクト\設定.rs",
                r"'C:\work\プロジェクト\設定.rs'",
            ),
            (
                "PowerShell Chinese",
                ShellKind::PowerShell,
                r"C:\work\项目\配置.rs",
                r"'C:\work\项目\配置.rs'",
            ),
            (
                "PowerShell Korean",
                ShellKind::PowerShell,
                r"C:\work\프로젝트\설정.rs",
                r"'C:\work\프로젝트\설정.rs'",
            ),
            (
                "PowerShell emoji",
                ShellKind::PowerShell,
                r"C:\work\project\🚀-deploy\config.json",
                r"'C:\work\project\🚀-deploy\config.json'",
            ),
            (
                "cmd spaces",
                ShellKind::Cmd,
                r"C:\Users\me\My File.txt",
                r#""C:\Users\me\My File.txt""#,
            ),
            (
                "cmd single quote",
                ShellKind::Cmd,
                r"C:\Users\me\Bob's File.txt",
                r#""C:\Users\me\Bob's File.txt""#,
            ),
            (
                "cmd backslash drive colon",
                ShellKind::Cmd,
                r"C:\Users\me\file.txt",
                r"C:\Users\me\file.txt",
            ),
            (
                "cmd Japanese",
                ShellKind::Cmd,
                r"C:\work\プロジェクト\設定.rs",
                r#""C:\work\プロジェクト\設定.rs""#,
            ),
            (
                "cmd Chinese",
                ShellKind::Cmd,
                r"C:\work\项目\配置.rs",
                r#""C:\work\项目\配置.rs""#,
            ),
            (
                "cmd Korean",
                ShellKind::Cmd,
                r"C:\work\프로젝트\설정.rs",
                r#""C:\work\프로젝트\설정.rs""#,
            ),
            (
                "cmd emoji",
                ShellKind::Cmd,
                r"C:\work\project\🚀-deploy\config.json",
                r#""C:\work\project\🚀-deploy\config.json""#,
            ),
        ];

        for (name, shell, path, expected) in cases {
            assert_eq!(shell_quote_for(Path::new(path), shell), expected, "{name}");
        }
    }

    #[test]
    fn shell_path_insert_bytes는_trailing_space만_붙이고_enter는_넣지_않는다() {
        for shell in [
            ShellKind::Posix,
            ShellKind::Fish,
            ShellKind::PowerShell,
            ShellKind::Cmd,
        ] {
            let bytes = shell_path_insert_bytes_for(Path::new("/tmp/Bob's File.txt"), shell);
            assert_eq!(bytes.last(), Some(&b' '), "{shell:?}");
            assert!(!bytes[..bytes.len() - 1].contains(&b'\n'), "{shell:?}");
            assert!(!bytes[..bytes.len() - 1].contains(&b'\r'), "{shell:?}");
        }
        assert_eq!(
            shell_path_insert_bytes(Path::new("/a/b_c-d.txt")),
            b"/a/b_c-d.txt ".to_vec()
        );
    }

    #[test]
    fn parent_dirs는_중복을_제거한다() {
        assert_eq!(
            parent_dirs(Path::new("/r/a/f"), Path::new("/r/b/f")),
            vec![PathBuf::from("/r/a"), PathBuf::from("/r/b")]
        );
        assert_eq!(
            parent_dirs(Path::new("/r/a/f"), Path::new("/r/a/g")),
            vec![PathBuf::from("/r/a")]
        );
    }

    #[test]
    fn set_root은_listing을_background로_요청하고_stale_root를_버린다() {
        let base = temp_root("async-root-stale");
        let root_a = base.join("root-a");
        let root_b = base.join("root-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        std::fs::write(root_a.join("a.txt"), b"a").unwrap();
        std::fs::write(root_b.join("b.txt"), b"b").unwrap();

        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(root_a.clone()));
        let stale = tree.take_maintenance_intent().expect("root-a intent");
        assert!(tree.children.is_none());

        tree.set_root(Some(root_b.clone()));
        tree.complete_maintenance(FileTreeMaintenanceCompletion {
            operation: stale.operation,
            generation: stale.generation,
            result: Ok(FileTreeMaintenanceResult::Listing(
                FileTreeListingSnapshot::try_new(vec![
                    FileTreeListingItem::try_new("a.txt".to_owned(), false).unwrap(),
                ])
                .unwrap(),
            )),
        });
        drain_listings(&mut tree);
        pump_listings_for(&mut tree, std::time::Duration::from_millis(100));

        assert!(
            tree.flat.iter().any(|row| row.name == "b.txt"),
            "현재 root 결과는 적용"
        );
        assert!(
            !tree.flat.iter().any(|row| row.name == "a.txt"),
            "이전 root late result는 epoch mismatch로 폐기"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn 권한거부_버튼_rect는_사이드바_화면중앙을_따른다() {
        let area = egui::Rect::from_min_max(egui::pos2(16.0, 180.0), egui::pos2(656.0, 780.0));
        let screen_center = egui::pos2(336.0, 360.0);
        let button = permission_denied_button_rect(area, screen_center);

        assert_eq!(button.center(), screen_center);
        assert_eq!(button.size(), egui::vec2(360.0, 44.0));

        let above_file_area = permission_denied_button_rect(area, egui::pos2(336.0, 100.0));
        assert_eq!(
            above_file_area.top(),
            area.top(),
            "화면 중앙이 파일 영역 위면 버튼은 겹치지 않고 파일 영역 상단에 붙는다"
        );
    }

    #[test]
    fn kittest_root_권한거부는_가운데_버튼으로_설정열기_action을_낸다() {
        use egui_kittest::kittest::Queryable;

        struct State {
            tree: FileTreeUi,
            open_settings: bool,
            fonts_ready: bool,
        }

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspaces = vec![SidebarWorkspaceEntry {
            id: "workspace-a".to_owned(),
            name: "Workspace A".to_owned(),
            repo: None,
            state: SidebarWorkspaceState::Active,
            summary: SidebarSessionSummary::default(),
        }];
        let mut tree = FileTreeUi::new(egui::Context::default());
        let denied = PathBuf::from("/permission-denied-fixture");
        tree.root = Some(denied.clone());
        tree.apply_maintenance_error(&denied, FileTreeMaintenanceErrorCode::PermissionDenied);
        assert_eq!(
            tree.root_error,
            Some(RootListingError::PermissionDenied),
            "PermissionDenied는 일반 문자열 오류와 구분돼야 한다"
        );

        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .build_ui_state(
                |ui, state: &mut State| {
                    if !state.fonts_ready {
                        return;
                    }
                    let sidebar = SidebarSnapshot {
                        active_workspace_id: "workspace-a",
                        workspaces: &workspaces,
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        inbox_count: 0,
                        fleet_count: 0,
                        agents_open: false,
                    };
                    if matches!(
                        state.tree.contents(
                            ui,
                            &std::collections::HashMap::new(),
                            &sidebar,
                            &catalog,
                        ),
                        Some(SidebarAction::OpenMacosFileAccessSettings)
                    ) {
                        state.open_settings = true;
                    }
                },
                State {
                    tree,
                    open_settings: false,
                    fonts_ready: false,
                },
            );
        install_sidebar_test_fonts(&harness.ctx);
        harness.state_mut().fonts_ready = true;
        harness.run();

        let label = catalog.t("file_tree.macos_access_denied", &[]);
        let button = harness.get_by_label(&label);
        assert!(
            (button.rect().center().x - 210.0).abs() <= 1.0,
            "권한 버튼은 파일 트리 가로 중앙에 있어야 한다: {:?}",
            button.rect()
        );
        button.click();
        harness.run();
        assert!(harness.state().open_settings);
    }

    #[test]
    fn collapse_후_도착한_listing_result는_폐기된다() {
        let base = temp_root("async-collapse-stale");
        std::fs::create_dir_all(base.join("d")).unwrap();
        std::fs::write(base.join("d/child.txt"), b"child").unwrap();

        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);

        tree.toggle_dir(&base.join("d"));
        let stale = tree
            .take_maintenance_intent()
            .expect("child listing intent");
        tree.toggle_dir(&base.join("d"));
        tree.complete_maintenance(FileTreeMaintenanceCompletion {
            operation: stale.operation,
            generation: stale.generation,
            result: Ok(FileTreeMaintenanceResult::Listing(
                FileTreeListingSnapshot::try_new(vec![
                    FileTreeListingItem::try_new("child.txt".to_owned(), false).unwrap(),
                ])
                .unwrap(),
            )),
        });

        pump_listings_for(&mut tree, std::time::Duration::from_millis(100));
        let d = tree.flat.iter().find(|row| row.name == "d").unwrap();
        assert!(!d.expanded);
        assert!(
            !tree.flat.iter().any(|row| row.name == "child.txt"),
            "collapse 이후 도착한 stale child listing은 tree state를 오염시키지 않는다"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn 워처_이벤트_경로의_부모만_부분_재나열된다() {
        // 워처 콜백이 보내는 "부모 디렉터리" 재나열 경로를 OS 워처 없이 검증한다
        // (실제 FSEvents 왕복은 타이밍 의존이라 단위 테스트에서 제외 — 수동 스모크).
        let base = temp_root("watch-reload");
        std::fs::create_dir_all(base.join("watched")).unwrap();
        std::fs::create_dir_all(base.join("other")).unwrap();
        std::fs::write(base.join("other/o.txt"), b"o").unwrap();

        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        tree.toggle_dir(&base.join("watched"));
        drain_listings(&mut tree);
        tree.toggle_dir(&base.join("other"));
        drain_listings(&mut tree);
        assert!(tree.flat.iter().any(|r| r.name == "o.txt"));
        assert!(!tree.flat.iter().any(|r| r.name == "new.txt"));

        // 디스크 변경 후 watched만 재나열 → 새 파일 반영, other는 캐시 유지 확인
        std::fs::write(base.join("watched/new.txt"), b"n").unwrap();
        std::fs::write(base.join("other/late.txt"), b"l").unwrap();
        tree.reload_dir(&base.join("watched"));
        drain_listings(&mut tree);

        assert!(
            tree.flat.iter().any(|r| r.name == "new.txt"),
            "부분 재나열 반영"
        );
        assert!(
            !tree.flat.iter().any(|r| r.name == "late.txt"),
            "다른 디렉터리는 재나열되지 않는다 (부분 갱신)"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn 워처_스로틀은_창_내_이벤트를_흡수만_하고_경과_후_일괄_재나열한다() {
        let base = temp_root("throttle");
        std::fs::create_dir_all(base.join("d")).unwrap();
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        tree.toggle_dir(&base.join("d"));
        drain_listings(&mut tree);

        std::fs::write(base.join("d/a.txt"), b"a").unwrap();
        let event = || {
            FileTreeWatchEvent::try_new(FileTreeWatchEventKind::DirtyDirectory, base.join("d"))
                .unwrap()
        };
        tree.apply_watch_snapshot(
            FileTreeWatchSnapshot::try_new(
                tree.maintenance_generation,
                1,
                false,
                vec![event(), event()],
            )
            .unwrap(),
        );
        assert!(!tree.flat.iter().any(|r| r.name == "a.txt"));
        drain_listings(&mut tree);
        assert!(
            tree.flat.iter().any(|r| r.name == "a.txt"),
            "async listing 적용 후 반영"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn 접힘_상태에서도_panel이_채널을_소비한다() {
        let base = temp_root("collapsed-drain");
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        tree.collapsed = true;

        std::fs::write(base.join("new.txt"), b"n").unwrap();
        tree.apply_watch_snapshot(
            FileTreeWatchSnapshot::try_new(
                tree.maintenance_generation,
                1,
                false,
                vec![
                    FileTreeWatchEvent::try_new(
                        FileTreeWatchEventKind::DirtyDirectory,
                        base.clone(),
                    )
                    .unwrap(),
                ],
            )
            .unwrap(),
        );
        assert!(tree.maintenance_intent.is_some());
        // 접힘 render는 host intent를 실행/소비하지 않는다.
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let sidebar = SidebarSnapshot {
            active_workspace_id: "default",
            workspaces: &[],
            view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
            home_notice_count: 0,
            inbox_count: 0,
            fleet_count: 0,
            agents_open: false,
        };
        let ctx = egui::Context::default();
        install_sidebar_test_fonts(&ctx);
        let _ = ctx.run_ui(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                assert!(
                    tree.panel(ui, &std::collections::HashMap::new(), &sidebar, &catalog,)
                        .is_none()
                );
            });
        });
        assert!(tree.maintenance_intent.is_some());
        drain_listings(&mut tree);

        assert!(
            tree.flat.iter().any(|r| r.name == "new.txt"),
            "접힘 중에도 워처 이벤트가 반영된다"
        );
        assert_eq!(tree.in_flight, 0);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn access_이벤트는_무시된다() {
        use notify::event::{AccessKind, CreateKind, EventKind, ModifyKind, RemoveKind};
        assert!(!relevant_fs_event(&EventKind::Access(AccessKind::Any)));
        assert!(relevant_fs_event(&EventKind::Create(CreateKind::Any)));
        assert!(relevant_fs_event(&EventKind::Remove(RemoveKind::Any)));
        assert!(relevant_fs_event(&EventKind::Modify(ModifyKind::Any)));
    }

    #[test]
    fn watcher_generated_경로도_이제_이벤트를_생성한다() {
        let root = PathBuf::from("workspace");
        for name in [
            ".git",
            "node_modules",
            "target",
            "dist",
            "build",
            ".next",
            ".turbo",
            "vendor",
            "logs",
            ".cache",
        ] {
            let path = root.join(name).join("generated.txt");
            assert!(
                !watch_events_for_path(&root, &path, true, &[]).is_empty(),
                "{name} should now produce events"
            );
        }
        assert!(
            !watch_events_for_path(&root, &root.join(".DS_Store"), true, &[]).is_empty(),
            ".DS_Store should now produce events"
        );
        assert!(
            watch_events_for_path(
                &root,
                &root.join("src/generated.txt"),
                true,
                &[root.join("src")]
            )
            .is_empty(),
            "caller-provided ignore prefixes still apply"
        );
    }

    #[test]
    fn gitignore_규칙은_listing에서_더_이상_적용되지_않는다() {
        let base = temp_root("gitignore-listing");
        std::fs::write(base.join(".gitignore"), "design/\n").unwrap();
        std::fs::create_dir_all(base.join(".git/info")).unwrap();
        std::fs::write(base.join(".git/info/exclude"), "info.log\n").unwrap();
        std::fs::create_dir_all(base.join("design")).unwrap();
        std::fs::write(base.join("design/mockup.png"), b"x").unwrap();
        std::fs::write(base.join("info.log"), b"x").unwrap();
        std::fs::create_dir_all(base.join("node_modules/pkg")).unwrap();
        std::fs::write(base.join("node_modules/pkg/index.js"), b"x").unwrap();

        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);

        assert!(
            tree.flat.iter().any(|r| r.name == "design"),
            "gitignore에 등록된 디렉터리도 이제 보인다"
        );
        assert!(
            tree.flat.iter().any(|r| r.name == "info.log"),
            "git/info/exclude 경로도 이제 보인다"
        );
        assert!(
            tree.flat.iter().any(|r| r.name == "node_modules"),
            "기본 generated-dir 필터는 더 이상 숨기지 않는다"
        );

        tree.toggle_dir(&base.join("design"));
        drain_listings(&mut tree);
        assert!(tree.flat.iter().any(|r| r.name == "mockup.png"));

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn watcher_gitignore_rules는_dirty_event를_더_이상_버리지_않는다() {
        let base = temp_root("gitignore-watch");
        std::fs::write(base.join(".gitignore"), "*.tmp\nignored-dir/\n").unwrap();
        std::fs::write(base.join("skip.tmp"), b"x").unwrap();
        std::fs::create_dir_all(base.join("ignored-dir")).unwrap();
        std::fs::write(base.join("ignored-dir/file.rs"), b"x").unwrap();
        std::fs::write(base.join("keep.rs"), b"k").unwrap();

        assert!(
            !watch_events_for_path(&base, &base.join("skip.tmp"), true, &[]).is_empty(),
            "gitignore로 무시된 파일 변경도 dirty event를 발생시킨다"
        );
        assert!(
            !watch_events_for_path(&base, &base.join("ignored-dir/file.rs"), true, &[]).is_empty(),
            "gitignore로 무시된 하위 경로 변경도 dirty event를 발생시킨다"
        );
        assert!(
            !watch_events_for_path(&base, &base.join("keep.rs"), true, &[]).is_empty(),
            "무시되지 않은 파일은 그대로 dirty event를 발생시킨다"
        );
        assert!(
            !watch_events_for_path(&base, &base.join("node_modules/pkg/index.js"), true, &[])
                .is_empty(),
            "기본 generated-dir은 더 이상 이벤트를 버리지 않는다"
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn watcher_env_파일은_hidden_off에서도_signal로_남는다() {
        let root = PathBuf::from("workspace");
        let env = root.join(".env.local");
        let events = watch_events_for_path(&root, &env, false, &[]);

        assert_eq!(
            events,
            vec![
                WatchEvent::EnvFileChanged(env.clone()),
                WatchEvent::DirtyDir(root.clone())
            ]
        );
        assert!(
            watch_events_for_path(&root, &root.join(".hidden/file.txt"), false, &[]).is_empty(),
            "일반 hidden 경로는 show_hidden=false에서 무시"
        );
    }

    #[test]
    fn watcher_env_signal은_ui_state에_누적되고_take로_소진된다() {
        let base = temp_root("env-signal");
        let env = base.join(".env.production");
        std::fs::write(&env, b"SECRET=value").unwrap();

        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);

        tree.apply_watch_snapshot(
            FileTreeWatchSnapshot::try_new(
                tree.maintenance_generation,
                1,
                false,
                vec![
                    FileTreeWatchEvent::try_new(
                        FileTreeWatchEventKind::EnvFileChanged,
                        env.clone(),
                    )
                    .unwrap(),
                ],
            )
            .unwrap(),
        );

        assert_eq!(tree.take_env_warning_candidates(), vec![env]);
        assert!(tree.take_env_warning_candidates().is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn watcher_dirty_dir_batch는_한_프레임_invalidation을_제한한다() {
        let events = (0..=FILE_TREE_WATCH_MAX_EVENTS)
            .map(|i| {
                FileTreeWatchEvent::try_new(
                    FileTreeWatchEventKind::DirtyDirectory,
                    PathBuf::from(format!("/workspace/dir-{i:02}")),
                )
                .unwrap()
            })
            .collect();
        assert!(matches!(
            FileTreeWatchSnapshot::try_new(1, 1, false, events),
            Err(FileTreeMaintenanceErrorCode::WatchPlanTooLarge)
        ));
    }

    #[test]
    fn reread는_펼침_상태를_이월하고_접힌_것은_캐시_없음() {
        let base = std::env::temp_dir().join(format!("deppy-ft-reread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("open/child")).unwrap();
        std::fs::create_dir_all(base.join("closed")).unwrap();
        std::fs::write(base.join("f.txt"), b"x").unwrap();

        // 이전 트리: open은 펼침(빈 캐시), closed는 접힘
        let mut open = dir("open");
        open.expanded = true;
        open.children = Some(Vec::new());
        let old = vec![open, dir("closed")];

        let fresh = reread(&base, &old).unwrap();
        assert_eq!(names(&fresh), vec!["closed", "open", "f.txt"]);
        let open = fresh.iter().find(|n| n.name == "open").unwrap();
        assert!(open.expanded);
        // 펼친 노드는 재나열돼 child가 보인다
        assert_eq!(names(open.children.as_ref().unwrap()), vec!["child"]);
        let closed = fresh.iter().find(|n| n.name == "closed").unwrap();
        assert!(!closed.expanded);
        assert!(closed.children.is_none());

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 파일 트리 스크롤과 같은 구조의 최소 재현: show_rows 가상화 + 행 내용(경로)
    /// 기반 id + 행 전폭 interact. offset이 정확히 행높이 배수만큼 이동하면 직전
    /// 프레임과 같은 rect에 다른 행 id가 들어온다.
    fn tree_like_show_rows(ui: &mut egui::Ui, offset: f32) {
        // 행 간격 0 — 실측 좌표를 행높이 배수로 고정해 rect 일치를 결정적으로 만든다.
        ui.spacing_mut().item_spacing.y = 0.0;
        let row_height = 25.0;
        egui::ScrollArea::vertical()
            .vertical_scroll_offset(offset)
            .show_rows(ui, row_height, 100, |ui, range| {
                for i in range {
                    let row_top = ui.cursor().min.y;
                    let row_rect = egui::Rect::from_min_max(
                        egui::pos2(ui.max_rect().left(), row_top),
                        egui::pos2(ui.max_rect().right(), row_top + row_height),
                    );
                    // 실제 트리의 행 id 체계와 동일: 경로(여기선 인덱스) 기반 + "row" salt
                    let drag_id = egui::Id::new(("file_tree_row", i)).with("row");
                    ui.interact(row_rect, drag_id, egui::Sense::click_and_drag());
                    ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), row_height),
                        egui::Sense::hover(),
                    );
                }
            });
    }

    /// egui 0.35 `warn_if_rect_changes_id` 경고가 그리는 도형(순수 RED 2px 테두리,
    /// context.rs `warn_if_rect_changes_id` 참조) 개수를 센다.
    fn red_warning_rect_count(output: &egui::FullOutput) -> usize {
        output
            .shapes
            .iter()
            .filter(|clipped| match &clipped.shape {
                egui::Shape::Rect(rect) => {
                    rect.stroke.color == egui::Color32::RED
                        && rect.stroke.width == 2.0
                        && rect.fill == egui::Color32::TRANSPARENT
                }
                _ => false,
            })
            .count()
    }

    /// 원인 특정(2026-07-18 "트리 스크롤 중 빨간 네모" 보고): egui 0.35 신설
    /// `Style.debug.warn_if_rect_changes_id`(디버그 빌드 기본 on)는 같은 rect의
    /// 위젯 id가 패스 사이에 바뀌면 Color32::RED 2px 테두리를 그린다. show_rows
    /// 가상화 트리에서 스크롤이 행높이만큼 이동하면 이것이 오발화함을 고정한다.
    // egui `Style::debug`(warn_if_rect_changes_id 등)는 debug_assertions로 게이트돼
    // release egui에는 없다 — 이 테스트는 그 디버그 전용 경고 동작을 검증하므로 debug
    // 빌드에서만 컴파일한다(release에서 bin unittest 타깃이 깨지던 문제 해소).
    #[cfg(debug_assertions)]
    #[test]
    fn kittest_행높이만큼_스크롤하면_rect_id변경_빨간경고가_발화한다() {
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, offset: &mut f32| tree_like_show_rows(ui, *offset),
            0.0_f32,
        );
        // 테스트 프로필과 무관하게 결정적이도록 경고를 명시적으로 켠다 (디버그 기본값).
        harness
            .ctx
            .all_styles_mut(|style| style.debug.warn_if_rect_changes_id = true);
        harness.step();
        harness.step(); // 첫 프레임 정착(초기 사이징 재패스 영향 제거)
        *harness.state_mut() = 25.0; // 정확히 행높이 한 칸 스크롤
        harness.step();
        assert!(
            red_warning_rect_count(harness.output()) > 0,
            "행높이 배수 스크롤 프레임에서 warn_if_rect_changes_id 빨간 테두리가 나와야 원인 재현"
        );
    }

    /// 수정 검증: main.rs `disable_egui_debug_warnings`를 적용하면 같은 스크롤
    /// 시나리오에서 빨간 경고 테두리가 그려지지 않는다.
    #[test]
    fn kittest_디버그경고를_끄면_스크롤중_빨간네모가_없다() {
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, offset: &mut f32| tree_like_show_rows(ui, *offset),
            0.0_f32,
        );
        crate::disable_egui_debug_warnings(&harness.ctx);
        harness.step();
        harness.step();
        *harness.state_mut() = 25.0;
        harness.step();
        assert_eq!(
            red_warning_rect_count(harness.output()),
            0,
            "디버그 경고를 끈 뒤에는 스크롤 중 빨간 테두리가 없어야 한다"
        );
    }

    /// egui `check_for_id_clash`가 그리는 "🔥 … use of … ID" 경고 텍스트 수집
    /// (env_profiles.rs 테스트의 동명 헬퍼와 같은 판정 — 그쪽 발화 테스트가 이 판정이
    /// 실제 충돌을 잡는다는 것을 함께 고정한다).
    fn clash_warning_texts(output: &egui::FullOutput) -> Vec<(String, egui::Pos2)> {
        output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) => {
                    let s = text.galley.text();
                    if s.contains("use of") {
                        Some((s.to_owned(), text.pos))
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect()
    }

    /// 회귀 고정(2026-07-18 "파일 트리 토글·설정 화면 빨간 경고" 보고 후속): 사이드바
    /// 전 조작(폴더 펼침/접힘·호버·컨텍스트 메뉴·드래그&드롭·워크스페이스 접기)을
    /// `warn_on_id_clash`를 켠 채 돌려도 같은-ID 위젯 쌍 경고(🔥)가 없어야 한다.
    /// (당시 보고의 실제 같은-ID 충돌은 env_profiles.rs 환경 변수 표에 있었고 —
    /// 그쪽 테스트 참조 — 트리 토글의 빨간 네모는 행 밀림이 `warn_if_rect_changes_id`
    /// 오탐을 발화시킨 것: 위 스크롤 테스트와 같은 메커니즘.)
    #[test]
    fn kittest_사이드바_조작_전반에_widget_id_충돌이_없다() {
        use egui_kittest::kittest::Queryable;
        let base = std::env::temp_dir().join(format!("deppy-ft-clash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("alpha/sub")).unwrap();
        std::fs::create_dir_all(base.join("beta")).unwrap();
        std::fs::write(base.join("alpha/one.txt"), b"x").unwrap();
        std::fs::write(base.join("alpha/sub/two.txt"), b"x").unwrap();
        std::fs::write(base.join("beta/three.txt"), b"x").unwrap();
        std::fs::write(base.join("root.txt"), b"x").unwrap();
        let base = base.canonicalize().unwrap();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        let workspaces: Vec<SidebarWorkspaceEntry> = (0..5)
            .map(|i| SidebarWorkspaceEntry {
                id: format!("ws-{i}"),
                name: format!("workspace-{i}"),
                repo: None,
                state: SidebarWorkspaceState::Idle,
                summary: SidebarSessionSummary::default(),
            })
            .collect();
        let make_session = |n: usize, agent: bool| SessionEntry {
            tab: runtime::MuxTabId(format!("t{n}")),
            pane: runtime::MuxPaneId(format!("p{n}")),
            session: Some(runtime::SessionId(n as u64)),
            title: format!("세션 {n}"),
            status: agent.then_some(runtime::SessionStatus::Running),
            summary: "요약".to_owned(),
            focused: n == 0,
            attention: false,
            pulse: None,
            agent_line: agent.then(|| "Codex · gpt-5.5 · high".to_owned()),
            status_label: agent.then(|| "실행 중".to_owned()),
            resumable: agent,
            has_cwd: true,
            in_worktree: false,
            status_line: agent.then(|| "PR #124 코드 리뷰".to_owned()),
        };
        let sessions = std::collections::HashMap::from([(
            "ws-2".to_owned(),
            vec![
                make_session(0, false),
                make_session(1, false),
                make_session(2, true),
            ],
        )]);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .with_step_dt(0.05)
            .build_ui_state(
                |ui, state: &mut (FileTreeUi, bool)| {
                    // 폰트(mono_bold) 설치 전 빌드 프레임은 건너뛴다.
                    if !state.1 {
                        return;
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: "ws-2",
                        workspaces: &workspaces,
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        inbox_count: 0,
                        fleet_count: 0,
                        agents_open: false,
                    };
                    state.0.panel(ui, &sessions, &snapshot, &catalog);
                },
                (tree, false),
            );
        harness.ctx.options_mut(|o| o.warn_on_id_clash = true);
        // workspace_row가 쓰는 mono_bold 패밀리를 테스트 컨텍스트에도 설치한다.
        let font_config = crate::config::Config::default();
        crate::fonts::install_cjk_fallback(
            &harness.ctx,
            None,
            &font_config.terminal.mono_font,
            &font_config.terminal.mono_weight,
        );
        harness.state_mut().1 = true;
        for _ in 0..200 {
            harness.step();
            if harness.query_by_label("alpha").is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let report = |stage: &str, output: &egui::FullOutput| {
            let warnings = clash_warning_texts(output);
            assert!(
                warnings.is_empty(),
                "[{stage}] 위젯 ID 충돌 경고 발생: {warnings:?}"
            );
        };
        report("초기", harness.output());
        harness.get_by_label("alpha").click();
        harness.step();
        report("펼침클릭", harness.output());
        drain_listings(&mut harness.state_mut().0);
        for _ in 0..30 {
            harness.step();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        report("펼침후", harness.output());
        // 호버 상태에서 한 프레임
        let alpha_rect = harness.get_by_label("alpha").rect();
        harness.hover_at(alpha_rect.center());
        harness.step();
        report("호버", harness.output());
        // 파일 행 우클릭 → 컨텍스트 메뉴 열림 상태로 한 프레임
        harness.get_by_label("one.txt").click_secondary();
        harness.step();
        harness.step();
        report("컨텍스트메뉴", harness.output());
        harness.key_press(egui::Key::Escape);
        harness.step();
        // 드래그: one.txt를 beta 폴더 위로 끌어 hover payload 상태 재현
        let src = harness.get_by_label("one.txt").rect().center();
        let dst = harness.get_by_label("beta").rect().center();
        harness.drag_at(src);
        harness.step();
        harness.hover_at(src + egui::vec2(6.0, 6.0));
        harness.step();
        report("드래그시작", harness.output());
        harness.hover_at(dst);
        harness.step();
        report("드래그중", harness.output());
        harness.drop_at(dst);
        harness.step();
        report("드롭", harness.output());
        for _ in 0..20 {
            harness.step();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // 활성 워크스페이스 행(생성순 3번째, painter 텍스트라 좌표 클릭) 접기/펼치기
        let ws_active = egui::pos2(200.0, 145.0);
        harness.drag_at(ws_active);
        harness.step();
        harness.drop_at(ws_active);
        harness.step();
        report("워크스페이스접기", harness.output());
        harness.drag_at(ws_active);
        harness.step();
        harness.drop_at(ws_active);
        harness.step();
        report("워크스페이스펼치기", harness.output());
        // 접힘 토글 후 다시 접기 클릭 프레임
        harness.get_by_label("alpha").click();
        harness.step();
        report("접힘클릭", harness.output());
        harness.step();
        report("접힘후", harness.output());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn kittest_워크스페이스_포커스이동은_기존_세션트리를_닫지않는다() {
        use egui_kittest::kittest::Queryable;

        struct State {
            tree: FileTreeUi,
            active: String,
            switch_target: Option<String>,
            focus_target: Option<String>,
            fonts_ready: bool,
        }

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspaces = vec![
            SidebarWorkspaceEntry {
                id: "workspace-a".to_owned(),
                name: "Workspace A".to_owned(),
                repo: None,
                state: SidebarWorkspaceState::Active,
                summary: SidebarSessionSummary::default(),
            },
            SidebarWorkspaceEntry {
                id: "workspace-b".to_owned(),
                name: "Workspace B".to_owned(),
                repo: None,
                state: SidebarWorkspaceState::Warm,
                summary: SidebarSessionSummary::default(),
            },
        ];
        let session = |workspace: &str, title: &str| SessionEntry {
            tab: runtime::MuxTabId(format!("tab-{workspace}")),
            pane: runtime::MuxPaneId(format!("pane-{workspace}")),
            session: Some(runtime::SessionId(1)),
            title: title.to_owned(),
            status: None,
            summary: String::new(),
            focused: false,
            attention: false,
            pulse: None,
            agent_line: None,
            status_label: None,
            resumable: false,
            has_cwd: false,
            in_worktree: false,
            status_line: None,
        };
        let sessions = std::collections::HashMap::from([
            ("workspace-a".to_owned(), vec![session("a", "Session A")]),
            ("workspace-b".to_owned(), vec![session("b", "Session B")]),
        ]);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .with_step_dt(0.05)
            .build_ui_state(
                |ui, state: &mut State| {
                    if !state.fonts_ready {
                        return;
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: &state.active,
                        workspaces: &workspaces,
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        inbox_count: 0,
                        fleet_count: 0,
                        agents_open: false,
                    };
                    match state.tree.panel(ui, &sessions, &snapshot, &catalog) {
                        Some(SidebarAction::SwitchWorkspace(workspace_id)) => {
                            state.switch_target = Some(workspace_id.clone());
                            state.active = workspace_id;
                        }
                        Some(SidebarAction::FocusSession { workspace_id, .. }) => {
                            state.focus_target = Some(workspace_id.clone());
                            state.active = workspace_id;
                        }
                        _ => {}
                    }
                },
                State {
                    tree: FileTreeUi::new(egui::Context::default()),
                    active: "workspace-a".to_owned(),
                    switch_target: None,
                    focus_target: None,
                    fonts_ready: false,
                },
            );
        install_sidebar_test_fonts(&harness.ctx);
        harness.state_mut().fonts_ready = true;
        harness.run();

        harness.get_by_label("Session A");
        assert!(harness.query_by_label("Session B").is_none());

        harness.get_by_label("Workspace B").click();
        harness.run();
        assert_eq!(harness.state().active, "workspace-b");
        harness.get_by_label("Session A");
        harness.get_by_label("Session B");

        // 실제 App의 workspace 전환은 파일 트리 루트도 바꾼다. 루트 교체가 sidebar
        // 인스턴스/확장 map을 초기화하면 이 시점에 A가 다시 닫히는 회귀가 생긴다.
        let root = std::env::temp_dir().join(format!(
            "deppy-ft-multi-workspace-root-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        harness.state_mut().tree.set_root(Some(root.clone()));
        drain_listings(&mut harness.state_mut().tree);
        harness.run();
        harness.get_by_label("Session A");
        harness.get_by_label("Session B");

        harness.get_by_label("Workspace A").click();
        harness.run();
        assert_eq!(harness.state().active, "workspace-a");
        assert_eq!(
            harness.state().switch_target.as_deref(),
            Some("workspace-a")
        );
        harness.get_by_label("Session A");
        harness.get_by_label("Session B");

        // 활성 행을 다시 누르면 접기와 함께 SwitchWorkspace(A)를 다시 방출한다. App은
        // 같은 runtime 전환은 생략하되 Home/Inbox에서 Terminal view로 복귀한다.
        harness.state_mut().switch_target = None;
        harness.get_by_label("Workspace A").click();
        harness.run();
        assert_eq!(
            harness.state().switch_target.as_deref(),
            Some("workspace-a")
        );
        assert!(harness.query_by_label("Session A").is_none());
        harness.get_by_label("Session B");

        // 열린 비활성 세션 클릭은 workspace뿐 아니라 정확한 세션 focus 요청을 낸다.
        harness.get_by_label("Session B").click();
        harness.run();
        assert_eq!(harness.state().active, "workspace-b");
        assert_eq!(harness.state().focus_target.as_deref(), Some("workspace-b"));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// codex 리뷰 P2 회귀: 활성 워크스페이스가 생성순 뒤쪽이면 이전 구현은
    /// `before_active` 행들을 유일한 스크롤 영역 **밖**에 그려 46px씩 사이드바를
    /// 잠식했고, 워크스페이스가 많으면 파일 트리가 클립 밖으로 밀려도 스크롤할
    /// 방법이 없었다. 전체 순서 목록이 하나의 bounded 스크롤을 공유한 뒤에는
    /// 워크스페이스 13개 + 활성이 마지막이어도 루트 파일 행이 계속 보여야 한다.
    #[test]
    fn kittest_활성_워크스페이스가_생성순_끝이어도_파일트리가_보인다() {
        use egui_kittest::kittest::Queryable;
        let base = std::env::temp_dir().join(format!("deppy-ft-wslist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("root.txt"), b"x").unwrap();
        let base = base.canonicalize().unwrap();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        let workspaces: Vec<SidebarWorkspaceEntry> = (0..13)
            .map(|i| SidebarWorkspaceEntry {
                id: format!("ws-{i}"),
                name: format!("workspace-{i}"),
                repo: None,
                state: SidebarWorkspaceState::Idle,
                summary: SidebarSessionSummary::default(),
            })
            .collect();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .build_ui_state(
                |ui, state: &mut (FileTreeUi, bool)| {
                    // 폰트(mono_bold) 설치 전 빌드 프레임은 건너뛴다.
                    if !state.1 {
                        return;
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: "ws-12",
                        workspaces: &workspaces,
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        inbox_count: 0,
                        fleet_count: 0,
                        agents_open: false,
                    };
                    state
                        .0
                        .panel(ui, &std::collections::HashMap::new(), &snapshot, &catalog);
                },
                (tree, false),
            );
        let font_config = crate::config::Config::default();
        crate::fonts::install_cjk_fallback(
            &harness.ctx,
            None,
            &font_config.terminal.mono_font,
            &font_config.terminal.mono_weight,
        );
        harness.state_mut().1 = true;
        for _ in 0..200 {
            harness.step();
            if harness.query_by_label("root.txt").is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // 라벨 노드는 클립 밖에서도 만들어지므로 존재가 아니라 **위치**를 본다.
        // 사이드바 본문은 패널 높이 700 − 하단 네비 139 = 최대 561 안이어야 보인다.
        // 이전 구현은 before_active 12행(552px)이 본문을 잠식해 행이 그 밖으로 밀렸다.
        let row_top = harness.get_by_label("root.txt").rect().top();
        assert!(
            row_top < 561.0,
            "루트 파일 행이 사이드바 본문 밖(y={row_top})으로 밀렸다 — 워크스페이스 목록이 bounded 스크롤을 공유해야 한다"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 파일 행 더블클릭은 native open을 실행하지 않고 host intent만 생성한다.
    #[test]
    fn kittest_파일_더블클릭은_외부_열기_액션을_낸다() {
        use egui_kittest::kittest::Queryable;
        let base = std::env::temp_dir().join(format!("deppy-ft-dclick-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // set_root는 canonicalize해 보관 — 행 경로 비교 기준을 맞춘다 (macOS /var→/private/var).
        let base = base.canonicalize().unwrap();
        std::fs::write(base.join("a.pdf"), b"x").unwrap();
        std::fs::write(base.join("run.sh"), b"x").unwrap();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        // step_dt를 더블클릭 판정 한계(0.3s) 아래로 — 클릭 2번이 한 스텝 간격으로 온다.
        let mut fonts_ready = false;
        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(0.05)
            .build_ui_state(
                move |ui, state: &mut (FileTreeUi, Vec<SidebarAction>)| {
                    if !fonts_ready {
                        install_sidebar_test_fonts(ui.ctx());
                        fonts_ready = true;
                        return;
                    }
                    // 워크스페이스 목록은 이 테스트와 무관 — 최소 스냅샷.
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: "ws-test",
                        workspaces: &[],
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        inbox_count: 0,
                        fleet_count: 0,
                        agents_open: false,
                    };
                    if let Some(a) =
                        state
                            .0
                            .panel(ui, &std::collections::HashMap::new(), &snapshot, &catalog)
                    {
                        state.1.push(a);
                    }
                },
                (tree, Vec::new()),
            );
        // 리스팅은 백그라운드 워커 — 파일 행이 나타날 때까지 프레임을 돌린다.
        for _ in 0..200 {
            harness.step();
            if harness.query_by_label("a.pdf").is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // 실행 위험군 판정도 host의 realpath/metadata 검증 몫이다.
        harness.get_by_label("run.sh").click();
        harness.step();
        harness.get_by_label("run.sh").click();
        harness.step();
        let first = harness.state_mut().0.take_io_intent().expect("open intent");
        let (operation, generation) = (first.operation, first.generation);
        match first.request {
            FileTreeIoRequest::OpenPath {
                target,
                require_openable_file,
            } => {
                assert!(require_openable_file);
                assert_eq!(target.as_path(), base.join("run.sh"));
            }
            other => panic!("unexpected request: {other:?}"),
        }
        harness.state_mut().0.complete_io(FileTreeIoCompletion {
            operation,
            generation,
            result: Err(FileTreeIoErrorCode::NativeFailure),
        });
        // 시뮬레이션 시간 경과 — 직전 클릭 연쇄를 끊는다 (egui triple 판정 창 0.6s는
        // 마지막 클릭과의 거리만 보므로, 붙여서 클릭하면 pdf 2번째가 triple로 잡힌다).
        for _ in 0..15 {
            harness.step();
        }
        // pdf도 동일하게 bounded/redacted open intent를 낸다.
        harness.get_by_label("a.pdf").click();
        harness.step();
        harness.get_by_label("a.pdf").click();
        harness.step();
        let second = harness
            .state_mut()
            .0
            .take_io_intent()
            .expect("second open intent");
        match second.request {
            FileTreeIoRequest::OpenPath { target, .. } => {
                assert_eq!(target.as_path(), base.join("a.pdf"));
            }
            other => panic!("unexpected request: {other:?}"),
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 워크스페이스 행(painter 기반 — 라벨 노드 없음) 우클릭용 헬퍼: 행 높이(46px)
    /// 범위를 훑으며 우클릭해 메뉴 라벨이 나타나는지 본다. 실제 App 연결은 app.rs의
    /// CloseWorkspace 핸들러가 하고, 여기서는 메뉴 → 액션 방출만 검증한다.
    fn right_click_scan(
        harness: &mut egui_kittest::Harness<'_, (FileTreeUi, Vec<SidebarAction>, bool)>,
        label: &str,
    ) -> bool {
        use egui_kittest::kittest::Queryable;
        for y_step in 0..20 {
            let pos = egui::pos2(100.0, 24.0 + y_step as f32 * 6.0);
            harness.event(egui::Event::PointerMoved(pos));
            harness.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            });
            harness.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            });
            harness.step();
            if harness.query_by_label(label).is_some() {
                return true;
            }
        }
        false
    }

    fn close_menu_harness<'a>(
        workspaces: &'a [SidebarWorkspaceEntry],
        active_id: &'static str,
        catalog: &'a i18n::Catalog,
    ) -> egui_kittest::Harness<'a, (FileTreeUi, Vec<SidebarAction>, bool)> {
        let tree = FileTreeUi::new(egui::Context::default());
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .with_step_dt(0.05)
            .build_ui_state(
                |ui, state: &mut (FileTreeUi, Vec<SidebarAction>, bool)| {
                    // 폰트(mono_bold) 설치 전 빌드 프레임은 건너뛴다.
                    if !state.2 {
                        return;
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: active_id,
                        workspaces,
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        inbox_count: 0,
                        fleet_count: 0,
                        agents_open: false,
                    };
                    if let Some(a) =
                        state
                            .0
                            .panel(ui, &std::collections::HashMap::new(), &snapshot, catalog)
                    {
                        state.1.push(a);
                    }
                },
                (tree, Vec::new(), false),
            );
        // workspace_row가 쓰는 mono_bold 패밀리를 테스트 컨텍스트에도 설치한다.
        let font_config = crate::config::Config::default();
        crate::fonts::install_cjk_fallback(
            &harness.ctx,
            None,
            &font_config.terminal.mono_font,
            &font_config.terminal.mono_weight,
        );
        harness.state_mut().2 = true;
        harness.step();
        harness
    }

    /// 워크스페이스 행 우클릭 → 「워크스페이스 종료」 메뉴가 열린다 (실제 팝업 경로).
    /// 팝업 안 버튼 클릭은 kittest가 press/release를 다른 프레임에 재생해 관측 불가
    /// — 액션 방출은 아래 분리 본문 테스트가 검증한다.
    #[test]
    fn kittest_워크스페이스_우클릭이_종료메뉴를_연다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspaces = vec![SidebarWorkspaceEntry {
            id: "ws-close".to_owned(),
            name: "closer".to_owned(),
            repo: None,
            state: SidebarWorkspaceState::Active,
            summary: SidebarSessionSummary::default(),
        }];
        let mut harness = close_menu_harness(&workspaces, "ws-close", &catalog);
        assert!(
            right_click_scan(&mut harness, "Close workspace sessions"),
            "워크스페이스 행 우클릭이 종료 메뉴를 열지 못함"
        );
    }

    /// 종료 메뉴 본문 클릭 → CloseWorkspace(id) 액션 방출 (last_output_menu_items 관례).
    #[test]
    fn kittest_종료메뉴_클릭이_close_workspace_액션을_낸다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspace = SidebarWorkspaceEntry {
            id: "ws-close".to_owned(),
            name: "closer".to_owned(),
            repo: None,
            state: SidebarWorkspaceState::Warm,
            summary: SidebarSessionSummary::default(),
        };
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, action: &mut Option<SidebarAction>| {
                workspace_context_menu_items(ui, &workspace, &catalog, action);
            },
            None,
        );
        harness.run();
        harness.get_by_label("Close workspace sessions").click();
        harness.run();
        assert!(
            matches!(
                harness.state(),
                Some(SidebarAction::CloseWorkspace(id)) if id == "ws-close"
            ),
            "종료 메뉴 클릭이 CloseWorkspace 액션을 내지 않음"
        );
    }

    /// 「이름 바꾸기」 클릭 → RenameWorkspace(id) 액션 방출. Idle이어도 노출된다 —
    /// 세션이 없어도 이름은 바꿀 수 있다.
    #[test]
    fn kittest_이름바꾸기_클릭이_rename_workspace_액션을_낸다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspace = SidebarWorkspaceEntry {
            id: "ws-rename".to_owned(),
            name: "sleeper".to_owned(),
            repo: None,
            state: SidebarWorkspaceState::Idle,
            summary: SidebarSessionSummary::inactive(0),
        };
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, action: &mut Option<SidebarAction>| {
                workspace_context_menu_items(ui, &workspace, &catalog, action);
            },
            None,
        );
        harness.run();
        harness.get_by_label("Rename workspace").click();
        harness.run();
        assert!(
            matches!(
                harness.state(),
                Some(SidebarAction::RenameWorkspace(id)) if id == "ws-rename"
            ),
            "이름 바꾸기 클릭이 RenameWorkspace 액션을 내지 않음"
        );
    }

    /// Idle(비활성) 워크스페이스 메뉴에는 「이름 바꾸기」만 있고 종료 항목은 없다 —
    /// 닫을 세션이 없다.
    #[test]
    fn kittest_비활성_워크스페이스에는_종료메뉴가_없다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspaces = vec![SidebarWorkspaceEntry {
            id: "ws-idle".to_owned(),
            name: "sleeper".to_owned(),
            repo: None,
            state: SidebarWorkspaceState::Idle,
            summary: SidebarSessionSummary::inactive(0),
        }];
        let mut harness = close_menu_harness(&workspaces, "ws-active-elsewhere", &catalog);
        assert!(
            right_click_scan(&mut harness, "Rename workspace"),
            "Idle 워크스페이스 행에 이름 바꾸기 메뉴가 없다"
        );
        assert!(
            !right_click_scan(&mut harness, "Close workspace sessions"),
            "Idle 워크스페이스 행에 종료 메뉴가 떴다"
        );
        assert!(harness.state().1.is_empty(), "Idle 행 우클릭이 액션을 냄");
    }

    /// 좁은 폭에서도 워크스페이스 메뉴 항목은 한 줄로 그려진다 — 이전에는 메뉴가
    /// 좁은 폭을 물려받아 「워크스페이스 종료」가 두 줄로 잘렸다(2026-07-18 스샷).
    /// 메뉴 본문이 가장 긴 항목의 no-wrap 폭으로 최소 폭을 강제하므로 100px 제약
    /// 안에서도 버튼이 제약 밖으로 확장되고(줄바꿈 없음) 두 항목의 행 높이가 같다.
    #[test]
    fn kittest_좁은_폭에서도_워크스페이스_메뉴가_한줄로_그려진다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspace = SidebarWorkspaceEntry {
            id: "ws-narrow".to_owned(),
            name: "narrow".to_owned(),
            repo: None,
            state: SidebarWorkspaceState::Active,
            summary: SidebarSessionSummary::default(),
        };
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, action: &mut Option<SidebarAction>| {
                ui.allocate_ui(egui::vec2(100.0, 300.0), |ui| {
                    workspace_context_menu_items(ui, &workspace, &catalog, action);
                });
            },
            None,
        );
        harness.run();
        let close = harness.get_by_label("Close workspace sessions").rect();
        let rename = harness.get_by_label("Rename workspace").rect();
        assert!(
            close.width() > 100.0,
            "종료 버튼이 최소 폭으로 확장되지 않음 (width={})",
            close.width()
        );
        assert!(
            (close.height() - rename.height()).abs() < 0.5,
            "종료 항목이 여러 줄로 접힘 (close={}, rename={})",
            close.height(),
            rename.height()
        );
    }

    /// 워크스페이스가 하나도 없으면(종료 숨김 반영) 목록 헤더 대신 빈 상태 CTA가 뜨고,
    /// 큰 + 버튼 클릭이 CreateWorkspaceFromPicker 액션을 낸다 — 폴더 선택/생성·전환은
    /// App 소관(rfd + ws_create 흐름)이라 여기서는 방출까지만 검증한다.
    #[test]
    fn kittest_워크스페이스_없으면_빈상태_cta가_액션을_낸다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut fonts_ready = false;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .build_ui_state(
                move |ui, state: &mut (FileTreeUi, Vec<SidebarAction>)| {
                    if !fonts_ready {
                        install_sidebar_test_fonts(ui.ctx());
                        fonts_ready = true;
                        return;
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: "ws-hidden",
                        workspaces: &[],
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        inbox_count: 0,
                        fleet_count: 0,
                        agents_open: false,
                    };
                    if let Some(a) =
                        state
                            .0
                            .panel(ui, &std::collections::HashMap::new(), &snapshot, &catalog)
                    {
                        state.1.push(a);
                    }
                },
                (FileTreeUi::new(egui::Context::default()), Vec::new()),
            );
        harness.run();
        assert!(
            harness.query_by_label("Start a workspace").is_some(),
            "빈 상태 안내문이 보이지 않음"
        );
        harness.get_by_label("+").click();
        harness.run();
        assert!(
            matches!(
                harness.state().1.as_slice(),
                [SidebarAction::CreateWorkspaceFromPicker]
            ),
            "빈 상태 + 클릭이 CreateWorkspaceFromPicker를 내지 않음"
        );
    }

    /// 하단 nav 작업함 배지 — 0이면 숨김(None), 그 외엔 카운트 문구.
    #[test]
    fn nav_badge는_0이면_숨긴다() {
        assert_eq!(nav_badge_text(0), None);
        assert_eq!(nav_badge_text(3), Some("3".to_owned()));
        assert_eq!(nav_badge_text(12), Some("12".to_owned()));
    }

    #[test]
    fn 하단_nav높이는_50px와_폴더영역180px_경계를_지킨다() {
        assert_eq!(
            sidebar_vertical_section_heights(700.0, 0.0),
            (644.0, SIDEBAR_NAV_MIN_HEIGHT)
        );
        assert_eq!(
            sidebar_vertical_section_heights(700.0, SIDEBAR_NAV_DEFAULT_HEIGHT),
            (586.0, SIDEBAR_NAV_DEFAULT_HEIGHT)
        );
        assert_eq!(
            sidebar_vertical_section_heights(700.0, 1_000.0),
            (SIDEBAR_BODY_MIN_HEIGHT, 514.0)
        );
    }

    /// 하단 nav 4항목 렌더 + 클릭 → 액션 방출. 재클릭 토글은 App 로직이라
    /// 여기서는 방출까지만 검증한다.
    #[test]
    fn kittest_하단_nav_클릭이_홈_작업함_플릿_에이전트_액션을_낸다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut fonts_ready = false;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .build_ui_state(
                move |ui, state: &mut (FileTreeUi, Vec<SidebarAction>)| {
                    if !fonts_ready {
                        install_sidebar_test_fonts(ui.ctx());
                        fonts_ready = true;
                        return;
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: "ws-test",
                        workspaces: &[],
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 4,
                        inbox_count: 2,
                        fleet_count: 0,
                        agents_open: false,
                    };
                    if let Some(a) =
                        state
                            .0
                            .panel(ui, &std::collections::HashMap::new(), &snapshot, &catalog)
                    {
                        state.1.push(a);
                    }
                },
                (FileTreeUi::new(egui::Context::default()), Vec::new()),
            );
        harness.run();
        harness.get_by_label("Home").click();
        harness.run();
        harness.get_by_label("Inbox").click();
        harness.run();
        harness.get_by_label("Fleet").click();
        harness.run();
        harness.get_by_label("Agents").click();
        harness.run();
        let kinds: Vec<&'static str> = harness
            .state()
            .1
            .iter()
            .map(|action| match action {
                SidebarAction::ShowHome => "home",
                SidebarAction::ShowInbox => "inbox",
                SidebarAction::ShowFleet => "fleet",
                SidebarAction::OpenAgents => "agents",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["home", "inbox", "fleet", "agents"],
            "nav 4항목 클릭이 각각의 액션을 순서대로 내야 한다"
        );
    }

    #[test]
    fn kittest_50px_하단_nav는_내부스크롤로_에이전트까지_접근한다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.navigation_section_height = SIDEBAR_NAV_MIN_HEIGHT;
        let mut fonts_ready = false;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .build_ui_state(
                move |ui, state: &mut (FileTreeUi, Vec<SidebarAction>)| {
                    if !fonts_ready {
                        install_sidebar_test_fonts(ui.ctx());
                        fonts_ready = true;
                        return;
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: "ws-test",
                        workspaces: &[],
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        inbox_count: 0,
                        fleet_count: 0,
                        agents_open: false,
                    };
                    if let Some(action) =
                        state
                            .0
                            .panel(ui, &std::collections::HashMap::new(), &snapshot, &catalog)
                    {
                        state.1.push(action);
                    }
                },
                (tree, Vec::new()),
            );
        harness.run();
        harness.get_by_label("Agents").scroll_to_me();
        harness.run();
        harness.get_by_label("Agents").click();
        harness.run();

        assert_eq!(
            harness.state().0.navigation_section_height,
            SIDEBAR_NAV_MIN_HEIGHT
        );
        assert!(matches!(
            harness.state().1.as_slice(),
            [SidebarAction::OpenAgents]
        ));
    }

    /// 반입 대상 폴더 판정(§과제①②) — 폴더 행은 자신, 파일 행은 부모.
    #[test]
    fn 반입_대상은_폴더행_자신_파일행_부모() {
        let root = PathBuf::from("/ws");
        let dir_row = FlatRow {
            path: root.join("sub"),
            name: "sub".to_owned(),
            depth: 0,
            is_dir: true,
            expanded: false,
        };
        let file_row = FlatRow {
            path: root.join("sub/a.txt"),
            name: "a.txt".to_owned(),
            depth: 1,
            is_dir: false,
            expanded: false,
        };
        assert_eq!(row_target_dir(&dir_row, Some(&root)), root.join("sub"));
        assert_eq!(row_target_dir(&file_row, Some(&root)), root.join("sub"));
    }

    /// 외부 반입 복사 유틸 — 원본 보존·덮어쓰기 거부·자기 자손 차단·tmp 잔재 없음.
    #[test]
    fn copy_into_dir은_원본보존_충돌거부_자기자손차단() {
        let base = temp_root("copy-into");
        let src = base.join("src.txt");
        std::fs::write(&src, b"payload").unwrap();
        let dst_dir = base.join("dst");
        std::fs::create_dir(&dst_dir).unwrap();
        let dst_dir = dst_dir.canonicalize().unwrap();

        copy_into_dir(&src, &dst_dir).unwrap();
        assert_eq!(std::fs::read(dst_dir.join("src.txt")).unwrap(), b"payload");
        assert!(src.exists(), "복사는 원본을 보존해야 한다");

        // 같은 이름 재복사 — 덮어쓰기 거부(§9-5 관례) + tmp 잔재 없음.
        std::fs::write(&src, b"changed").unwrap();
        let err = copy_into_dir(&src, &dst_dir).unwrap_err();
        assert!(err.contains("같은 이름"), "충돌 메시지가 아님: {err}");
        assert_eq!(
            std::fs::read(dst_dir.join("src.txt")).unwrap(),
            b"payload",
            "충돌 시 기존 파일이 덮이면 안 된다"
        );
        let names: Vec<String> = std::fs::read_dir(&dst_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["src.txt"], "tmp 잔재가 남음: {names:?}");

        // 디렉터리를 자기 자손으로 복사 — 무한 재귀 사전 차단.
        let outer = base.join("outer");
        std::fs::create_dir_all(outer.join("inner")).unwrap();
        let inner = outer.join("inner").canonicalize().unwrap();
        let err = copy_into_dir(&outer, &inner).unwrap_err();
        assert!(err.contains("자기 자신"), "자손 차단 메시지가 아님: {err}");

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// AppKit 스크린 좌표(bottom-left) → egui 창 좌표 환산 — 드래그 중 winit 포인터
    /// 부재(draggingUpdated 미구현) 보강 경로의 순수 수식 검증.
    #[test]
    fn 스크린좌표_변환은_bottomleft를_뒤집고_창원점을_뺀다() {
        // primary 높이 1000, 마우스 (500, 900)(bottom-left) → top-left y=100.
        // 창 내용 원점 (100, 50) → 창 좌표 (400, 50).
        assert_eq!(
            screen_to_window_pos((500.0, 900.0), 1000.0, 1.0, egui::pos2(100.0, 50.0)),
            egui::pos2(400.0, 50.0)
        );
        // zoom 2배면 AppKit points를 절반 스케일로 환산한 뒤 원점을 뺀다.
        assert_eq!(
            screen_to_window_pos((500.0, 900.0), 1000.0, 2.0, egui::pos2(100.0, 50.0)),
            egui::pos2(150.0, 0.0)
        );
    }

    /// 트리 ⌘V 신호 판정 — Event::Paste 또는 (macOS) command+V key-up.
    #[test]
    fn 트리_paste_신호는_paste이벤트나_v_keyup이다() {
        assert!(is_tree_paste_signal(&egui::Event::Paste(String::new())));
        let v_up = egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        };
        assert_eq!(is_tree_paste_signal(&v_up), cfg!(target_os = "macos"));
        let v_down = egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        };
        assert!(!is_tree_paste_signal(&v_down), "press는 신호가 아니다");
        assert!(!is_tree_paste_signal(&egui::Event::Copy));
    }

    fn drop_harness(
        catalog: &i18n::Catalog,
        tree: FileTreeUi,
    ) -> egui_kittest::Harness<'_, (FileTreeUi, Vec<SidebarAction>)> {
        let mut fonts_ready = false;
        egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .build_ui_state(
                move |ui, state: &mut (FileTreeUi, Vec<SidebarAction>)| {
                    if !fonts_ready {
                        install_sidebar_test_fonts(ui.ctx());
                        fonts_ready = true;
                        return;
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: "ws-test",
                        workspaces: &[],
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        inbox_count: 0,
                        fleet_count: 0,
                        agents_open: false,
                    };
                    if let Some(a) =
                        state
                            .0
                            .panel(ui, &std::collections::HashMap::new(), &snapshot, catalog)
                    {
                        state.1.push(a);
                    }
                },
                (tree, Vec::new()),
            )
    }

    /// Finder → 트리 OS 드롭은 포인터 밑 폴더를 대상으로 bounded host intent를 낸다.
    #[test]
    fn kittest_finder_드롭은_포인터_밑_폴더로_복사한다() {
        use egui_kittest::kittest::Queryable;
        let base = temp_root("os-drop-dir");
        let base = base.canonicalize().unwrap();
        std::fs::create_dir(base.join("dropdir")).unwrap();
        let src_home = temp_root("os-drop-src");
        let src = src_home.join("payload.txt");
        std::fs::write(&src, b"drop").unwrap();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        let mut harness = drop_harness(&catalog, tree);
        for _ in 0..200 {
            harness.step();
            if harness.query_by_label("dropdir").is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // 포인터를 폴더 행 위에 두고 OS 드롭 주입 — drag_pos는 egui 포인터 폴백을 쓴다
        // (kittest는 viewport inner_rect가 없어 NSEvent 경로가 꺼진다).
        let row_pos = harness.get_by_label("dropdir").rect().center();
        harness
            .input_mut()
            .events
            .push(egui::Event::PointerMoved(row_pos));
        harness.input_mut().dropped_files.push(egui::DroppedFile {
            path: Some(src.clone()),
            ..Default::default()
        });
        harness.step();
        let intent = harness.state_mut().0.take_io_intent().expect("copy intent");
        match intent.request {
            FileTreeIoRequest::CopyInto {
                sources,
                destination,
            } => {
                assert_eq!(sources.into_paths(), vec![src.clone()]);
                assert_eq!(destination.as_path(), base.join("dropdir"));
            }
            other => panic!("unexpected request: {other:?}"),
        }
        assert!(src.exists(), "드롭은 원본을 보존해야 한다(복사)");
        assert!(
            harness.state().0.error.is_none(),
            "에러 라벨이 남음: {:?}",
            harness.state().0.error
        );
        std::fs::remove_dir_all(&base).unwrap();
        std::fs::remove_dir_all(&src_home).unwrap();
    }

    /// 클립보드 ⌘V 붙여넣기(§과제②): 트리 빈 영역(hover 행 없음)에서는 루트로 복사되고,
    /// 트리가 신호를 소비했음을 App에 알린다(터미널 이중 처리 억제 배선).
    #[test]
    fn kittest_클립보드_파일_붙여넣기는_트리영역에서_루트로_복사한다() {
        use egui_kittest::kittest::Queryable;
        let base = temp_root("paste-root");
        let base = base.canonicalize().unwrap();
        std::fs::write(base.join("seed.txt"), b"x").unwrap();
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        let mut harness = drop_harness(&catalog, tree);
        for _ in 0..200 {
            harness.step();
            if harness.query_by_label("seed.txt").is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // 행 아래 빈 트리 영역에 포인터 — 대상 행이 없으니 루트로 복사돼야 한다.
        harness
            .input_mut()
            .events
            .push(egui::Event::PointerMoved(egui::pos2(200.0, 400.0)));
        harness
            .input_mut()
            .events
            .push(egui::Event::Paste(String::new()));
        harness.step();
        let intent = harness
            .state_mut()
            .0
            .take_io_intent()
            .expect("paste intent");
        match intent.request {
            FileTreeIoRequest::PasteFromClipboard { destination } => {
                assert_eq!(destination.as_path(), base.as_path());
            }
            other => panic!("unexpected request: {other:?}"),
        }
        let (paste_consumed, copy_consumed) =
            harness.state_mut().0.take_clipboard_shortcut_consumption();
        assert!(paste_consumed, "트리가 ⌘V 소비를 App에 알려야 한다");
        assert!(!copy_consumed);
        assert!(
            harness.state().0.error.is_none(),
            "에러 라벨이 남음: {:?}",
            harness.state().0.error
        );
        std::fs::remove_dir_all(&base).unwrap();
    }
}
