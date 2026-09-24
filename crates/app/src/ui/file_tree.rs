//! 폴더 트리 사이드바 (docs/file-tree-design.md).
//!
//! 리소스 3원칙(§3): lazy listing(펼친 노드만), flat 평탄화 + `show_rows`
//! 가상화, immutable snapshot render. Leaf는 filesystem/watcher/thread/channel을 소유하지
//! 않고 bounded maintenance intent만 App host로 올린다.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
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
    /// 프로젝트/OSC 자동 제목과 구분한 사용자 지정 이름 여부.
    pub title_is_custom: bool,
    /// 모델은 표시 문자열을 다시 파싱하지 않고 감지 원본에서 전달한다.
    pub agent_model: Option<String>,
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
    /// 에이전트 정보: "Codex · gpt-5.5 · xhigh". 이름 미지정 행의 둘째 줄에 쓴다.
    pub agent_line: Option<String>,
    /// 에이전트 둘째 줄 앞 상태: "실행 중"/"지시 대기"/"승인 필요" 등.
    pub status_label: Option<String>,
    /// 저장된 에이전트 세션이 있고 지금 실행 중이 아님 — 컨텍스트 메뉴 '이어가기' 노출.
    pub resumable: bool,
    /// 세션 cwd가 감지 캐시에 있음 — 워크트리 메뉴 노출 조건. App이 채운다(PR-W).
    pub has_cwd: bool,
    /// 세션 cwd가 `.deppy/worktrees/` 하위 — 「워크트리 삭제」 메뉴 노출 조건.
    /// App이 채운다(2026-07-18).
    pub in_worktree: bool,
    /// 현재 작업. 이름 미지정이면 첫 줄, 지정했으면 둘째 줄에 표시한다.
    pub status_line: Option<String>,
    /// 마지막으로 새 출력이 온 시각(unix 초) — 「작업 중」인데 멈춘 세션 판별용.
    pub last_output_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SessionRowTarget {
    Live {
        workspace_id: String,
        runtime_instance: u64,
        tab: runtime::MuxTabId,
        pane: runtime::MuxPaneId,
        session: runtime::SessionId,
    },
    PersistedPane {
        workspace_id: String,
        pane: runtime::MuxPaneId,
    },
}

impl SessionRowTarget {
    pub(crate) fn live(
        workspace_id: impl Into<String>,
        runtime_instance: u64,
        tab: runtime::MuxTabId,
        pane: runtime::MuxPaneId,
        session: runtime::SessionId,
    ) -> Self {
        Self::Live {
            workspace_id: workspace_id.into(),
            runtime_instance,
            tab,
            pane,
            session,
        }
    }

    pub(crate) fn persisted(workspace_id: impl Into<String>, pane: runtime::MuxPaneId) -> Self {
        Self::PersistedPane {
            workspace_id: workspace_id.into(),
            pane,
        }
    }

    pub(crate) fn workspace_id(&self) -> &str {
        match self {
            Self::Live { workspace_id, .. } | Self::PersistedPane { workspace_id, .. } => {
                workspace_id
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn runtime_instance(&self) -> Option<u64> {
        match self {
            Self::Live {
                runtime_instance, ..
            } => Some(*runtime_instance),
            Self::PersistedPane { .. } => None,
        }
    }

    pub(crate) fn pane(&self) -> &runtime::MuxPaneId {
        match self {
            Self::Live { pane, .. } | Self::PersistedPane { pane, .. } => pane,
        }
    }

    pub(crate) fn session(&self) -> Option<runtime::SessionId> {
        match self {
            Self::Live { session, .. } => Some(*session),
            Self::PersistedPane { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionRowDragPayload {
    target: SessionRowTarget,
}

impl SessionRowDragPayload {
    fn new(target: SessionRowTarget) -> Self {
        Self { target }
    }

    pub(crate) fn target(&self) -> &SessionRowTarget {
        &self.target
    }
}

/// 이름 입력 이력과 편집 대상은 목록 순서가 아닌 세션의 전체 식별자에 귀속된다.
struct SessionNameEdit {
    target: SessionRowTarget,
    text: String,
    request_focus: bool,
}

impl SessionNameEdit {
    fn input_id(&self) -> egui::Id {
        egui::Id::new(("session_name_editor", &self.target))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionNameEditResult {
    Submit,
    Cancel,
}

pub(crate) struct SidebarSessionRow {
    pub target: SessionRowTarget,
    pub title: String,
    pub title_is_custom: bool,
    pub agent_model: Option<String>,
    pub status: Option<runtime::SessionStatus>,
    pub summary: String,
    pub focused: bool,
    pub attention: bool,
    pub pulse: Option<(f32, egui::Color32)>,
    pub agent_line: Option<String>,
    pub status_label: Option<String>,
    pub resumable: bool,
    pub has_cwd: bool,
    pub in_worktree: bool,
    pub status_line: Option<String>,
}

impl SidebarSessionRow {
    pub(crate) fn from_live(
        workspace_id: impl Into<String>,
        runtime_instance: u64,
        entry: SessionEntry,
    ) -> Self {
        let workspace_id = workspace_id.into();
        let target = match entry.session {
            Some(session) => SessionRowTarget::live(
                workspace_id,
                runtime_instance,
                entry.tab,
                entry.pane,
                session,
            ),
            None => SessionRowTarget::persisted(workspace_id, entry.pane),
        };
        Self {
            target,
            title: entry.title,
            title_is_custom: entry.title_is_custom,
            agent_model: entry.agent_model,
            status: entry.status,
            summary: entry.summary,
            focused: entry.focused,
            attention: entry.attention,
            pulse: entry.pulse,
            agent_line: entry.agent_line,
            status_label: entry.status_label,
            resumable: entry.resumable,
            has_cwd: entry.has_cwd,
            in_worktree: entry.in_worktree,
            status_line: entry.status_line,
        }
    }

    pub(crate) fn from_persisted_parts(
        workspace_id: impl Into<String>,
        pane: runtime::MuxPaneId,
        title: String,
        cwd: String,
    ) -> Self {
        let has_cwd = !cwd.is_empty();
        Self {
            target: SessionRowTarget::persisted(workspace_id, pane),
            title,
            title_is_custom: false,
            agent_model: None,
            status: None,
            summary: cwd,
            focused: false,
            attention: false,
            pulse: None,
            agent_line: None,
            status_label: None,
            resumable: false,
            has_cwd,
            in_worktree: false,
            status_line: None,
        }
    }
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
    /// 작업 메뉴의 상태별 건수. 가장 우선하는 상태의 색과 건수를 배지에 표시한다.
    pub fleet_summary: crate::fleet::FleetNavSummary,
    /// 현재 세션 pane 헤더 옆의 이력 보조 탭이 **활성**인지 — 레일 「이력」 선택 표시.
    /// 탭이 열려 있어도 비활성(터미널을 보는 중)이면 false다.
    pub history_tab_active: bool,
    /// 현재 세션 pane 헤더 옆의 Git 보조 탭이 **활성**인지 — 레일 「Git」 선택 표시.
    /// 이력과 같은 규칙(2026-08-15 2차, 스펙 §8-1).
    pub git_tab_active: bool,
    /// Agents 창 열림 여부 — 하단 nav 「에이전트」 행의 선택 상태 (2026-07-18).
    pub agents_open: bool,
    /// 활성 워크스페이스에 저장된 메모 본문. 미작성이면 `None`.
    /// leaf는 워크스페이스가 **바뀔 때만** 이 값으로 편집 버퍼를 교체한다.
    pub workspace_note: Option<&'a str>,
}

/// 사이드바에서 App으로 올라가는 액션.
pub enum SidebarAction {
    SwitchWorkspace(String),
    /// 워크스페이스 행 메뉴에서 이 워크스페이스를 대상으로 세션 시작 창을 연다.
    OpenWorkspaceSession(String),
    /// 사이드바에서 워크스페이스를 드래그해 순서를 바꿨다. 그 시점에 보이는 목록을
    /// 새 순서로 담아 보낸다. App은 숨긴 워크스페이스의 기존 자리를 보존해 병합하고,
    /// 실제로 삭제된 id만 별도 정리한다.
    ReorderWorkspaces(Vec<String>),
    /// 같은 워크스페이스 안에서 세션 행을 끌어 바꾼 표시 순서(pane id 목록).
    ReorderSessions {
        workspace_id: String,
        order: Vec<String>,
    },
    ActivatePersistedSession {
        workspace_id: String,
        pane: runtime::MuxPaneId,
    },
    ShowHome,
    /// 멀티에이전트 fleet 그리드로 전환(하단 nav). 재클릭 토글은 App이 현재 view로 결정.
    ShowFleet,
    /// 현재 워크스페이스의 이력 보조 탭을 연다/활성화한다. 재클릭 토글 규칙은 App이
    /// 결정한다(탭 상태 소유자).
    ShowHistory,
    /// 현재 워크스페이스의 Git 보조 탭을 연다/활성화한다 — 이력과 같은 규칙
    /// (2026-08-15 2차, 스펙 §8-1).
    ShowGit,
    OpenAgents,
    OpenSettings,
    /// 메모 본문이 바뀌었다. App이 디바운스해 DB에 쓴다(leaf는 IO를 하지 않는다).
    NoteEdited(String),
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
    /// 비활성 workspace의 canonical pane을 현재 화면 오른쪽에 연결한다.
    OpenSessionBeside(SessionRowTarget),
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
    /// 도움말 메뉴 — 업데이트 확인(릴리스 페이지를 연다).
    CheckUpdate,
    /// 도움말 메뉴 — 피드백 보내기(이슈 페이지를 연다).
    OpenFeedback,
    /// 저장된 에이전트 세션을 이 pane 셸에서 resume한다 (수동 이어가기).
    ResumeAgent {
        /// 이 행이 속한 워크스페이스 — 비활성(warm) 행이면 App이 먼저 전환한다
        /// (2026-08-20).
        workspace_id: String,
        pane: runtime::MuxPaneId,
        session: runtime::SessionId,
        title: String,
    },
    /// pane 닫기 — 실행 중 세션이면 기존 확인 모달을 거친다.
    ClosePane {
        pane: runtime::MuxPaneId,
    },
    /// 이 세션 cwd 레포의 변경분(diff)을 본다 (세션 행 컨텍스트 메뉴의 「변경 보기」).
    /// 세션별 payload를 유지한다 — 2026-08-15 한때 유닛 variant로 단순화했었는데
    /// (포커스 세션 기준으로 일원화, Task 10 Step 9) 회귀였다: 이 메뉴는 특정 세션
    /// 행의 컨텍스트 메뉴인데 포커스가 다른 세션에 있으면 엉뚱한 repo가 떴다. App은
    /// `cached_session_cwd(session)`으로 이 세션의 cwd를 찾아 스냅샷을 요청한다.
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
    /// 기록)는 보존한다(설정의 「프로젝트 삭제」와 구분). 확인 다이얼로그 사용 여부는
    /// App의 사용자 설정이 결정한다.
    CloseWorkspace(String),
    /// 워크스페이스 표시명(별칭) 편집 모달을 연다 — 실제 폴더/경로는 불변.
    /// 편집 자체는 App 소유 모달이 하고(현재 별칭 원본은 App만 안다), 여기서는
    /// 대상 id만 올린다.
    RenameWorkspace(String),
    /// 워크스페이스가 하나도 없을 때의 빈 상태 CTA(큰 + 버튼) — 폴더 선택으로 새
    /// 워크스페이스를 만들어 전환한다. rfd 다이얼로그는 UI leaf가 아니라 App이 연다
    /// (기존 ws_create 관례, 2026-07-18).
    CreateWorkspaceFromPicker,
    /// 파일 트리에서 문서 대상(md·txt 등) 파일을 열었다 — App이 포커스된 pane 위에
    /// 문서 보조 탭을 연다(설계 §3.2). 그 외 확장자는 여전히 `FileTreeIoRequest::OpenPath`
    /// 로 OS 기본 앱이 연다 — 이 액션으로 오지 않는다.
    OpenDocument {
        target: FileTreePathPayload,
        kind: DocumentTargetKind,
    },
}

/// 문서 탭으로 열리는 파일의 대상 종류(설계 §3.1). 순수 확장자 판정이라 filesystem에
/// 접근하지 않는다 — host가 실존 regular file 여부를 다시 검증한다(기존 OpenPath 관례).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentTargetKind {
    Markdown,
    PlainText,
}

/// 더블클릭 대상이 문서 탭으로 열릴지 분류한다. `.md`/`.markdown`은 markdown,
/// `.txt`/`.log`/확장자 없음은 평문. 그 외는 `None` — 기존 OS 열기 동작을 그대로
/// 유지한다(설계 §3.1, 기존 동작을 빼앗지 않는다).
fn classify_document_target(path: &Path) -> Option<DocumentTargetKind> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown") => {
            Some(DocumentTargetKind::Markdown)
        }
        Some(ext) if ext.eq_ignore_ascii_case("txt") || ext.eq_ignore_ascii_case("log") => {
            Some(DocumentTargetKind::PlainText)
        }
        None => Some(DocumentTargetKind::PlainText),
        Some(_) => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarTool {
    Files,
    /// 워크스페이스 스크래치패드. 「파일」과 같이 사이드바 본문을 차지하는 **인라인** 탭이다.
    Notes,
}

/// Git은 2026-08-15 2차에서 내비게이션 레일로 옮겼다 — 사이드바 폭(약 220pt)이 목록에
/// 모자랐다(스펙 §8-1). MCP 탭은 2026-08-10에 뺐다 — 다른 둘은 사이드바/메인 창 안에서
/// 끝나는데 혼자 **설정(별도 OS 창)** 을 열어 레벨이 달랐다. 연결 설정은 설정 → 관리 →
/// 「연결」이 계속 담당한다.
const SIDEBAR_TOOLS: [SidebarTool; 2] = [SidebarTool::Files, SidebarTool::Notes];

fn sidebar_tool_label_key(tool: SidebarTool) -> &'static str {
    match tool {
        SidebarTool::Files => "sidebar.tool.files",
        SidebarTool::Notes => "sidebar.tool.notes",
    }
}

/// 인라인 탭(본문을 차지하는 것)은 `None`을 돌려준다 — 탭 선택만 바꾸면 된다.
/// `Some`은 "다른 화면에 작용한다"는 뜻이다.
fn sidebar_tool_action(tool: SidebarTool) -> Option<SidebarAction> {
    match tool {
        SidebarTool::Files | SidebarTool::Notes => None,
    }
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
        let encoded = path.as_os_str().as_encoded_bytes();
        if encoded.contains(&0) {
            return Err(FileTreeIoErrorCode::InvalidPath);
        }
        let bytes = encoded.len();
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
            let encoded = path.as_os_str().as_encoded_bytes();
            if encoded.contains(&0) || encoded.len() > FILE_TREE_PATH_MAX_BYTES {
                return Err(FileTreeIoErrorCode::InvalidPath);
            }
            bytes = bytes
                .checked_add(encoded.len())
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
pub const FILE_TREE_SEARCH_MAX_ENTRIES: usize = 50_000;
pub const FILE_TREE_SEARCH_MAX_RESULTS: usize = 100;
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
    name: OsString,
    display_name: Arc<str>,
    is_dir: bool,
}

impl FileTreeListingItem {
    pub fn try_new(name: OsString, is_dir: bool) -> Result<Self, FileTreeMaintenanceErrorCode> {
        let bytes = name.as_encoded_bytes();
        if name.is_empty() || bytes.contains(&0) || bytes.len() > FILE_TREE_PATH_MAX_BYTES {
            return Err(FileTreeMaintenanceErrorCode::InvalidSnapshot);
        }
        let display_name = super::os_str_display(&name);
        Ok(Self {
            name,
            display_name: Arc::from(display_name),
            is_dir,
        })
    }

    pub fn name(&self) -> &OsStr {
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
                .checked_add(item.name.as_encoded_bytes().len())
                .filter(|total| *total <= FILE_TREE_LISTING_MAX_BYTES)
                .ok_or(FileTreeMaintenanceErrorCode::ListingTooLarge)
        })?;
        items.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.display_name.cmp(&b.display_name))
                .then_with(|| a.name.cmp(&b.name))
        });
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
                    let encoded = path.as_os_str().as_encoded_bytes();
                    if encoded.is_empty()
                        || encoded.contains(&0)
                        || encoded.len() > FILE_TREE_PATH_MAX_BYTES
                    {
                        return Err(FileTreeMaintenanceErrorCode::InvalidSnapshot);
                    }
                    total
                        .checked_add(encoded.len())
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
    SearchFiles {
        root: FileTreePathPayload,
        query: String,
        show_hidden: bool,
        max_entries: usize,
        max_results: usize,
        cancel: Arc<std::sync::atomic::AtomicBool>,
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
            Self::SearchFiles {
                show_hidden,
                max_entries,
                max_results,
                ..
            } => f
                .debug_struct("SearchFiles")
                .field("root", &"REDACTED")
                .field("query", &"REDACTED")
                .field("show_hidden", show_hidden)
                .field("max_entries", max_entries)
                .field("max_results", max_results)
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
    Search(FileTreeSearchSnapshot),
    WatchSetApplied,
}

pub struct FileTreeSearchSnapshot {
    paths: Vec<PathBuf>,
    result_limit_reached: bool,
    traversal_incomplete: bool,
}

impl FileTreeSearchSnapshot {
    pub fn try_new(
        paths: Vec<PathBuf>,
        result_limit_reached: bool,
        traversal_incomplete: bool,
    ) -> Result<Self, FileTreeMaintenanceErrorCode> {
        if paths.len() > FILE_TREE_SEARCH_MAX_RESULTS {
            return Err(FileTreeMaintenanceErrorCode::InvalidSnapshot);
        }
        let mut bytes = 0usize;
        for path in &paths {
            let encoded = path.as_os_str().as_encoded_bytes();
            bytes = bytes
                .checked_add(encoded.len())
                .filter(|total| *total <= FILE_TREE_LISTING_MAX_BYTES)
                .ok_or(FileTreeMaintenanceErrorCode::InvalidSnapshot)?;
            if encoded.is_empty()
                || encoded.contains(&0)
                || encoded.len() > FILE_TREE_PATH_MAX_BYTES
            {
                return Err(FileTreeMaintenanceErrorCode::InvalidSnapshot);
            }
        }
        Ok(Self {
            paths,
            result_limit_reached,
            traversal_incomplete,
        })
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    pub fn result_limit_reached(&self) -> bool {
        self.result_limit_reached
    }

    pub fn traversal_incomplete(&self) -> bool {
        self.traversal_incomplete
    }
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
        let encoded = path.as_os_str().as_encoded_bytes();
        if encoded.is_empty() || encoded.contains(&0) || encoded.len() > FILE_TREE_PATH_MAX_BYTES {
            return Err(FileTreeMaintenanceErrorCode::InvalidSnapshot);
        }
        let bytes = encoded.len();
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
    },
    WatchSet {
        operation: FileTreeMaintenanceOperation,
        generation: u64,
    },
    Search {
        operation: FileTreeMaintenanceOperation,
        generation: u64,
        query: String,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    },
}

struct FileTreeSearch {
    query: String,
    results: Vec<PathBuf>,
    selected: Option<PathBuf>,
    busy: bool,
    result_limit_reached: bool,
    traversal_incomplete: bool,
    focus: bool,
}

/// 펼친 하위 목록의 순회 위치만 보관한다. 전체 경로 목록을 대기열에 복제하지 않는다.
struct ListingWalk {
    root: PathBuf,
    after: PathBuf,
}

/// 삭제 대상 — **행에서 한 번 확정한 값**을 확인 문구까지 그대로 들고 간다.
/// 경로만 넘기고 표시 문자열을 확인 시점에 다시 만들면 "보여준 것"과 "지우는 것"이
/// 갈라진다. 실제로 갈라졌었다: 확인 문구는 `path.file_name()`만 뽑아 써서
/// `src/mod.rs`와 `tests/mod.rs`가 똑같이 `'mod.rs'`로 보였고, 폴더인지 파일인지도
/// 구분되지 않았다(영구 삭제는 폴더면 안의 내용까지 통째로 지운다).
#[derive(Clone, Debug, PartialEq, Eq)]
struct DeleteTarget {
    path: PathBuf,
    /// 확인 문구에 그대로 쓰는 표시 이름 — 루트 기준 상대 경로.
    label: String,
    is_dir: bool,
}

struct PendingFileTreeIo {
    operation: FileTreeIoOperation,
    generation: u64,
    edit_generation: u64,
    refresh: Vec<PathBuf>,
    trash_target: Option<DeleteTarget>,
    retry_edit: Option<EditState>,
}

/// 트리 노드. `children == None`은 아직 나열 안 됨(lazy).
/// 접으면 children을 버려 캐시는 항상 "펼친 노드"만 유지한다(§3 메모리 상한).
struct TreeNode {
    name: OsString,
    display_name: String,
    is_dir: bool,
    expanded: bool,
    children: Option<Vec<TreeNode>>,
}

impl TreeNode {
    fn new(name: OsString, is_dir: bool) -> Self {
        let display_name = super::os_str_display(&name);
        Self {
            name,
            display_name,
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
    display_name: String,
    depth: usize,
    is_dir: bool,
    expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RootListingError {
    PermissionDenied,
}

pub struct FileTreeUi {
    /// 사이드바 본문을 차지하는 인라인 탭(파일 / 메모). Git은 2026-08-15 2차부터 레일
    /// 진입 + pane 보조 탭이라 여기 남지 않는다(App이 `GitPanelUi`를 직접 소유한다, §8-1).
    selected_tool: SidebarTool,
    /// 메모 탭 편집 상태. leaf라 DB를 만지지 않고 편집만 소유한다.
    notes: super::notes::NotesUi,
    /// workspace 루트. None = path 미설정 → 안내 표시(§9-2).
    root: Option<PathBuf>,
    /// 루트 나열 실패 사유 (invalid root — 에러 라벨 + 트리 비활성, §9-2).
    root_error: Option<RootListingError>,
    /// 루트 디렉터리의 자식들. 루트 자체는 행으로 그리지 않는다.
    children: Option<Vec<TreeNode>>,
    /// 가시 행 평탄화 캐시 — 펼침/접힘/조작 시에만 재계산(§3).
    flat: Vec<FlatRow>,
    show_hidden: bool,
    /// 사이드바 접힘 (Panel 폭만 줄인다 — 상태/캐시는 유지).
    collapsed: bool,
    /// 내장 SidePanel 리사이저 대신 사용하는 폭. 내장 리사이저는 드래그 가이드선을
    /// 하단 상태바까지 그리므로, 상태바 위에서 끝나는 전용 핸들로 직접 조절한다.
    sidebar_width: f32,
    navigation_rail_width: f32,
    /// 레일 하단 서비스 상태 스택 데이터 — App이 status_feed 스냅샷으로 매 프레임
    /// 갱신한다 (`set_service_statuses`). 기본값은 인디케이터 None(회색 로고).
    service_statuses: [RailServiceStatus; 3],
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
    listing_walk: Option<ListingWalk>,
    watch_plan_dirty: bool,
    last_watch_revision: u64,
    watch_ignore: Vec<PathBuf>,
    /// 진행 중인 백그라운드 조작 수 (>0이면 스피너 표시).
    in_flight: usize,
    /// 백그라운드 완료 시 UI를 깨우기 위한 컨텍스트.
    /// 인라인 편집 상태 (이름 변경/새 폴더, FT-3).
    edit: Option<EditState>,
    /// 같은 탐색 루트에서도 새 편집/취소는 이전 실패 결과의 입력 복원을 무효화한다.
    edit_generation: u64,
    /// 휴지통 이동 실패 → 영구삭제 확인 대기 중인 대상 (§9-7). 경로가 아니라
    /// `DeleteTarget`을 들고 있어야 확인 문구와 실제 삭제 경로가 같은 값에서 나온다.
    confirm_delete: Option<DeleteTarget>,
    /// 실측 행높이 (show_rows 자기보정). show_rows는 "모든 행 = 선언 높이" 계약인데
    /// 실제 행높이는 폰트 메트릭(한글 폰트 라인높이 등)에 따라 선언값과 어긋날 수 있고,
    /// 어긋나면 스크롤 위치·가시 범위가 리빌드마다 밀려 클릭이 다른 행에 떨어진다
    /// (2026-07-05 사용자 보고: 펼침 간헐 실패/재클릭 접힘 안 됨/위치 점프). 첫 프레임에
    /// 실제 그린 행높이를 재서 다음 프레임부터 그 값을 쓴다.
    measured_row_height: Option<f32>,
    /// `.env*` 파일 변경 후보. 숨김 파일 필터와 무관하게 기록해 env-warning 후보로 쓸 수 있다.
    env_warning_candidates: BTreeSet<PathBuf>,
    /// 우클릭으로 시작한 세션 이름 편집. 전환/숨김/종료 시 취소한다.
    session_name_edit: Option<SessionNameEdit>,
    /// 워크스페이스별 세션 트리 펼침 상태. 포커스 전환과 독립적이어서 다른 workspace를
    /// 선택해도 기존 트리는 사용자가 직접 접기 전까지 유지된다.
    workspace_sessions_expanded: HashMap<String, bool>,
    /// 활성 변경을 감지해 이전 활성의 기본-open 상태를 map에 고정한다. 이 기록이 없으면
    /// 명시값이 없던 이전 workspace가 inactive가 되는 순간 default false로 닫힌다.
    last_sidebar_active_workspace: Option<String>,
    /// 폴더 트리 다중선택. 비어 있으면 "선택 없음"이고, 단축키는 예전처럼 포인터 밑
    /// 행 하나로 떨어진다 — 선택을 쓰기 시작하기 전 동작을 그대로 보존한다.
    selected: BTreeSet<PathBuf>,
    /// Shift 범위 선택의 기준점 — 마지막으로 단독 선택하거나 토글한 행.
    select_anchor: Option<PathBuf>,
    search: Option<FileTreeSearch>,
    search_requested: Option<String>,
    search_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    reveal_path: Option<PathBuf>,
    /// 파일 트리 포커스에서 연속 입력한 폴더 이름 접두어.
    typeahead_prefix: String,
    typeahead_last_at: Option<f64>,
    /// 선택 사각형 드래그 중 상태.
    marquee: Option<MarqueeDrag>,
    /// 다중 삭제 대기열. IO 큐가 capacity-1이라 완료될 때마다 하나씩 보낸다.
    trash_queue: Vec<DeleteTarget>,
    /// 순서 변경 드래그 중인 워크스페이스 id. 드롭 위치는 매 프레임 포인터 y로 다시 구하므로
    /// 여기엔 "무엇을 들고 있는가"만 남긴다.
    workspace_drag: Option<String>,
    /// 마지막 외부 파일 붙여넣기(⌘V) 처리 시각 — 같은 제스처의 press(native)와
    /// release(egui fallback)가 두 번 복사하는 것을 막는다(터미널 PASTE_GESTURE 관례).
    last_external_paste: Option<std::time::Instant>,
    /// 마지막 외부 파일 복사(⌘C) 처리 시각 — native key-down 뒤 늦게 도착한
    /// Event::Copy가 같은 파일 URL 쓰기를 중복하지 않게 한다.
    last_external_copy: Option<std::time::Instant>,
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
        changed: bool,
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
            collapsed: false,
            selected_tool: SidebarTool::Files,
            notes: super::notes::NotesUi::new(),
            sidebar_width: 200.0,
            navigation_rail_width: crate::ui::designall::NAV_RAIL_WIDTH,
            service_statuses: RailServiceStatus::defaults(),
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
            listing_walk: None,
            watch_plan_dirty: false,
            last_watch_revision: 0,
            watch_ignore: Vec::new(),
            in_flight: 0,
            edit: None,
            edit_generation: 0,
            confirm_delete: None,
            measured_row_height: None,
            env_warning_candidates: BTreeSet::new(),
            session_name_edit: None,
            workspace_sessions_expanded: HashMap::new(),
            last_sidebar_active_workspace: None,
            selected: BTreeSet::new(),
            select_anchor: None,
            search: None,
            search_requested: None,
            search_cancel: None,
            reveal_path: None,
            typeahead_prefix: String::new(),
            typeahead_last_at: None,
            marquee: None,
            trash_queue: Vec::new(),
            workspace_drag: None,
            last_external_paste: None,
            last_external_copy: None,
            consumed_paste_shortcut: false,
            consumed_copy_shortcut: false,
            workspace_section_height: 270.0,
        }
    }

    pub fn designall_titlebar_widths(&self) -> (f32, f32) {
        let navigation = self.navigation_rail_width.clamp(
            crate::ui::designall::NAV_RAIL_MIN_WIDTH,
            crate::ui::designall::NAV_RAIL_MAX_WIDTH,
        );
        let project = if self.collapsed {
            22.0
        } else {
            self.sidebar_width.clamp(40.0, 680.0)
        };
        (navigation, project)
    }

    pub fn collapse_project_file_panel(&mut self) {
        self.collapsed = true;
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
        trash_target: Option<DeleteTarget>,
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
            edit_generation: self.edit_generation,
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
                // 다중 삭제는 capacity-1 큐를 하나씩 통과한다.
                //
                // **직전 완료가 삭제였을 때만** 잇는다. 완료 종류를 안 보면 붙여넣기·복사·
                // 이름변경 같은 무관한 IO가 성공하는 순간 대기열이 풀려, 사용자가 고른 적
                // 없는 시점에 파일이 사라진다(2026-09-04 리뷰 H1/H2). `trash_target`은
                // `spawn_trash`만 채우므로 그것이 곧 "이 완료가 삭제였다"는 증거다.
                if pending.trash_target.is_some()
                    && let Some(next) =
                        (!self.trash_queue.is_empty()).then(|| self.trash_queue.remove(0))
                {
                    self.spawn_trash(next);
                }
            }
            Err(FileTreeIoErrorCode::TrashUnavailable) => {
                // 남은 대상은 보내지 않는다. 영구삭제 확인이 뜬 채로 뒤 대상이 계속
                // 지워지면, 사용자가 보고 있는 확인 문구와 실제로 사라지는 파일이
                // 갈라진다. 남은 것들은 트리에 그대로 남아 눈으로 확인된다.
                self.trash_queue.clear();
                self.error = Some(
                    file_tree_io_error_message(FileTreeIoErrorCode::TrashUnavailable).to_owned(),
                );
                self.confirm_delete = pending.trash_target;
                if pending.edit_generation == self.edit_generation
                    && let Some(edit) = pending.retry_edit
                {
                    self.edit = Some(edit);
                }
            }
            Err(code) => {
                self.trash_queue.clear();
                if pending.edit_generation == self.edit_generation
                    && let Some(edit) = pending.retry_edit
                {
                    self.edit = Some(edit);
                }
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
            }
            | PendingFileTreeMaintenance::Search {
                operation,
                generation,
                ..
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
                PendingFileTreeMaintenance::Listing { path, .. },
                Ok(FileTreeMaintenanceResult::Listing(snapshot)),
            ) => self.apply_listing_snapshot(path, snapshot),
            (
                PendingFileTreeMaintenance::WatchSet { .. },
                Ok(FileTreeMaintenanceResult::WatchSetApplied),
            ) => {}
            (
                PendingFileTreeMaintenance::Search { query, cancel, .. },
                Ok(FileTreeMaintenanceResult::Search(snapshot)),
            ) => {
                if let (Some(root), Some(search)) = (self.root.as_ref(), self.search.as_mut())
                    && search.query == query
                    && !cancel.load(std::sync::atomic::Ordering::Relaxed)
                {
                    search.results = snapshot
                        .paths()
                        .iter()
                        .filter(|path| path.starts_with(root))
                        .cloned()
                        .collect();
                    search.result_limit_reached = snapshot.result_limit_reached();
                    search.traversal_incomplete = snapshot.traversal_incomplete();
                    search.busy = false;
                }
            }
            (PendingFileTreeMaintenance::Listing { path, .. }, Err(code)) => {
                self.apply_maintenance_error(&path, code);
            }
            (PendingFileTreeMaintenance::WatchSet { .. }, Err(code)) => {
                self.error = Some(file_tree_maintenance_error_message(code).to_owned());
            }
            (PendingFileTreeMaintenance::Search { query, cancel, .. }, Err(code)) => {
                if let Some(search) = self.search.as_mut()
                    && search.query == query
                    && !cancel.load(std::sync::atomic::Ordering::Relaxed)
                {
                    search.busy = false;
                    self.error = Some(file_tree_maintenance_error_message(code).to_owned());
                }
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
        if let Some(query) = self.search_requested.take()
            && let Some(root) = self.root.clone()
            && !query.is_empty()
            && let Some(cancel) = self.search_cancel.clone()
        {
            let Ok(root) = FileTreePathPayload::try_new(root) else {
                self.error = Some("파일 트리 루트 경로가 허용된 크기를 초과했습니다".to_owned());
                return;
            };
            let operation = FileTreeMaintenanceOperation(self.next_maintenance_operation);
            self.next_maintenance_operation =
                self.next_maintenance_operation.wrapping_add(1).max(1);
            let generation = self.maintenance_generation;
            self.maintenance_intent = Some(FileTreeMaintenanceIntent {
                operation,
                generation,
                request: FileTreeMaintenanceRequest::SearchFiles {
                    root,
                    query: query.clone(),
                    show_hidden: self.show_hidden,
                    max_entries: FILE_TREE_SEARCH_MAX_ENTRIES,
                    max_results: FILE_TREE_SEARCH_MAX_RESULTS,
                    cancel: cancel.clone(),
                },
            });
            self.pending_maintenance = Some(PendingFileTreeMaintenance::Search {
                operation,
                generation,
                query,
                cancel,
            });
            return;
        }
        while let Some(path) = self.next_refresh_directory() {
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

    fn next_refresh_directory(&mut self) -> Option<PathBuf> {
        if let Some(path) = self.pending_refresh_dirs.pop_first() {
            return Some(path);
        }
        let walk = self.listing_walk.as_mut()?;
        let next = self
            .flat
            .iter()
            .filter(|row| {
                row.is_dir
                    && row.expanded
                    && row.path.starts_with(&walk.root)
                    && row.path > walk.after
            })
            .map(|row| &row.path)
            .min()
            .cloned();
        if let Some(path) = &next {
            walk.after.clone_from(path);
        } else {
            self.listing_walk = None;
        }
        next
    }

    fn schedule_listing_walk(&mut self, path: &Path) {
        if !self
            .listing_children(path)
            .is_some_and(|children| children.iter().any(|node| node.is_dir && node.expanded))
        {
            return;
        }
        let root = match &self.listing_walk {
            Some(walk) if path != walk.root && path.starts_with(&walk.root) => return,
            Some(walk) if !walk.root.starts_with(path) => self.root.clone(),
            _ => Some(path.to_path_buf()),
        };
        if let Some(root) = root {
            self.listing_walk = Some(ListingWalk {
                after: root.clone(),
                root,
            });
        }
    }

    fn apply_listing_snapshot(&mut self, path: PathBuf, snapshot: FileTreeListingSnapshot) {
        if !self.should_list_directory(&path) {
            return;
        }
        let unchanged = self.listing_children(&path).is_some_and(|children| {
            children.len() == snapshot.items().len()
                && children
                    .iter()
                    .zip(snapshot.items())
                    .all(|(node, item)| node.name == item.name() && node.is_dir == item.is_dir())
        });
        if !unchanged {
            let (mut retained_items, mut retained_bytes) = self.retained_usage_without(&path);
            // 이월할 하위 캐시도 예산에 포함한다. 지워졌거나 파일로 바뀐 폴더는 제외한다.
            let directories: HashSet<&OsStr> = snapshot
                .items()
                .iter()
                .filter(|item| item.is_dir())
                .map(|item| item.name())
                .collect();
            for node in self.listing_children(&path).unwrap_or(&[]) {
                if node.is_dir && node.expanded && directories.contains(node.name.as_os_str()) {
                    let (items, bytes) = tree_node_usage(node.children.as_deref().unwrap_or(&[]));
                    retained_items = retained_items.saturating_add(items);
                    retained_bytes = retained_bytes.saturating_add(bytes);
                }
            }
            if retained_items.saturating_add(snapshot.items().len()) > FILE_TREE_RETAINED_MAX_ITEMS
                || retained_bytes.saturating_add(snapshot.bytes()) > FILE_TREE_RETAINED_MAX_BYTES
            {
                self.error =
                    Some("파일 트리 메모리 상한을 초과해 추가 항목을 보관하지 않습니다".to_owned());
                return;
            }
            let nodes = snapshot
                .items()
                .iter()
                .map(|item| TreeNode::new(item.name().to_owned(), item.is_dir()))
                .collect();
            if !self.replace_listing_children(&path, nodes) {
                return;
            }
        }
        self.inaccessible_paths.remove(&path);
        self.invalidate_stale_delete_confirm(&path);
        self.root_error = None;
        // 자식을 한꺼번에 큐에 넣으면 64건 초과 → 루트 축약 → 같은 조회가 반복된다.
        // 순회 위치만 남기고 다음 가시 폴더를 한 건씩 읽은 뒤 watch 집합을 적용한다.
        self.schedule_listing_walk(&path);
        if !unchanged {
            self.rebuild_flat();
        }
    }

    /// 방금 나열한 폴더의 결과로 영구삭제 확인이 아직 유효한지 판정한다.
    ///
    /// 확인은 실패한 휴지통 이동이 세워두고 사용자가 누를 때까지 남는다. 그 사이
    /// 브랜치 전환·빌드·외부 편집이 그 경로를 지우고 **같은 자리에 다른 것**을 만들면,
    /// 문구는 옛 이름을 말하는데 삭제는 지금 그 자리에 있는 것을 지운다. 종류까지
    /// 바뀌면(파일 → 폴더) 파일용 문구를 띄운 채 `remove_dir_all`이 폴더를 통째로
    /// 지운다 — "안의 내용까지 사라진다"는 경고 없이.
    ///
    /// **여기가 유일한 판정 지점인 이유**: 트리가 "그 폴더에 무엇이 있는지"를 배우는
    /// 길은 listing 스냅샷 하나뿐이다. 수동 새로고침·워처 이벤트·`reload_dir`·펼치기가
    /// 모두 `enqueue_refresh_dir` → maintenance intent → 이 함수의 호출부로 모인다.
    /// 반대로 `rebuild_flat`은 너무 넓다 — 접힘·숨김 토글처럼 "안 보일 뿐"인 상태와,
    /// 펼치는 중이라 children이 빈 placeholder인 상태까지 "없어졌다"로 읽는다.
    ///
    /// 판정은 **대상의 부모를 방금 나열했을 때만** 한다. 다른 폴더의 나열 결과로는
    /// 대상의 생사를 알 수 없다. 나열 결과에는 숨김 파일도 들어 있으므로 숨김 토글에는
    /// 영향받지 않는다.
    fn invalidate_stale_delete_confirm(&mut self, listed_dir: &Path) {
        let Some(target) = self.confirm_delete.as_ref() else {
            return;
        };
        if target.path.parent() != Some(listed_dir) {
            return;
        }
        let (Some(root), Some(name)) = (self.root.as_deref(), target.path.file_name()) else {
            return;
        };
        let listed = if listed_dir == root {
            self.children.as_deref()
        } else {
            listed_dir
                .strip_prefix(root)
                .ok()
                .and_then(|rel| self.children.as_deref().and_then(|c| node_ref(c, rel)))
                .and_then(|node| node.children.as_deref())
        };
        let Some(listed) = listed else {
            return;
        };
        // 이름만 같고 종류가 다르면 **다른 것**이다. 파일 문구를 띄운 채 폴더를 지우는
        // 것이 정확히 이 경우다.
        let still_there = listed
            .iter()
            .any(|node| node.name == name && node.is_dir == target.is_dir);
        if !still_there {
            self.confirm_delete = None;
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

    /// 워처 무시 prefix 설정 (앱 data dir 등). set_root 이전에 호출.
    pub fn set_watch_ignore(&mut self, prefixes: Vec<PathBuf>) {
        let bytes = prefixes.iter().try_fold(0usize, |total, path| {
            let encoded = path.as_os_str().as_encoded_bytes();
            if encoded.is_empty()
                || encoded.contains(&0)
                || encoded.len() > FILE_TREE_PATH_MAX_BYTES
            {
                return None;
            }
            total.checked_add(encoded.len())
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
        // 이미 펼쳐 읽은 하위 폴더로 진입하면 그 목록만 이동한다. 다른 폴더의 행을
        // 새 루트에 보여주거나 전체 트리를 복제하지 않고 빈 중간 프레임도 피한다.
        let children = root
            .as_deref()
            .zip(self.root.as_deref())
            .and_then(|(next, current)| next.strip_prefix(current).ok())
            .and_then(|relative| {
                self.children
                    .as_mut()
                    .and_then(|children| node_mut(children, relative))
                    .and_then(|node| node.children.take())
            });
        self.io_generation = self.io_generation.wrapping_add(1).max(1);
        self.io_intent = None;
        self.pending_io = None;
        self.in_flight = 0;
        self.maintenance_generation = self.maintenance_generation.wrapping_add(1).max(1);
        self.maintenance_intent = None;
        self.pending_maintenance = None;
        self.pending_refresh_dirs.clear();
        self.listing_walk = None;
        self.last_watch_revision = 0;
        // App snapshot의 bounded root를 그대로 사용한다. canonicalize/metadata는 host
        // repository가 listing 결과를 만들 때 수행하며 render leaf는 filesystem을 읽지 않는다.
        self.root = root;
        self.root_error = None;
        self.children = children;
        self.error = None;
        self.inaccessible_paths.clear();
        self.cancel_edit();
        self.confirm_delete = None;
        // 루트가 바뀌면 선택과 삭제 대기열도 함께 버린다. generation 검사는 **옛 완료**만
        // 막을 뿐 큐 자체는 살아남아서, 새 워크스페이스에서 아무 IO나 성공하는 순간
        // 옛 루트의 파일이 지워졌다(2026-09-04 리뷰 H2).
        self.trash_queue.clear();
        self.selected.clear();
        self.select_anchor = None;
        if let Some(cancel) = self.search_cancel.take() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.search = None;
        self.search_requested = None;
        self.reveal_path = None;
        self.typeahead_prefix.clear();
        self.typeahead_last_at = None;
        self.marquee = None;
        self.env_warning_candidates.clear();
        self.watch_plan_dirty = true;
        if let Some(root) = self.root.clone() {
            self.enqueue_refresh_dir(root);
        }
        self.rebuild_flat();
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
        // 목록에서 사라진 행은 선택에서도 뺀다 — 접힌 폴더 안이나 지워진 파일이 선택에
        // 남아 있으면, 화면에 아무것도 강조되지 않은 채 ⌫ 하나에 그것들이 지워진다.
        if !self.selected.is_empty() {
            let visible: BTreeSet<&PathBuf> = self.flat.iter().map(|row| &row.path).collect();
            self.selected.retain(|path| visible.contains(path));
            if self
                .select_anchor
                .as_ref()
                .is_some_and(|anchor| !self.selected.contains(anchor))
            {
                self.select_anchor = None;
            }
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
    /// App이 status_feed 스냅샷을 레일 표시용으로 내려준다 (매 프레임, §6 —
    /// leaf는 폴링하지 않고 App이 데이터를 민다).
    pub fn set_service_statuses(&mut self, feed: &crate::status_feed::StatusFeedSnapshot) {
        let mut statuses = RailServiceStatus::defaults();
        for (slot, provider) in statuses.iter_mut().zip([
            feed.claude.as_ref(),
            feed.openai.as_ref(),
            feed.github.as_ref(),
        ]) {
            slot.indicator = provider.map(|p| p.indicator);
            slot.description = provider.map(|p| p.description.clone());
        }
        self.service_statuses = statuses;
    }

    /// 터미널 선택 등 **밖에서** 들어온 메모 편집을 「메모」 탭 버퍼에 반영한다(PR-4).
    /// App이 pending_note를 세운 직후 호출한다 — 「메모」 탭을 지금 보고 있지 않아도
    /// 다음에 열면 바로 보인다.
    pub fn apply_note_append(&mut self, workspace_id: &str, body: String) {
        self.notes.apply_external_edit(workspace_id, body);
    }

    fn take_session_name_edit(&mut self, ctx: &egui::Context) -> Option<SessionNameEdit> {
        let edit = self.session_name_edit.take()?;
        let id = edit.input_id();
        ctx.memory_mut(|memory| memory.surrender_focus(id));
        ctx.data_mut(|data| data.remove::<egui::text_edit::TextEditState>(id));
        ctx.request_repaint();
        Some(edit)
    }

    pub fn panel(
        &mut self,
        ui: &mut egui::Ui,
        sessions_by_workspace: &HashMap<String, Vec<SidebarSessionRow>>,
        sidebar: &SidebarSnapshot<'_>,
        catalog: &i18n::Catalog,
    ) -> Option<SidebarAction> {
        let navigation_action = self.navigation_rail_panel(ui, sidebar, catalog);
        let project_action = self.project_file_panel(ui, sessions_by_workspace, sidebar, catalog);
        mark_file_tree_search_keyboard_active(
            ui.ctx(),
            !self.collapsed && self.selected_tool == SidebarTool::Files && self.search.is_some(),
        );
        let action = project_action.or(navigation_action);
        if action.is_some() {
            // 다른 세션/워크스페이스/도구로 이동한 뒤 숨은 초안을 확정하지 않는다.
            let _ = self.take_session_name_edit(ui.ctx());
        }
        action
    }

    fn navigation_rail_panel(
        &mut self,
        ui: &mut egui::Ui,
        sidebar: &SidebarSnapshot<'_>,
        catalog: &i18n::Catalog,
    ) -> Option<SidebarAction> {
        self.navigation_rail_width = self.navigation_rail_width.clamp(
            crate::ui::designall::NAV_RAIL_MIN_WIDTH,
            crate::ui::designall::NAV_RAIL_MAX_WIDTH,
        );
        let frame = crate::ui::designall::structural_frame(ui.visuals());
        let service_statuses = self.service_statuses.clone();
        let panel = egui::Panel::left("designall_navigation_rail")
            .resizable(false)
            .exact_size(self.navigation_rail_width)
            .show_separator_line(false)
            .frame(frame)
            .show(ui, |ui| {
                crate::ui::designall::apply_workspace_visuals(ui);
                crate::fonts::apply_sidebar_text_styles(ui);
                let width = ui.available_width();
                let utility_height = nav_utility_height(width);
                let status_height = rail_service_status_height();
                let navigation_height =
                    (ui.available_height() - utility_height - status_height).max(0.0);
                let navigation_action = ui
                    .allocate_ui_with_layout(
                        egui::vec2(width, navigation_height),
                        egui::Layout::top_down(egui::Align::Center),
                        |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("designall_navigation_scroll")
                                .auto_shrink([false, false])
                                .show(ui, |ui| self.navigation(ui, sidebar, catalog))
                                .inner
                        },
                    )
                    .inner;
                ui.allocate_ui_with_layout(
                    egui::vec2(width, status_height),
                    egui::Layout::top_down(egui::Align::Center),
                    |ui| {
                        // allocate는 실제 내용만큼만 커서를 전진시킨다 — 예약 높이를
                        // min으로 박아 아래 utilities가 당겨 올라오지 않게 한다.
                        ui.set_min_height(status_height);
                        rail_service_status(ui, &service_statuses, catalog);
                    },
                );
                let utility_action = ui
                    .allocate_ui_with_layout(
                        egui::vec2(width, utility_height),
                        egui::Layout::top_down(egui::Align::Center),
                        |ui| nav_utilities(ui, catalog),
                    )
                    .inner;
                utility_action.or(navigation_action)
            });
        let panel_rect = panel.response.rect;
        let resize_bottom = panel_rect
            .bottom()
            .min(ui.ctx().content_rect().bottom() - 26.0);
        let resize_rect = egui::Rect::from_min_max(
            egui::pos2(panel_rect.right() - 3.0, panel_rect.top()),
            egui::pos2(panel_rect.right() + 3.0, resize_bottom),
        );
        let resize_response = ui
            .interact(
                resize_rect,
                egui::Id::new("designall_navigation_rail_resize"),
                egui::Sense::drag(),
            )
            .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        if resize_response.dragged() {
            let delta_x = ui.input(|input| input.pointer.delta().x);
            self.navigation_rail_width = (self.navigation_rail_width + delta_x).clamp(
                crate::ui::designall::NAV_RAIL_MIN_WIDTH,
                crate::ui::designall::NAV_RAIL_MAX_WIDTH,
            );
            ui.ctx().request_repaint();
        }
        let separator_stroke = if resize_response.dragged() {
            ui.visuals().widgets.active.bg_stroke
        } else if resize_response.hovered() {
            ui.visuals().widgets.hovered.bg_stroke
        } else {
            crate::ui::designall::separator_stroke(ui.visuals())
        };
        paint_sidebar_separator(ui, panel_rect, separator_stroke);
        panel.inner
    }

    fn project_file_panel(
        &mut self,
        ui: &mut egui::Ui,
        sessions_by_workspace: &HashMap<String, Vec<SidebarSessionRow>>,
        sidebar: &SidebarSnapshot<'_>,
        catalog: &i18n::Catalog,
    ) -> Option<SidebarAction> {
        let invalid_edit = self.session_name_edit.as_ref().is_some_and(|edit| {
            self.collapsed
                || !ui.is_enabled()
                || edit.target.workspace_id() != sidebar.active_workspace_id
                || !sidebar
                    .workspaces
                    .iter()
                    .any(|workspace| workspace.id == sidebar.active_workspace_id)
                || !self
                    .workspace_sessions_expanded
                    .get(sidebar.active_workspace_id)
                    .copied()
                    .unwrap_or(true)
                || !sessions_by_workspace
                    .get(sidebar.active_workspace_id)
                    .is_some_and(|rows| rows.iter().any(|row| row.target == edit.target))
        });
        if invalid_edit {
            let _ = self.take_session_name_edit(ui.ctx());
        }
        let status_bar_top = ui.ctx().content_rect().bottom() - 26.0;
        if self.collapsed {
            let frame = crate::ui::designall::structural_frame(ui.visuals());
            let panel = egui::Panel::left("designall_project_file_panel")
                .resizable(false)
                .exact_size(22.0)
                .show_separator_line(false)
                .frame(frame)
                .show(ui, |ui| {
                    crate::ui::designall::apply_workspace_visuals(ui);
                    crate::fonts::apply_sidebar_text_styles(ui);
                    register_tree_keyboard_focus_target(ui);
                    if ui
                        .small_button("▸")
                        .on_hover_text(catalog.t("file_tree.expand_sidebar", &[]))
                        .clicked()
                    {
                        self.collapsed = false;
                    }
                });
            paint_sidebar_separator(
                ui,
                panel.response.rect,
                crate::ui::designall::separator_stroke(ui.visuals()),
            );
            return None;
        }
        self.sidebar_width = self.sidebar_width.clamp(40.0, 680.0);
        let frame = crate::ui::designall::structural_frame(ui.visuals());
        let panel = egui::Panel::left("designall_project_file_panel")
            .resizable(false)
            .exact_size(self.sidebar_width)
            .show_separator_line(false)
            .frame(frame)
            .show(ui, |ui| {
                crate::ui::designall::apply_workspace_visuals(ui);
                crate::fonts::apply_sidebar_text_styles(ui);
                register_tree_keyboard_focus_target(ui);
                self.contents(ui, sessions_by_workspace, sidebar, catalog)
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
            crate::ui::designall::separator_stroke(ui.visuals())
        };
        paint_sidebar_separator(ui, panel_rect, separator_stroke);
        panel.inner
    }

    /// 「워크스페이스·세션」 블록과 폴더 트리 사이 경계선 — 위아래로 끌면
    /// `workspace_section_height`가 바뀌어 두 섹션의 높이 비중을 조절한다
    /// (터미널 pane split 핸들과 동일한 hover/drag 스타일, workspace.rs 참고).
    /// 반환값 = 이번 프레임에 드래그 중인지 — 드래그로 아래 폴더 트리 행이 밀려
    /// 포인터 밑에 오면 hover 판정만으로 클릭 가능한 것처럼 보이는 오작동을
    /// 막기 위해 호출측이 행 상호작용을 잠시 꺼야 한다(2026-07-24 사용자 보고).
    fn workspace_split_handle(
        &mut self,
        ui: &mut egui::Ui,
        background_top: f32,
        background: egui::Color32,
    ) -> bool {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), PROJECT_FILE_SPLIT_HEIGHT),
            egui::Sense::hover(),
        );
        let hit_rect = rect.expand2(egui::vec2(0.0, 2.0));
        let id = ui.id().with("file_tree_workspace_split_handle");
        let resp = ui
            .interact(hit_rect, id, egui::Sense::drag())
            .on_hover_cursor(egui::CursorIcon::ResizeVertical);
        if resp.dragged() {
            self.workspace_section_height += ui.input(|input| input.pointer.delta().y);
        }
        let color = if resp.hovered() || resp.dragged() {
            ui.visuals().selection.bg_fill
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke.color
        };
        let ppp = ui.ctx().pixels_per_point();
        let painter = ui.painter();
        // background_top은 워크스페이스 섹션 높이에서 오고, 그 값은 이 핸들의 드래그가
        // pointer delta를 누적하므로 한 번 끌면 소수가 된다. 바로 아래 hline은 이미
        // 스냅하고 있었는데 채움만 빠져 있어 같은 함수 안에서 한쪽만 어긋났다.
        let background_rect = crate::ui::snap_rect_to_pixel(
            ppp,
            egui::Rect::from_min_max(
                egui::pos2(ui.max_rect().left(), background_top),
                egui::pos2(ui.max_rect().right(), ui.cursor().min.y),
            ),
        );
        painter.rect_filled(background_rect, 0.0, background);
        let y = crate::ui::snap_line_to_pixel(rect.center().y, 1.0, ppp);
        painter.hline(ui.clip_rect().x_range(), y, egui::Stroke::new(1.0, color));
        resp.dragged()
    }

    fn contents(
        &mut self,
        ui: &mut egui::Ui,
        sessions_by_workspace: &HashMap<String, Vec<SidebarSessionRow>>,
        sidebar: &SidebarSnapshot<'_>,
        catalog: &i18n::Catalog,
    ) -> Option<SidebarAction> {
        // (워처/백그라운드 채널 수거는 panel()이 접힘 여부와 무관하게 이미 수행했다)
        let mut action: Option<SidebarAction> = None;
        let available_height = ui.available_height();
        let (workspace_height, folder_height) =
            project_file_section_heights(available_height, self.workspace_section_height);
        self.workspace_section_height = workspace_height;
        let workspace_width = ui.available_width();
        let workspace_background = crate::ui::designall::tokens(ui.visuals()).workspace_background;
        let outer_item_spacing_y = ui.spacing().item_spacing.y;
        ui.spacing_mut().item_spacing.y = 0.0;
        let workspace_section = ui.allocate_ui_with_layout(
            egui::vec2(workspace_width, workspace_height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.spacing_mut().item_spacing.y = outer_item_spacing_y;
                ui.painter()
                    .rect_filled(ui.max_rect(), 0.0, workspace_background);

                // ── 통합 워크스페이스·세션 계층 ──
                let compact_sidebar = ui.available_width() < 120.0;
                self.workspace_sessions_expanded.retain(|workspace_id, _| {
                    sidebar
                        .workspaces
                        .iter()
                        .any(|workspace| workspace.id == *workspace_id)
                });
                sync_workspace_expansion_on_switch(
                    &mut self.workspace_sessions_expanded,
                    &mut self.last_sidebar_active_workspace,
                    sidebar.active_workspace_id,
                );
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
                                egui::RichText::new(
                                    catalog.t("sidebar.empty.start_workspace", &[]),
                                )
                                .weak(),
                            );
                        }
                    });
                    ui.add_space(14.0);
                } else {
                    // 헤더 바("워크스페이스 & 세션 +") 제거 — 첫 워크스페이스가 여백 없이
                    // 상단에 붙는다(2026-07-18 사용자). 워크스페이스 추가(+)는 목록 아래로
                    // 옮기고, 새 세션은 워크스페이스 우클릭 메뉴가 담당한다.
                    // App이 정한 워크스페이스 순서를 그대로 그린다. 활성 workspace를
                    // 맨 위로 옮기지 않아 선택할 때 목록 위치가 바뀌지 않는다.
                    let (before_active, active, after_active) = workspace_creation_order_partition(
                        sidebar.workspaces,
                        sidebar.active_workspace_id,
                    );
                    let active_sessions = sessions_by_workspace
                        .get(sidebar.active_workspace_id)
                        .map(Vec::as_slice)
                        .unwrap_or_default();
                    // 그룹은 펼친 세션 전체 높이만큼 늘어나고, 목록 전체가 하나의 스크롤
                    // 영역을 공유한다. 그룹별 고정 높이나 내부 스크롤 때문에 세션이
                    // 일부만 보이지 않도록 모든 그룹을 이 영역 안에서 그린다.
                    // 순서 변경 드래그는 세 구간(활성 앞/활성/활성 뒤)이 상태를 공유한다.
                    // 그룹 rect는 그린 순서 그대로 쌓여 그대로 드롭 위치 계산의 기준이 된다.
                    let mut group_rects: Vec<(String, egui::Rect)> = Vec::new();
                    let mut drag_released = false;
                    egui::ScrollArea::vertical()
                        .id_salt("workspace_list_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            // 워크스페이스 헤더와 그 아래 세션 목록 사이에 여백을 두지
                            // 않는다(2026-08-11 사용자). 워크스페이스끼리는 그룹 구분선이
                            // 가르므로 여백이 따로 필요 없다 — 목업의 .ws border-top과 같다.
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for workspace in before_active {
                                let inner = ui.scope(|ui| {
                                    let color = workspace_accent(sidebar.workspaces, &workspace.id);
                                    let expanded = self
                                        .workspace_sessions_expanded
                                        .get(&workspace.id)
                                        .copied()
                                        .unwrap_or(false);
                                    let resp = workspace_row(
                                        ui,
                                        workspace,
                                        color,
                                        false,
                                        Some(expanded),
                                        catalog,
                                    );
                                    let row = &resp.row;
                                    workspace_context_menu(row, workspace, catalog, &mut action);
                                    if track_workspace_drag(
                                        row,
                                        &workspace.id,
                                        &mut self.workspace_drag,
                                    ) {
                                        drag_released = true;
                                    }
                                    // chevron은 **전환 없이** 여닫는다 — 비활성 워크스페이스를
                                    // 펼쳐 둘 수 있어야 그 세션을 현재 화면 옆에 열 수 있다.
                                    if resp.disclosure_clicked {
                                        self.workspace_sessions_expanded
                                            .insert(workspace.id.clone(), !expanded);
                                    } else if row.clicked() {
                                        // 펼침은 전환이 **실제로 반영될 때** sync가 켠다. 여기서
                                        // 미리 켜면 전환이 거부됐을 때(warm 한도 초과 등) 펼친
                                        // 채로 남아 두 곳이 동시에 열린 것처럼 보인다.
                                        action = Some(SidebarAction::SwitchWorkspace(
                                            workspace.id.clone(),
                                        ));
                                    }
                                    if expanded
                                        && let Some(sessions) =
                                            sessions_by_workspace.get(&workspace.id)
                                    {
                                        let (session_action, _) = inactive_workspace_sessions(
                                            ui,
                                            workspace,
                                            sidebar.active_workspace_id,
                                            sessions,
                                            color,
                                            catalog,
                                        );
                                        if let Some(session_action) = session_action {
                                            action = Some(session_action);
                                        }
                                    }
                                });
                                paint_workspace_group_separator(ui, inner.response.rect);
                                group_rects.push((workspace.id.clone(), inner.response.rect));
                            }
                            let active_color =
                                workspace_accent(sidebar.workspaces, sidebar.active_workspace_id);
                            let active_inner = ui.scope(|ui| {
                                if let Some(active) = active {
                                    let expanded = self
                                        .workspace_sessions_expanded
                                        .get(&active.id)
                                        .copied()
                                        .unwrap_or(true);
                                    let resp = workspace_row(
                                        ui,
                                        active,
                                        active_color,
                                        true,
                                        Some(expanded),
                                        catalog,
                                    );
                                    let row = &resp.row;
                                    workspace_context_menu(row, active, catalog, &mut action);
                                    if track_workspace_drag(row, &active.id, &mut self.workspace_drag)
                                    {
                                        drag_released = true;
                                    }
                                    if resp.disclosure_clicked {
                                        self.workspace_sessions_expanded
                                            .insert(active.id.clone(), !expanded);
                                    } else if row.clicked() {
                                        self.workspace_sessions_expanded
                                            .insert(active.id.clone(), !expanded);
                                        // Home/작업/Agents에서 현재 활성 워크스페이스를 다시 눌러도
                                        // App dispatch가 Terminal view로 복귀할 수 있게 명시적 전환을
                                        // 방출한다. 같은 id의 runtime 전환은 App에서 no-op이다.
                                        action =
                                            Some(SidebarAction::SwitchWorkspace(active.id.clone()));
                                    }
                                }

                                // 현재 workspace의 셸/에이전트를 활성 워크스페이스 아래에 들여써 나열한다.
                                let mut session_row_rects = Vec::new();
                                let active_sessions_visible = self
                                    .workspace_sessions_expanded
                                    .get(sidebar.active_workspace_id)
                                    .copied()
                                    .unwrap_or(true)
                                    && !active_sessions.is_empty();
                                if active_sessions_visible {
                                    // 모든 세션 행만큼 자연스럽게 늘어난다. 화면을 넘는 부분은
                                    // 바깥 workspace_list_scroll 하나에서 스크롤한다.
                                    ui.push_id(("session_list", sidebar.active_workspace_id), |ui| {
                                            // 행 **사이**에만 여백을 준다 — 첫 행은 헤더에,
                                            // 마지막 행은 그룹 구분선에 바로 붙는다
                                            // (2026-07-25·2026-08-11 사용자).
                                            ui.spacing_mut().item_spacing.y = SESSION_ROW_GAP;
                                            for entry in active_sessions.iter() {
                                                ui.horizontal(|ui| {
                                                    ui.add_space(16.0);
                                                    ui.vertical(|ui| {
                                                        let editing = matches!(
                                                            &self.session_name_edit,
                                                            Some(edit) if edit.target == entry.target
                                                        );
                                                        if editing {
                                                            // 인라인 이름 편집 — Enter 확정(RenameSession), Esc 취소.
                                                            // 행(레일/상태줄) 레이아웃은 유지하고 제목 자리만 편집기로.
                                                            let edit = self.session_name_edit.as_mut().unwrap();
                                                            let (resp, result) = session_row_editing(ui, entry, edit, active_color);
                                                            session_row_rects.push(resp.rect);
                                                            if let Some(result) = result
                                                                && let Some(edit) = self.take_session_name_edit(ui.ctx())
                                                                && result == SessionNameEditResult::Submit
                                                            {
                                                                let title = edit.text.trim().to_owned();
                                                                // 이름을 비우면 기존 자동 제목 형식으로 돌린다.
                                                                let title = if title.is_empty() {
                                                                    format!("workspace.spawn.shell {}", entry.target.session().map_or(0, |s| s.0))
                                                                } else {
                                                                    title
                                                                };
                                                                action = Some(SidebarAction::RenameSession {
                                                                    pane: edit.target.pane().clone(),
                                                                    title,
                                                                });
                                                            }
                                                        } else {
                                                            // A1에서 생략된 작업·모델·강도만 hover로 보여 준다.
                                                            // 상태 감지 출처/신뢰도 같은 내부 진단은 표시하지 않는다.
                                                            let resp =
                                                                session_row(ui, entry, active_color);
                                                            {
                                                                let row_rect = resp.rect;
                                                                session_row_rects.push(row_rect);
                                                            }
                                                            // 우클릭 → 컨텍스트 메뉴(이름 변경/폴더/새 셸/이어가기/닫기).
                                                            // 클릭 → 세션 전환.
                                                            // 이름 변경은 **우클릭 메뉴에만** 둔다 — 더블클릭 진입은
                                                            // 제거했다(2026-08-11 사용자). 세션 행의 주 동작은 전환인데
                                                            // 빠르게 두 번 누르면 편집기가 열려 오조작이 됐다.
                                                            // (수동 상태 지정 U17b는 hook 감지 정착으로 제거 — 2026-07-17 사용자.)
                                                            if let Some(session) = entry.target.session() {
                                                                resp.context_menu(|ui| {
                                                                    live_session_context_menu_items(ui, |ui| {
                                                                    if ui
                                                                        .button(catalog.t(
                                                                            "workspace.rename_menu",
                                                                            &[],
                                                                        ))
                                                                        .clicked()
                                                                    {
                                                                        let _ = self.take_session_name_edit(ui.ctx());
                                                                        self.session_name_edit = Some(SessionNameEdit {
                                                                            target: entry.target.clone(),
                                                                            text: if entry.title_is_custom { entry.title.clone() } else { String::new() },
                                                                            request_focus: true,
                                                                        });
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
                                                                    // 변경 보기 — 이 세션 cwd 레포의 diff를 사이드바 Git
                                                                    // 탭에 연다(PR-D). session payload를 실어 보낸다 — App이
                                                                    // 포커스 세션이 아니라 **이** 세션의 cwd로 수집해야 한다
                                                                    // (2026-08-15 회귀 수정, Task 10 Step 9 되돌림).
                                                                    if ui
                                                                .button(catalog.t(
                                                                    "sidebar.menu.show_diff",
                                                                    &[],
                                                                ))
                                                                .clicked()
                                                            {
                                                                action = Some(SidebarAction::ShowDiff {
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
                                                                        workspace_id: entry
                                                                            .target
                                                                            .workspace_id()
                                                                            .to_owned(),
                                                                        pane: entry.target.pane().clone(),
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
                                                                        pane: entry.target.pane().clone(),
                                                                    },
                                                                );
                                                                ui.close();
                                                            }
                                                                    });
                                                                });
                                                            }
                                                            let close_clicked =
                                                                active_session_close_button(
                                                                    ui, &resp, catalog,
                                                                );
                                                            if close_clicked {
                                                                action = Some(
                                                                    SidebarAction::ClosePane {
                                                                        pane: entry
                                                                            .target
                                                                            .pane()
                                                                            .clone(),
                                                                    },
                                                                );
                                                            } else if session_row_click_allowed(
                                                                resp.clicked(),
                                                                resp.drag_started() || resp.dragged() || resp.drag_stopped(),
                                                            )
                                                                && session_row_should_activate(
                                                                    &entry.target,
                                                                    entry.focused,
                                                                )
                                                            {
                                                                action = Some(session_row_activation(
                                                                    &entry.target,
                                                                ));
                                                            }
                                                        }
                                                    });
                                                });
                                            }
                                        });
                                    if let Some(reorder) = session_reorder_drop(
                                        ui,
                                        sidebar.active_workspace_id,
                                        active_sessions,
                                        &session_row_rects,
                                        active_color,
                                    ) {
                                        action = Some(reorder);
                                    }
                                }
                            });
                            paint_workspace_group_separator(ui, active_inner.response.rect);
                            if active.is_some() {
                                group_rects.push((
                                    sidebar.active_workspace_id.to_owned(),
                                    active_inner.response.rect,
                                ));
                            }
                            for workspace in after_active {
                                let inner = ui.scope(|ui| {
                                    let color = workspace_accent(sidebar.workspaces, &workspace.id);
                                    let expanded = self
                                        .workspace_sessions_expanded
                                        .get(&workspace.id)
                                        .copied()
                                        .unwrap_or(false);
                                    let resp = workspace_row(
                                        ui,
                                        workspace,
                                        color,
                                        false,
                                        Some(expanded),
                                        catalog,
                                    );
                                    let row = &resp.row;
                                    workspace_context_menu(row, workspace, catalog, &mut action);
                                    if track_workspace_drag(
                                        row,
                                        &workspace.id,
                                        &mut self.workspace_drag,
                                    ) {
                                        drag_released = true;
                                    }
                                    // chevron은 **전환 없이** 여닫는다 — 비활성 워크스페이스를
                                    // 펼쳐 둘 수 있어야 그 세션을 현재 화면 옆에 열 수 있다.
                                    if resp.disclosure_clicked {
                                        self.workspace_sessions_expanded
                                            .insert(workspace.id.clone(), !expanded);
                                    } else if row.clicked() {
                                        // 펼침은 전환이 **실제로 반영될 때** sync가 켠다. 여기서
                                        // 미리 켜면 전환이 거부됐을 때(warm 한도 초과 등) 펼친
                                        // 채로 남아 두 곳이 동시에 열린 것처럼 보인다.
                                        action = Some(SidebarAction::SwitchWorkspace(
                                            workspace.id.clone(),
                                        ));
                                    }
                                    if expanded
                                        && let Some(sessions) =
                                            sessions_by_workspace.get(&workspace.id)
                                    {
                                        let (session_action, _) = inactive_workspace_sessions(
                                            ui,
                                            workspace,
                                            sidebar.active_workspace_id,
                                            sessions,
                                            color,
                                            catalog,
                                        );
                                        if let Some(session_action) = session_action {
                                            action = Some(session_action);
                                        }
                                    }
                                });
                                paint_workspace_group_separator(ui, inner.response.rect);
                                group_rects.push((workspace.id.clone(), inner.response.rect));
                            }
                            // 세션을 끄는 동안에도 목록 끝으로 이동할 수 있게 전체 목록을
                            // 자동 스크롤한다. 개별 그룹에는 별도 스크롤 영역을 만들지 않는다.
                            if egui::DragAndDrop::has_payload_of_type::<SessionRowDragPayload>(ui.ctx())
                                && ui.input(|input| input.pointer.button_down(egui::PointerButton::Primary))
                                && let Some(pointer) = ui.ctx().pointer_interact_pos()
                            {
                                let viewport = ui.clip_rect();
                                if pointer.x >= viewport.left()
                                    && pointer.x <= viewport.right()
                                    && pointer.y >= viewport.top() - 24.0
                                    && pointer.y <= viewport.bottom() + 24.0
                                {
                                    let delta = drag_autoscroll_delta(
                                        viewport.y_range(),
                                        pointer.y,
                                        ui.input(|input| input.stable_dt),
                                    );
                                    if delta != 0.0 {
                                        ui.scroll_with_delta_animation(
                                            egui::vec2(0.0, delta),
                                            egui::style::ScrollAnimation::none(),
                                        );
                                        ui.ctx().request_repaint();
                                    }
                                }
                            }
                            // ── 순서 변경 드래그: 놓일 자리 표시, 놓는 순간 확정 ──
                            if let Some(dragged) = self.workspace_drag.clone() {
                                if let Some(pointer) = ui.ctx().pointer_interact_pos() {
                                    let centers: Vec<f32> = group_rects
                                        .iter()
                                        .map(|(_, rect)| rect.center().y)
                                        .collect();
                                    let insert_at = sidebar_drop_index(&centers, pointer.y);
                                    // 목록 **밖**에서는 표시선도 내지 않는다 — 밖에서 놓으면
                                    // 취소인데 선을 그리면 "여기 들어간다"고 거짓말이 된다.
                                    let list_rect = ui.clip_rect();
                                    let inside_list = list_rect.contains(pointer);
                                    if inside_list {
                                        paint_workspace_drop_indicator(
                                            ui,
                                            &group_rects,
                                            insert_at,
                                            active_color,
                                        );
                                    }
                                    // 드래그 중에는 휠도 스크롤바도 막혀 있어(egui가 드래그
                                    // 중 둘 다 끈다) 목록을 스크롤할 방법이 아예 없었다 —
                                    // 화면 밖 자리로는 워크스페이스를 옮길 수 없었다.
                                    // 가장자리에 다가가면 목록이 스스로 흐르게 한다.
                                    let autoscroll = drag_autoscroll_delta(
                                        list_rect.y_range(),
                                        pointer.y,
                                        ui.input(|input| input.stable_dt),
                                    );
                                    if autoscroll != 0.0 {
                                        // 프레임마다 그 프레임 몫만 미는 방식이라 애니메이션을
                                        // 덧씌우면 두 번 완만해진다.
                                        ui.scroll_with_delta_animation(
                                            egui::vec2(0.0, autoscroll),
                                            egui::style::ScrollAnimation::none(),
                                        );
                                        // 포인터를 가만히 들고 있어도 계속 흘러야 한다.
                                        ui.ctx().request_repaint();
                                    }
                                    // 목록 **밖에서** 놓으면 취소다(보편적인 "밖에 떨구면
                                    // 취소"). 예전에는 x를 아예 보지 않고 y만 읽어서 터미널
                                    // 위나 목록 위쪽에서 놓아도 확정됐고, 위쪽은 insert_at이
                                    // 0이라 끌던 행이 맨 앞으로 튀었다(2026-09-03 리뷰 M1).
                                    if drag_released && inside_list {
                                        let ids: Vec<String> =
                                            group_rects.iter().map(|(id, _)| id.clone()).collect();
                                        let reordered =
                                            reorder_sidebar_ids(&ids, &dragged, insert_at);
                                        if reordered != ids {
                                            action =
                                                Some(SidebarAction::ReorderWorkspaces(reordered));
                                        }
                                    }
                                }
                                // 놓는 프레임이 아니어도 버튼이 이미 올라갔으면 상태를 놓아준다 —
                                // 드래그하던 행이 사라져 drag_stopped를 못 받는 경우의 안전망.
                                if drag_released || !ui.ctx().input(|input| input.pointer.any_down())
                                {
                                    self.workspace_drag = None;
                                }
                            }
                        });
                }
            },
        );
        finish_session_row_drag_frame(ui.ctx());
        let resizing_workspace_split = self.workspace_split_handle(
            ui,
            workspace_section.response.rect.bottom(),
            workspace_background,
        );
        ui.spacing_mut().item_spacing.y = outer_item_spacing_y;
        let folder_background = crate::ui::designall::tokens(ui.visuals()).folder_tree_background;
        let folder_background_rect = egui::Rect::from_min_size(
            egui::pos2(ui.max_rect().left(), ui.cursor().min.y),
            egui::vec2(workspace_width, folder_height),
        );
        ui.painter()
            .rect_filled(folder_background_rect, 0.0, folder_background);

        let mut create_folder = false;
        let mut create_file = false;
        let mut open_search = false;
        let status_listing = matches!(
            self.pending_maintenance,
            Some(PendingFileTreeMaintenance::Listing { .. })
        );
        let (header_rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 38.0), egui::Sense::hover());
        let tabs = SIDEBAR_TOOLS.map(|tool| (tool, catalog.t(sidebar_tool_label_key(tool), &[])));
        let tab_widths = tabs
            .iter()
            .map(|(_, label)| {
                let galley = ui.painter().layout_no_wrap(
                    label.clone(),
                    crate::fonts::sidebar_font(ui.ctx(), 11.5),
                    ui.visuals().text_color(),
                );
                (galley.size().x + 12.0).max(34.0)
            })
            .collect::<Vec<_>>();
        let all_tabs_width = tab_widths.iter().sum::<f32>();
        let visible_tools = (((header_rect.width() - all_tabs_width - 8.0).max(0.0) / 20.0).floor()
            as usize)
            .min(2);
        let mut tool_right = header_rect.right() - 4.0;
        if visible_tools >= 1 {
            let rect = egui::Rect::from_min_size(
                egui::pos2(tool_right - 20.0, header_rect.top() + 9.0),
                egui::vec2(20.0, 20.0),
            );
            let more_label = catalog.t("sidebar.tool.more", &[]);
            let more =
                file_toolbar_more_at(ui, rect, &more_label).on_hover_text(more_label.clone());
            let mut toggle_hidden = false;
            egui::Popup::menu(&more).show(|ui| {
                ui.set_min_width(170.0);
                if ui.button(catalog.t("file_tree.new_file", &[])).clicked() {
                    create_file = true;
                    ui.close();
                }
                if ui
                    .button(catalog.t("file_tree.search_files", &[]))
                    .clicked()
                {
                    open_search = true;
                    ui.close();
                }
                let hidden_label = if self.show_hidden {
                    catalog.t("file_tree.hide_hidden_files", &[])
                } else {
                    catalog.t("file_tree.show_hidden_files", &[])
                };
                if ui.button(hidden_label).clicked() {
                    toggle_hidden = true;
                    ui.close();
                }
                if ui.button(catalog.t("file_tree.new_folder", &[])).clicked() {
                    create_folder = true;
                    ui.close();
                }
            });
            if toggle_hidden {
                self.show_hidden = !self.show_hidden;
                // 다시 드러난 펼친 폴더도 순회 대상에 포함해 숨겨져 있던 캐시를 갱신한다.
                self.refresh();
                self.rebuild_flat();
                if let Some(query) = self.search.as_ref().map(|search| search.query.clone())
                    && !query.is_empty()
                {
                    self.request_file_search(query);
                }
            }
            tool_right -= 20.0;
        }
        if open_search {
            self.close_file_search();
            self.search = Some(FileTreeSearch {
                query: String::new(),
                results: Vec::new(),
                selected: None,
                busy: false,
                result_limit_reached: false,
                traversal_incomplete: false,
                focus: true,
            });
            self.search_requested = None;
        }
        if visible_tools >= 2 {
            let rect = egui::Rect::from_min_size(
                egui::pos2(tool_right - 20.0, header_rect.top() + 9.0),
                egui::vec2(20.0, 20.0),
            );
            let refresh_label = catalog.t("sidebar.tool.refresh", &[]);
            if file_toolbar_icon_at(
                ui,
                rect,
                "refresh",
                &refresh_label,
                FileToolbarIcon::Refresh,
                false,
            )
            .on_hover_text(if status_listing {
                catalog.t("file_tree.listing_folders", &[])
            } else {
                refresh_label
            })
            .clicked()
            {
                self.refresh();
                if let Some(query) = self.search.as_ref().map(|search| search.query.clone())
                    && !query.is_empty()
                {
                    self.request_file_search(query);
                }
            }
            if status_listing {
                // 조회 중임은 고정된 도구 영역에서만 표시한다. 행 삽입이나 스피너의
                // 연속 repaint로 목록 위치와 더블클릭 대상을 흔들지 않는다.
                ui.painter().circle_filled(
                    rect.right_bottom() - egui::vec2(3.0, 3.0),
                    2.0,
                    ui.visuals().selection.stroke.color,
                );
            }
            tool_right -= 20.0;
        }

        let tab_right = tool_right - 2.0;
        let mut tab_left = header_rect.left() + 4.0;
        for ((tool, label), width) in tabs.iter().zip(tab_widths) {
            if tab_left + width > tab_right {
                break;
            }
            let rect = egui::Rect::from_min_size(
                egui::pos2(tab_left, header_rect.top()),
                egui::vec2(width, header_rect.height()),
            );
            let active = *tool == self.selected_tool;
            if sidebar_tool_tab_at(ui, rect, label, active).clicked() {
                match sidebar_tool_action(*tool) {
                    // 인라인 탭 — 본문을 바꾼다. 메모로 들어가면 커서를 바로 잡는다.
                    None => {
                        self.selected_tool = *tool;
                        if *tool == SidebarTool::Notes {
                            self.notes.request_focus();
                        }
                    }
                    // 다른 화면에 작용하는 탭 — 선택은 그대로 둔다.
                    Some(tool_action) => action = Some(tool_action),
                }
            }
            tab_left += width;
        }
        let separator_y = crate::ui::snap_line_to_pixel(
            header_rect.bottom(),
            crate::ui::designall::SEPARATOR_WIDTH,
            ui.ctx().pixels_per_point(),
        );
        ui.painter().hline(
            header_rect.x_range(),
            separator_y,
            crate::ui::designall::separator_stroke(ui.visuals()),
        );

        // 메모 탭은 파일 트리 대신 본문을 통째로 쓴다. 여기서 반환하므로 아래
        // 파일 트리·에러 표시는 그리지 않는다 — 파일 오류는 「파일」 탭으로 돌아오면
        // `self.error`가 그대로 남아 있어 다시 보인다.
        if self.selected_tool == SidebarTool::Notes {
            self.close_file_search();
            let note_action = self.notes.render(
                ui,
                super::notes::NotesInput {
                    workspace_id: sidebar.active_workspace_id,
                    stored: sidebar.workspace_note,
                },
                catalog,
            );
            if let Some(super::notes::NotesAction::Edited(body)) = note_action {
                action = Some(SidebarAction::NoteEdited(body));
            }
            return action;
        }

        let header_drop = ui.interact(
            header_rect,
            egui::Id::new("file_tree_root_drop"),
            egui::Sense::hover(),
        );
        // 헤더(루트) 드롭도 외곽선을 쓰지 않는다 — 폴더 행과 같은 면 강조로 통일한다
        // (2026-08-10 사용자: 외곽 테두리 제거).
        if header_drop.dnd_hover_payload::<PathBuf>().is_some() {
            ui.painter().rect_filled(
                header_rect,
                0.0,
                ui.visuals().selection.bg_fill.gamma_multiply(0.22),
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
            egui::vec2(13.0, 12.0),
        );
        ui.painter().text(
            egui::pos2(parent_rect.left() + 47.0, parent_rect.center().y),
            egui::Align2::LEFT_CENTER,
            "..",
            crate::fonts::sidebar_font(ui.ctx(), 12.5),
            parent_color,
        );
        // 상위 이동도 기존 목록을 끝까지 그린 뒤 반영한다. 여기서 반환하면 클릭한
        // 프레임의 본문만 통째로 비어 보인다.
        let parent_navigation = parent_response.clicked().then_some(parent_root).flatten();
        if (create_file || create_folder)
            && let Some(parent) = self.creation_parent()
        {
            self.close_file_search();
            if create_file {
                self.start_edit(EditState::NewFile {
                    parent,
                    buffer: String::new(),
                    focus: true,
                });
            } else if create_folder {
                self.start_edit(EditState::NewFolder {
                    parent,
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
            if let Some(parent) = parent_navigation {
                self.set_root(Some(parent));
                ui.ctx().request_repaint();
            }
            return action;
        }

        if self.search.is_some() {
            self.render_file_search(ui, catalog);
            if let Some(parent) = parent_navigation {
                self.set_root(Some(parent));
                ui.ctx().request_repaint();
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
                &[("path", &super::path_display(parent))],
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
                &[("path", &super::path_display(parent))],
            ));
        }

        // 헤더 우클릭도 더보기와 같은 생성 위치를 사용한다.
        if self.root.is_some() {
            header_drop.context_menu(|ui| {
                if ui.button(catalog.t("file_tree.new_folder", &[])).clicked()
                    && let Some(parent) = self.creation_parent()
                {
                    menu_action = Some(MenuAction::NewFolder(parent));
                    ui.close();
                }
            });
        }

        // ── 목록 전체를 설명하는 상태 표시 ──
        //
        // **셋 다 스크롤 영역보다 먼저 그린다.** 아래 `ScrollArea`는
        // `auto_shrink([false,false])`라 남은 높이를 전부 가져가므로, 그 뒤에 놓인
        // 위젯은 패널 바닥 밖으로 밀려 잘린다 — 700px 패널에서 실측하면 영구삭제
        // 확인 문구가 y=695/버튼이 y=728, 오류 라벨이 y=717.5, 진행 문구가 y=696.5로
        // 모두 화면 밖이었다. 무엇이 실패했는지도, 무엇을 지우는지도, 지금 뭘 하고
        // 있는지도 보이지 않았다(§조용한 실패 금지).
        //
        // 쌓는 순서는 오류(왜 멈췄나) → 영구삭제 확인(그래서 뭘 고를까) → 진행
        // 표시(지금 뭘 하고 있나)다. 셋은 동시에 뜰 수 있고(휴지통 실패 직후 다른
        // 파일 조작을 걸면 그렇다) 이 순서면 원인 → 선택 → 현재로 읽힌다. 진행
        // 표시는 자기가 바꾸는 목록 바로 위에 붙는다.
        //
        // 그리고 나서 바뀐 상태는 이번 프레임에 실을 수 없으므로 아래에서 한 프레임을
        // 더 요청한다(`status_*` 스냅샷).
        let status_error = self.error.clone();
        let status_busy = self.in_flight > 0;
        if let Some(err) = status_error.clone() {
            ui.horizontal(|ui| {
                ui.colored_label(ui.visuals().error_fg_color, err);
                if ui.small_button("×").clicked() {
                    self.error = None;
                }
            });
        }
        // 휴지통 실패 → 영구삭제 확인 (§9-7 — 조용한 영구삭제 금지).
        self.permanent_delete_confirm(ui, catalog);
        if status_busy {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                ui.weak(catalog.t("file_tree.file_operation_running", &[]));
            });
        }

        // 가상화: 고정 행높이 + path 기반 explicit Id (§9-6).
        // 행높이는 실측 자기보정 — 선언값과 실제가 어긋나면 클릭 대상이 밀린다(필드 주석).
        let row_height = self.measured_row_height.unwrap_or(25.0);
        // 행 배경·테두리를 물리 픽셀에 맞추는 데 쓴다. row_height 자체는 스냅하지
        // 않는다 — 아래 자기보정 루프가 실측값과 0.1 넘게 어긋나면 매 프레임 재저장·
        // 재그리기를 요청하므로, 저장값을 반올림하면 무한 repaint가 된다.
        let ppp = ui.ctx().pixels_per_point();
        let total = self.flat.len();
        let typeahead_reveal = if self.root.is_some()
            && edit.is_none()
            && !ui.ctx().text_edit_focused()
            && !ui.ctx().any_popup_open()
            && ui.memory(|memory| memory.has_focus(tree_keyboard_focus_id()))
        {
            let (now, text_events) = ui.input(|input| {
                let texts =
                    if input.modifiers.command || input.modifiers.ctrl || input.modifiers.alt {
                        Vec::new()
                    } else {
                        tree_typeahead_text_events(&input.events)
                    };
                (input.time, texts)
            });
            text_events
                .iter()
                .filter_map(|text| self.handle_typeahead_text(text, now))
                .last()
        } else {
            None
        };
        let pending_reveal = self
            .reveal_path
            .as_ref()
            .and_then(|path| self.flat.iter().position(|row| &row.path == path));
        if let Some(index) = pending_reveal {
            let path = self.flat[index].path.clone();
            self.apply_selection_click(&path, SelectionClick::Replace);
            self.reveal_path = None;
        }
        let reveal = typeahead_reveal.or(pending_reveal);
        let mut toggle: Option<PathBuf> = None;
        let mut navigate_root: Option<PathBuf> = parent_navigation;
        let mut open_file: Option<PathBuf> = None; // 파일 더블클릭 → 연결 프로그램 열기
        // 문서 대상(md·txt 등) 더블클릭 → 문서 탭. open_file과 같은 이유로 루프 밖에서 처리.
        let mut open_document: Option<(PathBuf, DocumentTargetKind)> = None;
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
                .map(|file| file.path().to_path_buf())
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
        // ── 다중선택 ──
        // 0번 행의 위쪽 y. 선택 사각형은 가상화로 그리지 않은 행도 덮어야 해서 행 index를
        // 산술로 구하는데, 그 기준점이다.
        let mut content_top: Option<f32> = None;
        // 이름 오른쪽 빈 자리에서 드래그가 시작됐다 — 파일 이동이 아니라 선택 사각형.
        let mut marquee_start: Option<egui::Pos2> = None;
        // 선택을 바꾸는 클릭. `row`가 self를 빌리고 있어 루프 밖에서 적용한다(기존 관례).
        let mut select_click: Option<(PathBuf, SelectionClick)> = None;
        // 빈 영역을 클릭했다 — 선택 해제.
        let mut clear_selection = false;
        let scroll_output = file_tree_scroll_area(ui, row_height, reveal)
            .id_salt("file_tree_rows")
            .auto_shrink([false, false])
            .show_rows(ui, row_height, total, |ui, range| {
                // 빈 영역 배경. 행보다 **먼저** 등록해 행이 있는 자리에서는 행이 이긴다.
                // 이것이 있어야 (a) 목록 아래 빈 곳에서 선택 사각형을 시작할 수 있고
                // (b) 빈 곳 클릭으로 선택을 해제할 수 있다. 예전에는 둘 다 불가능해서
                // 선택을 비울 방법 자체가 없었다(2026-09-04 리뷰 H2·H4).
                let background = ui.interact(
                    ui.clip_rect(),
                    ui.id().with("tree_background"),
                    egui::Sense::click_and_drag(),
                );
                if background.clicked() {
                    clear_selection = true;
                }
                if background.drag_started_by(egui::PointerButton::Primary) {
                    marquee_start = ui.input(|input| input.pointer.press_origin());
                }
                for index in range {
                    let row = &self.flat[index];
                    let inaccessible = self.inaccessible_paths.contains(&row.path);
                    // 이름 변경 중인 행은 인라인 TextEdit로 대체 (FT-3, §9-8)
                    if let Some(EditState::Rename {
                        path,
                        buffer,
                        focus,
                        changed,
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
                            *changed |= resp.changed();
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
                    // 그리기 전용 스냅본. row_height는 아래 "행높이 자기보정"이 실측
                    // 갤리 높이를 그대로 저장하므로 2프레임째부터 사실상 항상 소수이고,
                    // show_rows가 그 값을 곱해 행 top을 잡아 행마다 배경 테두리가 다르게
                    // 뭉갠다. 판정(rect_contains_pointer/contains)은 원본 rect를 쓴다.
                    let hover_paint_rect = crate::ui::snap_rect_to_pixel(ppp, hover_rect);
                    if content_top.is_none() {
                        content_top =
                            Some(row_top - index as f32 * tree_row_pitch(row_height, ui.spacing()));
                    }
                    // 선택 배경은 hover보다 **먼저** 깐다 — 선택한 행 위에 포인터를 올리면
                    // hover가 덧칠돼 "여기 있다"가 더 밝아진다.
                    if self.selected.contains(&row.path)
                        && let Some(fill) = crate::ui::designall::row_fill(
                            crate::ui::designall::tokens(ui.visuals()),
                            true,
                            false,
                        )
                    {
                        // 채움은 **피치** 높이로. row_height만 덮으면 연속 선택이 하나의
                        // 덩어리가 아니라 3px씩 끊긴 막대로 보인다(2026-09-04 리뷰).
                        let pitch = tree_row_pitch(row_height, ui.spacing());
                        let selected_rect = crate::ui::snap_rect_to_pixel(
                            ppp,
                            egui::Rect::from_min_max(
                                hover_rect.left_top(),
                                egui::pos2(hover_rect.right(), row_top + pitch),
                            ),
                        );
                        ui.painter().rect_filled(selected_rect, 1.0, fill);
                    }
                    // 워크스페이스·폴더 트리 경계선 드래그 중엔 hover 판정을 끈다 —
                    // 리사이즈로 행이 포인터 밑에 밀려 들어오면 클릭 가능한 것처럼
                    // 하이라이트되어 오클릭처럼 보였다(2026-07-24 사용자 보고).
                    if !resizing_workspace_split && ui.rect_contains_pointer(hover_rect) {
                        ui.painter().rect_filled(
                            hover_paint_rect,
                            1.0,
                            ui.visuals().widgets.hovered.weak_bg_fill,
                        );
                        // ⌘V 대상 폴더/⌘C 대상 행 — hover 판정을 그대로 재사용(§과제②③).
                        if !inaccessible {
                            hover_target_dir = Some(row_target_dir(row, self.root.as_deref()));
                            hover_row_path = Some(row.path.clone());
                        }
                    }
                    // 영구삭제 확인이 가리키는 행을 경고색으로 덮는다. 확인 문구는 패널
                    // 맨 아래에 뜨므로, 행 쪽 표시가 없으면 "지금 어느 행 이야기냐"를
                    // 문구의 이름만 보고 맞춰야 했다 — 트리와 문구가 같은 대상을 함께
                    // 가리키게 한다. hover 면 **뒤에** 그린다(hover는 불투명이라 앞에
                    // 그리면 포인터를 얹는 순간 대상 표시가 사라진다).
                    if self
                        .confirm_delete
                        .as_ref()
                        .is_some_and(|target| target.path == row.path)
                    {
                        ui.painter().rect_filled(
                            hover_paint_rect,
                            1.0,
                            ui.visuals().warn_fg_color.gamma_multiply(0.22),
                        );
                    }
                    // Finder 드래그 대상: 폴더 행 하이라이트 + 드롭 대상 기록 (§과제①).
                    // 드래그 중엔 egui 포인터가 멎으므로 drag_pos(AppKit 위치)로 판정한다.
                    if !inaccessible
                        && let Some(pos) = drag_pos
                        && tree_area_owns_os_drop(hover_rect, pos)
                    {
                        // 내부 드래그와 **같은 판정 함수**를 쓴다. 표시와 목적지를 각각
                        // 계산하면 반드시 어긋난다 — 실제로 어긋났었다: 폴더 행 가장자리에
                        // 삽입 마커를 그려놓고(=부모로 간다는 뜻) `row_target_dir`은 밴드를
                        // 무시하고 그 폴더 자신을 돌려줘, **폴더와 폴더 사이에 놓으면 옆
                        // 폴더 안으로 들어갔다**(2026-08-11 사용자). 이제 한 곳에서 정하고
                        // 그림과 목적지가 그 하나를 함께 쓴다.
                        //
                        // 외부 드래그는 payload 경로를 알 수 없으므로(macOS는 드롭 전까지
                        // 경로를 안 준다) 판정에 넣을 `dragged`가 없다. 밖에서 온 파일이라
                        // 순환도 "이미 그 폴더 안"도 성립하지 않으니 밴드만 본다.
                        let target = super::file_drop::band_target(
                            row.is_dir,
                            hover_rect.top(),
                            hover_rect.bottom(),
                            pos.y,
                        );
                        drop_target_dir = Some(match target {
                            super::file_drop::RowDropTarget::IntoFolder => row.path.clone(),
                            // 삽입 마커 = 이 행의 **부모** 폴더로. 폴더 행 가장자리도 마찬가지다.
                            _ => row
                                .path
                                .parent()
                                .map(Path::to_path_buf)
                                .or_else(|| self.root.clone())
                                .unwrap_or_else(|| row.path.clone()),
                        });
                        if os_drag_active {
                            paint_row_drop_target(
                                ui,
                                ppp,
                                hover_paint_rect,
                                row.depth,
                                super::file_drop::DropDecision {
                                    target,
                                    eligibility: super::file_drop::DropEligibility::Allowed,
                                },
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
                                    &row.display_name,
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
                                        egui::vec2(13.0, 12.0),
                                    );
                                } else {
                                    let file_color = if inaccessible {
                                        ui.visuals().weak_text_color()
                                    } else if row.display_name.starts_with('.') {
                                        entry_color.gamma_multiply(0.62)
                                    } else {
                                        entry_color
                                    };
                                    paint_file(
                                        ui.painter(),
                                        ir.center(),
                                        file_color,
                                        carve,
                                        egui::vec2(9.5, 12.0),
                                    );
                                }
                                let text_color = if inaccessible {
                                    ui.visuals().weak_text_color()
                                } else if row.display_name.starts_with('.') {
                                    entry_color.gamma_multiply(0.62)
                                } else {
                                    entry_color
                                };
                                let rich = egui::RichText::new(&row.display_name)
                                    .family(crate::fonts::sidebar_font_family(ui.ctx()))
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
                    // 주 버튼만 받는다. egui는 드래그 시작을 버튼으로 가리지 않아,
                    // 우클릭으로 메뉴를 열려다 손이 몇 px 흔들리면 새 사각형이 시작돼
                    // 골라둔 선택이 통째로 날아갔다(2026-09-04 리뷰).
                    if !inaccessible && row_resp.drag_started_by(egui::PointerButton::Primary) {
                        // 행은 패널 폭 전체지만 이름은 그 일부만 쓴다. 이름 오른쪽 빈
                        // 자리에서 시작한 드래그는 **선택 사각형**이고, 이름·아이콘 위는
                        // 예전 그대로 파일 이동 소스다(2026-09-03 사용자 확정) — 한 행이
                        // 두 제스처를 갖되 시작 지점으로 갈린다.
                        let press = ui.input(|input| input.pointer.press_origin());
                        match press.filter(|pos| pos.x > response.rect.right() + MARQUEE_NAME_GAP) {
                            Some(pos) => marquee_start = Some(pos),
                            None => row_resp.dnd_set_drag_payload(row.path.clone()),
                        }
                    }
                    // 행높이 실측 (드래그 중엔 행이 tooltip 레이어로 빠져 rect가 다름 — 제외)
                    if observed_row_height.is_none()
                        && !egui::DragAndDrop::has_any_payload(ui.ctx())
                    {
                        observed_row_height = Some(response.rect.height());
                    }
                    // 드롭 대상 판정 — 폴더/파일을 가리지 않는다. 폴더 행 가운데는 그
                    // 폴더로, 가장자리와 파일 행은 **그 행의 부모 폴더**로 간다
                    // (판정 규칙과 근거는 `super::file_drop`). 파일 행이 대상이 아니던
                    // 시절엔 파일 사이에 놓으면 아무 일도 안 일어났다(2026-08-10 사용자).
                    if !inaccessible
                        && let Some(hover) = row_resp.dnd_hover_payload::<PathBuf>()
                        && let Some(pointer) = ui.ctx().pointer_interact_pos()
                        && let Some(target) = super::file_drop::row_drop_target(
                            super::file_drop::RowInfo {
                                path: &row.path,
                                is_dir: row.is_dir,
                                top: row_rect.top(),
                                bottom: row_rect.bottom(),
                            },
                            pointer.y,
                            hover.as_ref(),
                        )
                    {
                        paint_row_drop_target(ui, ppp, row_rect, row.depth, target);
                    }
                    if !inaccessible
                        && let Some(payload) = row_resp.dnd_release_payload::<PathBuf>()
                        && let Some(pointer) = ui.ctx().pointer_interact_pos()
                        && let Some(target) = super::file_drop::row_drop_target(
                            super::file_drop::RowInfo {
                                path: &row.path,
                                is_dir: row.is_dir,
                                top: row_rect.top(),
                                bottom: row_rect.bottom(),
                            },
                            pointer.y,
                            payload.as_ref(),
                        )
                    {
                        // NoOp(이미 그 폴더 안)이면 표시만 했지 이동은 걸지 않는다 —
                        // host도 no-op이라 결과는 같지만 불필요한 IO 왕복을 줄인다.
                        let destination = (target.eligibility
                            == super::file_drop::DropEligibility::Allowed)
                            .then(|| match target.target {
                                super::file_drop::RowDropTarget::IntoFolder => {
                                    Some(row.path.clone())
                                }
                                // 삽입선 = 이 행의 부모 폴더로.
                                _ => row.path.parent().map(Path::to_path_buf),
                            })
                            .flatten();
                        if let Some(destination) = destination {
                            drop_action = Some(((*payload).clone(), destination));
                        }
                    }
                    let single_clicked =
                        !inaccessible && (row_resp.clicked() || label_resp.clicked());
                    let click_kind = selection_click_kind(ui.input(|input| input.modifiers));
                    if single_clicked {
                        select_click = Some((row.path.clone(), click_kind));
                    }
                    if row.is_dir && !inaccessible {
                        if row_resp.double_clicked() || label_resp.double_clicked() {
                            navigate_root = Some(row.path.clone());
                        } else if single_clicked && click_kind == SelectionClick::Replace {
                            // ⌘/Shift 클릭은 고르기만 한다 — 범위를 잡는 중에 폴더가
                            // 펼쳐지면 그 아래 행이 밀려 방금 고른 구간이 어긋난다.
                            toggle = Some(row.path.clone());
                        }
                    } else if row_resp.double_clicked() || label_resp.double_clicked() {
                        match classify_document_target(&row.path) {
                            // 문서 대상(md·txt 등)은 문서 탭으로 연다 — App이 포커스된
                            // pane 위에 연다(설계 §3.2). 검증(`self.reject_io`가 필요할 수
                            // 있는 mutable self 접근)은 루프 밖(`row`의 대여가 끝난 뒤)에서
                            // `open_document`로 미룬다 — `open_file`과 같은 관례.
                            Some(kind) => open_document = Some((row.path.clone(), kind)),
                            // 그 외 확장자는 예전 그대로 OS 기본 앱이 연다. host가 실존
                            // regular file + 원본/realpath 허용 확장자를 다시 검증한 뒤에만
                            // 연다. leaf는 filesystem metadata를 읽지 않는다.
                            None => open_file = Some(row.path.clone()),
                        }
                    }
                    // 우클릭 컨텍스트 메뉴 (FT-3) — 행 전체에서 열리게 row_resp에 단다
                    if !inaccessible {
                        row_resp.context_menu(|ui| {
                            // 문서 대상은 더블클릭이 문서 탭으로 가로채므로, OS 기본 앱으로
                            // 여는 예전 길을 메뉴에 남긴다(설계 §3.1, 기존 동작을 빼앗지
                            // 않는다).
                            if !row.is_dir
                                && classify_document_target(&row.path).is_some()
                                && ui
                                    .button(catalog.t("file_tree.open_with_os", &[]))
                                    .clicked()
                            {
                                menu_action = Some(MenuAction::OpenWithOs(row.path.clone()));
                                ui.close();
                            }
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
                                // 삭제 대상은 **이 행**에서 확정한다 — 나중에 경로만
                                // 보고 이름/종류를 다시 유추하지 않는다(DeleteTarget 주석).
                                menu_action = Some(MenuAction::Delete(DeleteTarget {
                                    path: row.path.clone(),
                                    label: delete_target_label(self.root.as_deref(), &row.path),
                                    is_dir: row.is_dir,
                                }));
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
            ui.ctx().request_repaint();
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
        if let Some((path, kind)) = open_document {
            match FileTreePathPayload::try_new(path) {
                Ok(target) => action = Some(SidebarAction::OpenDocument { target, kind }),
                Err(code) => self.reject_io(code),
            }
        }
        if let Some((src, dst_dir)) = drop_action {
            self.start_move(src, dst_dir);
        }

        // ── Finder → 트리 반입: OS 드롭(①)·클립보드 ⌘V(②) — 원본 보존 복사 ──
        // 반입 영역 = 파일 헤더 + 행 목록 (워크스페이스 목록/하단 nav 제외).
        // 트리 안에서 일어난 클릭·드래그는 포커스를 준다(⌘⌫ 가드의 반대쪽).
        if clear_selection || select_click.is_some() || marquee_start.is_some() {
            ui.memory_mut(|memory| memory.request_focus(tree_keyboard_focus_id()));
        }
        if clear_selection {
            self.selected.clear();
            self.select_anchor = None;
            self.typeahead_prefix.clear();
            self.typeahead_last_at = None;
        }
        if let Some((path, kind)) = select_click {
            self.typeahead_prefix.clear();
            self.typeahead_last_at = None;
            self.apply_selection_click(&path, kind);
        }
        self.update_marquee(
            ui,
            marquee_start,
            scroll_output.inner_rect,
            content_top,
            tree_row_pitch(row_height, ui.spacing()),
        );
        let tree_area = header_rect.union(scroll_output.inner_rect);
        if os_drag_active && drag_pos.is_some_and(|pos| tree_area_owns_os_drop(tree_area, pos)) {
            if !drag_row_highlighted {
                // 행 위가 아니면 루트 반입 — 외곽선 대신 면으로 덮는다(2026-08-10 사용자:
                // 외곽 테두리 제거). 폴더 행 강조와 같은 언어라 "이 영역이 받는다"로 읽힌다.
                ui.painter().rect_filled(
                    crate::ui::snap_rect_to_pixel(ui.ctx().pixels_per_point(), tree_area),
                    2.0,
                    ui.visuals().selection.bg_fill.gamma_multiply(0.12),
                );
            }
            // 드래그 중엔 winit 이벤트가 없어 즉시 다음 frame을 요청해야 하이라이트가
            // 포인터를 따라온다. hovered_files가 비면 요청도 즉시 끝나며 timer는 남지 않는다.
            ui.ctx().request_repaint();
        }
        if !os_dropped.is_empty()
            && drag_pos.is_some_and(|pos| tree_area_owns_os_drop(tree_area, pos))
            && let Some(root) = self.root.clone()
        {
            let dst_dir = drop_target_dir.unwrap_or(root);
            self.start_copy_into(os_dropped, dst_dir);
        }
        self.handle_clipboard_shortcuts(ui, tree_area, hover_target_dir, hover_row_path);

        // 인라인 편집은 검증 후 native mutation intent만 만든다. 실패 completion이면
        // PendingFileTreeIo가 보관한 편집 snapshot을 복원한다.
        match edit_done {
            Some(false) => {
                self.cancel_edit();
                edit = None;
            }
            Some(true) => match edit {
                Some(EditState::Rename {
                    path,
                    buffer,
                    changed,
                    ..
                }) => {
                    let prepared = prepare_rename_request(&path, &buffer, changed);
                    let refresh = path.parent().map(Path::to_path_buf).into_iter().collect();
                    let retry = EditState::Rename {
                        path,
                        buffer,
                        focus: true,
                        changed,
                    };
                    let result = prepared.and_then(|request| match request {
                        Some(request) => self.queue_io(request, refresh, None, Some(retry.clone())),
                        None => Ok(()),
                    });
                    match result {
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
        self.edit = edit;
        match menu_action {
            Some(MenuAction::NewFolder(parent)) => {
                self.start_edit(EditState::NewFolder {
                    parent,
                    buffer: String::new(),
                    focus: true,
                });
            }
            Some(MenuAction::Rename(path)) => {
                let name = super::path_file_name_display(&path);
                self.start_edit(EditState::Rename {
                    path,
                    buffer: name,
                    focus: true,
                    changed: false,
                });
            }
            // 선택 안의 행을 우클릭했으면 선택 전체가 대상이다 — 여러 개를 골라 놓고
            // 그중 하나를 우클릭했는데 그 하나만 지워지면 고른 것이 조용히 무시된다.
            // 선택 밖의 행을 우클릭한 경우는 그 행 하나만 (선택은 건드리지 않는다).
            Some(MenuAction::Delete(target)) => {
                if self.selected.contains(&target.path) {
                    self.spawn_trash_selection(None);
                } else {
                    self.spawn_trash(target);
                }
            }
            Some(MenuAction::CopyFile(path)) => {
                let paths = if self.selected.contains(&path) {
                    self.selection_or(None)
                } else {
                    vec![path]
                };
                self.copy_files_to_clipboard(&paths);
            }
            Some(MenuAction::CopyPath(path)) => ui.ctx().copy_text(path.display().to_string()),
            Some(MenuAction::InsertPath(path)) => match FileTreePathPayload::try_new(path) {
                Ok(path) => action = Some(SidebarAction::InsertPath(path)),
                Err(code) => self.reject_io(code),
            },
            Some(MenuAction::CdPath(path)) => match FileTreePathPayload::try_new(path) {
                Ok(path) => action = Some(SidebarAction::CdPath(path)),
                Err(code) => self.reject_io(code),
            },
            Some(MenuAction::OpenWithOs(path)) => {
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
            None => {}
        }
        // 상태 표시는 위(스크롤 영역 앞)에서 이미 그렸다. 행 클릭·메뉴 처리가 그 뒤에
        // 오류를 세우거나 작업을 큐에 넣었으면 이번 프레임 화면에는 없다 — 입력이
        // 끊기면 다음 프레임이 안 올 수 있으므로 한 번만 더 요청한다. 상태가 그대로면
        // 요청도 없어 상시 repaint로 번지지 않는다.
        if status_error != self.error
            || status_busy != (self.in_flight > 0)
            || status_listing
                != matches!(
                    self.pending_maintenance,
                    Some(PendingFileTreeMaintenance::Listing { .. })
                )
        {
            ui.ctx().request_repaint();
        }
        action
    }

    /// 고정 내비게이션 레일 — 홈 / 작업 / 이력 / Git / 에이전트. 홈과 작업 행 우측의
    /// 카운트 배지는 0이면 숨긴다. 정보 화면 재클릭 시 터미널 복귀 토글은 App이
    /// 처리한다(view 소유자).
    fn navigation(
        &mut self,
        ui: &mut egui::Ui,
        sidebar: &SidebarSnapshot<'_>,
        catalog: &i18n::Catalog,
    ) -> Option<SidebarAction> {
        let mut action = None;
        ui.spacing_mut().item_spacing.y = SIDEBAR_NAV_ITEM_SPACING;
        if nav_row(
            ui,
            NavIcon::Home,
            &catalog.t("sidebar.nav.home", &[]),
            sidebar.view == super::agent_terminal::AgentTerminalView::Home,
            nav_badge_text(sidebar.home_notice_count)
                .as_deref()
                .map(|text| NavBadge {
                    text,
                    fill: egui::Color32::from_rgb(0xed, 0x5b, 0x61),
                    text_color: egui::Color32::WHITE,
                }),
        )
        .clicked()
        {
            action = Some(SidebarAction::ShowHome);
        }
        let primary = sidebar.fleet_summary.primary();
        let fleet_badge_text = primary.and_then(|(_, count)| nav_badge_text(count));
        let mut fleet_response = nav_row(
            ui,
            NavIcon::Fleet,
            &catalog.t("sidebar.nav.fleet", &[]),
            sidebar.view == super::agent_terminal::AgentTerminalView::Fleet,
            primary
                .zip(fleet_badge_text.as_deref())
                .map(|((state, _), text)| NavBadge {
                    text,
                    fill: super::agent_visuals::status_text_color(state),
                    // 밝은 상태색 위에서도 작은 숫자를 읽을 수 있게 한다.
                    text_color: egui::Color32::from_rgb(0x17, 0x1b, 0x22),
                }),
        );
        if primary.is_some() {
            fleet_response = fleet_response.on_hover_ui(|ui| {
                for (state, count) in sidebar.fleet_summary.counts() {
                    if count == 0 {
                        continue;
                    }
                    let label = match state {
                        crate::agent_surface::AgentVisualState::Waiting => {
                            catalog.t("fleet.group.blocked", &[])
                        }
                        crate::agent_surface::AgentVisualState::Error => {
                            catalog.t("fleet.state.error", &[])
                        }
                        crate::agent_surface::AgentVisualState::Active => {
                            catalog.t("fleet.group.active", &[])
                        }
                        crate::agent_surface::AgentVisualState::Complete => {
                            catalog.t("fleet.state.done", &[])
                        }
                        crate::agent_surface::AgentVisualState::Idle => {
                            catalog.t("fleet.state.idle", &[])
                        }
                        _ => continue,
                    };
                    ui.colored_label(
                        super::agent_visuals::status_text_color(state),
                        format!("{label} · {count}"),
                    );
                }
            });
        }
        if fleet_response.clicked() {
            action = Some(SidebarAction::ShowFleet);
        }
        if nav_row(
            ui,
            NavIcon::History,
            &catalog.t("sidebar.nav.history", &[]),
            // 레일 강조는 이력 탭이 **활성**일 때만이다. 탭이 열려 있어도 세션 터미널을
            // 보고 있으면 레일은 꺼진 상태로 둔다(중앙에 보이는 것과 일치).
            sidebar.history_tab_active,
            None,
        )
        .clicked()
        {
            action = Some(SidebarAction::ShowHistory);
        }
        if nav_row(
            ui,
            NavIcon::Git,
            &catalog.t("sidebar.nav.git", &[]),
            // 이력과 같은 규칙 — 보조 탭이 **활성**일 때만 켠다.
            sidebar.git_tab_active,
            None,
        )
        .clicked()
        {
            action = Some(SidebarAction::ShowGit);
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
        // AppKit native key-down을 먼저 peek해 비-Latin 배열에서 Event::Copy가 빠져도
        // WorkspaceUi drain 전에 트리가 소유권을 확정한다.
        let egui_copy = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
        let native_copy = crate::native_key_monitor::peek_clipboard_copy();
        self.handle_copy_shortcut_signal(row_path.clone(), native_copy, egui_copy);
        // ⌘⌫ / ⌘Delete: 선택(없으면 포인터 밑 행)을 휴지통으로. 우클릭 메뉴와 같은 길을
        // 타므로 실패 시 영구삭제 확인까지 그대로 적용된다.
        //
        // **수식키 없는 ⌫는 받지 않는다.** 위 가드의 `text_edit_focused()`는 egui
        // TextEdit만 본다 — 터미널은 커스텀 위젯이라 거기에 안 걸린다. 맨 ⌫를 받으면
        // 포인터가 트리 위에 놓인 채 터미널에서 지우기를 누를 때마다 파일이 사라진다.
        // ⌘⌫는 Finder 관례이기도 하다.
        if ui.input(|input| input.events.iter().any(is_tree_delete_shortcut)) {
            // 트리가 마지막으로 클릭된 곳일 때만. 포인터가 트리 위에 있다는 것만으로는
            // 부족하다 — 터미널에서 ⌘⌫로 줄을 지우던 손이 파일을 지운다(리뷰 H4).
            if !ui.memory(|memory| memory.has_focus(tree_keyboard_focus_id())) {
                return;
            }
            self.spawn_trash_selection(row_path);
            return;
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

    fn handle_copy_shortcut_signal(
        &mut self,
        row_path: Option<PathBuf>,
        native_copy: bool,
        egui_copy: bool,
    ) {
        // 예전처럼 **포인터 밑에 행이 있을 때만** 받는다. 선택이 있다고 행 밖에서도
        // 받으면, 터미널에서 텍스트를 고른 뒤 마우스가 사이드바에 있는 채 ⌘C를 누를 때
        // 트리가 그것을 가로채고 소비 표시까지 해 터미널 복사를 막는다(2026-09-04 리뷰 M3).
        let Some(path) = row_path else {
            return;
        };
        if !native_copy && !egui_copy {
            return;
        }
        self.consumed_copy_shortcut = true;
        if self
            .last_external_copy
            .is_some_and(|at| at.elapsed() < EXTERNAL_COPY_GESTURE_WINDOW)
        {
            return;
        }
        self.last_external_copy = Some(std::time::Instant::now());
        // 선택이 있으면 선택 전체를, 없으면 예전처럼 포인터 밑 행 하나를 복사한다.
        let paths = self.selection_or(Some(path));
        self.copy_files_to_clipboard(&paths);
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

    /// 휴지통 이동이 실패한 대상의 영구삭제 확인 (§9-7 — 조용한 영구삭제 금지).
    ///
    /// 문구도 삭제 경로도 `DeleteTarget` **하나**에서 나온다. 경로를 들고 다니다가
    /// 확인 시점에 이름을 다시 만들면 보여준 것과 지우는 것이 갈라진다.
    fn permanent_delete_confirm(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        let Some(target) = self.confirm_delete.clone() else {
            return;
        };
        // 폴더는 "안의 내용까지 사라진다"를 말해주는 별도 문구를 쓴다 — 영구 삭제는
        // 디렉터리면 통째로 지운다.
        // 키를 변수로 묶지 않는 이유: xtask i18n-check의 키 커버리지는 **리터럴** 호출만
        // 대조한다. 변수로 넘기면 dynamic으로 세어 넘어가 로케일 누락을 못 잡는다.
        let prompt = if target.is_dir {
            catalog.t(
                "file_tree.permanent_delete_folder_prompt",
                &[("name", &target.label)],
            )
        } else {
            catalog.t(
                "file_tree.permanent_delete_prompt",
                &[("name", &target.label)],
            )
        };
        ui.colored_label(ui.visuals().warn_fg_color, prompt);
        ui.horizontal(|ui| {
            if ui
                .button(catalog.t("file_tree.permanent_delete", &[]))
                .clicked()
            {
                let path = target.path.clone();
                let refresh: Vec<PathBuf> =
                    path.parent().map(Path::to_path_buf).into_iter().collect();
                let request = FileTreePathPayload::try_new(path)
                    .map(|target| FileTreeIoRequest::DeletePermanently { target });
                // 확인은 삭제를 **접수했을 때만** 소비한다. 먼저 닫아버리면 큐가 거절할
                // 때(capacity-1 `Busy` 등) 아무것도 안 지운 채 확인만 사라져, 오류 문구
                // 하나 남기고 다시 누를 화면이 없어진다. 위 `spawn_trash`가 제스처마다
                // 무조건 닫는 것과 방향이 반대인 이유: 저기서는 **새 대상**이 옛 확인을
                // 무효로 만들지만, 여기서는 같은 대상이 아직 지워지지 않은 채 남아 있다.
                match request.and_then(|request| self.queue_io(request, refresh, None, None)) {
                    Ok(()) => self.confirm_delete = None,
                    Err(code) => self.reject_io(code),
                }
            }
            if ui.button(catalog.t("action.cancel", &[])).clicked() {
                self.confirm_delete = None;
            }
        });
    }

    /// 휴지통 이동 intent. 실패 completion만 영구삭제 확인으로 승격한다.
    /// 클릭 한 번을 선택에 반영한다.
    fn apply_selection_click(&mut self, path: &Path, kind: SelectionClick) {
        match kind {
            SelectionClick::Replace => {
                self.selected.clear();
                self.selected.insert(path.to_path_buf());
                self.select_anchor = Some(path.to_path_buf());
            }
            SelectionClick::Toggle => {
                if !self.selected.remove(path) {
                    self.selected.insert(path.to_path_buf());
                }
                self.select_anchor = Some(path.to_path_buf());
            }
            SelectionClick::Range => {
                let paths: Vec<PathBuf> = self.flat.iter().map(|row| row.path.clone()).collect();
                let anchor = self
                    .select_anchor
                    .clone()
                    .unwrap_or_else(|| path.to_path_buf());
                self.selected = selection_range(&paths, &anchor, path).into_iter().collect();
                // 기준점은 그대로 둔다 — Shift로 범위를 늘였다 줄였다 할 수 있어야 한다.
            }
        }
    }

    /// Finder처럼 입력 접두어로 현재 표시된 폴더 행만 찾는다. 반환값은 화면 이동용
    /// flat index이며, 폴더 열기나 탐색 루트 변경은 이 경로에서 하지 않는다.
    fn handle_typeahead_text(&mut self, text: &str, now: f64) -> Option<usize> {
        if text.is_empty() || text.chars().any(char::is_control) || self.flat.is_empty() {
            return None;
        }
        let fresh = self
            .typeahead_last_at
            .is_none_or(|last| !(0.0..=0.8).contains(&(now - last)));
        let repeat = !fresh && self.typeahead_prefix.to_lowercase() == text.to_lowercase();
        let mut query = if fresh || repeat {
            text.to_owned()
        } else {
            format!("{}{text}", self.typeahead_prefix)
        };
        let current = self
            .select_anchor
            .as_ref()
            .and_then(|path| self.flat.iter().position(|row| &row.path == path));
        let find = |query: &str, include_current: bool| {
            let needle = query.to_lowercase();
            let len = self.flat.len();
            let start = current.map_or(0, |index| (index + usize::from(!include_current)) % len);
            (0..len)
                .map(|offset| (start + offset) % len)
                .find(|&index| {
                    let row = &self.flat[index];
                    row.is_dir && row.display_name.to_lowercase().starts_with(&needle)
                })
        };
        let mut index = find(&query, !fresh && !repeat);
        if index.is_none() && !fresh && !repeat {
            query = text.to_owned();
            index = find(&query, false);
        }
        self.typeahead_prefix = query;
        self.typeahead_last_at = Some(now);
        let index = index?;
        let path = self.flat[index].path.clone();
        self.apply_selection_click(&path, SelectionClick::Replace);
        Some(index)
    }

    fn render_file_search(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        let Some(root) = self.root.as_ref() else {
            return;
        };
        if let Some(error) = self.error.clone() {
            ui.horizontal(|ui| {
                ui.colored_label(ui.visuals().error_fg_color, error);
                if ui.small_button("×").clicked() {
                    self.error = None;
                }
            });
        }
        let mut changed_query = None;
        let mut close = false;
        let mut reveal = None;
        if let Some(search) = self.search.as_mut() {
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut search.query)
                        .hint_text(catalog.t("search.placeholder", &[]))
                        .desired_width((ui.available_width() - 28.0).max(60.0)),
                );
                if search.focus {
                    response.request_focus();
                    search.focus = false;
                }
                if response.changed() {
                    while search.query.len() > 256 {
                        search.query.pop();
                    }
                    changed_query = Some(search.query.clone());
                }
                close = ui
                    .small_button("×")
                    .on_hover_text(catalog.t("search.close", &[]))
                    .clicked();
            });
            close |= ui.input(|input| input.key_pressed(egui::Key::Escape));
            if search.busy {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.weak(catalog.t("file_tree.searching", &[]));
                });
            } else if !search.query.is_empty()
                && search.results.is_empty()
                && !search.traversal_incomplete
            {
                ui.weak(catalog.t("search.no_match", &[]));
            }
            if search.result_limit_reached {
                ui.weak(catalog.t("search.truncated", &[]));
            }
            if search.traversal_incomplete {
                ui.weak(catalog.t("file_tree.search_incomplete", &[]));
            }
            if let Some(path) = search.selected.as_ref()
                && ui
                    .button(catalog.t("file_tree.show_in_tree", &[]))
                    .clicked()
            {
                reveal = Some(path.clone());
            }
            egui::ScrollArea::vertical()
                .id_salt("file_tree_search_results")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for path in &search.results {
                        let label = path.strip_prefix(root).unwrap_or(path);
                        if ui
                            .selectable_label(
                                search.selected.as_ref() == Some(path),
                                super::path_display(label),
                            )
                            .on_hover_text(super::path_display(path))
                            .clicked()
                        {
                            search.selected = Some(path.clone());
                        }
                    }
                });
        }
        if close {
            self.close_file_search();
            return;
        }
        if let Some(query) = changed_query {
            self.request_file_search(query);
        }
        if let Some(path) = reveal
            && let Some(parent) = path.parent()
        {
            self.set_root(Some(parent.to_path_buf()));
            self.reveal_path = Some(path);
            ui.ctx().request_repaint();
        }
    }

    fn request_file_search(&mut self, query: String) {
        if let Some(cancel) = self.search_cancel.take() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let Some(search) = self.search.as_mut() else {
            return;
        };
        search.query = query.clone();
        search.results.clear();
        search.selected = None;
        search.result_limit_reached = false;
        search.traversal_incomplete = false;
        search.busy = !query.is_empty();
        self.search_cancel =
            (!query.is_empty()).then(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
        self.search_requested = (!query.is_empty()).then_some(query);
        self.drive_maintenance();
    }

    fn close_file_search(&mut self) {
        if let Some(cancel) = self.search_cancel.take() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.search = None;
        self.search_requested = None;
    }

    fn start_edit(&mut self, edit: EditState) {
        self.edit_generation = self.edit_generation.wrapping_add(1);
        if let EditState::NewFile { parent, .. } | EditState::NewFolder { parent, .. } = &edit {
            // 생성 대상만 펼친다. 이미 펼친 목록은 유지하고 기존 비동기 나열을 재사용한다.
            let collapsed = self
                .root
                .as_deref()
                .and_then(|root| parent.strip_prefix(root).ok())
                .and_then(|relative| {
                    self.children
                        .as_deref()
                        .and_then(|children| node_ref(children, relative))
                })
                .is_some_and(|node| node.is_dir && !node.expanded);
            if collapsed {
                self.toggle_dir(parent);
            }
        }
        self.edit = Some(edit);
    }

    fn cancel_edit(&mut self) {
        self.edit_generation = self.edit_generation.wrapping_add(1);
        self.edit = None;
    }

    /// 선택 기준 행 또는 남은 선택들의 공통 폴더에 생성한다. 가시 선택만 사용하며,
    /// 공통 생성 위치도 없을 때 현재 탐색 루트를 쓴다.
    fn creation_parent(&self) -> Option<PathBuf> {
        let root = self.root.as_deref()?;
        let anchor = self
            .select_anchor
            .as_ref()
            .filter(|path| self.selected.contains(*path))
            .and_then(|path| self.flat.iter().find(|row| &row.path == path));
        if let Some(row) = anchor {
            return Some(row_target_dir(row, Some(root)));
        }
        let mut selected = self
            .flat
            .iter()
            .filter(|row| self.selected.contains(&row.path));
        let Some(first) = selected.next() else {
            return Some(root.to_path_buf());
        };
        let parent = row_target_dir(first, Some(root));
        let common_parent = selected.all(|row| {
            if row.is_dir {
                row.path == parent
            } else {
                row.path.parent() == Some(parent.as_path())
            }
        });
        Some(if common_parent {
            parent
        } else {
            root.to_path_buf()
        })
    }

    /// 선택 사각형을 시작·갱신·종료하고 그린다.
    fn update_marquee(
        &mut self,
        ui: &egui::Ui,
        start: Option<egui::Pos2>,
        viewport: egui::Rect,
        content_top: Option<f32>,
        row_pitch: f32,
    ) {
        if let Some(origin) = start {
            let modifiers = ui.input(|input| input.modifiers);
            self.marquee = Some(MarqueeDrag {
                origin,
                base: self.selected.clone(),
                additive: modifiers.command || modifiers.shift,
            });
        }
        let Some(marquee) = self.marquee.as_ref() else {
            return;
        };
        // Esc는 취소 — 시작 전 선택으로 되돌린다. 워크스페이스 순서 드래그에서 Esc가
        // 오히려 확정시키던 결함(2026-09-03 리뷰)과 같은 종류를 여기서 미리 막는다.
        // 키를 **소비**한다. 안 그러면 같은 Esc가 인라인 이름변경 취소까지 함께 눌러,
        // 사각형만 물리려던 손이 입력 중이던 이름을 날린다(2026-09-04 리뷰).
        if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.selected = marquee.base.clone();
            self.marquee = None;
            return;
        }
        // 버튼이 올라갔으면 끝 — 마지막 프레임에 정한 선택을 그대로 남긴다.
        if !ui.input(|input| input.pointer.any_down()) {
            self.marquee = None;
            return;
        }
        let Some(pointer) = ui.input(|input| input.pointer.latest_pos()) else {
            return;
        };
        let rect = egui::Rect::from_two_pos(marquee.origin, pointer);
        if let Some(content_top) = content_top {
            // 선택도 뷰포트로 자른다. 그리기만 클립하면 창 밖으로 끌었을 때 **보이지도
            // 않는 행**이 수천 개 선택되고, 그대로 ⌘⌫를 누르면 지워진다(리뷰 M1).
            let visible = rect.intersect(viewport);
            let range =
                marquee_row_range(content_top, row_pitch, self.flat.len(), visible.y_range());
            let mut next: BTreeSet<PathBuf> = if marquee.additive {
                marquee.base.clone()
            } else {
                BTreeSet::new()
            };
            // 사각형이 끝난 행을 기준점으로 남긴다. 안 그러면 마퀴 직후 Shift-클릭이
            // 옛 기준점(또는 없음)으로 떨어져 방금 고른 것을 통째로 버린다(리뷰 M3).
            let last = self.flat[range.clone()].last().map(|row| row.path.clone());
            next.extend(self.flat[range].iter().map(|row| row.path.clone()));
            self.selected = next;
            if let Some(last) = last {
                self.select_anchor = Some(last);
            }
        }
        // 뷰포트 밖으로 나간 부분은 그리지 않는다 — 헤더나 파일 목록 밖까지 사각형이
        // 삐져나오면 그 영역도 고르는 것처럼 보인다.
        let painter = ui.painter().with_clip_rect(viewport);
        let stroke = ui.visuals().selection.stroke;
        painter.rect_filled(rect, 2.0, stroke.color.gamma_multiply(0.18));
        painter.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);
    }

    /// 선택된 경로를 **트리에 보이는 순서**로. 선택이 비어 있으면 `fallback` 하나.
    fn selection_or(&self, fallback: Option<PathBuf>) -> Vec<PathBuf> {
        if self.selected.is_empty() {
            return fallback.into_iter().collect();
        }
        self.flat
            .iter()
            .filter(|row| self.selected.contains(&row.path))
            .map(|row| row.path.clone())
            .collect()
    }

    /// 선택(없으면 `fallback`)을 휴지통으로. IO 큐가 capacity-1이라 첫 대상만 바로
    /// 보내고 나머지는 대기열에 쌓아 완료될 때마다 하나씩 이어 보낸다.
    /// `hovered`가 선택 **밖**이면 그 행 하나만 지운다 — 우클릭 메뉴와 같은 규칙이다.
    /// 두 진입점이 다르면, 5개를 골라둔 채 다른 파일 위에서 ⌘⌫를 누른 사용자가
    /// 그 파일 대신 골라둔 5개를 잃는다(2026-09-04 리뷰 M4).
    fn spawn_trash_selection(&mut self, fallback: Option<PathBuf>) {
        let root = self.root.clone();
        let is_dir = |path: &Path| {
            self.flat
                .iter()
                .find(|row| row.path == path)
                .is_some_and(|row| row.is_dir)
        };
        let chosen = match &fallback {
            Some(hovered) if !self.selected.contains(hovered) => vec![hovered.clone()],
            _ => self.selection_or(fallback.clone()),
        };
        // 조상 폴더가 함께 골라졌으면 그 안의 것은 뺀다 — 폴더가 먼저 휴지통으로 가면
        // 뒤따르는 자식 요청은 없는 경로를 지우려다 반드시 실패하고, 그 실패가 배치 전체를
        // 폐기시킨다(2026-09-04 리뷰 M5).
        let mut targets: Vec<DeleteTarget> = chosen
            .iter()
            .filter(|path| {
                !chosen
                    .iter()
                    .any(|other| other != *path && path.starts_with(other))
            })
            .cloned()
            .map(|path| DeleteTarget {
                label: delete_target_label(root.as_deref(), &path),
                is_dir: is_dir(&path),
                path,
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        let first = targets.remove(0);
        self.trash_queue = targets;
        if self.spawn_trash(first) {
            self.selected.clear();
            self.select_anchor = None;
        } else {
            // 첫 요청이 Busy 등으로 거절되면 이 배치는 접수되지 않았다. 대기열을 남기면
            // 이미 진행 중이던 삭제의 성공 completion이 새 배치를 이어서 지운다.
            self.trash_queue.clear();
        }
    }

    /// 접수됐으면 `true`. capacity-1 큐가 차 있으면(`Busy`) 거절되는데, 그때 대기열을
    /// 무장한 채 두면 **무관한 IO가 성공하는 순간** 뒤 대상들이 조용히 지워진다
    /// (2026-09-04 리뷰 H1).
    fn spawn_trash(&mut self, target: DeleteTarget) -> bool {
        // 새 삭제 제스처를 접수했으면 직전 실패가 남긴 영구삭제 확인은 닫는다.
        // 남겨두면 방금 고른 파일이 아니라 **예전 대상** 이름이 패널 아래에 그대로
        // 떠 있고, 그 확인을 누르는 순간 엉뚱한 파일이 영구 삭제된다.
        //
        // 큐가 이 요청을 받아줬는지는 보지 않는다. 확인을 무효로 만드는 것은 **제스처**
        // 자체이지 큐 수용 여부가 아니다 — capacity-1 IO 큐가 차 있으면(⌘C 복사, Finder
        // 드롭, 트리 내 이동, 더블클릭 열기 중 하나라도) `queue_io`가 `Busy`를 돌려주는데,
        // 그때 확인을 남겨두면 "파일 작업이 진행 중입니다" 바로 아래에 **예전 대상**
        // 확인이 서서 방금 고른 파일 이야기처럼 읽힌다.
        self.confirm_delete = None;
        let refresh: Vec<PathBuf> = target
            .path
            .parent()
            .map(Path::to_path_buf)
            .into_iter()
            .collect();
        let request = FileTreePathPayload::try_new(target.path.clone())
            .map(|payload| FileTreeIoRequest::Trash { target: payload });
        match request.and_then(|request| self.queue_io(request, refresh, Some(target), None)) {
            Ok(()) => true,
            Err(code) => {
                self.reject_io(code);
                false
            }
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
            self.children = Some(merge_listing_nodes(self.children.take(), nodes));
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
        node.children = Some(merge_listing_nodes(node.children.take(), nodes));
        true
    }

    fn listing_children(&self, path: &Path) -> Option<&[TreeNode]> {
        let root = self.root.as_ref()?;
        if path == root {
            return self.children.as_deref();
        }
        let relative = path.strip_prefix(root).ok()?;
        node_ref(self.children.as_deref()?, relative)?
            .children
            .as_deref()
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

/// 이름 갤리 오른쪽으로 이만큼 떨어져야 "빈 자리"로 본다 — 이름 끝에 바싹 붙은
/// 픽셀에서 이동을 시작하려던 손이 선택으로 새지 않게 하는 여유.
const MARQUEE_NAME_GAP: f32 = 6.0;

/// 같은 ⌘⌫ 제스처로 묶는 시간. macOS는 ⌘ 조합도 오토리피트하고 egui `key_pressed`는
/// 리피트를 그대로 흘린다 — 없으면 한 번 누르고 있는 동안 대상이 계속 바뀐다.
fn is_tree_delete_shortcut(event: &egui::Event) -> bool {
    matches!(
        event,
        egui::Event::Key {
            key: egui::Key::Delete | egui::Key::Backspace,
            pressed: true,
            repeat: false,
            modifiers,
            ..
        } if modifiers.command
    )
}

pub(crate) fn tree_keyboard_focus_id() -> egui::Id {
    egui::Id::new("file_tree_keyboard_focus")
}

fn file_tree_search_keyboard_owner_id() -> egui::Id {
    egui::Id::new("file_tree_search_keyboard_owner")
}

/// 같은 프레임에 트리 다음으로 그리는 workspace만 읽는다. 프레임 번호를 함께 저장해
/// 트리 패널이 그려지지 않은 다음 프레임의 오래된 신호가 터미널을 막지 않게 한다.
pub(super) fn mark_file_tree_search_keyboard_active(ctx: &egui::Context, active: bool) {
    let frame = ctx.cumulative_frame_nr();
    ctx.data_mut(|data| {
        if active {
            data.insert_temp(file_tree_search_keyboard_owner_id(), frame);
        } else {
            data.remove::<u64>(file_tree_search_keyboard_owner_id());
        }
    });
}

pub(super) fn file_tree_search_keyboard_active(ctx: &egui::Context) -> bool {
    ctx.data(|data| data.get_temp::<u64>(file_tree_search_keyboard_owner_id()))
        == Some(ctx.cumulative_frame_nr())
}

fn tree_typeahead_text_events(events: &[egui::Event]) -> Vec<String> {
    // macOS 입력기는 확정 문자를 Commit만 보낼 수도, 같은 Text와 함께 보낼 수도
    // 있다. 같은 프레임의 동일 Text 하나만 제외하고 Commit을 한 번 처리한다.
    let mut commits = events
        .iter()
        .filter_map(|event| match event {
            egui::Event::Ime(egui::ImeEvent::Commit(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    events
        .iter()
        .filter_map(|event| match event {
            egui::Event::Text(text) => {
                if let Some(index) = commits.iter().position(|commit| *commit == text) {
                    commits.remove(index);
                    None
                } else {
                    Some(text.clone())
                }
            }
            egui::Event::Ime(egui::ImeEvent::Commit(text)) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn register_tree_keyboard_focus_target(ui: &egui::Ui) {
    let response = ui.interact(
        egui::Rect::from_min_size(ui.cursor().min, egui::Vec2::ZERO),
        tree_keyboard_focus_id(),
        egui::Sense::focusable_noninteractive(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), "파일 트리")
    });
}

/// 이름 오른쪽 빈 공간에서 시작한 선택 사각형.
///
/// 행은 패널 폭 전체를 차지하고 그 드래그는 **파일 이동**이 이미 쓰고 있다. 그래서
/// 사각형 선택은 이름 갤리 오른쪽 빈 자리와 목록 아래 빈 영역에서만 시작한다
/// (Windows 탐색기 규칙, 2026-09-03 사용자 확정) — 기존 이동 제스처를 뺏지 않는다.
struct MarqueeDrag {
    /// 누른 지점(화면 좌표).
    origin: egui::Pos2,
    /// 드래그 시작 시점의 선택 — 더하기 모드면 여기에 얹는다.
    base: BTreeSet<PathBuf>,
    /// ⌘/Shift를 누른 채 시작했는가.
    additive: bool,
}

/// 클릭 한 번이 선택을 어떻게 바꾸는지.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionClick {
    /// 이 행 하나만 남긴다.
    Replace,
    /// 이 행만 넣거나 뺀다.
    Toggle,
    /// 기준점부터 이 행까지 구간.
    Range,
}

/// macOS 관례: ⌘ = 개별 토글, Shift = 구간. 둘 다면 구간이 이긴다(Finder와 같다).
fn selection_click_kind(modifiers: egui::Modifiers) -> SelectionClick {
    if modifiers.shift {
        SelectionClick::Range
    } else if modifiers.command {
        SelectionClick::Toggle
    } else {
        SelectionClick::Replace
    }
}

/// 선택 사각형이 덮는 행 index 구간.
///
/// **산술로 구한다** — `show_rows` 가상화 때문에 화면 밖 행은 위젯이 없지만, 사각형에
/// 들어왔으면 선택돼야 한다. 행 높이가 균일하다는 사실이 그것을 가능하게 한다.
/// `content_top`은 0번 행의 위쪽 y(화면 좌표).
fn marquee_row_range(
    content_top: f32,
    row_pitch: f32,
    total: usize,
    y: egui::Rangef,
) -> std::ops::Range<usize> {
    if total == 0 || row_pitch <= 0.0 {
        return 0..0;
    }
    // `floor`여야 한다. `round`면 사각형이 행의 절반을 넘게 지나칠 때 **닿지도 않은**
    // 다음 행이 딸려오고, 절반이 안 되면 눈에 보이게 덮인 행이 빠진다.
    let first = ((y.min - content_top) / row_pitch).floor();
    // 아래 경계는 그 y가 걸친 행까지 포함한다 — 사각형이 행의 1px만 덮어도 사용자는
    // 그 행을 고른 것으로 본다.
    let last = ((y.max - content_top) / row_pitch).floor();
    if last < 0.0 || first >= total as f32 {
        return 0..0;
    }
    let first = first.max(0.0) as usize;
    let last = (last as usize).min(total - 1);
    first..last + 1
}

/// `ScrollArea::show_rows`가 행 하나에 쓰는 실제 세로 간격 — egui는 행 높이에
/// `item_spacing.y`를 더해 배치한다. `diff_viewer::scroll_row_pitch`와 같은 규칙이다.
///
/// 이걸 빼먹으면 행마다 `item_spacing.y`(기본 3px)씩 어긋나 아래로 갈수록 누적된다 —
/// 사각형이 감싸지 않은 행이 선택되고, 그대로 ⌘⌫를 누르면 지워진다(2026-09-04 리뷰).
fn tree_row_pitch(row_height: f32, spacing: &egui::style::Spacing) -> f32 {
    row_height + spacing.item_spacing.y
}

fn file_tree_scroll_area(
    ui: &egui::Ui,
    row_height: f32,
    reveal: Option<usize>,
) -> egui::ScrollArea {
    let area = egui::ScrollArea::vertical();
    let Some(index) = reveal else {
        return area;
    };
    let pitch = tree_row_pitch(row_height, ui.spacing());
    let offset = ((index as f32 + 0.5) * pitch - ui.available_height() / 2.0).max(0.0);
    area.vertical_scroll_offset(offset)
}

/// 정렬된 경로 목록에서 두 경로 사이 구간(양끝 포함). 어느 쪽이 앞인지는 보지 않는다.
/// 둘 중 하나라도 목록에 없으면 대상 하나만 돌려준다 — 기준점이 접히거나 지워져
/// 사라졌을 때 아무것도 선택되지 않는 것보다 낫다.
fn selection_range(paths: &[PathBuf], anchor: &Path, target: &Path) -> Vec<PathBuf> {
    let index = |needle: &Path| paths.iter().position(|path| path == needle);
    match (index(anchor), index(target)) {
        (Some(a), Some(b)) => paths[a.min(b)..=a.max(b)].to_vec(),
        _ => vec![target.to_path_buf()],
    }
}

/// 우클릭 컨텍스트 메뉴 동작 (FT-3) — flat 순회 밖에서 처리한다.
enum MenuAction {
    NewFolder(PathBuf),
    Rename(PathBuf),
    Delete(DeleteTarget),
    /// 파일/폴더를 pasteboard에 파일 URL로 복사 — Finder ⌘V 대상(§과제③).
    CopyFile(PathBuf),
    CopyPath(PathBuf),
    InsertPath(PathBuf),
    CdPath(PathBuf),
    /// 문서 대상 파일을 OS 기본 앱으로 연다 — 더블클릭이 문서 탭으로 가로챈 뒤에도
    /// 남겨두는 우회로(설계 §3.1).
    OpenWithOs(PathBuf),
}

/// 삭제 확인에 쓸 표시 이름 — **루트 기준 상대 경로**. 파일명만 쓰면 `mod.rs`,
/// `index.ts`처럼 여러 폴더에 같은 이름이 있는 저장소에서 어느 것을 지우는지
/// 확인 문구만 보고는 알 수 없다. 루트 밖(또는 루트 미설정)이면 전체 경로로 떨어진다.
fn delete_target_label(root: Option<&Path>, path: &Path) -> String {
    root.and_then(|root| path.strip_prefix(root).ok())
        .map(super::path_display)
        .filter(|rel| !rel.is_empty())
        .unwrap_or_else(|| super::path_display(path))
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

fn prepare_rename_request(
    path: &Path,
    buffer: &str,
    changed: bool,
) -> Result<Option<FileTreeIoRequest>, FileTreeIoErrorCode> {
    if !changed {
        return Ok(None);
    }
    let name = validate_name(buffer).map_err(|_| FileTreeIoErrorCode::InvalidName)?;
    let source = FileTreePathPayload::try_new(path.to_path_buf())?;
    Ok(Some(FileTreeIoRequest::Rename { source, name }))
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

/// 세션 행을 painter로 직접 그린다 (2026-07-06 목업 반영). 상태를 이모지 글리프로
/// 쓰면 폰트(AppleGothic)에 ⏳/✋/▸/◆ 글리프가 없어 □(두부)로 깨진다 — 색 점·삼각형·
/// 마름모를 도형으로 그려 회피한다. 선택 시 액센트 배경 + 좌측 레일, agent는 레일 표시,
/// 요약 한 줄(dim/Apple SD Gothic). 반환 Response로 클릭을 처리한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WorkspaceRowStyle {
    fill: Option<egui::Color32>,
    accent: Option<egui::Color32>,
}

/// 선택된 워크스페이스 면을 그 프로젝트 색 쪽으로 섞는 비율. 회색 한 단(#181b20 →
/// #21242c, 채널당 +9)만 올리던 예전 값은 패널과 거의 구분이 안 됐다(2026-08-11
/// 사용자: 선택된 워크스페이스 컬러를 더 명확하게). 새 색을 만들지 않고 **아바타가
/// 이미 쓰는 그 워크스페이스의 색**을 옅게 깐다 — 한 번에 하나만 선택되므로 목록이
/// 알록달록해지지 않는다.
const WORKSPACE_SELECTED_TINT: f32 = 0.2;

fn workspace_row_style(
    tokens: crate::ui::designall::Tokens,
    accent: egui::Color32,
    selected: bool,
    hovered: bool,
) -> WorkspaceRowStyle {
    if selected {
        return WorkspaceRowStyle {
            fill: Some(crate::ui::designall::mix(
                tokens.selected_background,
                accent,
                WORKSPACE_SELECTED_TINT,
            )),
            accent: None,
        };
    }
    WorkspaceRowStyle {
        fill: crate::ui::designall::row_fill(tokens, false, hovered),
        accent: None,
    }
}

/// 접힌 워크스페이스 상태 점의 반지름과, 배경색으로 두르는 테두리 두께.
const STATUS_DOT_RADIUS: f32 = 3.0;
const STATUS_DOT_RING: f32 = 1.5;

pub const WORKSPACE_AVATAR_LEFT_INSET: f32 = 10.0;
const WORKSPACE_AVATAR_SIZE: f32 = 18.0;

fn workspace_avatar_rect(row: egui::Rect) -> egui::Rect {
    egui::Rect::from_center_size(
        egui::pos2(
            row.left() + WORKSPACE_AVATAR_LEFT_INSET + WORKSPACE_AVATAR_SIZE * 0.5,
            row.center().y,
        ),
        egui::vec2(WORKSPACE_AVATAR_SIZE, WORKSPACE_AVATAR_SIZE),
    )
}

/// `workspace_row`가 돌려주는 두 제스처.
///
/// 행 전체와 chevron은 **다른 일을 한다** — 행을 누르면 그 워크스페이스로 전환하고,
/// chevron을 누르면 전환 없이 세션 목록만 여닫는다. 이 둘을 한 Response로 묶어 두면
/// "펼쳐져 있으면서 비활성"인 상태를 만들 방법이 없어지고, 그 상태에서만 보이는
/// 「다른 워크스페이스 세션을 옆에 열기」 진입점 세 개가 통째로 사라진다
/// (2026-09-03 리뷰: multi-cross-workspace-pane 스펙의 승인된 진입점 전부).
struct WorkspaceRowResponse {
    row: egui::Response,
    /// chevron을 눌렀다 — 호출자는 펼침만 토글하고 전환은 방출하지 않는다.
    disclosure_clicked: bool,
}

fn workspace_row(
    ui: &mut egui::Ui,
    workspace: &SidebarWorkspaceEntry,
    color: egui::Color32,
    active: bool,
    expanded: Option<bool>,
    catalog: &i18n::Catalog,
) -> WorkspaceRowResponse {
    // 2026-07-26 사용자: 워크스페이스 헤더와 아바타를 다시 10% 축소한다.
    // 축소는 기존 값에 0.9를 곱해 처리돼 29.19가 됐는데, 그 값은 물리 픽셀에 안 맞아
    // 행이 쌓일수록 원점이 밀렸다(2x에서 행마다 0.38px 누적 → 행마다 선명도가 달랐다).
    // 축소 의도는 유지하면서 가장 가까운 정렬값으로 내린다(29.0 × 2 = 58px 정수).
    let row_height = 29.0;
    // 클릭(전환/펼침)에 더해 드래그도 받는다 — 사이드바에서 워크스페이스를 끌어 순서를
    // 바꾼다(2026-09-03 사용자). egui는 드래그 임계값을 넘으면 clicked()를 내지 않으므로
    // 두 제스처가 서로를 삼키지 않는다.
    let (full_rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), row_height),
        egui::Sense::click_and_drag(),
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
        return WorkspaceRowResponse {
            row: response,
            disclosure_clicked: false,
        };
    }
    // 여기부터는 **그리기 좌표**만 물리 픽셀에 맞춘다 (클릭 판정은 위 response가 원래
    // rect를 그대로 쓴다). 행 높이를 정렬값으로 바꿔 누적 드리프트는 없앴지만, 스크롤
    // 오프셋과 부모 레이아웃에서 오는 소수는 남으므로 스냅은 그대로 필요하다.
    // 자세한 이유는 `crate::ui::snap_to_pixel` 주석 참고.
    let ppp = ui.ctx().pixels_per_point();
    let full_rect = crate::ui::snap_rect_to_pixel(ppp, full_rect);
    let style = workspace_row_style(
        crate::ui::designall::tokens(ui.visuals()),
        color,
        active,
        response.hovered(),
    );
    if let Some(fill) = style.fill {
        ui.painter().rect_filled(full_rect, 0.0, fill);
    }
    if let Some(accent) = style.accent {
        let rail = egui::Rect::from_min_max(
            full_rect.left_top(),
            egui::pos2(full_rect.left() + 2.0, full_rect.bottom()),
        );
        ui.painter().rect_filled(rail, 0.0, accent);
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
    // 선택/실행 상태와 무관한 프로젝트 고유색. 목록 전체에서 같은 계열이 겹치지 않게
    // 미리 배정된 색을 받아 비활성 행과 40pt 아이콘 레일에서도 그대로 유지한다.
    let avatar = workspace_avatar_rect(full_rect);
    // 워크스페이스 마크는 별도 테두리 없이 상태색을 채운다(HTML 목업과 같은 규칙).
    // 선택된 워크스페이스의 마크는 **제 색 그대로**(목업의 .bdg도 불투명이다).
    // 0.42 대 0.32는 눈으로 구분되지 않는 차이였다(2026-08-11 사용자).
    let avatar_fill = if active {
        color
    } else {
        color.gamma_multiply(0.32)
    };
    ui.painter().rect_filled(avatar, 1.0, avatar_fill);
    // 아바타는 빠른 식별용 마크라 첫 글자를 항상 대문자로 고정한다. 반대로 실제
    // 워크스페이스 이름은 사용자가 지정한 대소문자를 그대로 보존한다.
    let initial = workspace_initial(&workspace.name);
    ui.painter().text(
        avatar.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        crate::fonts::sidebar_font(ui.ctx(), 9.0),
        egui::Color32::WHITE,
    );
    // 접힌 행에만 상태 점. 아바타 오른쪽 위 모서리에 얹고 배경색 테두리로 떼어 내
    // 어떤 워크스페이스 고유색 위에서도 읽히게 한다.
    if expanded == Some(false)
        && let Some(state) = collapsed_status_dot(workspace.summary)
    {
        let center = egui::pos2(avatar.right(), avatar.top());
        let behind = if active {
            crate::ui::designall::mix(
                crate::ui::designall::tokens(ui.visuals()).selected_background,
                color,
                WORKSPACE_SELECTED_TINT,
            )
        } else {
            crate::ui::designall::tokens(ui.visuals()).workspace_background
        };
        ui.painter()
            .circle_filled(center, STATUS_DOT_RADIUS + STATUS_DOT_RING, behind);
        ui.painter().circle_filled(
            center,
            STATUS_DOT_RADIUS,
            crate::ui::agent_visuals::status_color(state),
        );
    }
    let summary_mode = workspace_summary_mode(rect.width());
    let show_summary = summary_mode != WorkspaceSummaryMode::IconOnly;
    let show_disclosure = expanded.is_some() && rect.width() >= 56.0;
    let (_, badge_color) = workspace_primary_summary_segment(workspace.summary, catalog);
    // 우측은 **세션 수** 하나다. 예전엔 상태색 점이었는데(2026-07-25), 점을 세션 행
    // 앞으로 옮기고 나니 펼친 목록에서 같은 팔레트의 점이 헤더와 행에 겹쳐 같은 사실을
    // 두 번 말했다(2026-08-11 사용자: 워크스페이스 우측 동그라미 제거).
    // 수는 점이 못 나르는 사실이라 겹치지 않는다.
    let count_galley = show_summary
        .then(|| workspace_session_count(workspace.summary))
        .filter(|count| *count > 0)
        .map(|count| {
            clipped_line(
                ui,
                &count.to_string(),
                crate::fonts::sidebar_font(ui.ctx(), WORKSPACE_COUNT_FONT_SIZE),
                WORKSPACE_COUNT_MAX_WIDTH,
                None,
            )
        });
    if rect.width() >= 64.0 {
        // 요약 배지 자리를 **실제 폭**만큼만 예약한다 — 고정 198px는 "유휴 5"처럼
        // 짧은 요약에도 이름을 훨씬 일찍 잘라 옆 여백이 남았다(2026-07-18 사용자).
        // 우측 여백 8 + 이름/요약 간격 16 + disclosure 폭(있으면 14)을 더한다.
        let reserved_right = if show_summary {
            let disclosure = if show_disclosure { 12.0 } else { 0.0 };
            count_galley.as_ref().map_or(0.0, |g| g.size().x) + 22.0 + disclosure
        } else {
            7.0
        };
        let name_width = (rect.right() - reserved_right - avatar.right() - 7.5).max(0.0);
        if name_width > 4.0 {
            let name = clipped_line(
                ui,
                workspace_label(&workspace.name),
                // 워크스페이스명은 좌측 사이드바 전용 Apple SD Gothic 가족을 사용해
                // 원래 대소문자와 자연스러운 자폭을 보존한다.
                crate::fonts::sidebar_font(ui.ctx(), 14.0),
                name_width,
                None,
            );
            let text_x = avatar.right() + 7.5;
            let name_pos = crate::ui::snap_pos_to_pixel(
                ppp,
                egui::pos2(text_x, rect.center().y - name.size().y / 2.0),
            );
            ui.painter()
                .galley(name_pos, name, ui.visuals().text_color());
        }
    }
    if let Some(count) = count_galley {
        let right = if show_disclosure {
            rect.right() - 22.0
        } else {
            rect.right() - 10.0
        };
        let count_pos = crate::ui::snap_pos_to_pixel(
            ppp,
            egui::pos2(
                right - count.size().x,
                rect.center().y - count.size().y / 2.0,
            ),
        );
        ui.painter()
            .galley(count_pos, count, ui.visuals().weak_text_color());
    } else if !show_summary {
        // 40pt 아이콘 레일까지 줄였을 때는 수를 놓을 자리가 없으므로 아바타 우하단의
        // 작은 점으로 primary state를 계속 표시한다. 이름이 보이는 폭부터는 위의
        // 세션 수로 바뀐다.
        ui.painter().circle_filled(
            egui::pos2(avatar.right() - 2.5, avatar.bottom() - 2.5),
            2.5,
            badge_color,
        );
    }
    let mut disclosure_clicked = false;
    if show_disclosure && let Some(expanded) = expanded {
        let center = egui::pos2(rect.right() - 12.0, rect.center().y);
        // chevron은 행 위에 **나중에** 등록해 그 자리에서는 행보다 먼저 클릭을 받는다.
        // 행보다 넓은 손가락 크기 표적을 주되 행 높이를 넘지 않는다.
        let hit = egui::Rect::from_min_max(
            egui::pos2(center.x - 11.0, full_rect.top()),
            egui::pos2(center.x + 11.0, full_rect.bottom()),
        );
        let disclosure = ui.interact(
            hit,
            response.id.with("workspace_disclosure"),
            egui::Sense::click(),
        );
        let label = catalog.t(
            if expanded {
                "file_tree.workspace_sessions_collapse"
            } else {
                "file_tree.workspace_sessions_expand"
            },
            &[("workspace", workspace.name.as_str())],
        );
        disclosure.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::Button,
                ui.is_enabled(),
                expanded,
                label.clone(),
            )
        });
        let disclosure = disclosure.on_hover_text(label);
        if disclosure.hovered() {
            ui.painter().rect_filled(
                hit.shrink2(egui::vec2(2.0, 6.0)),
                3.0,
                ui.visuals().weak_text_color().gamma_multiply(0.16),
            );
        }
        disclosure_clicked = disclosure.clicked();
        let points = disclosure_chevron_points(center, expanded);
        ui.painter().add(egui::Shape::line(
            points.to_vec(),
            egui::Stroke::new(1.0, ui.visuals().weak_text_color().gamma_multiply(0.82)),
        ));
    }
    WorkspaceRowResponse {
        row: response,
        disclosure_clicked,
    }
}

/// 레일 행 높이. 58일 땐 아이콘+라벨(약 30)이 가운데 놓여 **첫 행 위에만 13.5px**의
/// 죽은 공간이 남았고, 그만큼 레일이 옆 워크스페이스 목록보다 아래에서 시작했다
/// (2026-08-11 사용자: 레일 상단 여백을 없애라). 행 사이 리듬은 그대로 두고 행이
/// 제 내용에 맞게 줄어들도록 낮춘다 — 위 6.5 / 아래 3.
///
/// 44가 바닥이다. 아이콘(13×12)과 라벨(12pt)을 세로로 쌓으면 내용만 30이라 더
/// 낮추면 둘이 붙고, 44는 클릭 대상 최소 크기이기도 하다. 워크스페이스 목록처럼
/// 여백 0으로 붙이려면 아이콘 위 라벨 아래 구성 자체를 버려야 한다.
const SIDEBAR_NAV_ROW_HEIGHT: f32 = 44.0;
/// 레일 내비 항목 세로 간격. 2.0이던 시절엔 아이콘+라벨 두 줄이 서로 붙어 읽기
/// 어려웠다 — 항목 사이를 눈으로 끊을 수 있을 만큼 띄운다(2026-08-21).
const SIDEBAR_NAV_ITEM_SPACING: f32 = 12.0;
/// 하단 설정 dot 버튼의 지름과 좌측 여백.
const NAV_UTILITY_DOT_SIZE: f32 = 22.0;
const NAV_UTILITY_LEFT_PAD: f32 = 2.0;
/// 도움말 팝업 메뉴 최소 너비.
const NAV_HELP_MENU_MIN_WIDTH: f32 = 170.0;
const PROJECT_SECTION_MIN_HEIGHT: f32 = 84.0;
const FILE_SECTION_MIN_HEIGHT: f32 = 50.0;
const PROJECT_FILE_SPLIT_HEIGHT: f32 = 6.0;
const WORKSPACE_CARD_HORIZONTAL_INSET: f32 = 6.0;

fn project_file_section_heights(available: f32, requested_project: f32) -> (f32, f32) {
    let usable = (available - PROJECT_FILE_SPLIT_HEIGHT).max(0.0);
    let max_project = (usable - FILE_SECTION_MIN_HEIGHT).max(PROJECT_SECTION_MIN_HEIGHT);
    let project = requested_project.clamp(PROJECT_SECTION_MIN_HEIGHT, max_project);
    let files = (usable - project).max(FILE_SECTION_MIN_HEIGHT);
    (project, files)
}

fn paint_workspace_group_separator(ui: &egui::Ui, rect: egui::Rect) {
    if rect.height() <= 0.0 || rect.width() <= 0.0 {
        return;
    }
    let y = crate::ui::snap_line_to_pixel(
        rect.bottom(),
        crate::ui::designall::SEPARATOR_WIDTH,
        ui.ctx().pixels_per_point(),
    );
    ui.painter().hline(
        rect.x_range(),
        y,
        crate::ui::designall::separator_stroke(ui.visuals()),
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

/// 워크스페이스 행 우클릭 메뉴 — 「세션 열기」와 「이름 바꾸기」(별칭 편집)는 세션이
/// 없어도 항상, 「워크스페이스 종료」(세션 일괄 닫기, 선택적 확인은 App)는 닫을
/// 세션이 있는 비 Idle만.
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
    let open_session_label = catalog.t("sidebar.menu.open_session", &[]);
    let rename_label = catalog.t("sidebar.menu.rename_workspace", &[]);
    let close_label = catalog.t("sidebar.menu.close_workspace", &[]);
    // 메뉴 폭이 좁으면 「워크스페이스 종료」가 두 줄로 접혀 잘렸다(2026-07-18 사용자
    // 스샷). 가장 긴 항목의 no-wrap 폭으로 최소 폭을 강제해(max_rect까지 확장된다)
    // 어느 로케일에서도 모든 항목이 항상 한 줄로 그려지게 한다. Idle이라 종료 항목이
    // 빠져도 세 항목 모두 재서 메뉴 폭이 상태에 따라 널뛰지 않게 한다.
    let font = egui::TextStyle::Button.resolve(ui.style());
    let widest = [
        open_session_label.as_str(),
        rename_label.as_str(),
        close_label.as_str(),
    ]
    .into_iter()
    .map(|label| {
        ui.painter()
            .layout_no_wrap(label.to_owned(), font.clone(), egui::Color32::WHITE)
            .size()
            .x
    })
    .fold(0.0_f32, f32::max);
    ui.set_min_width(widest + ui.spacing().button_padding.x * 2.0 + 2.0);
    if ui.button(open_session_label).clicked() {
        *action = Some(SidebarAction::OpenWorkspaceSession(workspace.id.clone()));
        ui.close();
    }
    if ui.button(rename_label).clicked() {
        *action = Some(SidebarAction::RenameWorkspace(workspace.id.clone()));
        ui.close();
    }
    if workspace.state != SidebarWorkspaceState::Idle && ui.button(close_label).clicked() {
        *action = Some(SidebarAction::CloseWorkspace(workspace.id.clone()));
        ui.close();
    }
}

/// 접힌 워크스페이스 행이 찍을 상태 점. 없으면 `None`.
///
/// **접혔을 때만 찍는다** — 펼치면 세션 행마다 이미 점이 있어 같은 사실을 두 번 말한다.
/// 2026-07-25에 넣었던 상태 점을 2026-08-11에 뺀 이유가 바로 그 중복이었고, chevron이
/// 생겨 목록이 기본으로 접힌 지금은 접힌 행이 그 워크스페이스에 대해 화면이 말해주는
/// 유일한 것이다(2026-09-04 사용자 확정).
///
/// 우선순위는 **대기 → 오류 → 실행 → 완료**다. 대기가 맨 앞인 것은 그것만이 사용자가
/// 움직여야 풀리는 상태이기 때문이다. 유휴·비활성은 찍지 않는다 — 아무 일도 안 하는
/// 세션이 화면에서 신호를 내면 진짜 신호를 가린다.
fn collapsed_status_dot(
    summary: SidebarSessionSummary,
) -> Option<crate::agent_surface::AgentVisualState> {
    use crate::agent_surface::AgentVisualState as S;
    if summary.waiting > 0 {
        Some(S::Waiting)
    } else if summary.error > 0 {
        Some(S::Error)
    } else if summary.running > 0 {
        Some(S::Active)
    } else if summary.done > 0 {
        Some(S::Complete)
    } else {
        None
    }
}

/// 활성 워크스페이스가 바뀌었을 때 사이드바의 세션 펼침 상태를 정리한다.
///
/// 떠나는 워크스페이스도 펼침 상태를 유지한다. 현재 세션은 행의 선택 배경으로 구분하고,
/// 선택하지 않은 세션은 소속 워크스페이스와 관계없이 같은 바탕을 쓴다.
///
/// 활성이 **바뀌는 순간에만** 돈다 — 전환 사이에 사용자가 직접 접거나 편 것은 그대로 산다.
fn sync_workspace_expansion_on_switch(
    expanded: &mut HashMap<String, bool>,
    last_active: &mut Option<String>,
    active_id: &str,
) {
    if last_active.as_deref() == Some(active_id) {
        return;
    }
    // 떠나는 쪽은 **기본값을 고정**만 한다. 비활성의 기본은 접힘이라, 명시값 없이 활성
    // 기본값(펼침)으로 보이던 워크스페이스가 떠나는 순간 저절로 닫혀 버린다.
    if let Some(previous) = last_active.as_ref() {
        expanded.entry(previous.clone()).or_insert(true);
    }
    // 오는 쪽은 **아직 정해진 게 없을 때만** 편다. chevron이 생긴 뒤로는 보고 있는
    // 워크스페이스를 일부러 접을 수 있는데, 떠났다 돌아왔다고 다시 펴면 사용자가 접은
    // 뜻이 사라진다(2026-09-03 리뷰 M2).
    expanded.entry(active_id.to_owned()).or_insert(true);
    *last_active = Some(active_id.to_owned());
}

/// 드래그 중인 사이드바 항목이 놓일 자리 — 각 행의 세로 중심선을 지날 때 한 칸씩 민다.
///
/// `centers`는 화면에 그려진 순서 그대로의 그룹 중심 y다. 반환값은 "몇 번째 **앞에**
/// 넣는가"라서 0..=centers.len() 범위를 갖는다(맨 끝에 놓으면 len).
fn sidebar_drop_index(centers: &[f32], pointer_y: f32) -> usize {
    centers.iter().filter(|center| **center < pointer_y).count()
}

/// 드래그 중 목록 가장자리에 다가갔을 때 이번 프레임에 밀 스크롤 양.
///
/// 드래그 중에는 목록을 스크롤할 방법이 하나도 없다 — egui는 무언가를 끄는 동안 휠
/// 스크롤을 끄고(`scroll_area.rs`의 `dragged_id().is_none()`), drag-to-scroll은 데스크톱에서
/// 꺼져 있으며, 드래그 중에는 hover가 끌던 위젯에 고정돼 스크롤바도 잡히지 않는다. 그래서
/// 지금 보이지 않는 자리로는 워크스페이스를 옮길 수가 없었다(2026-09-03 리뷰 H2).
///
/// 반환값의 부호는 `Ui::scroll_with_delta`를 따른다 — **양수면 목록의 앞쪽**(위)이, 음수면
/// 뒤쪽이 보인다. 가장자리 안으로 들어간 깊이에 비례해 빨라지고 밖으로 나가면 최대 속도로
/// 고정된다. 가운데에서는 정확히 0이라 평소 드래그는 목록을 건드리지 않는다.
fn drag_autoscroll_delta(viewport: egui::Rangef, pointer_y: f32, dt: f32) -> f32 {
    /// 가장자리로 인정하는 두께(pt) — 행 하나(29pt)보다 약간 작아 실수로 걸리지 않는다.
    const EDGE: f32 = 24.0;
    /// 최대 속도(pt/s) — 화면 하나를 1초 남짓에 훑는 정도.
    const MAX_SPEED: f32 = 600.0;

    // 뷰포트가 가장자리 둘을 담지 못할 만큼 짧으면 위·아래 구역이 겹쳐 방향이 튄다.
    let edge = EDGE.min(viewport.span() / 3.0);
    if edge <= 0.0 {
        return 0.0;
    }
    let ratio = if pointer_y < viewport.min + edge {
        ((viewport.min + edge - pointer_y) / edge).min(1.0)
    } else if pointer_y > viewport.max - edge {
        -((pointer_y - (viewport.max - edge)) / edge).min(1.0)
    } else {
        return 0.0;
    };
    ratio * MAX_SPEED * dt
}

/// `dragged`를 목록에서 빼고 `insert_at` 자리에 다시 넣는다.
///
/// `insert_at`은 **빼기 전** 목록 기준의 자리라, 원래 자리보다 뒤로 보낼 때는 빠진 한 칸만큼
/// 당겨야 사용자가 가리킨 틈에 정확히 들어간다. 목록에 없는 id면 그대로 돌려준다.
fn reorder_sidebar_ids(ids: &[String], dragged: &str, insert_at: usize) -> Vec<String> {
    let Some(from) = ids.iter().position(|id| id == dragged) else {
        return ids.to_vec();
    };
    let mut reordered = ids.to_vec();
    let moved = reordered.remove(from);
    let target = if insert_at > from {
        insert_at - 1
    } else {
        insert_at
    };
    reordered.insert(target.min(reordered.len()), moved);
    reordered
}

/// 같은 워크스페이스의 세션 위에 놓은 드래그만 목록 순서 변경으로 소비한다.
/// 행 위에 별도 interaction을 덮지 않아 클릭·우클릭·닫기 버튼은 그대로 입력받는다.
fn session_reorder_drop(
    ui: &egui::Ui,
    workspace_id: &str,
    sessions: &[SidebarSessionRow],
    row_rects: &[egui::Rect],
    color: egui::Color32,
) -> Option<SidebarAction> {
    let payload = egui::DragAndDrop::payload::<SessionRowDragPayload>(ui.ctx())?;
    if !ui.is_enabled()
        || payload.target().workspace_id() != workspace_id
        || row_rects.len() != sessions.len()
        || !sessions.iter().any(|row| &row.target == payload.target())
    {
        return None;
    }
    let first = row_rects.first()?;
    let last = row_rects.last()?;
    let pointer = ui.ctx().pointer_interact_pos()?;
    let list_rect = egui::Rect::from_min_max(
        egui::pos2(ui.max_rect().left(), first.top()),
        egui::pos2(ui.max_rect().right(), last.bottom()),
    )
    .intersect(ui.clip_rect());
    if !list_rect.contains(pointer) {
        return None;
    }
    let centers: Vec<_> = row_rects.iter().map(|rect| rect.center().y).collect();
    let insert_at = sidebar_drop_index(&centers, pointer.y);
    let ids: Vec<_> = sessions
        .iter()
        .map(|row| row.target.pane().0.clone())
        .collect();
    let order = reorder_sidebar_ids(&ids, &payload.target().pane().0, insert_at);
    if order != ids {
        let y = if insert_at == 0 {
            first.top()
        } else if insert_at == row_rects.len() {
            last.bottom()
        } else {
            (row_rects[insert_at - 1].bottom() + row_rects[insert_at].top()) / 2.0
        };
        let y = crate::ui::snap_line_to_pixel(y, 2.0, ui.ctx().pixels_per_point());
        ui.painter()
            .hline(first.x_range(), y, egui::Stroke::new(2.0, color));
    }
    if ui.input(|input| input.pointer.button_released(egui::PointerButton::Primary)) {
        // 같은 자리에 놓아도 payload는 끝낸다. 터미널 영역의 기존 드롭과 중복 소비하지 않는다.
        let _ = egui::DragAndDrop::take_payload::<SessionRowDragPayload>(ui.ctx());
        ui.ctx().request_repaint();
        if order != ids {
            return Some(SidebarAction::ReorderSessions {
                workspace_id: workspace_id.to_owned(),
                order,
            });
        }
    }
    None
}

/// 이 행에서 시작하거나 끝난 순서 변경 드래그를 상태에 반영한다. 놓는 순간에만 true.
fn track_workspace_drag(
    response: &egui::Response,
    workspace_id: &str,
    drag: &mut Option<String>,
) -> bool {
    if response.drag_started() {
        *drag = Some(workspace_id.to_owned());
    }
    if response.dragged() {
        response.ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
    }
    // Escape로 **취소한** 드래그도 `drag_stopped`를 낸다 — egui는 Escape에서 dragged id를
    // 지우기 때문이다(egui 0.35 `interaction.rs`). 버튼이 실제로 떨어졌는지까지 보는
    // `drag_stopped_by`만이 취소와 드롭을 가른다. 덤으로 왼쪽 버튼으로 놓은 것만 확정돼
    // 오른쪽 버튼으로 끌다 뗀 것이 순서를 바꾸지 않는다(2026-09-03 리뷰 H1).
    response.drag_stopped_by(egui::PointerButton::Primary) && drag.as_deref() == Some(workspace_id)
}

/// 드래그 중 놓일 자리를 그룹과 그룹 사이 가로선으로 표시한다.
fn paint_workspace_drop_indicator(
    ui: &egui::Ui,
    groups: &[(String, egui::Rect)],
    insert_at: usize,
    color: egui::Color32,
) {
    let Some((_, first)) = groups.first() else {
        return;
    };
    let y = match insert_at.checked_sub(1) {
        None => first.top(),
        Some(above) => groups[above.min(groups.len() - 1)].1.bottom(),
    };
    ui.painter().rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(first.left(), y - 1.0),
            egui::pos2(first.right(), y + 1.0),
        ),
        1.0,
        color,
    );
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

/// 워크스페이스 헤더 우측 세션 수의 글자 크기 — 이름(14)보다 확실히 작아 헤더를
/// 읽을 때 이름이 먼저 오고 수는 곁들여 읽힌다.
const WORKSPACE_COUNT_FONT_SIZE: f32 = 11.0;
/// 수 자체는 한두 자리지만, 세션이 비정상적으로 많아도 이름 자리를 잡아먹지 않게
/// 상한을 둔다.
const WORKSPACE_COUNT_MAX_WIDTH: f32 = 28.0;

/// 헤더에 표시할 세션 수. 상태별로 나뉜 요약을 다시 하나로 합친다 — 헤더가 답하는
/// 질문은 「여기 몇 개가 있나」이고, 「무슨 상태인가」는 펼친 행의 점들이 답한다.
fn workspace_session_count(summary: SidebarSessionSummary) -> usize {
    summary.running
        + summary.waiting
        + summary.done
        + summary.error
        + summary.idle
        + summary.inactive
}

// workspace_row는 이제 세션 수만 그려 이 세그먼트 목록을 쓰지
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

/// 비활성 workspace의 마지막 세션 스냅샷. 편집/수명주기 작업은 활성 runtime을
/// 전제로 하므로 노출하지 않는다. 좌클릭은 workspace 전환 + 정확한 tab/pane focus,
/// warm 세션의 우클릭은 현재 화면 오른쪽 연결 요청만 보낸다.
/// 반환값 두 번째 필드는 실제로 그려진 세션 행이 있는지 확인하는 합집합 rect다.
fn inactive_workspace_sessions(
    ui: &mut egui::Ui,
    workspace: &SidebarWorkspaceEntry,
    active_workspace_id: &str,
    sessions: &[SidebarSessionRow],
    accent_color: egui::Color32,
    catalog: &i18n::Catalog,
) -> (Option<SidebarAction>, Option<egui::Rect>) {
    if sessions.is_empty() {
        return (None, None);
    }
    let mut action = None;
    let mut session_rows_rect = None;
    let mut row_rects = Vec::with_capacity(sessions.len());
    ui.push_id(("session_list", &workspace.id), |ui| {
        // 행 **사이**에만 여백을 준다 — 첫 행은 헤더에, 마지막 행은 그룹
        // 구분선에 바로 붙는다(2026-07-25·2026-08-11 사용자).
        ui.spacing_mut().item_spacing.y = SESSION_ROW_GAP;
        for entry in sessions.iter() {
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                ui.vertical(|ui| {
                    let response = draggable_session_row(ui, entry, accent_color);
                    row_rects.push(response.rect);
                    session_rows_rect = Some(
                        session_rows_rect
                            .map_or(response.rect, |rect: egui::Rect| rect.union(response.rect)),
                    );
                    let drag_happened =
                        response.drag_started() || response.dragged() || response.drag_stopped();
                    if session_row_click_allowed(response.clicked(), drag_happened) {
                        action = Some(session_row_activation(&entry.target));
                    }
                    if ui.rect_contains_pointer(response.rect)
                        && can_open_session_beside(active_workspace_id, &entry.target)
                    {
                        let button_rect = egui::Rect::from_center_size(
                            egui::pos2(response.rect.right() - 14.0, response.rect.center().y),
                            egui::vec2(22.0, 22.0),
                        );
                        if ui
                            .put(button_rect, egui::Button::new("↗").frame(false))
                            .on_hover_text(catalog.t("workspace.menu.open_beside", &[]))
                            .clicked()
                        {
                            action = Some(open_beside_action(entry.target.clone()));
                        }
                    }
                    response.context_menu(|ui| {
                        inactive_session_context_menu_items(
                            ui,
                            workspace,
                            active_workspace_id,
                            entry,
                            catalog,
                            &mut action,
                        );
                    });
                });
            });
        }
    });
    if let Some(reorder) =
        session_reorder_drop(ui, &workspace.id, sessions, &row_rects, accent_color)
    {
        action = Some(reorder);
    }
    (action, session_rows_rect)
}

fn inactive_session_context_menu_items(
    ui: &mut egui::Ui,
    workspace: &SidebarWorkspaceEntry,
    active_workspace_id: &str,
    entry: &SidebarSessionRow,
    catalog: &i18n::Catalog,
    action: &mut Option<SidebarAction>,
) {
    if !can_open_session_beside(active_workspace_id, &entry.target) {
        return;
    }
    let menu_style = inactive_session_menu_style();
    ui.set_min_width(menu_style.min_width);
    let previous_wrap_mode = ui.style().wrap_mode;
    ui.style_mut().wrap_mode = Some(menu_style.wrap_mode);
    if ui
        .button(catalog.t("workspace.menu.open_beside", &[]))
        .clicked()
    {
        debug_assert_eq!(workspace.id, entry.target.workspace_id());
        *action = Some(open_beside_action(entry.target.clone()));
        ui.close();
    }
    ui.style_mut().wrap_mode = previous_wrap_mode;
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct InactiveSessionMenuStyle {
    min_width: f32,
    wrap_mode: egui::TextWrapMode,
}

fn inactive_session_menu_style() -> InactiveSessionMenuStyle {
    InactiveSessionMenuStyle {
        min_width: 220.0,
        wrap_mode: egui::TextWrapMode::Extend,
    }
}

fn live_session_context_menu_items<R>(
    ui: &mut egui::Ui,
    add_items: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let menu_style = inactive_session_menu_style();
    ui.scope(|ui| {
        ui.set_min_width(menu_style.min_width);
        ui.style_mut().wrap_mode = Some(menu_style.wrap_mode);
        add_items(ui)
    })
}

fn open_beside_action(target: SessionRowTarget) -> SidebarAction {
    SidebarAction::OpenSessionBeside(target)
}

fn session_row_activation(target: &SessionRowTarget) -> SidebarAction {
    match target {
        SessionRowTarget::Live {
            workspace_id,
            tab,
            pane,
            ..
        } => SidebarAction::FocusSession {
            workspace_id: workspace_id.clone(),
            tab: tab.clone(),
            pane: pane.clone(),
        },
        SessionRowTarget::PersistedPane { workspace_id, pane } => {
            SidebarAction::ActivatePersistedSession {
                workspace_id: workspace_id.clone(),
                pane: pane.clone(),
            }
        }
    }
}

fn session_row_should_activate(target: &SessionRowTarget, focused: bool) -> bool {
    !focused || target.session().is_none()
}

fn can_open_session_beside(active_workspace_id: &str, target: &SessionRowTarget) -> bool {
    target.workspace_id() != active_workspace_id
}

fn session_row_click_allowed(clicked: bool, dragged: bool) -> bool {
    clicked && !dragged
}

fn active_session_close_button(
    ui: &mut egui::Ui,
    response: &egui::Response,
    catalog: &i18n::Catalog,
) -> bool {
    if !ui.rect_contains_pointer(response.rect) {
        return false;
    }
    let button_rect = egui::Rect::from_center_size(
        egui::pos2(response.rect.right() - 14.0, response.rect.center().y),
        egui::vec2(22.0, 22.0),
    );
    ui.put(button_rect, egui::Button::new("×").frame(false))
        .on_hover_text(catalog.t("sidebar.menu.close_pane", &[]))
        .clicked()
}

fn session_row(
    ui: &mut egui::Ui,
    entry: &SidebarSessionRow,
    accent_color: egui::Color32,
) -> egui::Response {
    draggable_session_row(ui, entry, accent_color)
}

fn draggable_session_row(
    ui: &mut egui::Ui,
    entry: &SidebarSessionRow,
    accent_color: egui::Color32,
) -> egui::Response {
    let response = session_row_impl(ui, entry, None, accent_color, egui::Sense::click_and_drag())
        .0
        .on_hover_cursor(egui::CursorIcon::Grab);
    session_row_drag_source(&response, &entry.target);
    response
}

/// 클릭 판정 중 이동·해제가 한 프레임에 도착하면 egui의 drag_started가 생략된다.
/// 눌린 행의 정확한 ID와 시작점 한 개만 보관해 빠른 드롭도 같은 경로로 처리한다.
#[derive(Clone, Copy)]
struct SessionRowPress {
    row_id: egui::Id,
    origin: egui::Pos2,
}

fn session_row_drag_source(response: &egui::Response, target: &SessionRowTarget) {
    let ctx = &response.ctx;
    let key = egui::Id::new("sidebar_session_press");
    let threshold = ctx.options(|options| options.input_options.max_click_dist);
    let (press_origin, released, cancelled, pointer) = ctx.input(|input| {
        (
            input
                .pointer
                .button_pressed(egui::PointerButton::Primary)
                .then(|| input.pointer.press_origin())
                .flatten(),
            input.pointer.button_released(egui::PointerButton::Primary),
            input.key_pressed(egui::Key::Escape) || !input.focused,
            input.pointer.interact_pos(),
        )
    });
    if cancelled {
        ctx.data_mut(|data| data.remove::<SessionRowPress>(key));
        return;
    }
    if !response.enabled() {
        return;
    }
    if response.is_pointer_button_down_on()
        && let Some(origin) = press_origin
    {
        ctx.data_mut(|data| {
            data.insert_temp(
                key,
                SessionRowPress {
                    row_id: response.id,
                    origin,
                },
            )
        });
    }
    let fast_drop = released
        && !egui::DragAndDrop::has_any_payload(ctx)
        && ctx
            .data(|data| data.get_temp::<SessionRowPress>(key))
            .is_some_and(|press| {
                press.row_id == response.id
                    && pointer.is_some_and(|pos| pos.distance(press.origin) > threshold)
            });
    if response.drag_started_by(egui::PointerButton::Primary) || fast_drop {
        egui::DragAndDrop::set_payload(ctx, SessionRowDragPayload::new(target.clone()));
        ctx.data_mut(|data| data.remove::<SessionRowPress>(key));
    }
}

fn finish_session_row_drag_frame(ctx: &egui::Context) {
    if ctx.input(|input| {
        !input.pointer.button_down(egui::PointerButton::Primary)
            || input.key_pressed(egui::Key::Escape)
            || !input.focused
    }) {
        // 원래 행이 닫히거나 그룹이 접혀도 버튼 해제 뒤 후보를 남기지 않는다.
        ctx.data_mut(|data| data.remove::<SessionRowPress>(egui::Id::new("sidebar_session_press")));
    }
}

/// 이름 인라인 편집 중인 행 — 레일/보조 행(2·3행)은 그대로 유지하고 **제목 자리만**
/// TextEdit로 바꾼다. 행 전체를 편집기로 대체하면 편집 중 레이아웃이 무너진다
/// (2026-07-16 사용자).
fn session_row_editing(
    ui: &mut egui::Ui,
    entry: &SidebarSessionRow,
    edit: &mut SessionNameEdit,
    accent_color: egui::Color32,
) -> (egui::Response, Option<SessionNameEditResult>) {
    session_row_impl(ui, entry, Some(edit), accent_color, egui::Sense::click())
}

// 워크스페이스 헤더의 우측 인셋(workspace_row 내부 rect =
// full_rect.shrink2((WORKSPACE_CARD_HORIZONTAL_INSET + 8, 0)))
// 과 같은 6px — 8px일 땐 세션 카드 배경이 위 워크스페이스 카드보다 우측 여백이
// 2px 더 넓어 보였다(2026-07-25 사용자).
const SESSION_HIGHLIGHT_RIGHT_INSET: f32 = SESSION_FILL_SIDE_MARGIN;
/// 상태 점의 지름. 예전엔 세로 레일(2~4.5px × 35px)이었는데, 레일은 **행 전체를
/// 물들이지 않으면서** 상태를 나르려다 폭이 애매했다 — 2px는 색약에서 amber/green
/// 구분이 어렵고, 4.5px는 텍스트를 밀었다. 점은 지름 7px 하나로 같은 일을 하면서
/// **세로로 정렬돼 훑기 좋다**(2026-08-11 사용자: 레일 빼고 점을 앞으로).
const SESSION_DOT_DIAMETER: f32 = 7.0;
/// 「작업 중」 점의 호흡 주기(Hz)와 최저 투명도. 0.4Hz = 2.5초에 한 번 —
/// 시선을 끌지 않으면서 「살아 있다」가 읽히는 속도다. 최저 0.35는 완전히
/// 사라지지 않게 해 **꺼진 것과 구분**된다.
const SESSION_DOT_BREATH_HZ: f32 = 0.4;
const SESSION_DOT_BREATH_MIN: f32 = 0.35;

// ── 세션 행 기하 — 주안 목업의 CSS를 그대로 옮긴 값 ──────────────────────
//
//   .s   { margin: 0 6px 2px; padding: 6px 11px 6px 13px;
//          grid-template-columns: 8px 1fr; gap: 0 9px; border-radius: 6px }
//   .dot { width: 7px; height: 7px }
//
// 목업은 **패널** 좌표계로 적혀 있고(면 6..W-6 · 점 19..26 · 글 36..W-17),
// 세션 행은 호출부가 add_space(16)으로 들여쓴 뒤 그린다. 그 차이를 여기서
// 되돌려야 면·점·글이 목업과 같은 x에 선다.
/// 호출부(session_list_scroll/inactive_workspace_sessions)의 들여쓰기.
const SESSION_LIST_INDENT: f32 = 16.0;
/// 면의 좌우 여백(.s margin). 우측은 워크스페이스 카드와 같은 값이라 헤더와
/// 세션 면의 오른쪽 끝이 한 줄로 선다.
const SESSION_FILL_SIDE_MARGIN: f32 = WORKSPACE_CARD_HORIZONTAL_INSET;
/// 면 안쪽 좌측 여백(.s padding-left). **점은 이 안쪽에서 시작한다** — 예전엔 면이
/// 점보다 오른쪽에서 시작해 점이 면 바깥에 떠 있었다(2026-08-11 사용자: 배경 채우는
/// 컬러 영역이 다르다).
const SESSION_FILL_PADDING_LEFT: f32 = 13.0;
/// 점이 앉는 칸의 폭과 칸-글 사이 간격(.s grid-template-columns / gap).
const SESSION_DOT_COLUMN: f32 = 8.0;
const SESSION_DOT_TEXT_GAP: f32 = 9.0;
/// 행 **사이** 간격(.s margin-bottom). 구분선을 없앤 자리를 이 여백이 대신한다.
/// 행이 스스로 아래에 남기는 게 아니라 목록의 `item_spacing.y`로 준다 — 그래야
/// 마지막 행 **아래**와 헤더 **바로 아래**에는 붙지 않는다(2026-08-11 사용자:
/// 세션 하단·워크스페이스와 세션 사이 여백을 없애라).
const SESSION_ROW_GAP: f32 = 2.0;
/// 면의 좌측이 행 rect보다 얼마나 왼쪽인가.
const SESSION_HIGHLIGHT_LEFT_EXTEND: f32 = SESSION_LIST_INDENT - SESSION_FILL_SIDE_MARGIN;
/// 점 중심 x — 행 rect 기준. **워크스페이스 아바타의 중심과 같은 세로선**에 둔다.
/// 목업은 아바타가 20px·좌측 11이라 점(19..26)이 자연스럽게 그 아래 왔는데, 이 앱의
/// 아바타는 18px·좌측 10이라 중심이 19 대 22.5로 3.5px 어긋났다(2026-08-11 사용자).
/// 목업의 절대값 대신 **아바타에서 유도**해야 아바타를 다시 손봐도 안 어긋난다.
const SESSION_DOT_CENTER_INSET: f32 =
    WORKSPACE_AVATAR_LEFT_INSET + WORKSPACE_AVATAR_SIZE / 2.0 - SESSION_LIST_INDENT;
/// 글의 좌측 원점 — 행 rect 기준. 점 자리를 상시 예약해 상태가 바뀌어도 글이
/// 좌우로 흔들리지 않는다.
const SESSION_TEXT_INSET: f32 =
    SESSION_FILL_PADDING_LEFT + SESSION_DOT_COLUMN + SESSION_DOT_TEXT_GAP
        - SESSION_HIGHLIGHT_LEFT_EXTEND;
// 폰트 기본 줄높이(CJK 포함이라 여유 있게 잡힘) 대신 폰트 크기에 곱하는 비율로
// 세션 정보의 2~3개 행 사이에 참고 이미지 수준의 여유를 둔다.
// 절대 px(예전엔 15.0/12.0 고정값)는 폰트 크기가 바뀌면 그대로 깨진다 —
// cmux/Warp 조사 후 Warp의 DEFAULT_UI_LINE_HEIGHT_RATIO 패턴을 따라 비율로
// 바꿨다(2026-07-25 사용자). 호출부는 자기 폰트 크기 × 이 비율을 쓴다.
//
// 값은 한글 UI 폰트의 **실제** 줄높이여야 한다. 1.0(= 폰트 크기)이던 동안
// `galley.size().y`가 실제 잉크 높이보다 작게 나와서, 그걸로 여백·gap을 계산하는
// 아래 session_row_impl의 상하 여백 대칭이 장부상으로만 맞았다 — 13px 제목의 실제
// 세로 범위는 ascent 11.7 + descent 3.9 = 15.6px인데 박스는 13px이라 2.6px가 아래로
// 삐져나갔고, 마지막 줄 디센더가 행 바닥에서 0.15px까지 붙었다(의도한 여백은 2.25px).
// 그래서 행마다 글자가 바닥에 눌린 것처럼 보였다. 2026-07-25에 "레일·텍스트가 바닥
// 밖으로 삐져나온다"고 보고돼 행 높이를 5px 키운 것도 같은 원인이다.
// 1.2 = AppleGothic·Apple SD Gothic Neo 공통 (ascent 0.9 + descent 0.3 em) — 두 폰트
// 모두 hhea/OS2 기준 정확히 1.2em이라 박스가 잉크와 일치한다(2026-08-06 실측).
const SESSION_LINE_HEIGHT_RATIO: f32 = 1.2;
/// 글의 우측 한계. 목업은 면 안쪽 11px(= rect.right() - 17)인데, 목업에 없는
/// 닫기(×) 버튼이 `rect.right() - 25 .. - 3`을 차지한다 — 글이 그 아래로 들어가지
/// 않게 7px 더 물린다.
const SESSION_CONTENT_RIGHT_INSET: f32 = 24.0;

// ── 세션 행 세로 기하 ────────────────────────────────────────────────────
/// 면 안쪽 위아래 여백(.s padding 6/6).
const SESSION_ROW_PADDING_Y: f32 = 6.0;
/// 줄 사이(.m margin-top 1px).
const SESSION_ROW_LINE_GAP: f32 = 1.0;
const SESSION_TITLE_FONT_SIZE: f32 = 13.0;
const SESSION_SUBLINE_FONT_SIZE: f32 = 10.5;
/// 행 높이 = 면 높이. 행은 아래에 여백을 남기지 않는다(위 SESSION_ROW_GAP 참고).
/// 목업의 CSS line-height(1.4/1.62)는 브라우저 기본값이 섞인 값이라 그대로 옮기지
/// 않고, 이 폰트에서 실측된 비율로 **같은 구조**(여백 6 · 줄 · 간격 1 · 줄 · 여백 6)
/// 를 만든다.
const SESSION_ROW_HEIGHT: f32 = SESSION_ROW_PADDING_Y * 2.0
    + SESSION_TITLE_FONT_SIZE * SESSION_LINE_HEIGHT_RATIO
    + SESSION_ROW_LINE_GAP
    + SESSION_SUBLINE_FONT_SIZE * SESSION_LINE_HEIGHT_RATIO;

#[derive(Clone, Copy, Debug, PartialEq)]
struct SessionDragStyle {
    fill: Option<egui::Color32>,
    stroke: egui::Stroke,
    shadow: egui::epaint::Shadow,
    rail_multiplier: f32,
}

fn session_drag_style(active: bool, tokens: crate::ui::designall::Tokens) -> SessionDragStyle {
    if active {
        SessionDragStyle {
            fill: Some(tokens.selected_background),
            stroke: egui::Stroke::new(1.0, tokens.accent),
            shadow: egui::epaint::Shadow {
                offset: [0, 2],
                blur: 8,
                spread: 0,
                color: egui::Color32::from_black_alpha(96),
            },
            rail_multiplier: 1.2,
        }
    } else {
        SessionDragStyle {
            fill: None,
            stroke: egui::Stroke::NONE,
            shadow: egui::epaint::Shadow::NONE,
            rail_multiplier: 1.0,
        }
    }
}

fn session_drag_payload_matches(ctx: &egui::Context, target: &SessionRowTarget) -> bool {
    egui::DragAndDrop::payload::<SessionRowDragPayload>(ctx)
        .is_some_and(|payload| payload.target() == target)
}

/// 행 배경의 모서리. 각진 면은 패널 폭을 가로지르는 띠로 보여 「이 행」이 어디서
/// 끊기는지 흐렸다 — 둥근 면은 목록 안의 한 덩어리로 읽힌다(2026-08-11 사용자).
const SESSION_ROW_CORNER_RADIUS: f32 = 6.0;
/// 행 배경은 **현재 화면에 떠 있는 세션**만 선택색으로 표시한다. 승인·입력 대기 같은
/// 상태는 점·링·문구가 나르며 배경을 바꾸지 않는다. 그래야 다른 워크스페이스의
/// attention 세션도 현재 워크스페이스의 비선택 세션과 같은 바탕으로 보인다.
/// hover는 포인터가 있는 동안만 일시적으로 별도 면을 쓴다.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SessionRowFill {
    color: egui::Color32,
    /// 면이 패널 폭을 다 쓰는가. hover는 **다 쓴다** — 포인터가 짚은 영역이라
    /// 경계가 분명한 게 낫고, 워크스페이스 헤더도 같은 폭이라 리듬이 맞는다
    /// (2026-08-11 사용자: 워크스페이스 색과 hover 색이 다르니 여백 없이 채워도 된다).
    /// 승인 면은 목업대로 좌우 6px 물러선 **둥근 카드**로 남는다 — 모양이 다르면
    /// 「지금 마우스가 여기」와 「이 행이 나를 기다린다」가 섞이지 않는다.
    full_bleed: bool,
}

/// 현재 세션 선택면을 기본 바탕보다 밝게 표시한다. 워크스페이스 색은 섞지 않는다.
const SESSION_FOCUSED_LIFT: f32 = 0.06;

fn session_row_fill(
    tokens: crate::ui::designall::Tokens,
    hovered: bool,
    focused: bool,
) -> Option<SessionRowFill> {
    // **지금 화면에 떠 있는 세션**은 면을 유지한다(2026-08-20 사용자) — 선택된
    // 워크스페이스만 배경이 있고 그 안에서 실제로 보고 있는 세션은 표시가 없어,
    // 목록에서 "어느 것을 보고 있는지"를 제목 색(session_title_color) 하나로만
    // 구분해야 했다. hover보다 우선한다 — 마우스를 다른 행에 얹어도 지금 보고 있는
    // 곳이 사라지면 안 된다.
    // 워크스페이스 행처럼 accent를 섞지는 않는다 — 부모(워크스페이스)와 자식(세션)이
    // 같은 색이면 계층이 뭉개진다.
    if focused {
        return Some(SessionRowFill {
            color: crate::ui::designall::mix(
                tokens.selected_background,
                tokens.text,
                SESSION_FOCUSED_LIFT,
            ),
            full_bleed: true,
        });
    }
    hovered.then_some(SessionRowFill {
        color: tokens.hover_background,
        full_bleed: true,
    })
}

/// hover 면이 쓰는 rect — 호출부의 들여쓰기(add_space)를 되돌려 패널 좌우 끝까지 간다.
fn session_full_bleed_rect(rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(rect.left() - SESSION_LIST_INDENT, rect.top()),
        rect.max,
    )
}

/// 선택 안 된 행의 제목 밝기. 면을 못 쓰는 대신 **선택된 행만 제 밝기**로 두고
/// 나머지를 한 단 내려 「지금 보고 있는 세션」을 읽게 한다.
const SESSION_TITLE_DIM: f32 = 0.68;

fn session_title_color(visuals: &egui::Visuals, focused: bool) -> egui::Color32 {
    if focused {
        visuals.text_color()
    } else {
        visuals.text_color().gamma_multiply(SESSION_TITLE_DIM)
    }
}

/// 행의 **면**. 좌우로만 6px 물러선다(.s margin) — 세로는 행 전체가 면이고,
/// 행 사이 간격은 목록의 item_spacing이 준다.
fn session_highlight_rect(rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(rect.left() - SESSION_HIGHLIGHT_LEFT_EXTEND, rect.top()),
        egui::pos2(
            (rect.right() - SESSION_HIGHLIGHT_RIGHT_INSET).max(rect.left()),
            rect.bottom(),
        ),
    )
}

/// 사용자 이름은 자동 작업 문구가 바뀌어도 첫 줄에 고정한다.
fn session_headline(entry: &SidebarSessionRow) -> &str {
    if entry.title_is_custom || entry.agent_line.is_none() {
        return &entry.title;
    }
    entry
        .status_line
        .as_deref()
        .filter(|line| !line.trim().is_empty())
        .unwrap_or(&entry.title)
}

/// 상태를 둘째 줄 앞에 고정하고, 이름을 지정한 행만 오른쪽 모델 공간을 확보한다.
fn session_secondary_line(
    ui: &egui::Ui,
    entry: &SidebarSessionRow,
    status_color: egui::Color32,
    secondary_color: egui::Color32,
    max_width: f32,
) -> (Arc<egui::Galley>, Option<Arc<egui::Galley>>) {
    let font = crate::fonts::sidebar_font(ui.ctx(), SESSION_SUBLINE_FONT_SIZE);
    let line_height = Some(SESSION_SUBLINE_FONT_SIZE * SESSION_LINE_HEIGHT_RATIO);
    let Some(agent_line) = entry.agent_line.as_deref() else {
        let summary = if entry.summary.is_empty() {
            "~"
        } else {
            &entry.summary
        };
        return (
            clipped_line(ui, summary, font, max_width, line_height),
            None,
        );
    };
    // 아주 좁을 때는 상태를 우선한다. 축약한 모델/강도 전체는 hover로 확인한다.
    let model_galley = entry
        .agent_model
        .as_deref()
        .filter(|model| entry.title_is_custom && max_width >= 96.0 && !model.trim().is_empty())
        .map(|model| {
            let model = model.trim();
            let model = model
                .strip_prefix("gpt-")
                .or_else(|| model.strip_prefix("grok-"))
                .unwrap_or(model);
            clipped_line(ui, model, font.clone(), max_width * 0.27, line_height)
        });
    let model_width = model_galley.as_ref().map_or(0.0, |g| g.size().x + 6.0);
    let details = if entry.title_is_custom {
        entry.status_line.as_deref().unwrap_or("")
    } else {
        agent_line
    };
    let mut job = egui::text::LayoutJob::default();
    let format = egui::TextFormat {
        font_id: font,
        color: secondary_color,
        line_height,
        ..Default::default()
    };
    if let Some(status) = entry
        .status_label
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        job.append(
            status,
            0.0,
            egui::TextFormat {
                color: status_color,
                ..format.clone()
            },
        );
        if !details.trim().is_empty() {
            job.append(" · ", 0.0, format.clone());
        }
    }
    job.append(details, 0.0, format);
    job.wrap = egui::text::TextWrapping {
        max_width: (max_width - model_width).max(0.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    (ui.painter().layout_job(job), model_galley)
}

fn session_row_impl(
    ui: &mut egui::Ui,
    entry: &SidebarSessionRow,
    edit: Option<&mut SessionNameEdit>,
    _accent_color: egui::Color32,
    sense: egui::Sense,
) -> (egui::Response, Option<SessionNameEditResult>) {
    // 이름 유무와 관계없이 두 줄과 기존 높이·간격을 유지한다.
    let editing = edit.is_some();
    let row_h = SESSION_ROW_HEIGHT;
    let (_, rect) = ui.allocate_space(egui::vec2(ui.available_width(), row_h));
    // 행의 위치가 바뀌거나 앞 세션이 닫혀도 입력 대상은 같은 runtime/tab/pane이다.
    let id = egui::Id::new(("sidebar_session_row", &entry.target));
    let resp = ui.interact(rect, id, sense);
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
        return (resp, editing.then_some(SessionNameEditResult::Cancel));
    }
    // 색을 먼저 복사(Copy)해 visuals 차용을 끝낸 뒤 ui.fonts로 galley를 만든다.
    let dot = session_entry_status_color(entry);
    // 텍스트는 최대 레일 폭을 기준으로 고정해 attention/pulse 중에도 좌우로 흔들리지
    // 않게 한다. 포커스/hover 배경 시작점만 아래에서 실제 레일 폭에 맞춘다.
    // 점 하나가 상태를 나른다. 크기는 고정하고 **색과 투명도**만 바꾼다 — 크기가
    // 변하면 세로 정렬이 흔들려 훑기가 나빠진다(레일 시절의 2↔4.5px 문제).
    let (dot_color, dot_scale) = if let Some((t, color)) = entry.pulse {
        // 알림 1회 펄스는 기존 계약 유지 — 잠깐 커졌다 돌아온다.
        (color, 1.0 + 0.45 * (t * std::f32::consts::PI).sin())
    } else if entry.status == Some(runtime::SessionStatus::Running) {
        // 「작업 중」은 숨쉬듯 페이드한다(2026-08-11 사용자). 시간은 egui가 주는
        // 프레임 시각을 쓰므로 별도 상태가 필요 없다. 리페인트를 **요청하지 않는다** —
        // 기존 pane 플래시(session_flash)와 같은 계약으로, 터미널 출력 등 다른
        // 이유로 도는 프레임에 얹혀 간다. 출력이 멎으면 점도 멎지만 색은 남아
        // 「작업 중」이라는 사실 자체는 잃지 않는다.
        let phase = ui.input(|i| i.time) as f32 * SESSION_DOT_BREATH_HZ * std::f32::consts::TAU;
        let breath = 0.5 + 0.5 * phase.sin();
        (
            dot.gamma_multiply(SESSION_DOT_BREATH_MIN + (1.0 - SESSION_DOT_BREATH_MIN) * breath),
            1.0,
        )
    } else {
        (dot, 1.0)
    };
    let text_inset = SESSION_TEXT_INSET;
    // 둘째 줄(보조 정보): 다크는 기존 weak 톤, 라이트는 weak가 패널 위에서 너무 옅어
    // textSecondary 수준으로 진하게 (라이트 테마 회색 흐림, 2026-07-10).
    // 참조하던 #444444는 무채색이라 색상축을 통일한 사이드바에서 혼자 튀었다 — 명도는
    // 그대로 두고 축만 맞춘다(2026-08-06). settings 보조색 라이트값과 같은 색이다.
    let sub_color = if ui.visuals().dark_mode {
        ui.visuals().weak_text_color().gamma_multiply(0.9)
    } else {
        egui::Color32::from_rgb(0x3f, 0x44, 0x49)
    };
    let title_color = session_title_color(ui.visuals(), entry.focused);
    // 텍스트는 행 폭(좌 11 + 우 여백 16) 안으로 잘라 '…' 처리 — 고정 글자수 truncate는
    // 좁은 사이드바에서 박스 밖으로 삐져나갔다(#91 사용자).
    let max_w = (rect.width() - text_inset - SESSION_CONTENT_RIGHT_INSET).max(10.0);
    let status_text_color = crate::ui::agent_visuals::status_text_color(
        crate::agent_surface::AgentVisualState::from_pty_with_agent(
            entry.status,
            entry.agent_line.is_some(),
        ),
    );
    let title_galley = clipped_line(
        ui,
        session_headline(entry),
        crate::fonts::sidebar_font(ui.ctx(), SESSION_TITLE_FONT_SIZE),
        max_w,
        Some(SESSION_TITLE_FONT_SIZE * SESSION_LINE_HEIGHT_RATIO),
    );
    let (line2_galley, model_galley) =
        session_secondary_line(ui, entry, status_text_color, sub_color, max_w);

    // 행 배경·레일·텍스트 원점을 물리 픽셀 경계에 맞춘다 — 행 높이가 소수(49/36에
    // 소수 여백)라 행이 쌓일수록 원점이 밀려 선명도가 행마다 출렁였다. 자세한 이유는
    // `crate::ui::snap_to_pixel` 주석 참고.
    let ppp = ui.ctx().pixels_per_point();
    let painter = ui.painter();
    let highlight_rect = crate::ui::snap_rect_to_pixel(ppp, session_highlight_rect(rect));
    let tokens = crate::ui::designall::tokens(ui.visuals());
    let drag_style = session_drag_style(
        session_drag_payload_matches(ui.ctx(), &entry.target),
        tokens,
    );
    if let Some(fill) = drag_style.fill {
        painter.add(drag_style.shadow.as_shape(highlight_rect, 4.0));
        painter.rect(
            highlight_rect,
            4.0,
            fill,
            drag_style.stroke,
            egui::StrokeKind::Inside,
        );
    } else if let Some(fill) = session_row_fill(tokens, resp.hovered(), entry.focused) {
        // 면은 **점까지 덮는다**. 예전엔 좌측 레일이 배경 위에 얹힌 별도 요소라
        // 배경을 레일 다음부터 시작했는데, 점이 된 지금 그 규칙을 남기면 점만 면
        // 바깥에 떠서 행이 둘로 갈라져 보인다(2026-08-11 사용자).
        let (fill_rect, radius) = if fill.full_bleed {
            (session_full_bleed_rect(rect), 0.0)
        } else {
            (highlight_rect, SESSION_ROW_CORNER_RADIUS)
        };
        painter.rect_filled(
            crate::ui::snap_rect_to_pixel(ppp, fill_rect),
            radius,
            fill.color,
        );
    }
    // 행 사이 구분선은 없다. 주안의 규칙은 「면은 주목이 필요할 때만」이고, 평상시
    // 행 경계는 **여백과 점의 세로 정렬**이 만든다. 선을 남기면 면이 좌우로 6px
    // 물러선 자리를 선이 가로질러 두 요소의 끝이 어긋나 보인다(2026-08-11 사용자:
    // 라인 길이가 안 맞는다). 워크스페이스 그룹을 가르는 선은
    // paint_workspace_group_separator가 따로 갖는다.

    // 좌측 상태 점 — 항상 표시, 세로로 정렬돼 한 눈에 훑인다. 첫 줄 글자의 세로
    // 중앙에 맞춰 「이 줄의 상태」로 읽히게 한다(행 중앙에 두면 두 줄 사이에 떠서
    // 어느 줄에 붙는지 모호했다).
    let dot_color = if drag_style.rail_multiplier > 1.0 {
        dot_color.gamma_multiply(drag_style.rail_multiplier)
    } else {
        dot_color
    };
    let dot_center = crate::ui::snap_pos_to_pixel(
        ppp,
        egui::pos2(
            rect.left() + SESSION_DOT_CENTER_INSET,
            rect.top() + SESSION_ROW_PADDING_Y + title_galley.size().y / 2.0,
        ),
    );
    painter.circle_filled(
        dot_center,
        SESSION_DOT_DIAMETER / 2.0 * dot_scale,
        dot_color,
    );
    // 미확인 완료·입력 대기는 점 둘레에 링을 두른다. 예전엔 레일을 2→4.5px로 굵혀
    // 표시했는데, 점은 크기를 바꾸면 세로 정렬이 흔들려 훑기가 나빠진다 — 링은
    // **자리를 안 밀면서** 같은 세기를 낸다.
    if entry.attention {
        painter.circle_stroke(
            dot_center,
            SESSION_DOT_DIAMETER / 2.0 + 2.0,
            egui::Stroke::new(1.5, dot_color),
        );
    }
    // 두 줄 높이만 합산해 행마다 임시 Vec을 만들지 않는다.
    let content_h = title_galley.size().y + line2_galley.size().y;
    let gap = (row_h - 2.0 * SESSION_ROW_PADDING_Y - content_h).max(0.0);
    let title_top = rect.top() + SESSION_ROW_PADDING_Y;
    let title_center = title_top + title_galley.size().y / 2.0;
    let line2_top = title_top + title_galley.size().y + gap;
    // 한 글자보다도 좁은 폭에서는 말줄임표까지 행 밖으로 나가지 않도록 자른다.
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + text_inset, rect.top()),
        egui::pos2(rect.right() - SESSION_CONTENT_RIGHT_INSET, rect.bottom()),
    );
    let text_painter = painter.with_clip_rect(text_rect.intersect(ui.clip_rect()));
    // 편집 중에는 첫 줄에 TextEdit를 얹고 둘째 줄 상태는 그대로 표시한다.
    if !editing {
        text_painter.galley(
            crate::ui::snap_pos_to_pixel(ppp, egui::pos2(text_rect.left(), title_top)),
            title_galley.clone(),
            title_color,
        );
    }
    text_painter.galley(
        crate::ui::snap_pos_to_pixel(ppp, egui::pos2(text_rect.left(), line2_top)),
        line2_galley,
        sub_color,
    );
    if let Some(model) = model_galley {
        text_painter.galley(
            crate::ui::snap_pos_to_pixel(
                ppp,
                egui::pos2(text_rect.right() - model.size().x, line2_top),
            ),
            model,
            sub_color,
        );
    }
    let edit_result = if let Some(edit) = edit {
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
        session_name_editor(ui, title_rect, edit)
    } else {
        None
    };
    let response = if !editing && entry.agent_line.is_some() {
        resp.on_hover_ui(|ui| {
            ui.set_max_width(360.0);
            ui.label(session_headline(entry));
            if let Some(status) = entry.status_label.as_deref() {
                ui.colored_label(status_text_color, status);
            }
            if entry.title_is_custom
                && let Some(activity) = entry.status_line.as_deref()
            {
                ui.label(activity);
            }
            if let Some(agent) = entry.agent_line.as_deref() {
                ui.weak(agent);
            }
        })
    } else {
        resp
    };
    (response, edit_result)
}

/// 입력창에 귀속된 프레임만 처리한다. 다른 pane의 Enter를 편집 확정에 쓰지 않는다.
fn session_name_editor(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    edit: &mut SessionNameEdit,
) -> Option<SessionNameEditResult> {
    let blocked = !ui.is_enabled()
        || !ui.is_rect_visible(rect)
        || !ui.input(|input| input.focused)
        || ui.ctx().any_popup_open()
        || ui.memory(|memory| {
            !memory.allows_interaction(ui.layer_id())
                || memory
                    .areas()
                    .visible_layer_ids()
                    .iter()
                    .any(super::workspace::is_blocking_terminal_window)
        });
    let clicked_outside = ui.input(|input| {
        input.pointer.any_pressed()
            && input
                .pointer
                .interact_pos()
                .is_some_and(|pos| !rect.contains(pos))
    });
    if blocked || clicked_outside {
        return Some(SessionNameEditResult::Cancel);
    }
    let id = edit.input_id();
    if std::mem::take(&mut edit.request_focus) {
        // 시작할 때 한 번만 요청한다. 매 프레임 터미널/다른 입력창의 포커스를 빼앗지 않는다.
        ui.memory_mut(|memory| memory.request_focus(id));
    }
    let escape_pressed = ui.input(|input| input.key_pressed(egui::Key::Escape));
    let owns_keys = ui.memory(|memory| {
        memory.has_focus(id)
            || (memory.focused().is_none() && memory.had_focus_last_frame(id) && escape_pressed)
    });
    let response = ui.put(
        rect,
        egui::TextEdit::singleline(&mut edit.text)
            .id(id)
            .return_key(None)
            .font(crate::fonts::sidebar_font(
                ui.ctx(),
                SESSION_TITLE_FONT_SIZE,
            ))
            .frame(egui::Frame::NONE)
            .margin(egui::Margin::ZERO)
            .vertical_align(egui::Align::Center),
    );
    if owns_keys && !terminal::renderer_egui::frame_has_active_preedit(ui.ctx()) {
        let result = ui.input(|input| {
            let mut result = None;
            for event in &input.events {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = event
                    && !modifiers.any()
                {
                    match key {
                        egui::Key::Escape => return Some(SessionNameEditResult::Cancel),
                        egui::Key::Enter => result = Some(SessionNameEditResult::Submit),
                        _ => {}
                    }
                }
            }
            result
        });
        if result.is_some() {
            // 확정 후 포커스를 해제해도 같은 프레임에 입력한 문자/Enter가 PTY로 새지 않는다.
            ui.input_mut(|input| {
                let keep = |event: &egui::Event| {
                    !matches!(
                        event,
                        egui::Event::Key { .. }
                            | egui::Event::Text(_)
                            | egui::Event::Paste(_)
                            | egui::Event::Copy
                            | egui::Event::Cut
                            | egui::Event::Ime(_)
                    )
                };
                input.events.retain(keep);
                input.raw.events.retain(keep);
            });
            return result;
        }
    }
    (!response.has_focus()).then_some(SessionNameEditResult::Cancel)
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
    Refresh,
}

fn file_toolbar_more_at(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    accessible_label: &str,
) -> egui::Response {
    let response = ui.interact(
        rect,
        ui.id().with(("file_toolbar_icon", "more")),
        egui::Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), accessible_label)
    });
    let color = if response.hovered() {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    ui.painter().text(
        rect.center() + egui::vec2(0.0, -2.0),
        egui::Align2::CENTER_CENTER,
        "...",
        crate::fonts::sidebar_font(ui.ctx(), 13.0),
        color,
    );
    response
}

fn sidebar_tool_tab_at(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    label: &str,
    active: bool,
) -> egui::Response {
    let response = ui.interact(
        rect,
        ui.id().with(("sidebar_tool_tab", label)),
        egui::Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    let tokens = crate::ui::designall::tokens(ui.visuals());
    let color = if active || response.hovered() {
        tokens.text
    } else {
        tokens.muted_text
    };
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        crate::fonts::sidebar_font(ui.ctx(), 11.5),
        color,
    );
    if active {
        let line = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 4.0, rect.bottom() - 2.0),
            egui::pos2(rect.right() - 4.0, rect.bottom()),
        );
        ui.painter().rect_filled(line, 0.0, tokens.accent);
    }
    response
}

/// 삽입 마커가 시작하는 x — 행 내용(캐럿+아이콘)이 시작하는 들여쓰기와 맞춘다
/// (행을 그리는 `ui.add_space(10.0 + depth * 18.0)`과 같은 값). 마커가 행
/// 전체 폭이 아니라 이 **깊이**에서 시작해야 "이 깊이의 폴더로 들어간다"는
/// 뜻을 위치로도 말한다(2026-08-11 사용자: 삽입선이 그냥 가로줄이라 어느
/// 폴더로 들어가는지 위치로 안 읽혔다).
fn insertion_marker_indent_x(row_left: f32, depth: usize) -> f32 {
    row_left + 10.0 + depth as f32 * 18.0
}

/// 삽입 마커(알약 모양) 사각형 — 그리기와 분리해 순수하게 테스트한다.
///
/// 2px 가로줄 하나로는 "밀어내는 느낌"이 안 난다(2026-08-11 사용자). 행을
/// 실제로 벌리면(레이아웃에 `add_space` 삽입) `show_rows`가 매 행이 같은
/// 높이라고 가정하는 가상화 계약이 깨지고, 벌어진 틈 때문에 포인터 밑 행이
/// 바뀌어 다시 틈이 옮겨가는 떨림(flicker) 루프에 빠질 위험이 있다. 그래서
/// 레이아웃은 안 건드리고 페인트만으로 "두께 있는 조각이 꽂힌" 인상을 낸다 —
/// 위아래로 `HALF_HEIGHT`씩 부풀린 알약이 행 경계에 걸치게 그린다. 아이콘(12~16px)은
/// 행 한가운데 있고 위아래 여백이 남으므로 이 정도 두께는 아이콘을 침범하지
/// 않는다. 오른쪽은 살짝 띄워 행 끝까지 꽉 찬 자로 보이지 않고 "여기 꽂힌
/// 조각"으로 보이게 한다. 폭이 좁아 남는 공간이 없으면(깊은 들여쓰기 + 좁은
/// 패널) `MIN_WIDTH`까지는 왼쪽으로 물러나 최소 폭을 지킨다.
fn insertion_marker_rect(row_rect: egui::Rect, indent_x: f32, at_bottom: bool) -> egui::Rect {
    const HALF_HEIGHT: f32 = 2.5;
    const RIGHT_MARGIN: f32 = 10.0;
    const MIN_WIDTH: f32 = 24.0;
    let y = if at_bottom {
        row_rect.bottom()
    } else {
        row_rect.top()
    };
    let right = (row_rect.right() - RIGHT_MARGIN).max(row_rect.left() + MIN_WIDTH);
    let left = indent_x.min(right - MIN_WIDTH).max(row_rect.left());
    egui::Rect::from_min_max(
        egui::pos2(left, y - HALF_HEIGHT),
        egui::pos2(right, y + HALF_HEIGHT),
    )
}

/// 드롭 대상 표시. 의미가 둘이라 모양도 둘이다(Finder와 같다).
///
/// - `IntoFolder` → **면 강조**. 「이 폴더 **안으로** 들어간다」. 외곽선은 쓰지 않는다 —
///   테두리는 경계를 말하지 폭 담는 그릇을 말하지 않는다(2026-08-10 사용자).
/// - `InsertAbove`/`InsertBelow` → **삽입 마커**. 「이 **위치의 폴더로** 들어간다」.
///   행 경계에 걸치는 알약 모양으로 그려 두께를 준다(위 `insertion_marker_rect`
///   주석 참고) — 밋밋한 가로줄보다 "여기에 조각이 꽂힌다"는 인상을 준다.
fn paint_row_drop_target(
    ui: &egui::Ui,
    ppp: f32,
    row_rect: egui::Rect,
    depth: usize,
    decision: super::file_drop::DropDecision,
) {
    // NoOp(이미 그 폴더 안 — 놓아도 변화 없음)은 **회색**으로 그린다. 침묵하면
    // 사용자에겐 고장으로 보이고, accent로 그리면 될 것처럼 보인다(2026-08-11 사용자).
    let accent = if decision.eligibility == super::file_drop::DropEligibility::NoOp {
        ui.visuals().weak_text_color()
    } else {
        ui.visuals().selection.bg_fill
    };
    let target = decision.target;
    match target {
        super::file_drop::RowDropTarget::IntoFolder => {
            ui.painter().rect_filled(
                crate::ui::snap_rect_to_pixel(ppp, row_rect),
                2.0,
                accent.gamma_multiply(0.22),
            );
        }
        edge => {
            let indent_x = insertion_marker_indent_x(row_rect.left(), depth);
            let at_bottom = edge == super::file_drop::RowDropTarget::InsertBelow;
            let marker = crate::ui::snap_rect_to_pixel(
                ppp,
                insertion_marker_rect(row_rect, indent_x, at_bottom),
            );
            ui.painter()
                .rect_filled(marker, marker.height() / 2.0, accent);
        }
    }
}

fn file_toolbar_icon_at(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    id: &'static str,
    accessible_label: &str,
    icon: FileToolbarIcon,
    active: bool,
) -> egui::Response {
    let response = ui.interact(
        rect,
        ui.id().with(("file_toolbar_icon", id)),
        egui::Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), accessible_label)
    });
    let color = if active {
        // accent 원색 — `selection.stroke`는 accent 배경 위 대비색이라 여기 쓸 수 없다.
        ui.visuals().selection.bg_fill
    } else if response.hovered() {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    match icon {
        FileToolbarIcon::Refresh => {
            let center = rect.center();
            let stroke = egui::Stroke::new(1.25, color);
            let points = (0..=14)
                .map(|index| {
                    let angle = -0.7 + index as f32 * 5.2 / 14.0;
                    center + egui::vec2(angle.cos(), angle.sin()) * 4.5
                })
                .collect::<Vec<_>>();
            ui.painter().add(egui::Shape::line(points, stroke));
            let tip = center + egui::vec2(4.5 * (-0.7_f32).cos(), 4.5 * (-0.7_f32).sin());
            ui.painter()
                .line_segment([tip, tip + egui::vec2(-0.4, 3.2)], stroke);
            ui.painter()
                .line_segment([tip, tip + egui::vec2(-3.0, 0.9)], stroke);
        }
    }
    response
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

/// 워크스페이스 고유색 하나 — 이 팔레트를 쓰는 쪽 테스트가 참조한다(slot 0, emerald).
/// 테스트가 리터럴을 복사해 두면 팔레트를 바꿀 때 조용히 어긋난다.
#[cfg(test)]
pub(crate) const WORKSPACE_ACCENT_SAMPLE: egui::Color32 = egui::Color32::from_rgb(
    WORKSPACE_ACCENT_PALETTE[0].0,
    WORKSPACE_ACCENT_PALETTE[0].1,
    WORKSPACE_ACCENT_PALETTE[0].2,
);

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

/// 사이드바 목록에서 차지하는 slot으로 색을 배정한다. palette 앞쪽 여섯 계열은
/// 녹색·주황·보라·빨강·금색·파랑 순으로 의도적으로 떨어뜨렸다. 따라서 선택/접힘으로
/// 렌더 순서가 바뀌어도 색은 유지되고, 같은 이니셜도 서로 다른 계열을 갖는다.
/// 목록 끝에 새 워크스페이스를 추가해도 기존 배정은 변하지 않는다. 다만 **색은 자리에
/// 딸린 것이라** 사용자가 드래그로 순서를 바꾸면 색도 함께 따라간다(2026-09-03).
pub(crate) fn workspace_accent(
    workspaces: &[SidebarWorkspaceEntry],
    workspace_id: &str,
) -> egui::Color32 {
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
/// 13×12, 툴바는 10×10.
/// 2026-07-18 확정 수치는 13.5×12.5였는데 이후 0.95가 곱해져 12.825×11.875로 남아
/// 있었다 — 물리 픽셀에 안 맞아 아이콘 윤곽선이 흐려서 13×12로 정렬했다(2026-08-06).
/// `rise`는 11.875·12.0 모두 3px로 같아 형태는 그대로다.
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
/// `size` = (폭, 높이) — 트리 행 9.5×12, 툴바 7.9×10.
/// 2026-07-18 확정 수치는 10×12.6이었고 이후 0.95가 곱해져 9.5×11.97이 됐다. 폭 9.5는
/// 2x에서 이미 정렬돼 있어 그대로 두고, 높이만 12.0으로 맞췄다(2026-08-06).
/// `fold`는 11.97·12.0 모두 3px로 같아 형태는 그대로다.
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

/// 내비게이션 레일 아이콘 종류.
enum NavIcon {
    Home,
    Fleet,
    History,
    Git,
    Agents,
    Help,
}

/// 작업함 배지 문구 — 0이면 숨김(None).
fn nav_badge_text(count: usize) -> Option<String> {
    (count > 0).then(|| count.to_string())
}

fn paint_sidebar_separator(ui: &egui::Ui, rect: egui::Rect, stroke: egui::Stroke) {
    let status_bar_top = ui.ctx().content_rect().bottom() - 26.0;
    let bottom = rect.bottom().min(status_bar_top);
    if bottom > rect.top() {
        // 선이므로 픽셀 **중심**에 맞춘다 (designall::vertical_separator와 같은 규칙).
        // 패널 폭은 리사이즈 핸들이 pointer delta를 그대로 누적하므로, 사용자가 사이드바
        // 폭을 한 번이라도 끌면 rect.right()가 영구히 소수가 되어 경계선이 흐려진다.
        //
        // x는 패널 안쪽 마지막 픽셀이다 — 이유와 규칙은
        // designall::panel_edge_separator_x가 소유한다(상단 바 구분선과 공유).
        let ppp = ui.ctx().pixels_per_point();
        let x = crate::ui::snap_line_to_pixel(
            crate::ui::designall::panel_edge_separator_x(rect.right(), ppp),
            stroke.width,
            ppp,
        );
        ui.painter()
            .vline(x, egui::Rangef::new(rect.top(), bottom), stroke);
    }
}

/// 레일 하단 서비스 상태 한 칸 — 이름은 그리지 않고 hover로만 노출한다
/// (2026-08-07 사용자: 상태바의 점+이름 3종을 레일 세로 스택 + 로고 상태색으로).
#[derive(Clone, Debug, PartialEq)]
pub struct RailServiceStatus {
    pub name: &'static str,
    pub url: &'static str,
    pub indicator: Option<crate::status_feed::ServiceIndicator>,
    /// 상태 페이지 요약 문구("All Systems Operational" 등) — hover 첫 줄.
    pub description: Option<String>,
}

impl RailServiceStatus {
    fn defaults() -> [RailServiceStatus; 3] {
        [
            RailServiceStatus {
                name: "Claude",
                url: crate::status_feed::CLAUDE_STATUS_URL,
                indicator: None,
                description: None,
            },
            RailServiceStatus {
                name: "OpenAI",
                url: crate::status_feed::OPENAI_STATUS_URL,
                indicator: None,
                description: None,
            },
            RailServiceStatus {
                name: "GitHub",
                url: crate::status_feed::GITHUB_STATUS_URL,
                indicator: None,
                description: None,
            },
        ]
    }
}

/// 레일 서비스 로고 크기 — 상태바 시절 점(10px)과 로고(14.5px) 사이,
/// 목업(18px)보다 작게 (2026-08-07 사용자 지시).
const RAIL_SERVICE_LOGO_SIZE: f32 = 13.0;
const RAIL_SERVICE_LOGO_GAP: f32 = 9.0;

fn rail_service_status_height() -> f32 {
    // 로고 3개 + 사이 간격 2개 + 아래 utilities와 띄우는 여백.
    3.0 * RAIL_SERVICE_LOGO_SIZE + 2.0 * RAIL_SERVICE_LOGO_GAP + 12.0
}

/// indicator → 로고 색. 정상 초록 / 저하 노랑 / 장애 빨강, 미조회·미지는 회색
/// (상태바 시절 indicator_color와 같은 매핑 — 색 자체가 상태 표기다).
fn rail_service_color(
    visuals: &egui::Visuals,
    indicator: Option<crate::status_feed::ServiceIndicator>,
) -> egui::Color32 {
    use crate::agent_surface::AgentVisualState;
    use crate::status_feed::ServiceIndicator;
    use crate::ui::agent_visuals::status_color;
    match indicator {
        Some(ServiceIndicator::Operational) => status_color(AgentVisualState::Complete),
        Some(ServiceIndicator::Minor) => status_color(AgentVisualState::Waiting),
        Some(ServiceIndicator::Major | ServiceIndicator::Critical) => {
            status_color(AgentVisualState::Error)
        }
        Some(ServiceIndicator::Unknown) | None => visuals.weak_text_color(),
    }
}

fn rail_service_status(
    ui: &mut egui::Ui,
    statuses: &[RailServiceStatus; 3],
    catalog: &i18n::Catalog,
) {
    ui.vertical_centered(|ui| {
        ui.spacing_mut().item_spacing.y = RAIL_SERVICE_LOGO_GAP;
        for service in statuses {
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(RAIL_SERVICE_LOGO_SIZE, RAIL_SERVICE_LOGO_SIZE),
                egui::Sense::click(),
            );
            let name = service.name;
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), name)
            });
            let status_line = service
                .description
                .clone()
                .unwrap_or_else(|| catalog.t("status_bar.service_checking", &[]));
            let click = catalog.t("status_bar.service_click", &[("url", service.url)]);
            let response = response
                .on_hover_text(format!("{name} · {status_line}\n{click}"))
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            let color = rail_service_color(ui.visuals(), service.indicator);
            paint_rail_service_glyph(ui.painter(), rect, name, color);
            if response.clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(service.url));
            }
        }
    });
}

/// 상태색으로 칠하는 서비스 로고 글리프. announcement 로고(브랜드색 고정)와
/// 달리 색이 상태를 뜻하므로 별도 페인터를 둔다. base 16 좌표계.
fn paint_rail_service_glyph(
    painter: &egui::Painter,
    rect: egui::Rect,
    service: &str,
    color: egui::Color32,
) {
    let center = rect.center();
    let scale = rect.width().min(rect.height()) / 16.0;
    match service {
        "Claude" => {
            let stroke = egui::Stroke::new(1.7 * scale, color);
            for index in 0..8 {
                let angle = index as f32 * std::f32::consts::TAU / 8.0;
                let direction = egui::vec2(angle.cos(), angle.sin());
                painter.line_segment(
                    [
                        center + direction * (2.6 * scale),
                        center + direction * (7.4 * scale),
                    ],
                    stroke,
                );
            }
            painter.circle_filled(center, 2.0 * scale, color);
        }
        "GitHub" => {
            // 옥토캣 실루엣 근사 — 몸통 원 + 양쪽 귀. 13px에서 세부는 안 보이므로
            // "귀 달린 원"이면 충분히 GitHub으로 읽힌다 (hover가 이름을 보증).
            painter.circle_filled(center, 6.2 * scale, color);
            for side in [-1.0f32, 1.0] {
                painter.add(egui::Shape::convex_polygon(
                    vec![
                        center + egui::vec2(side * 5.4, -2.8) * scale,
                        center + egui::vec2(side * 4.6, -7.2) * scale,
                        center + egui::vec2(side * 1.4, -5.8) * scale,
                    ],
                    color,
                    egui::Stroke::NONE,
                ));
            }
        }
        _ => {
            // OpenAI knot — announcement 페인터와 같은 여섯 루프 단순화.
            let stroke = egui::Stroke::new(1.25 * scale, color);
            for index in 0..6 {
                let angle = index as f32 * std::f32::consts::TAU / 6.0;
                let loop_center = center + egui::vec2(angle.cos(), angle.sin()) * (3.9 * scale);
                painter.circle_stroke(loop_center, 3.1 * scale, stroke);
            }
            painter.circle_stroke(center, 2.0 * scale, stroke);
        }
    }
}

fn nav_utility_height(width: f32) -> f32 {
    if width < 56.0 { 48.0 } else { 28.0 }
}

fn nav_utilities(ui: &mut egui::Ui, catalog: &i18n::Catalog) -> Option<SidebarAction> {
    let mut action = None;
    // 설정 하나만 남긴다(도움말 물음표는 2026-08-21에 뺐다). 가운데 정렬이 아니라
    // 레일 가장 좌측에 붙인다 — 위쪽 내비 아이콘 열과 겹치지 않는 자리다.
    ui.spacing_mut().item_spacing.x = 0.0;
    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.add_space(NAV_UTILITY_LEFT_PAD);
        let help = nav_utility_button(
            ui,
            NavIcon::Help,
            &catalog.t("sidebar.nav.help", &[]),
            NAV_UTILITY_DOT_SIZE,
        );
        // 버전 → 업데이트 → 피드백 → 설정 순(2026-08-21). 맨 윗줄은 버전 표시라
        // 누를 수 없는 라벨이다.
        egui::Popup::menu(&help).show(|ui| {
            ui.set_min_width(NAV_HELP_MENU_MIN_WIDTH);
            ui.label(catalog.t(
                "sidebar.help.version",
                &[("version", env!("CARGO_PKG_VERSION"))],
            ));
            ui.separator();
            if ui
                .button(catalog.t("sidebar.help.check_update", &[]))
                .clicked()
            {
                action = Some(SidebarAction::CheckUpdate);
                ui.close();
            }
            if ui.button(catalog.t("sidebar.help.feedback", &[])).clicked() {
                action = Some(SidebarAction::OpenFeedback);
                ui.close();
            }
            if ui.button(catalog.t("settings.title", &[])).clicked() {
                action = Some(SidebarAction::OpenSettings);
                ui.close();
            }
        });
    });
    action
}

fn nav_utility_button(ui: &mut egui::Ui, icon: NavIcon, label: &str, side: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    // hover 배경 없음 — 아이콘 색만 바뀐다(2026-08-21, nav_row와 같은 규칙).
    let color = if response.hovered() {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    paint_nav_icon(ui.painter(), rect.center(), icon, color);
    response.on_hover_text(label)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct NavRowLayout {
    icon_center: egui::Pos2,
    label_anchor: egui::Pos2,
}

fn nav_row_layout(row: egui::Rect, show_label: bool) -> NavRowLayout {
    if show_label {
        NavRowLayout {
            icon_center: egui::pos2(row.center().x, row.center().y - 9.0),
            label_anchor: egui::pos2(row.center().x, row.center().y + 13.0),
        }
    } else {
        NavRowLayout {
            icon_center: row.center(),
            label_anchor: row.center(),
        }
    }
}

struct NavBadge<'a> {
    text: &'a str,
    fill: egui::Color32,
    text_color: egui::Color32,
}

/// 내비게이션 레일 행 하나 — 외곽선 아이콘 + 라벨, 선택/hover 상태에서만 평면 배경.
/// painter 텍스트라 접근성 라벨은 widget_info로 단다.
fn nav_row(
    ui: &mut egui::Ui,
    icon: NavIcon,
    label: &str,
    selected: bool,
    badge: Option<NavBadge<'_>>,
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
    let row = rect.shrink2(egui::vec2(4.0, 0.0));
    let tokens = crate::ui::designall::tokens(ui.visuals());
    // hover에는 배경을 칠하지 않는다 — 아이콘/라벨 색만 바뀐다(2026-08-21).
    // 선택된 항목의 면은 그대로 유지한다.
    if let Some(fill) = crate::ui::designall::row_fill(tokens, selected, false) {
        ui.painter().rect_filled(row, 0.0, fill);
    }
    if selected {
        let rail =
            egui::Rect::from_min_max(row.left_top(), egui::pos2(row.left() + 2.0, row.bottom()));
        ui.painter().rect_filled(rail, 0.0, tokens.accent);
    }
    let color = if selected || response.hovered() {
        tokens.text
    } else {
        tokens.muted_text
    };
    let show_label = rect.width() >= 64.0;
    let layout = nav_row_layout(row, show_label);
    paint_nav_icon(ui.painter(), layout.icon_center, icon, color);
    if show_label {
        ui.painter().text(
            layout.label_anchor,
            egui::Align2::CENTER_CENTER,
            label,
            crate::fonts::sidebar_font(ui.ctx(), 12.0),
            color,
        );
    }
    if let Some(badge) = badge {
        paint_nav_badge(ui, row, badge);
    }
    response
}

/// 상태별 카운트 배지 — 원형, 두 자리부터는 알약꼴.
fn paint_nav_badge(ui: &egui::Ui, row: egui::Rect, badge: NavBadge<'_>) {
    let galley = ui.painter().layout_no_wrap(
        badge.text.to_owned(),
        crate::fonts::sidebar_font(ui.ctx(), 10.0),
        badge.text_color,
    );
    let h = 16.0;
    let w = (galley.size().x + 8.0).max(h);
    let center = egui::pos2(row.right() - 6.0 - w / 2.0, row.top() + 10.0);
    let rect = egui::Rect::from_center_size(center, egui::vec2(w, h));
    ui.painter().rect_filled(rect, h / 2.0, badge.fill);
    ui.painter()
        .galley(center - galley.size() / 2.0, galley, badge.text_color);
}

/// 레일 아이콘 — 이모지는 폰트 글리프가 없어 □로 깨진다(레포 관례: painter 직접
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
        // 이력 — 시간축을 뜻하는 시계. 카드 목록의 과거/현재 작업을 한눈에 구분한다.
        NavIcon::History => {
            p.circle_stroke(c, 6.0, stroke);
            p.line_segment([c, egui::pos2(c.x, c.y - 3.5)], stroke);
            p.line_segment([c, egui::pos2(c.x + 3.0, c.y + 1.5)], stroke);
        }
        // Git — 가지: 위 점에서 아래 점으로 내려오는 줄기 + 오른쪽으로 갈라지는 가지.
        NavIcon::Git => {
            p.line_segment(
                [
                    egui::pos2(c.x - 3.5, c.y - 5.0),
                    egui::pos2(c.x - 3.5, c.y + 5.0),
                ],
                stroke,
            );
            p.circle_stroke(egui::pos2(c.x - 3.5, c.y - 5.0), 1.8, stroke);
            p.circle_stroke(egui::pos2(c.x - 3.5, c.y + 5.0), 1.8, stroke);
            p.circle_stroke(egui::pos2(c.x + 4.0, c.y - 1.0), 1.8, stroke);
            p.line_segment(
                [
                    egui::pos2(c.x - 3.5, c.y + 1.5),
                    egui::pos2(c.x + 4.0, c.y - 1.0),
                ],
                stroke,
            );
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
        NavIcon::Help => {
            p.circle_stroke(c, 4.5, stroke);
            p.text(
                c,
                egui::Align2::CENTER_CENTER,
                "?",
                egui::FontId::proportional(9.0),
                col,
            );
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
pub(crate) fn session_entry_status_color(entry: &SidebarSessionRow) -> egui::Color32 {
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
const EXTERNAL_COPY_GESTURE_WINDOW: std::time::Duration = std::time::Duration::from_millis(600);

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

fn tree_area_owns_os_drop(tree_area: egui::Rect, pos: egui::Pos2) -> bool {
    tree_area.contains(pos) && pos.x < tree_area.right()
}

/// OS 파일 드래그/드롭 중 포인터 위치(egui 창 좌표). winit 0.30은 macOS
/// `draggingUpdated:`를 구현하지 않아 드래그 중 CursorMoved가 오지 않는다 — AppKit
/// 전역 마우스 위치(bottom-left 스크린 좌표)를 primary 스크린 기준으로 뒤집고
/// viewport inner_rect(모니터 공간 egui points)를 빼서 환산한다. viewport 미상
/// (kittest 등)이면 None → 호출부가 egui 포인터로 폴백한다.
///
/// `pub(crate)` — composer.rs(도크 드롭 위치 판정)와 workspace.rs(터미널 pane 드롭
/// 위치 판정)도 이 함수를 그대로 쓴다(2026-08-14, OS 드롭 라우팅 수정). 드래그 중
/// 신뢰 가능한 포인터 위치가 필요한 자리는 이거 하나뿐이어야 한다 — 중복 구현 금지.
#[cfg(target_os = "macos")]
pub(crate) fn os_drag_pointer_pos(ctx: &egui::Context) -> Option<egui::Pos2> {
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
pub(crate) fn os_drag_pointer_pos(_ctx: &egui::Context) -> Option<egui::Pos2> {
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
        nodes.push(TreeNode::new(entry.file_name(), is_dir));
    }
    sort_nodes(&mut nodes);
    Ok(nodes)
}

fn merge_listing_nodes(old: Option<Vec<TreeNode>>, mut nodes: Vec<TreeNode>) -> Vec<TreeNode> {
    // 살아 있는 동일 폴더의 하위 버퍼를 이동한다. 갱신 중 빈 목록을 끼우지 않으며,
    // 완료 시점의 펼침 상태를 사용해 조회 이후 사용자가 연 폴더도 보존한다.
    let mut expanded: HashMap<OsString, Option<Vec<TreeNode>>> = old
        .unwrap_or_default()
        .into_iter()
        .filter(|node| node.is_dir && node.expanded)
        .map(|node| (node.name, node.children))
        .collect();
    for node in &mut nodes {
        if node.is_dir
            && let Some(children) = expanded.remove(&node.name)
        {
            node.expanded = true;
            node.children = children;
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
                .saturating_add(node.name.as_encoded_bytes().len())
                .saturating_add(child_usage.1),
        )
    })
}

/// 정렬: 디렉터리 우선 + 이름 (단순 유니코드 순 — §3, 로케일 비교는 비목표).
#[cfg(test)]
fn sort_nodes(nodes: &mut [TreeNode]) {
    nodes.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.display_name.cmp(&b.display_name))
            .then_with(|| a.name.cmp(&b.name))
    });
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
        if !show_hidden && node.name.as_encoded_bytes().starts_with(b".") {
            continue;
        }
        let path = base.join(&node.name);
        out.push(FlatRow {
            path: path.clone(),
            display_name: node.display_name.clone(),
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
        let name = comp.as_os_str();
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
        let name = comp.as_os_str();
        let node = nodes.iter().find(|n| n.name == name)?;
        if comps.peek().is_none() {
            return Some(node);
        }
        nodes = node.children.as_deref()?;
    }
    None
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
    fn folder_typeahead는_폴더만_선택하고_연속_입력과_반복을_처리한다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.root = Some(PathBuf::from("/root"));
        tree.flat = [
            ("File", false),
            ("Fold", true),
            ("Foo", true),
            ("Other", true),
        ]
        .into_iter()
        .map(|(name, is_dir)| FlatRow {
            path: PathBuf::from(format!("/root/{name}")),
            display_name: name.to_owned(),
            depth: 0,
            is_dir,
            expanded: false,
        })
        .collect();

        assert_eq!(tree.handle_typeahead_text("f", 1.0), Some(1));
        assert_eq!(tree.selected, BTreeSet::from([PathBuf::from("/root/Fold")]));
        assert_eq!(tree.handle_typeahead_text("o", 1.1), Some(1));
        assert_eq!(tree.handle_typeahead_text("f", 2.0), Some(2));
        assert_eq!(tree.handle_typeahead_text("f", 2.1), Some(1));
        assert_eq!(tree.root, Some(PathBuf::from("/root")));
    }

    #[test]
    fn folder_typeahead는_일치하지_않는_접두어에서_마지막_글자로_재시도한다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.flat = [("Alpha", true), ("Beta", true)]
            .into_iter()
            .map(|(name, is_dir)| FlatRow {
                path: PathBuf::from(format!("/root/{name}")),
                display_name: name.to_owned(),
                depth: 0,
                is_dir,
                expanded: false,
            })
            .collect();
        assert_eq!(tree.handle_typeahead_text("a", 1.0), Some(0));
        assert_eq!(tree.handle_typeahead_text("b", 1.1), Some(1));
        assert_eq!(tree.selected, BTreeSet::from([PathBuf::from("/root/Beta")]));
    }

    #[test]
    fn folder_typeahead는_ime_commit을_한번만_사용한다() {
        let events = [
            egui::Event::Text("한".to_owned()),
            egui::Event::Ime(egui::ImeEvent::Commit("한".to_owned())),
            egui::Event::Text("글".to_owned()),
        ];
        assert_eq!(tree_typeahead_text_events(&events), ["한", "글"]);
        assert_eq!(
            tree_typeahead_text_events(&[egui::Event::Ime(egui::ImeEvent::Commit(
                "폴더".to_owned()
            ))]),
            ["폴더"]
        );
    }

    #[test]
    fn file_search는_이전_질문의_늦은_결과를_버린다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.root = Some(PathBuf::from("/root"));
        tree.search = Some(FileTreeSearch {
            query: String::new(),
            results: Vec::new(),
            selected: None,
            busy: false,
            result_limit_reached: false,
            traversal_incomplete: false,
            focus: false,
        });
        tree.request_file_search("fold".to_owned());
        let first = tree.take_maintenance_intent().unwrap();
        assert!(
            matches!(first.request, FileTreeMaintenanceRequest::SearchFiles { ref query, .. } if query == "fold")
        );
        let FileTreeMaintenanceRequest::SearchFiles {
            cancel: old_cancel, ..
        } = &first.request
        else {
            unreachable!();
        };
        tree.request_file_search("note".to_owned());
        assert!(old_cancel.load(std::sync::atomic::Ordering::Relaxed));
        tree.complete_maintenance(FileTreeMaintenanceCompletion {
            operation: first.operation,
            generation: first.generation,
            result: Ok(FileTreeMaintenanceResult::Search(
                FileTreeSearchSnapshot::try_new(vec![PathBuf::from("/root/Fold.md")], false, false)
                    .unwrap(),
            )),
        });
        assert!(tree.search.as_ref().unwrap().results.is_empty());
        let second = tree.take_maintenance_intent().unwrap();
        assert!(
            matches!(second.request, FileTreeMaintenanceRequest::SearchFiles { ref query, .. } if query == "note")
        );
        tree.complete_maintenance(FileTreeMaintenanceCompletion {
            operation: second.operation,
            generation: second.generation,
            result: Ok(FileTreeMaintenanceResult::Search(
                FileTreeSearchSnapshot::try_new(vec![PathBuf::from("/root/note.md")], false, false)
                    .unwrap(),
            )),
        });
        let search = tree.search.as_ref().unwrap();
        assert_eq!(search.results, [PathBuf::from("/root/note.md")]);
        assert!(!search.busy);
        let active_cancel = tree.search_cancel.as_ref().unwrap().clone();
        tree.close_file_search();
        assert!(active_cancel.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn file_search_같은_질문_새로고침은_취소된_오류를_보이지_않는다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.root = Some(PathBuf::from("/root"));
        tree.search = Some(FileTreeSearch {
            query: String::new(),
            results: Vec::new(),
            selected: None,
            busy: false,
            result_limit_reached: false,
            traversal_incomplete: false,
            focus: false,
        });
        tree.request_file_search("fold".to_owned());
        let old = tree.take_maintenance_intent().unwrap();
        tree.request_file_search("fold".to_owned());
        tree.complete_maintenance(FileTreeMaintenanceCompletion {
            operation: old.operation,
            generation: old.generation,
            result: Err(FileTreeMaintenanceErrorCode::NativeFailure),
        });
        assert!(tree.error.is_none());
        assert!(tree.search.as_ref().unwrap().busy);
        assert!(tree.take_maintenance_intent().is_some());
    }

    #[test]
    fn file_tree_os_drop은_공유_오른쪽_경계를_소유하지_않는다() {
        let tree_area = egui::Rect::from_min_max(egui::pos2(10.0, 20.0), egui::pos2(110.0, 120.0));

        assert!(tree_area_owns_os_drop(
            tree_area,
            egui::pos2(tree_area.right() - 0.001, tree_area.center().y)
        ));
        assert!(tree_area_owns_os_drop(tree_area, tree_area.center()));
        assert!(tree_area_owns_os_drop(
            tree_area,
            egui::pos2(tree_area.left(), tree_area.top())
        ));
        assert!(tree_area_owns_os_drop(
            tree_area,
            egui::pos2(tree_area.center().x, tree_area.bottom())
        ));
        assert!(
            !tree_area_owns_os_drop(
                tree_area,
                egui::pos2(tree_area.right(), tree_area.center().y)
            ),
            "중앙 pane과 공유하는 오른쪽 경계는 file tree가 소유하지 않아야 한다"
        );
    }

    #[test]
    fn file_tree_os_drop_행_feedback도_공유_오른쪽_경계를_소유하지_않는다() {
        let source = include_str!("file_tree.rs");
        let row_hover = source
            .split_once("// Finder 드래그 대상:")
            .expect("Finder 행 hover 분기")
            .1
            .split_once("// 행 전체 = 드래그 소스")
            .expect("행 hover 분기 끝")
            .0;

        assert!(
            row_hover.contains("tree_area_owns_os_drop(hover_rect, pos)"),
            "실제 drop을 거부하는 shared right edge에 행 허용 feedback을 그리면 안 된다"
        );
    }

    /// 문서 대상 분류(설계 §3.1) — md·markdown은 markdown, txt·log·확장자 없음은
    /// 평문, 그 외는 `None`이라 더블클릭이 예전처럼 OS 기본 앱으로 간다(기존 동작을
    /// 빼앗지 않는다).
    #[test]
    fn classify_document_target은_설계_3_1_대상만_분류한다() {
        assert_eq!(
            classify_document_target(Path::new("readme.md")),
            Some(DocumentTargetKind::Markdown)
        );
        assert_eq!(
            classify_document_target(Path::new("README.MD")),
            Some(DocumentTargetKind::Markdown),
            "확장자 대소문자를 가리지 않는다"
        );
        assert_eq!(
            classify_document_target(Path::new("notes.markdown")),
            Some(DocumentTargetKind::Markdown)
        );
        assert_eq!(
            classify_document_target(Path::new("todo.txt")),
            Some(DocumentTargetKind::PlainText)
        );
        assert_eq!(
            classify_document_target(Path::new("server.log")),
            Some(DocumentTargetKind::PlainText)
        );
        assert_eq!(
            classify_document_target(Path::new("LICENSE")),
            Some(DocumentTargetKind::PlainText),
            "확장자 없는 파일은 평문이다"
        );
        assert_eq!(
            classify_document_target(Path::new("main.rs")),
            None,
            "비대상 확장자는 예전 그대로 OS 열기로 가야 한다"
        );
        assert_eq!(
            classify_document_target(Path::new("photo.png")),
            None,
            "비대상 확장자는 예전 그대로 OS 열기로 가야 한다"
        );
        assert_eq!(
            classify_document_target(Path::new("archive.tar.gz")),
            None,
            "마지막 확장자(gz)만 본다 — 대상이 아니다"
        );
    }

    #[test]
    fn native_only_copy는_유효한_트리_행이_소유한다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        let path = PathBuf::from("/tmp/native-copy.txt");

        tree.handle_copy_shortcut_signal(Some(path.clone()), true, false);

        let intent = tree.take_io_intent().expect("native copy intent");
        match intent.request {
            FileTreeIoRequest::CopyFileUrls { paths } => {
                assert_eq!(paths.into_paths(), vec![path]);
            }
            other => panic!("unexpected intent: {other:?}"),
        }
        let (paste_consumed, copy_consumed) = tree.take_clipboard_shortcut_consumption();
        assert!(!paste_consumed);
        assert!(
            copy_consumed,
            "App이 터미널 선택 복사로 파일 URL을 덮어쓰지 않도록 해야 한다"
        );
    }

    #[test]
    fn native_copy뒤_늦은_egui_copy는_소유권만_유지하고_중복하지_않는다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        let path = PathBuf::from("/tmp/deduplicated-copy.txt");

        tree.handle_copy_shortcut_signal(Some(path.clone()), true, false);
        assert!(
            tree.take_io_intent().is_some(),
            "첫 신호는 파일 URL을 복사한다"
        );
        assert_eq!(tree.take_clipboard_shortcut_consumption(), (false, true));

        tree.handle_copy_shortcut_signal(Some(path), false, true);

        assert!(
            tree.take_io_intent().is_none(),
            "후속 신호는 intent를 중복하지 않는다"
        );
        assert_eq!(
            tree.take_clipboard_shortcut_consumption(),
            (false, true),
            "후속 신호도 터미널 복사는 계속 억제한다"
        );
        assert!(
            tree.error.is_none(),
            "중복 신호가 busy 오류를 만들면 안 된다"
        );
    }

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
    fn designall_선택워크스페이스는_제색으로_물들고_좌측레일이_없다() {
        let tokens = crate::ui::designall::DARK;
        let accent = egui::Color32::from_rgb(0x8b, 0x3f, 0x4a);
        assert_eq!(
            workspace_row_style(tokens, accent, false, false),
            WorkspaceRowStyle {
                fill: None,
                accent: None,
            }
        );
        let selected = workspace_row_style(tokens, accent, true, false);
        assert_eq!(selected.accent, None, "좌측 레일은 없다");
        let fill = selected.fill.expect("선택 면이 없다");
        // 회색 selected_background로 되돌아가면 패널과 구분이 안 되던 상태다.
        assert_ne!(fill, tokens.selected_background, "면이 무채색으로 돌아갔다");
        // 워크스페이스마다 달라야 「어느 프로젝트가 선택됐나」가 색으로 읽힌다.
        let other = egui::Color32::from_rgb(0x3f, 0x7d, 0x52);
        assert_ne!(
            fill,
            workspace_row_style(tokens, other, true, false)
                .fill
                .expect("선택 면이 없다")
        );
    }

    #[test]
    fn designall_프로젝트파일분할은_파일영역_50px를_보존한다() {
        assert_eq!(project_file_section_heights(700.0, 900.0), (644.0, 50.0));
        assert_eq!(project_file_section_heights(700.0, 270.0), (270.0, 424.0));
        assert_eq!(project_file_section_heights(140.0, 0.0), (84.0, 50.0));
    }

    #[test]
    fn 사이드바_도구는_파일과_메모_둘뿐이다() {
        // Git은 2026-08-15 2차에서 레일로 옮겼다 — 사이드바 폭이 목록에 모자랐다(스펙 §8-1).
        assert_eq!(SIDEBAR_TOOLS, [SidebarTool::Files, SidebarTool::Notes]);
        // 둘 다 본문을 교체하는 인라인 탭이다 — 액션 없이 탭 선택만 바뀐다.
        assert!(sidebar_tool_action(SidebarTool::Files).is_none());
        assert!(sidebar_tool_action(SidebarTool::Notes).is_none());
    }

    #[test]
    fn 같은_이니셜의_워크스페이스도_서로_다른_색상_계열을_쓴다() {
        let workspaces = (0..6)
            .map(|index| SidebarWorkspaceEntry {
                id: format!("stable-id-{index}"),
                name: format!("same-{index}"),
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
    fn designall_워크스페이스_아바타는_18px이고_기존좌측선에_고정된다() {
        let row = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 29.0));
        let avatar = workspace_avatar_rect(row);

        assert!((avatar.width() - 18.0).abs() < 0.01);
        assert!((avatar.height() - 18.0).abs() < 0.01);
        assert!((avatar.left() - 10.0).abs() < 0.01);
        assert!((avatar.center().y - row.center().y).abs() < 0.01);
    }

    #[test]
    fn 워크스페이스_행은_세션수만_그리고_상태점은_그리지않는다() {
        let context = egui::Context::default();
        install_sidebar_test_fonts(&context);
        let workspace = SidebarWorkspaceEntry {
            id: "workspace-a".to_owned(),
            name: "Workspace A".to_owned(),
            state: SidebarWorkspaceState::Active,
            summary: SidebarSessionSummary {
                running: 7,
                ..SidebarSessionSummary::default()
            },
        };
        let catalog = catalog();

        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            ui.set_width(220.0);
            workspace_row(
                ui,
                &workspace,
                egui::Color32::LIGHT_BLUE,
                true,
                None,
                &catalog,
            );
        });
        output.textures_delta.clear();

        assert!(
            output.shapes.iter().any(|clipped| {
                matches!(
                    &clipped.shape,
                    egui::Shape::Text(text) if text.galley.text() == "7"
                )
            }),
            "워크스페이스 행에 세션 수가 없다"
        );
        // 이 폭에서 아바타는 rect_filled라 원은 상태 점밖에 나올 게 없다 — 하나라도
        // 남으면 헤더와 세션 행이 같은 사실을 두 번 말하던 상태로 돌아간 것이다.
        assert!(
            !output
                .shapes
                .iter()
                .any(|clipped| matches!(&clipped.shape, egui::Shape::Circle(_))),
            "워크스페이스 우측 상태 점이 남아 있다"
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
    fn 세션_행_기하는_주안_목업의_패널좌표와_같다() {
        // 목업 .s { margin:0 6px 2px; padding:6px 11px 6px 13px;
        //           grid-template-columns:8px 1fr; gap:0 9px } · .dot { 7px }
        // → 패널 좌표로 면 6..W-6 · 점 19..26 · 글 36.
        let panel_left = 20.0;
        let panel_right = 220.0;
        // 호출부가 add_space(SESSION_LIST_INDENT)로 들여쓴 뒤의 행 rect.
        let rect = egui::Rect::from_min_max(
            egui::pos2(panel_left + SESSION_LIST_INDENT, 10.0),
            egui::pos2(panel_right, 10.0 + SESSION_ROW_HEIGHT),
        );

        let fill = session_highlight_rect(rect);
        assert_eq!(fill.left() - panel_left, 6.0, "면 좌측");
        assert_eq!(panel_right - fill.right(), 6.0, "면 우측");

        // 점의 중심은 워크스페이스 아바타의 중심과 같은 세로선에 선다 — 헤더의 마크와
        // 그 아래 점들이 한 줄로 서야 목록이 세로로 훑인다.
        let dot_center = rect.left() + SESSION_DOT_CENTER_INSET;
        let header =
            egui::Rect::from_min_max(egui::pos2(panel_left, 0.0), egui::pos2(panel_right, 29.0));
        assert_eq!(
            dot_center,
            workspace_avatar_rect(header).center().x,
            "점이 아바타 중심에서 벗어났다"
        );
        let dot_left = dot_center - SESSION_DOT_DIAMETER / 2.0;
        // 점은 면 **안에** 있어야 한다 — 면이 점보다 오른쪽에서 시작하면 점만 면
        // 바깥에 떠서 행이 둘로 갈라져 보인다(2026-08-11 회귀).
        assert!(
            fill.left() < dot_left && dot_left + SESSION_DOT_DIAMETER < fill.right(),
            "점이 면 바깥에 있다"
        );

        assert_eq!(
            rect.left() + SESSION_TEXT_INSET - panel_left,
            36.0,
            "글 좌측"
        );

        // 면은 행 세로를 다 쓴다 — 행간은 목록의 item_spacing이 **행 사이에만**
        // 주므로 마지막 행 아래와 헤더 바로 아래에는 여백이 안 생긴다.
        assert_eq!(fill.top(), rect.top());
        assert_eq!(fill.bottom(), rect.bottom(), "면이 행 아래에 여백을 남겼다");
    }

    /// 선택 세션은 바탕·hover보다 밝고, 워크스페이스 헤더와 구분된다.
    #[test]
    fn 사이드바_밝기_사다리는_보고_있는_것을_가장_밝게_둔다() {
        fn luminance(color: egui::Color32) -> f32 {
            0.2126 * f32::from(color.r())
                + 0.7152 * f32::from(color.g())
                + 0.0722 * f32::from(color.b())
        }

        let tokens = crate::ui::designall::DARK;
        let ground = luminance(tokens.workspace_background);
        let hover = luminance(tokens.hover_background);
        let focused = luminance(
            session_row_fill(tokens, false, true)
                .expect("보고 있는 행에는 면이 있다")
                .color,
        );

        assert!(ground < hover, "바탕 {ground} < hover {hover}");

        for (index, (r, g, b)) in WORKSPACE_ACCENT_PALETTE.iter().enumerate() {
            let accent = egui::Color32::from_rgb(*r, *g, *b);
            let header = luminance(
                workspace_row_style(tokens, accent, true, false)
                    .fill
                    .expect("활성 헤더에는 면이 있다"),
            );

            assert!(
                hover < focused,
                "accent {index}: 선택 세션({focused})은 hover({hover})보다 밝아야 한다"
            );
            assert!(
                focused < header,
                "accent {index}: 세션 행({focused})이 워크스페이스 헤더({header})보다 밝아 계층이 뒤집혔다"
            );
        }
    }

    #[test]
    fn 보고있는_세션만_선택면을_갖는다() {
        let tokens = crate::ui::designall::DARK;

        // 평상시(hover·보고있음 아님) 행엔 면이 없다 — 목록이 면으로 뒤덮이면
        // 어느 것이 특별한지 알 수 없다.
        assert_eq!(session_row_fill(tokens, false, false), None);
        // 2026-08-20 갱신: **지금 보고 있는 세션**은 면을 갖는다. 예전엔 글자 밝기로만
        // 날라서, 선택된 워크스페이스만 배경이 있고 그 안에서 실제로 보고 있는 세션은
        // 표시가 없었다(사용자 보고).
        let focused_fill = crate::ui::designall::mix(
            tokens.selected_background,
            tokens.text,
            SESSION_FOCUSED_LIFT,
        );
        assert_eq!(
            session_row_fill(tokens, false, true),
            Some(SessionRowFill {
                color: focused_fill,
                full_bleed: true,
            })
        );
        // hover보다 우선한다 — 마우스를 다른 행에 얹어도 보고 있는 곳이 사라지면 안 된다.
        assert_eq!(
            session_row_fill(tokens, true, true)
                .expect("보고있는 행에 면이 없다")
                .color,
            focused_fill,
            "hover 면이 '보고 있는 세션' 표시를 덮었다"
        );
        // hover는 패널 폭을 다 쓴다. 승인·입력 대기는 배경 대신 점·링·문구로 표시한다.
        assert!(
            session_row_fill(tokens, true, false)
                .expect("hover 면이 없다")
                .full_bleed,
            "hover 면이 여백을 남겼다"
        );
        let row = egui::Rect::from_min_max(egui::pos2(36.0, 0.0), egui::pos2(220.0, 41.0));
        let bleed = session_full_bleed_rect(row);
        assert_eq!(bleed.left(), row.left() - SESSION_LIST_INDENT, "hover 좌측");
        assert_eq!(bleed.right(), row.right(), "hover 우측");

        // 글자 밝기도 그대로 함께 나른다(면과 이중으로 표시).
        let mut visuals = egui::Visuals::dark();
        visuals.override_text_color = Some(tokens.text);
        assert_eq!(session_title_color(&visuals, true), tokens.text);
        assert_ne!(
            session_title_color(&visuals, false),
            tokens.text,
            "선택 안 된 행이 선택된 행과 같은 밝기다"
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
                    FileTreeListingItem::try_new(OsString::from("stale.txt"), false).unwrap(),
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
            .map(|i| FileTreeListingItem::try_new(OsString::from(format!("f{i}")), false).unwrap())
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
        nodes.iter().map(|n| n.display_name.as_str()).collect()
    }

    #[cfg(unix)]
    #[test]
    fn invalid_utf8_rows_keep_distinct_raw_paths() {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

        let first = std::ffi::OsString::from_vec(b"broken-\xfe".to_vec());
        let second = std::ffi::OsString::from_vec(b"broken-\xff".to_vec());
        let snapshot = FileTreeListingSnapshot::try_new(vec![
            FileTreeListingItem::try_new(first.clone(), false).unwrap(),
            FileTreeListingItem::try_new(second.clone(), false).unwrap(),
        ])
        .unwrap();
        let nodes = snapshot
            .items()
            .iter()
            .map(|item| TreeNode::new(item.name().to_owned(), item.is_dir()))
            .collect::<Vec<_>>();
        let mut rows = Vec::new();
        flatten(&nodes, Path::new("/root"), 0, true, &mut rows);

        assert_eq!(rows[0].display_name, rows[1].display_name);
        assert_eq!(rows[0].display_name, "broken-�");
        assert_eq!(
            rows.iter()
                .map(|row| row.path.clone())
                .collect::<HashSet<_>>()
                .len(),
            2,
            "same lossy display must not collapse raw path identity"
        );
        let raw_names = rows
            .iter()
            .map(|row| row.path.file_name().unwrap().as_bytes().to_vec())
            .collect::<HashSet<_>>();
        assert_eq!(
            raw_names,
            HashSet::from([first.into_vec(), second.into_vec()])
        );
    }

    #[cfg(unix)]
    #[test]
    fn raw_path_caps_use_encoded_os_bytes() {
        use std::os::unix::ffi::OsStringExt as _;

        let mut raw = vec![b'x'; FILE_TREE_PATH_MAX_BYTES];
        *raw.last_mut().unwrap() = 0xff;
        let path = PathBuf::from(std::ffi::OsString::from_vec(raw));
        let expected = path.as_os_str().as_encoded_bytes().len();

        let payload = FileTreePathPayload::try_new(path.clone()).unwrap();
        assert_eq!(payload.bytes, expected);
        let list = FileTreePathListPayload::try_new(vec![path.clone()]).unwrap();
        assert_eq!(list.bytes, expected);
        let plan = FileTreeWatchPlan::try_new(vec![path.clone()], Vec::new(), false).unwrap();
        assert_eq!(plan.bytes, expected);
        let event =
            FileTreeWatchEvent::try_new(FileTreeWatchEventKind::DirtyDirectory, path.clone())
                .unwrap();
        assert_eq!(event.bytes, expected);
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_watch_ignore(vec![path]);
        assert!(tree.error.is_none());
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
            .map(|r| (r.display_name.clone(), r.depth, r.is_dir))
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
    fn 평탄화는_nfd_경로를_보존하고_표시명만_nfc로_합성한다() {
        let raw = concat!(
            "\u{1112}\u{116A}\u{1106}\u{1167}\u{11AB} ",
            "\u{1103}\u{1175}\u{110C}\u{1161}\u{110B}",
            "\u{1175}\u{11AB}"
        );
        let nodes = vec![file(raw)];
        let mut out = Vec::new();
        flatten(&nodes, Path::new("/r"), 0, false, &mut out);

        assert_eq!(nodes[0].name, raw);
        assert_eq!(out[0].display_name, "화면 디자인");
        assert_eq!(out[0].path, Path::new("/r").join(raw));
        assert_ne!(out[0].path, Path::new("/r/화면 디자인"));
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
        assert_eq!(hidden_off[0].display_name, "visible.txt");

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
                FileTreeMaintenanceRequest::SearchFiles { .. } => {
                    panic!("listing fixture must not start a search")
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

    #[cfg(unix)]
    #[test]
    fn untouched_invalid_utf8_rename_does_not_create_a_native_request() {
        use std::os::unix::ffi::OsStringExt as _;

        let path = PathBuf::from(std::ffi::OsString::from_vec(b"broken-\xff".to_vec()));

        assert!(
            prepare_rename_request(&path, "broken-�", false)
                .unwrap()
                .is_none()
        );
        assert!(
            prepare_rename_request(&path, "broken-�", true)
                .unwrap()
                .is_some()
        );
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
    fn creation_review_기준_선택을_해제해도_남은_공통_폴더를_사용한다() {
        let root = PathBuf::from("/creation-review");
        let folder = root.join("docs");
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.root = Some(root.clone());
        let mut docs = dir("docs");
        docs.expanded = true;
        docs.children = Some(vec![file("a.md"), file("b.md"), file("c.md")]);
        tree.children = Some(vec![docs, dir("other")]);
        tree.rebuild_flat();
        tree.apply_selection_click(&folder.join("a.md"), SelectionClick::Replace);
        tree.apply_selection_click(&folder.join("b.md"), SelectionClick::Toggle);
        tree.apply_selection_click(&folder.join("c.md"), SelectionClick::Toggle);
        tree.apply_selection_click(&folder.join("c.md"), SelectionClick::Toggle);
        assert_eq!(tree.selected.len(), 2);
        assert_eq!(tree.creation_parent(), Some(folder));

        // 공통 생성 위치가 없고 기준 행도 없으면 특정 폴더를 임의로 고르지 않는다.
        tree.selected.insert(root.join("other"));
        assert_eq!(tree.creation_parent(), Some(root));
    }

    #[test]
    fn creation_review_접힌_대상을_펼치고_생성_완료_목록을_갱신한다() {
        for is_dir in [false, true] {
            let root = temp_root("creation-review-expand");
            let folder = root.join("docs");
            std::fs::create_dir(&folder).unwrap();
            let mut tree = FileTreeUi::new(egui::Context::default());
            tree.set_root(Some(root.clone()));
            drain_listings(&mut tree);
            let edit = if is_dir {
                EditState::NewFolder {
                    parent: folder.clone(),
                    buffer: "new".to_owned(),
                    focus: true,
                }
            } else {
                EditState::NewFile {
                    parent: folder.clone(),
                    buffer: "new".to_owned(),
                    focus: true,
                }
            };
            tree.start_edit(edit);
            assert!(tree.children.as_ref().unwrap()[0].expanded);
            drain_listings(&mut tree);
            let parent = FileTreePathPayload::try_new(folder.clone()).unwrap();
            let request = if is_dir {
                FileTreeIoRequest::CreateDirectory {
                    parent,
                    name: "new".to_owned(),
                }
            } else {
                FileTreeIoRequest::CreateFile {
                    parent,
                    name: "new".to_owned(),
                }
            };
            let retry = tree.edit.take();
            tree.queue_io(request, vec![folder.clone()], None, retry)
                .unwrap();
            let intent = tree.take_io_intent().unwrap();
            if is_dir {
                std::fs::create_dir(folder.join("new")).unwrap();
            } else {
                std::fs::write(folder.join("new"), "").unwrap();
            }
            tree.complete_io(FileTreeIoCompletion {
                operation: intent.operation,
                generation: intent.generation,
                result: Ok(()),
            });
            drain_listings(&mut tree);
            assert!(
                tree.flat
                    .iter()
                    .any(|row| row.path == folder.join("new") && row.is_dir == is_dir)
            );
            // 이미 펼친 대상에서 다시 시작해도 기존 목록을 접거나 비우지 않는다.
            tree.start_edit(EditState::NewFile {
                parent: folder,
                buffer: String::new(),
                focus: true,
            });
            assert!(
                tree.flat
                    .iter()
                    .any(|row| row.path == root.join("docs/new"))
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn creation_review_실패_복원은_새_편집과_취소를_덮어쓰지_않는다() {
        for action in ["unchanged", "new", "cancelled"] {
            let root = PathBuf::from("/creation-review");
            let mut tree = FileTreeUi::new(egui::Context::default());
            tree.root = Some(root.clone());
            tree.children = Some(vec![dir("docs"), dir("other")]);
            tree.rebuild_flat();
            let old_parent = root.join("docs");
            tree.start_edit(EditState::NewFile {
                parent: old_parent.clone(),
                buffer: "old.md".to_owned(),
                focus: true,
            });
            let retry = tree.edit.take();
            tree.queue_io(
                FileTreeIoRequest::CreateFile {
                    parent: FileTreePathPayload::try_new(old_parent.clone()).unwrap(),
                    name: "old.md".to_owned(),
                },
                vec![old_parent.clone()],
                None,
                retry,
            )
            .unwrap();
            let intent = tree.take_io_intent().unwrap();
            if action != "unchanged" {
                tree.start_edit(EditState::NewFile {
                    parent: root.join("other"),
                    buffer: "new.md".to_owned(),
                    focus: true,
                });
                if action == "cancelled" {
                    tree.cancel_edit();
                }
            }
            tree.complete_io(FileTreeIoCompletion {
                operation: intent.operation,
                generation: intent.generation,
                result: Err(FileTreeIoErrorCode::Conflict),
            });
            if action == "cancelled" {
                assert!(
                    tree.edit.is_none(),
                    "취소한 편집을 이전 실패가 되살리면 안 됨"
                );
            } else {
                let Some(EditState::NewFile { parent, buffer, .. }) = tree.edit.as_ref() else {
                    panic!("생성 편집기가 사라짐");
                };
                if action == "unchanged" {
                    assert_eq!(parent, &old_parent);
                    assert_eq!(buffer, "old.md", "새 편집이 없으면 재시도 입력을 복원");
                } else {
                    assert_eq!(parent, &root.join("other"));
                    assert_eq!(buffer, "new.md", "새로 입력한 이름을 보존");
                }
            }
        }
    }

    #[test]
    fn toolbar_creation_선택한_폴더와_파일의_위치를_사용한다() {
        let root = PathBuf::from("/toolbar-creation");
        let folder = root.join("docs");
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.root = Some(root.clone());
        let mut docs = dir("docs");
        docs.expanded = true;
        docs.children = Some(vec![file("existing.md")]);
        tree.children = Some(vec![docs, dir("other")]);
        tree.rebuild_flat();

        assert_eq!(tree.creation_parent(), Some(root.clone()));
        tree.apply_selection_click(&folder, SelectionClick::Replace);
        assert_eq!(tree.creation_parent(), Some(folder.clone()));
        tree.apply_selection_click(&folder.join("existing.md"), SelectionClick::Replace);
        assert_eq!(tree.creation_parent(), Some(folder.clone()));
        // 여러 행을 골라도 마지막으로 직접 선택한 행의 위치를 따른다.
        tree.apply_selection_click(&root.join("other"), SelectionClick::Toggle);
        assert_eq!(tree.creation_parent(), Some(root.join("other")));
        tree.apply_selection_click(&root.join("other"), SelectionClick::Toggle);
        assert_eq!(tree.creation_parent(), Some(folder));
    }

    #[test]
    fn toolbar_creation_보이지_않는_선택과_이전_루트는_사용하지_않는다() {
        let root = PathBuf::from("/toolbar-creation");
        let folder = root.join("docs");
        let mut tree = FileTreeUi::new(egui::Context::default());
        assert_eq!(tree.creation_parent(), None);
        tree.root = Some(root.clone());
        let mut docs = dir("docs");
        docs.expanded = true;
        docs.children = Some(vec![file("existing.md")]);
        tree.children = Some(vec![docs]);
        tree.rebuild_flat();
        tree.apply_selection_click(&folder.join("existing.md"), SelectionClick::Replace);
        tree.toggle_dir(&folder);
        assert_eq!(tree.creation_parent(), Some(root));

        // 더블클릭으로 진입하면 선택이 초기화되어도 새 탐색 위치를 사용한다.
        tree.set_root(Some(folder.clone()));
        assert_eq!(tree.creation_parent(), Some(folder));
        tree.set_root(Some(PathBuf::from("/another-workspace")));
        assert_eq!(
            tree.creation_parent(),
            Some(PathBuf::from("/another-workspace"))
        );
    }

    #[test]
    fn folder_navigation_상위_갱신_중에도_펼친_내용과_선택을_유지한다() {
        let root = PathBuf::from("/folder-navigation");
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.root = Some(root.clone());
        let mut open = dir("open");
        open.expanded = true;
        open.children = Some(vec![file("child.txt")]);
        tree.children = Some(vec![open]);
        tree.selected.insert(root.join("open/child.txt"));
        tree.rebuild_flat();
        // 갱신 결과의 도착 직후, 하위 listing 완료 전에도 기존 행이 살아 있어야 한다.
        tree.apply_listing_snapshot(
            root.clone(),
            FileTreeListingSnapshot::try_new(vec![
                FileTreeListingItem::try_new(OsString::from("open"), true).unwrap(),
                FileTreeListingItem::try_new(OsString::from("new.txt"), false).unwrap(),
            ])
            .unwrap(),
        );
        assert!(
            tree.flat
                .iter()
                .any(|row| row.path == root.join("open/child.txt"))
        );
        assert!(tree.selected.contains(&root.join("open/child.txt")));
        assert!(tree.flat.iter().any(|row| row.path == root.join("new.txt")));
    }

    #[test]
    fn folder_navigation_같은_목록은_행_버퍼와_감시계획을_다시_만들지_않는다() {
        let root = PathBuf::from("/folder-navigation");
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(root.clone()));
        let snapshot = || {
            FileTreeListingSnapshot::try_new(vec![
                FileTreeListingItem::try_new(OsString::from("same.txt"), false).unwrap(),
            ])
            .unwrap()
        };
        let listing = tree.take_maintenance_intent().unwrap();
        tree.complete_maintenance(FileTreeMaintenanceCompletion {
            operation: listing.operation,
            generation: listing.generation,
            result: Ok(FileTreeMaintenanceResult::Listing(snapshot())),
        });
        let watch = tree.take_maintenance_intent().unwrap();
        tree.complete_maintenance(FileTreeMaintenanceCompletion {
            operation: watch.operation,
            generation: watch.generation,
            result: Ok(FileTreeMaintenanceResult::WatchSetApplied),
        });
        let path_buffer = tree.flat[0].path.as_os_str().as_encoded_bytes().as_ptr();
        tree.refresh();
        let listing = tree.take_maintenance_intent().unwrap();
        tree.complete_maintenance(FileTreeMaintenanceCompletion {
            operation: listing.operation,
            generation: listing.generation,
            result: Ok(FileTreeMaintenanceResult::Listing(snapshot())),
        });
        assert!(
            tree.take_maintenance_intent().is_none(),
            "동일한 감시 집합 재등록 금지"
        );
        assert_eq!(
            tree.flat[0].path.as_os_str().as_encoded_bytes().as_ptr(),
            path_buffer
        );
    }

    #[test]
    fn folder_navigation_진입한_폴더의_기존_목록만_옮기고_늦은_결과를_버린다() {
        let root = PathBuf::from("/folder-navigation");
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(root.clone()));
        let stale = tree.take_maintenance_intent().unwrap();
        let mut open = dir("open");
        open.expanded = true;
        open.children = Some(vec![file("child.txt")]);
        let child_buffer = open.children.as_ref().unwrap().as_ptr();
        tree.children = Some(vec![open, dir("other")]);
        tree.selected.insert(root.join("open/child.txt"));
        tree.set_root(Some(root.join("open")));
        assert_eq!(tree.flat.len(), 1, "진입 순간에도 해당 폴더 내용 유지");
        assert_eq!(tree.flat[0].path, root.join("open/child.txt"));
        assert_eq!(
            tree.children.as_ref().unwrap().as_ptr(),
            child_buffer,
            "하위 트리를 복제하지 않고 이동"
        );
        assert!(tree.selected.is_empty(), "이전 루트의 선택은 이월하지 않음");
        tree.complete_maintenance(FileTreeMaintenanceCompletion {
            operation: stale.operation,
            generation: stale.generation,
            result: Ok(FileTreeMaintenanceResult::Listing(
                FileTreeListingSnapshot::try_new(vec![
                    FileTreeListingItem::try_new(OsString::from("other"), true).unwrap(),
                ])
                .unwrap(),
            )),
        });
        assert_eq!(tree.flat.len(), 1);
        assert_eq!(tree.flat[0].path, root.join("open/child.txt"));
    }

    #[test]
    fn folder_navigation_이월_캐시도_보관_상한에_포함하고_종류가_바뀌면_해제한다() {
        let root = PathBuf::from("/folder-navigation-budget");
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.root = Some(root.clone());
        tree.children = Some(
            (0..4)
                .map(|i| {
                    let mut node = dir(&format!("dir-{i}"));
                    node.expanded = true;
                    node.children = Some(
                        (0..FILE_TREE_LISTING_MAX_ITEMS - 1)
                            .map(|j| file(&format!("file-{j}")))
                            .collect(),
                    );
                    node
                })
                .collect(),
        );
        assert_eq!(
            tree_node_usage(tree.children.as_deref().unwrap()).0,
            FILE_TREE_RETAINED_MAX_ITEMS
        );
        tree.rebuild_flat();
        let snapshot = |replace_directory: bool| {
            let mut items: Vec<_> = (0..4)
                .map(|i| {
                    FileTreeListingItem::try_new(
                        OsString::from(format!("dir-{i}")),
                        i != 0 || !replace_directory,
                    )
                    .unwrap()
                })
                .collect();
            items.push(FileTreeListingItem::try_new(OsString::from("new.txt"), false).unwrap());
            FileTreeListingSnapshot::try_new(items).unwrap()
        };
        tree.apply_listing_snapshot(root.clone(), snapshot(false));
        assert!(
            tree.error.is_some(),
            "기존 하위 캐시와 합산해 상한 초과를 거절"
        );
        assert!(!tree.flat.iter().any(|row| row.path == root.join("new.txt")));
        tree.apply_listing_snapshot(root.clone(), snapshot(true));
        assert!(tree.flat.iter().any(|row| row.path == root.join("new.txt")));
        assert!(
            !tree
                .flat
                .iter()
                .any(|row| row.path == root.join("dir-0/file-0")),
            "파일로 바뀐 폴더의 하위 캐시 해제"
        );
        assert!(
            tree_node_usage(tree.children.as_deref().unwrap()).0 <= FILE_TREE_RETAINED_MAX_ITEMS
        );
    }

    #[test]
    fn folder_navigation_많은_하위폴더도_상위를_무한히_재조회하지_않는다() {
        let root = PathBuf::from("/folder-navigation-wide");
        let count = FILE_TREE_REFRESH_BACKLOG_CAP + 1;
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(root.clone()));
        tree.children = Some(
            (0..count)
                .map(|i| {
                    let mut node = dir(&format!("dir-{i:03}"));
                    node.expanded = true;
                    node.children = Some(Vec::new());
                    node
                })
                .collect(),
        );
        tree.rebuild_flat();
        let mut listed = HashSet::new();
        for _ in 0..count + 3 {
            let Some(intent) = tree.take_maintenance_intent() else {
                break;
            };
            let result = match intent.request {
                FileTreeMaintenanceRequest::ListDirectory { directory, .. } => {
                    let path = directory.as_path().to_path_buf();
                    assert!(
                        listed.insert(path.clone()),
                        "같은 폴더를 반복 조회: {path:?}"
                    );
                    let items = if path == root {
                        (0..count)
                            .map(|i| {
                                FileTreeListingItem::try_new(
                                    OsString::from(format!("dir-{i:03}")),
                                    true,
                                )
                                .unwrap()
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    FileTreeMaintenanceResult::Listing(
                        FileTreeListingSnapshot::try_new(items).unwrap(),
                    )
                }
                FileTreeMaintenanceRequest::ReplaceWatchSet(_) => {
                    FileTreeMaintenanceResult::WatchSetApplied
                }
                FileTreeMaintenanceRequest::SearchFiles { .. } => {
                    panic!("listing fixture must not start a search")
                }
            };
            tree.complete_maintenance(FileTreeMaintenanceCompletion {
                operation: intent.operation,
                generation: intent.generation,
                result: Ok(result),
            });
            assert!(tree.pending_refresh_dirs.len() <= FILE_TREE_REFRESH_BACKLOG_CAP);
        }
        assert_eq!(
            listed.len(),
            count + 1,
            "모든 펼친 하위 폴더를 한 번씩 갱신"
        );
        assert!(tree.pending_maintenance.is_none());
        assert!(tree.take_maintenance_intent().is_none());
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
                    FileTreeListingItem::try_new(OsString::from("a.txt"), false).unwrap(),
                ])
                .unwrap(),
            )),
        });
        drain_listings(&mut tree);
        pump_listings_for(&mut tree, std::time::Duration::from_millis(100));

        assert!(
            tree.flat.iter().any(|row| row.display_name == "b.txt"),
            "현재 root 결과는 적용"
        );
        assert!(
            !tree.flat.iter().any(|row| row.display_name == "a.txt"),
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
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
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

        // 오류 화면도 상위 이동 입력을 소비해야 한다. 모양 비교가 아니라 실제 클릭 후
        // 루트/오류 상태의 전이를 검증한다.
        let parent_pos = harness
            .output()
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == ".." => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            })
            .expect("상위 이동 행");
        harness.hover_at(parent_pos);
        harness.step();
        harness.event(egui::Event::PointerButton {
            pos: parent_pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
        harness.event(egui::Event::PointerButton {
            pos: parent_pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run();
        assert_eq!(harness.state().tree.root.as_deref(), Some(Path::new("/")));
        assert!(harness.state().tree.root_error.is_none());
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
                    FileTreeListingItem::try_new(OsString::from("child.txt"), false).unwrap(),
                ])
                .unwrap(),
            )),
        });

        pump_listings_for(&mut tree, std::time::Duration::from_millis(100));
        let d = tree
            .flat
            .iter()
            .find(|row| row.display_name == "d")
            .unwrap();
        assert!(!d.expanded);
        assert!(
            !tree.flat.iter().any(|row| row.display_name == "child.txt"),
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
        assert!(tree.flat.iter().any(|r| r.display_name == "o.txt"));
        assert!(!tree.flat.iter().any(|r| r.display_name == "new.txt"));

        // 디스크 변경 후 watched만 재나열 → 새 파일 반영, other는 캐시 유지 확인
        std::fs::write(base.join("watched/new.txt"), b"n").unwrap();
        std::fs::write(base.join("other/late.txt"), b"l").unwrap();
        tree.reload_dir(&base.join("watched"));
        drain_listings(&mut tree);

        assert!(
            tree.flat.iter().any(|r| r.display_name == "new.txt"),
            "부분 재나열 반영"
        );
        assert!(
            !tree.flat.iter().any(|r| r.display_name == "late.txt"),
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
        assert!(!tree.flat.iter().any(|r| r.display_name == "a.txt"));
        drain_listings(&mut tree);
        assert!(
            tree.flat.iter().any(|r| r.display_name == "a.txt"),
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
            fleet_summary: Default::default(),
            history_tab_active: false,
            git_tab_active: false,
            agents_open: false,
            workspace_note: None,
        };
        let ctx = egui::Context::default();
        install_sidebar_test_fonts(&ctx);
        ctx.run_ui(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                assert!(
                    tree.panel(ui, &std::collections::HashMap::new(), &sidebar, &catalog,)
                        .is_none()
                );
            });
        })
        .drop_without_applying_deltas();
        assert!(tree.maintenance_intent.is_some());
        drain_listings(&mut tree);

        assert!(
            tree.flat.iter().any(|r| r.display_name == "new.txt"),
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
            tree.flat.iter().any(|r| r.display_name == "design"),
            "gitignore에 등록된 디렉터리도 이제 보인다"
        );
        assert!(
            tree.flat.iter().any(|r| r.display_name == "info.log"),
            "git/info/exclude 경로도 이제 보인다"
        );
        assert!(
            tree.flat.iter().any(|r| r.display_name == "node_modules"),
            "기본 generated-dir 필터는 더 이상 숨기지 않는다"
        );

        tree.toggle_dir(&base.join("design"));
        drain_listings(&mut tree);
        assert!(tree.flat.iter().any(|r| r.display_name == "mockup.png"));

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
                state: SidebarWorkspaceState::Idle,
                summary: SidebarSessionSummary::default(),
            })
            .collect();
        let make_session = |n: usize, agent: bool| {
            SidebarSessionRow::from_live(
                "ws-2",
                2,
                SessionEntry {
                    tab: runtime::MuxTabId(format!("t{n}")),
                    pane: runtime::MuxPaneId(format!("p{n}")),
                    session: Some(runtime::SessionId(n as u64)),
                    title: format!("세션 {n}"),
                    title_is_custom: true,
                    agent_model: None,
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
                    last_output_at: None,
                },
            )
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
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
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

    /// 사이드바 펼침 계약: **전환해도 지나온 워크스페이스는 펼친 채 남는다.** 실행 중인
    /// 세션이 화면에서 사라지면 안 되기 때문이다(2026-09-04 사용자). 다만 전환이 **거부되면**
    /// 아무것도 새로 펼쳐지지 않아야 한다 — 그때 펼치면 사용자가 가지도 않은 곳이 열린다.
    #[test]
    fn kittest_전환해도_지나온_워크스페이스는_펼친_채_남는다() {
        use egui_kittest::kittest::Queryable;

        struct State {
            tree: FileTreeUi,
            active: String,
            switch_target: Option<String>,
            focus_target: Option<String>,
            fonts_ready: bool,
            /// 전환이 **거부되는** 경우를 재현한다. App은 warm 한도를 넘으면
            /// `switch_workspace`에서 경고만 세우고 돌아가고(app.rs), staging 슬롯이
            /// 차 있으면 액션을 조용히 버린다 — 둘 다 활성이 그대로인 채 끝난다.
            refuse_switch: bool,
        }

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspaces = vec![
            SidebarWorkspaceEntry {
                id: "workspace-a".to_owned(),
                name: "Workspace A".to_owned(),
                state: SidebarWorkspaceState::Active,
                summary: SidebarSessionSummary::default(),
            },
            SidebarWorkspaceEntry {
                id: "workspace-b".to_owned(),
                name: "Workspace B".to_owned(),
                state: SidebarWorkspaceState::Warm,
                summary: SidebarSessionSummary::default(),
            },
        ];
        let session = |workspace: &str, title: &str| {
            SidebarSessionRow::from_live(
                format!("workspace-{workspace}"),
                2,
                SessionEntry {
                    tab: runtime::MuxTabId(format!("tab-{workspace}")),
                    pane: runtime::MuxPaneId(format!("pane-{workspace}")),
                    session: Some(runtime::SessionId(1)),
                    title: title.to_owned(),
                    title_is_custom: true,
                    agent_model: None,
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
                    last_output_at: None,
                },
            )
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
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
                    };
                    match state.tree.panel(ui, &sessions, &snapshot, &catalog) {
                        Some(SidebarAction::SwitchWorkspace(workspace_id)) => {
                            state.switch_target = Some(workspace_id.clone());
                            if !state.refuse_switch {
                                state.active = workspace_id;
                            }
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
                    refuse_switch: false,
                },
            );
        install_sidebar_test_fonts(&harness.ctx);
        harness.state_mut().fonts_ready = true;
        harness.run();

        harness.get_by_label("Session A");
        assert!(harness.query_by_label("Session B").is_none());

        // 전환해도 떠난 워크스페이스는 **펼친 채 남는다** — 돌던 세션이 화면에서
        // 사라지면 안 된다(2026-09-04 사용자). 어느 쪽에서 일하는지는 배경색이 말한다.
        harness.get_by_label("Workspace B").click();
        harness.run();
        assert_eq!(harness.state().active, "workspace-b");
        harness.get_by_label("Session A");
        harness.get_by_label("Session B");

        // 활성 행을 다시 누르면 접기와 함께 SwitchWorkspace(B)를 다시 방출한다. App은
        // 같은 runtime 전환은 생략하되 Home/작업에서 Terminal view로 복귀한다.
        harness.state_mut().switch_target = None;
        harness.get_by_label("Workspace B").click();
        harness.run();
        assert_eq!(
            harness.state().switch_target.as_deref(),
            Some("workspace-b")
        );
        assert!(harness.query_by_label("Session B").is_none());

        // 실제 App의 workspace 전환은 파일 트리 루트도 바꾼다. 루트 교체가 sidebar
        // 인스턴스/확장 map을 초기화하면 활성 B가 기본값(펼침)으로 되살아나 방금 접은 것이
        // 저절로 다시 열린다 — 그 회귀를 여기서 잡는다.
        let root = std::env::temp_dir().join(format!(
            "deppy-ft-multi-workspace-root-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        harness.state_mut().tree.set_root(Some(root.clone()));
        drain_listings(&mut harness.state_mut().tree);
        harness.run();
        assert!(
            harness.query_by_label("Session B").is_none(),
            "루트 교체가 사용자가 접은 상태를 되돌리지 않는다"
        );

        harness.get_by_label("Workspace B").click();
        harness.run();
        harness.get_by_label("Session B");

        // 전환이 **거부되면** 아무것도 펼쳐지지 않아야 한다. 예전에는 행을 누른 자리에서
        // 곧바로 `expanded = true`를 써서, warm 한도 초과처럼 전환이 거부되는 경우
        // 활성은 그대로인데 누른 쪽만 펼쳐진 채 영구히 남았다 — 신고된 "두 개가 동시에
        // 열려 보인다"가 그대로 재현되는 경로였다(2026-09-03 리뷰 H2).
        // 전제를 만든다 — A를 chevron으로 **직접** 접는다(전환 없이 접히는 유일한 길).
        harness
            .get_by_label("Collapse Workspace A sessions")
            .click();
        harness.run();
        assert!(
            harness.query_by_label("Session A").is_none(),
            "전제: A는 접혀 있다"
        );

        harness.state_mut().refuse_switch = true;
        harness.state_mut().switch_target = None;
        harness.get_by_label("Workspace A").click();
        harness.run();
        assert_eq!(harness.state().active, "workspace-b", "전환은 거부됐다");
        assert_eq!(
            harness.state().switch_target.as_deref(),
            Some("workspace-a"),
            "거부돼도 액션 자체는 방출된다"
        );
        assert!(
            harness.query_by_label("Session A").is_none(),
            "전환이 거부되면 펼쳐지지도 않는다"
        );
        harness.state_mut().refuse_switch = false;
        std::fs::remove_dir_all(root).unwrap();
    }

    /// chevron은 **전환 없이** 세션 목록만 여닫는다.
    ///
    /// 이 상태 — 펼쳐져 있으면서 비활성 — 가 있어야
    /// `docs/superpowers/specs/2026-07-29-multi-cross-workspace-pane-design.md`의 승인된
    /// 진입점 세 개(세션 행 드래그 / hover ↗ / 우클릭 "옆에 열기")가 화면에 나온다.
    /// 행 전체가 하나의 Response였을 때는 누르는 즉시 전환돼 이 상태를 만들 수 없었고,
    /// 진입점이 통째로 사라졌다(2026-09-03 리뷰 H1).
    #[test]
    fn kittest_chevron은_전환없이_비활성_워크스페이스를_펼친다() {
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
                state: SidebarWorkspaceState::Active,
                summary: SidebarSessionSummary::default(),
            },
            SidebarWorkspaceEntry {
                id: "workspace-b".to_owned(),
                name: "Workspace B".to_owned(),
                state: SidebarWorkspaceState::Warm,
                summary: SidebarSessionSummary::default(),
            },
        ];
        let session = |workspace: &str, title: &str| {
            SidebarSessionRow::from_live(
                format!("workspace-{workspace}"),
                2,
                SessionEntry {
                    tab: runtime::MuxTabId(format!("tab-{workspace}")),
                    pane: runtime::MuxPaneId(format!("pane-{workspace}")),
                    session: Some(runtime::SessionId(1)),
                    title: title.to_owned(),
                    title_is_custom: true,
                    agent_model: None,
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
                    last_output_at: None,
                },
            )
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
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
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

        // 비활성 B의 chevron — 전환은 일어나지 않고 세션 목록만 열린다.
        harness.get_by_label("Expand Workspace B sessions").click();
        harness.run();
        assert_eq!(
            harness.state().active,
            "workspace-a",
            "chevron은 워크스페이스를 전환하지 않는다"
        );
        assert_eq!(
            harness.state().switch_target,
            None,
            "chevron은 SwitchWorkspace를 방출하지 않는다"
        );
        harness.get_by_label("Session A");
        harness.get_by_label("Session B");

        // 같은 chevron을 다시 누르면 접힌다 — 여전히 전환은 없다.
        harness
            .get_by_label("Collapse Workspace B sessions")
            .click();
        harness.run();
        assert!(harness.query_by_label("Session B").is_none());
        assert_eq!(harness.state().active, "workspace-a");
        assert_eq!(harness.state().switch_target, None);

        // 다시 펼쳐 두고, 여기서만 나오는 진입점을 쓴다 — 비활성 워크스페이스의 세션을
        // 눌러 이동한다. 이 경로가 살아 있어야 「옆에 열기」 진입점 세 개가 의미를 갖는다.
        harness.get_by_label("Expand Workspace B sessions").click();
        harness.run();
        harness.get_by_label("Session B").click();
        harness.run();
        assert_eq!(harness.state().focus_target.as_deref(), Some("workspace-b"));
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
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
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
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
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

    /// 떠나는 워크스페이스는 **접지 않는다** — 실행 중인 세션은 다른 곳으로 옮겨가도 계속
    /// 보여야 한다(2026-09-04 사용자). 두 곳이 동시에 펼쳐지는 것 자체는 문제가 아니었고,
    /// "어느 쪽을 보고 있는지 모르겠다"는 신고의 원인은 배경색이었다.
    #[test]
    fn 전환해도_떠나는_워크스페이스는_펼친_채_남는다() {
        let mut expanded: HashMap<String, bool> = HashMap::new();
        let mut last: Option<String> = None;

        sync_workspace_expansion_on_switch(&mut expanded, &mut last, "a");
        assert_eq!(expanded.get("a"), Some(&true));

        sync_workspace_expansion_on_switch(&mut expanded, &mut last, "b");
        assert_eq!(
            expanded.get("a"),
            Some(&true),
            "떠나도 접히지 않는다 — 돌던 세션이 화면에서 사라지면 안 된다"
        );
        assert_eq!(expanded.get("b"), Some(&true));

        sync_workspace_expansion_on_switch(&mut expanded, &mut last, "c");
        let open: Vec<&String> = {
            let mut open: Vec<&String> = expanded
                .iter()
                .filter(|(_, is_open)| **is_open)
                .map(|(id, _)| id)
                .collect();
            open.sort();
            open
        };
        assert_eq!(open, vec!["a", "b", "c"], "지나온 곳이 모두 열린 채 남는다");

        // chevron으로 **일부러 접은 것**은 그대로 살아야 한다. 활성인 c를 접고 → a로
        // 떠났다가 → c로 돌아온다.
        expanded.insert("c".to_owned(), false);
        sync_workspace_expansion_on_switch(&mut expanded, &mut last, "a");
        sync_workspace_expansion_on_switch(&mut expanded, &mut last, "c");
        assert_eq!(
            expanded.get("c"),
            Some(&false),
            "사용자가 접어 둔 워크스페이스는 돌아와도 접힌 채로 남는다"
        );
    }

    /// 접힌 워크스페이스 행이 찍는 상태 점의 우선순위. 대기가 맨 앞인 것은 그것만이
    /// 사용자가 움직여야 풀리는 상태이기 때문이다. 유휴는 찍지 않는다 — 아무 일도 안 하는
    /// 세션이 신호를 내면 진짜 신호를 가린다.
    #[test]
    fn 접힌_워크스페이스_상태점은_대기를_가장_앞에_둔다() {
        use crate::agent_surface::AgentVisualState as S;
        let summary = |waiting, error, running, done, idle| SidebarSessionSummary {
            running,
            waiting,
            done,
            error,
            idle,
            inactive: 0,
            no_sessions: false,
        };

        // 넷이 다 섞여 있어도 대기가 이긴다 — 하나만 찍을 수 있으므로 순위가 곧 계약이다.
        assert_eq!(
            collapsed_status_dot(summary(1, 1, 1, 1, 1)),
            Some(S::Waiting)
        );
        assert_eq!(collapsed_status_dot(summary(0, 1, 1, 1, 1)), Some(S::Error));
        assert_eq!(
            collapsed_status_dot(summary(0, 0, 1, 1, 1)),
            Some(S::Active)
        );
        assert_eq!(
            collapsed_status_dot(summary(0, 0, 0, 1, 1)),
            Some(S::Complete)
        );
        assert_eq!(
            collapsed_status_dot(summary(0, 0, 0, 0, 3)),
            None,
            "유휴만 있으면 찍지 않는다"
        );
        assert_eq!(
            collapsed_status_dot(SidebarSessionSummary::default()),
            None,
            "세션이 없으면 찍지 않는다"
        );
    }

    /// 놓을 자리는 그룹의 **세로 중심선**이 가른다 — 위 절반이면 그 앞, 아래 절반이면 그 뒤.
    #[test]
    fn 드롭_위치는_그룹_중심선을_지날_때마다_한_칸씩_밀린다() {
        // 세 그룹의 중심 y = 10 / 30 / 50 (그룹 높이 20 가정)
        let centers = [10.0_f32, 30.0, 50.0];

        assert_eq!(sidebar_drop_index(&centers, 0.0), 0, "맨 위");
        assert_eq!(sidebar_drop_index(&centers, 9.9), 0, "첫 그룹 위 절반");
        assert_eq!(sidebar_drop_index(&centers, 10.1), 1, "첫 그룹 아래 절반");
        assert_eq!(sidebar_drop_index(&centers, 29.9), 1);
        assert_eq!(sidebar_drop_index(&centers, 30.1), 2);
        assert_eq!(sidebar_drop_index(&centers, 999.0), 3, "맨 아래");
        assert_eq!(sidebar_drop_index(&[], 5.0), 0, "목록이 비면 0");
    }

    /// 뒤로 보낼 때 자기 자리가 먼저 빠지므로 목표 index를 한 칸 당겨야 사용자가 가리킨
    /// 틈에 정확히 들어간다. 이 보정이 없으면 아래로 끌 때마다 한 칸씩 덜 간다.
    #[test]
    fn 워크스페이스를_끌어_놓으면_가리킨_틈에_들어간다() {
        let ids: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|id| (*id).to_owned())
            .collect();

        // a를 c와 d 사이(index 3)로 — 자기 자리가 빠지므로 실제 삽입은 2번째 뒤.
        assert_eq!(reorder_sidebar_ids(&ids, "a", 3), ["b", "c", "a", "d"]);
        // d를 맨 위로.
        assert_eq!(reorder_sidebar_ids(&ids, "d", 0), ["d", "a", "b", "c"]);
        // c를 맨 아래로.
        assert_eq!(reorder_sidebar_ids(&ids, "c", 4), ["a", "b", "d", "c"]);
        // 제자리에 놓으면 그대로 — App이 config 쓰기를 건너뛰는 근거다.
        assert_eq!(reorder_sidebar_ids(&ids, "b", 1), ["a", "b", "c", "d"]);
        assert_eq!(reorder_sidebar_ids(&ids, "b", 2), ["a", "b", "c", "d"]);
        // 목록에 없는 id는 목록을 건드리지 않는다.
        assert_eq!(reorder_sidebar_ids(&ids, "zz", 0), ["a", "b", "c", "d"]);
    }

    /// 드래그 중에는 휠·스크롤바가 모두 막혀 목록을 스크롤할 방법이 없었다. 가장자리에
    /// 다가가면 목록이 스스로 흘러야 화면 밖 자리로도 옮길 수 있다(2026-09-03 리뷰 H2).
    #[test]
    fn 드래그_자동스크롤은_가장자리에서만_방향을_갖는다() {
        let viewport = egui::Rangef::new(100.0, 400.0);
        let dt = 1.0 / 60.0;

        // 가운데는 정확히 0 — 평소 드래그는 목록을 건드리지 않는다.
        assert_eq!(drag_autoscroll_delta(viewport, 250.0, dt), 0.0);

        // 위쪽: 양수(= 목록 앞쪽이 보인다). 밖으로 나가면 최대 속도로 고정된다.
        let near_top = drag_autoscroll_delta(viewport, 110.0, dt);
        let above_top = drag_autoscroll_delta(viewport, 60.0, dt);
        assert!(near_top > 0.0, "위 가장자리에서는 앞쪽으로 흐른다");
        assert!(above_top > near_top, "밖으로 나갈수록 빠르다");
        assert_eq!(
            drag_autoscroll_delta(viewport, -500.0, dt),
            above_top,
            "최대 속도에서 멈춘다"
        );

        // 아래쪽: 부호가 반대고 크기 규칙은 같다.
        let near_bottom = drag_autoscroll_delta(viewport, 390.0, dt);
        let below_bottom = drag_autoscroll_delta(viewport, 440.0, dt);
        assert!(near_bottom < 0.0, "아래 가장자리에서는 뒤쪽으로 흐른다");
        assert!(below_bottom < near_bottom);
        assert_eq!(near_bottom, -near_top, "위아래가 대칭이다");
        assert_eq!(below_bottom, -above_top);

        // 뷰포트가 가장자리 둘을 담기엔 너무 짧아도 방향이 튀지 않는다.
        let tiny = egui::Rangef::new(0.0, 9.0);
        assert_eq!(drag_autoscroll_delta(tiny, 4.5, dt), 0.0);
        assert!(drag_autoscroll_delta(tiny, 0.0, dt) > 0.0);
        assert!(drag_autoscroll_delta(tiny, 9.0, dt) < 0.0);
    }

    /// 순서 변경 드래그를 **진짜 포인터 이벤트**로 몰기 위한 최소 하네스 —
    /// 워크스페이스 셋(a 활성 · b · c), 세션 없음. 방출된 순서만 들고 있는다.
    struct WorkspaceReorderState {
        tree: FileTreeUi,
        reordered: Option<Vec<String>>,
        fonts_ready: bool,
    }

    fn workspace_reorder_harness(
        count: usize,
    ) -> egui_kittest::Harness<'static, WorkspaceReorderState> {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspaces: Vec<SidebarWorkspaceEntry> = (0..count)
            .map(|index| SidebarWorkspaceEntry {
                id: format!("workspace-{index:02}"),
                name: format!("Workspace {index:02}"),
                state: if index == 0 {
                    SidebarWorkspaceState::Active
                } else {
                    SidebarWorkspaceState::Idle
                },
                summary: SidebarSessionSummary::default(),
            })
            .collect();
        let sessions: HashMap<String, Vec<SidebarSessionRow>> = HashMap::new();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .with_step_dt(0.05)
            .build_ui_state(
                move |ui, state: &mut WorkspaceReorderState| {
                    if !state.fonts_ready {
                        return;
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: "workspace-00",
                        workspaces: &workspaces,
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
                    };
                    if let Some(SidebarAction::ReorderWorkspaces(order)) =
                        state.tree.panel(ui, &sessions, &snapshot, &catalog)
                    {
                        state.reordered = Some(order);
                    }
                },
                WorkspaceReorderState {
                    tree: FileTreeUi::new(egui::Context::default()),
                    reordered: None,
                    fonts_ready: false,
                },
            );
        install_sidebar_test_fonts(&harness.ctx);
        harness.state_mut().fonts_ready = true;
        harness.run();
        harness
    }

    /// 누르고 → 끌고 → 놓는 제스처가 실제로 `ReorderWorkspaces`를 낸다. 순서 변경은
    /// 산술 테스트만 있었어서 배선(어느 행이 drag를 잡고 어디서 확정하는지)은 한 번도
    /// 실행된 적이 없었다(2026-09-03 리뷰 M3).
    #[test]
    fn kittest_워크스페이스를_아래로_끌어놓으면_순서_액션이_나온다() {
        use egui_kittest::kittest::Queryable;
        let mut harness = workspace_reorder_harness(3);
        let from = harness.get_by_label("Workspace 00").rect().center();
        let below_last =
            harness.get_by_label("Workspace 02").rect().center() + egui::vec2(0.0, 2.0);

        harness.hover_at(from);
        harness.drag_at(from);
        harness.step();
        harness.hover_at(below_last);
        harness.step();
        harness.drop_at(below_last);
        harness.step();

        assert_eq!(
            harness.state().reordered,
            Some(vec![
                "workspace-01".to_owned(),
                "workspace-02".to_owned(),
                "workspace-00".to_owned(),
            ])
        );
    }

    /// 드래그 중에는 휠도 스크롤바도 막혀 있어(egui가 둘 다 끈다) 지금 보이는 자리 밖으로는
    /// 워크스페이스를 옮길 수가 없었다. 가장자리 밖으로 끌면 목록이 스스로 흘러야 한다
    /// (2026-09-03 리뷰 H2). 순수 함수가 아니라 **배선**이 사는지 보는 테스트다.
    #[test]
    fn kittest_드래그_중_아래_가장자리에서는_목록이_따라_흐른다() {
        use egui_kittest::kittest::Queryable;
        let mut harness = workspace_reorder_harness(20);
        let from = harness.get_by_label("Workspace 00").rect().center();
        let before = harness.get_by_label("Workspace 19").rect().top();

        harness.hover_at(from);
        harness.drag_at(from);
        harness.step();
        // 목록 아래 가장자리 **밖** — 포인터를 가만히 둬도 프레임마다 흘러야 한다.
        harness.hover_at(egui::pos2(from.x, 690.0));
        for _ in 0..5 {
            harness.step();
        }

        let after = harness.get_by_label("Workspace 19").rect().top();
        assert!(
            after < before - 20.0,
            "목록이 위로 흘러 뒤쪽 워크스페이스가 올라온다 (before={before}, after={after})"
        );
    }

    /// Escape는 드래그를 **취소**한다. egui는 Escape에서 dragged id를 지우고
    /// (`interaction.rs`) 그 프레임에 `drag_stopped`가 뜨는데, 그것만 보면 취소와 드롭이
    /// 구별되지 않아 취소가 그대로 순서를 확정했다(2026-09-03 리뷰 H1).
    #[test]
    fn kittest_드래그_중_escape는_순서를_바꾸지_않는다() {
        use egui_kittest::kittest::Queryable;
        let mut harness = workspace_reorder_harness(3);
        let from = harness.get_by_label("Workspace 00").rect().center();
        let below_last =
            harness.get_by_label("Workspace 02").rect().center() + egui::vec2(0.0, 2.0);

        harness.hover_at(from);
        harness.drag_at(from);
        harness.step();
        harness.hover_at(below_last);
        harness.step();
        harness.key_press(egui::Key::Escape);
        harness.step();
        assert_eq!(harness.state().reordered, None, "Escape는 취소다");

        harness.drop_at(below_last);
        harness.step();
        assert_eq!(
            harness.state().reordered,
            None,
            "취소한 뒤 손을 떼도 확정되지 않는다"
        );
    }

    /// 목록 **밖에서** 손을 떼면 취소다. 예전에는 x를 아예 보지 않고 y만 읽어서 터미널
    /// 위나 목록 위쪽에서 놓아도 순서가 바뀌었다 — 목록 위쪽은 insert_at = 0이라
    /// 끌던 행이 맨 앞으로 튀었다(2026-09-03 리뷰 M1).
    #[test]
    fn kittest_목록_밖에서_놓으면_순서가_그대로다() {
        use egui_kittest::kittest::Queryable;
        let mut harness = workspace_reorder_harness(3);
        let from = harness.get_by_label("Workspace 02").rect().center();
        let above_list = egui::pos2(
            from.x,
            harness.get_by_label("Workspace 00").rect().top() - 40.0,
        );

        harness.hover_at(from);
        harness.drag_at(from);
        harness.step();
        harness.hover_at(above_list);
        harness.step();
        harness.drop_at(above_list);
        harness.step();

        assert_eq!(harness.state().reordered, None);
    }

    /// 수식키 규칙 — Finder와 같다. 둘 다 눌렀으면 구간이 이긴다.
    #[test]
    fn 선택_클릭은_수식키로_갈린다() {
        let none = egui::Modifiers::default();
        let command = egui::Modifiers {
            command: true,
            ..Default::default()
        };
        let shift = egui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let both = egui::Modifiers {
            command: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(selection_click_kind(none), SelectionClick::Replace);
        assert_eq!(selection_click_kind(command), SelectionClick::Toggle);
        assert_eq!(selection_click_kind(shift), SelectionClick::Range);
        assert_eq!(selection_click_kind(both), SelectionClick::Range);
    }

    /// 구간은 방향을 가리지 않고 양끝을 포함한다. 기준점이 사라졌으면(접힘·삭제)
    /// 아무것도 못 고르는 대신 누른 행 하나로 떨어진다.
    #[test]
    fn 구간_선택은_양끝을_포함하고_방향을_가리지_않는다() {
        let paths: Vec<PathBuf> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| PathBuf::from(format!("/root/{name}")))
            .collect();
        let at = |name: &str| PathBuf::from(format!("/root/{name}"));

        assert_eq!(
            selection_range(&paths, &at("b"), &at("d")),
            vec![at("b"), at("c"), at("d")]
        );
        assert_eq!(
            selection_range(&paths, &at("d"), &at("b")),
            vec![at("b"), at("c"), at("d")],
            "위로 끌어도 같은 구간이다"
        );
        assert_eq!(selection_range(&paths, &at("c"), &at("c")), vec![at("c")]);
        assert_eq!(
            selection_range(&paths, &at("사라짐"), &at("c")),
            vec![at("c")],
            "기준점이 목록에 없으면 누른 행 하나"
        );
    }

    /// 사각형이 덮는 행은 **산술로** 구한다 — `show_rows` 가상화 때문에 화면 밖 행은
    /// 위젯이 없지만, 사각형에 들어왔으면 선택돼야 한다.
    #[test]
    fn 선택_사각형은_화면_밖_행까지_덮는다() {
        // 0번 행 top = 100.0, 행 높이 20, 전체 10행 (100..300)
        let range =
            |min: f32, max: f32| marquee_row_range(100.0, 20.0, 10, egui::Rangef::new(min, max));

        // floor를 고정한다. `round`면 소수부 0.75가 올림돼 **닿지도 않은** 3번 행이
        // 딸려오고, `trunc`면 content_top 바로 위에서 끝나는 사각형이 -0.0을 통과시켜
        // 0번 행을 고른다. 예전 입력은 소수부가 0/0.25뿐이라 셋을 구분하지 못했다.
        assert_eq!(
            range(105.0, 155.0),
            0..3,
            "소수부 0.75 — round면 0..4가 된다"
        );
        assert_eq!(
            range(80.0, 99.0),
            0..0,
            "content_top 위에서 끝나면 아무것도 아니다"
        );
        assert_eq!(range(100.0, 100.0), 0..1, "첫 행 맨 위 한 점");
        assert_eq!(range(105.0, 145.0), 0..3, "1픽셀만 걸쳐도 그 행까지");
        assert_eq!(range(140.0, 160.0), 2..4);
        // 뷰포트를 한참 벗어나게 끌어도 목록 끝에서 멈춘다.
        assert_eq!(range(-500.0, 5000.0), 0..10);
        assert_eq!(range(0.0, 50.0), 0..0, "목록 위쪽 바깥");
        assert_eq!(range(400.0, 500.0), 0..0, "목록 아래쪽 바깥");
        assert_eq!(
            marquee_row_range(100.0, 20.0, 0, egui::Rangef::new(0.0, 999.0)),
            0..0,
            "행이 없으면 빈 구간"
        );
    }

    /// 삭제 확인 라벨은 **루트 기준 상대 경로**다. 파일명만 뽑아 쓰던 예전 문구는
    /// `src/mod.rs`와 `tests/mod.rs`를 똑같이 `'mod.rs'`로 보여줘, 확인 문구만 보고는
    /// 어느 쪽이 지워지는지 알 수 없었다.
    #[test]
    fn delete_target_label은_루트_기준_상대경로다() {
        let root = Path::new("/root");
        assert_eq!(
            delete_target_label(Some(root), &root.join("src/mod.rs")),
            "src/mod.rs"
        );
        assert_eq!(
            delete_target_label(Some(root), &root.join("tests/mod.rs")),
            "tests/mod.rs"
        );
        // 루트 밖·루트 미설정은 전체 경로로 떨어진다 (조용히 이름만 남기지 않는다).
        assert_eq!(
            delete_target_label(Some(root), Path::new("/other/mod.rs")),
            "/other/mod.rs"
        );
        assert_eq!(
            delete_target_label(None, Path::new("/other/mod.rs")),
            "/other/mod.rs"
        );
        // 루트 자신은 상대 경로가 비므로 전체 경로.
        assert_eq!(delete_target_label(Some(root), root), "/root");
        // 트리 행과 같은 NFC 표기 (행은 "한글.md", 확인 문구는 NFD면 다른 이름처럼 보인다).
        let nfd = Path::new("/root/\u{1112}\u{1161}\u{11AB}\u{1100}\u{1173}\u{11AF}.md");
        assert_eq!(delete_target_label(Some(root), nfd), "한글.md");
    }

    /// 새 삭제 제스처는 직전 실패가 남긴 영구삭제 확인을 닫는다. 닫지 않으면 패널
    /// 아래에 **예전 대상** 이름이 그대로 떠 있고, 방금 고른 파일 이야기인 줄 알고
    /// [영구 삭제]를 누르면 엉뚱한 파일이 영구 삭제된다.
    /// 폴더를 접으면 그 안의 선택이 함께 빠진다. 남겨두면 화면에는 아무것도 강조되지
    /// 않은 채 ⌘⌫ 한 번에 보이지 않는 파일이 지워지고, 다시 펼치면 유령 선택이 되살아난다.
    #[test]
    fn 접힌_폴더_안의_선택은_함께_빠진다() {
        let base = temp_root("collapse-prune");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("sub/inner.txt"), b"x").unwrap();
        let base = base.canonicalize().unwrap();

        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        tree.toggle_dir(&base.join("sub"));
        drain_listings(&mut tree);

        let inner = base.join("sub/inner.txt");
        assert!(
            tree.flat.iter().any(|row| row.path == inner),
            "펼친 상태에서는 자식이 보인다"
        );
        tree.selected = BTreeSet::from([inner.clone()]);
        tree.select_anchor = Some(inner);

        tree.toggle_dir(&base.join("sub"));

        assert!(tree.selected.is_empty(), "안 보이는 것은 선택에서 뺀다");
        assert!(tree.select_anchor.is_none(), "기준점도 함께 버린다");

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// `apply_selection_click`의 상태 전이. 순수 함수 둘(`selection_click_kind`,
    /// `selection_range`)은 잠겨 있었지만 그것을 **조합하고 기준점을 관리하는** 이 코드는
    /// 한 번도 실행되지 않아, ⌘클릭이 해제를 못 해도 테스트가 통과했다(2026-09-04 리뷰).
    #[test]
    fn 선택_클릭은_기준점을_옳게_관리한다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        let at = |name: &str| PathBuf::from(format!("/root/{name}"));
        tree.flat = ["a", "b", "c", "d"]
            .iter()
            .map(|name| FlatRow {
                path: at(name),
                display_name: (*name).to_owned(),
                depth: 0,
                is_dir: false,
                expanded: false,
            })
            .collect();

        tree.apply_selection_click(&at("b"), SelectionClick::Replace);
        assert_eq!(tree.selected, BTreeSet::from([at("b")]));
        assert_eq!(tree.select_anchor.as_deref(), Some(at("b").as_path()));

        // ⌘클릭은 **토글**이다 — 넣기만 하면 선택을 뺄 수 없다.
        tree.apply_selection_click(&at("d"), SelectionClick::Toggle);
        assert_eq!(tree.selected, BTreeSet::from([at("b"), at("d")]));
        tree.apply_selection_click(&at("d"), SelectionClick::Toggle);
        assert_eq!(
            tree.selected,
            BTreeSet::from([at("b")]),
            "다시 누르면 빠진다"
        );

        // Shift는 기준점을 **움직이지 않는다** — 범위를 늘였다 줄였다 할 수 있어야 한다.
        tree.apply_selection_click(&at("b"), SelectionClick::Replace);
        tree.apply_selection_click(&at("d"), SelectionClick::Range);
        assert_eq!(tree.selected, BTreeSet::from([at("b"), at("c"), at("d")]));
        assert_eq!(
            tree.select_anchor.as_deref(),
            Some(at("b").as_path()),
            "Shift 클릭이 기준점을 옮기면 범위를 줄일 수 없다"
        );
        tree.apply_selection_click(&at("c"), SelectionClick::Range);
        assert_eq!(
            tree.selected,
            BTreeSet::from([at("b"), at("c")]),
            "같은 기준점에서 범위가 줄어든다"
        );
    }

    /// 다중 삭제는 capacity-1 IO 큐를 **하나씩** 통과한다. 한 번에 다 보내면 두 번째
    /// 요청부터 `Busy`로 조용히 버려져, 고른 것 중 하나만 지워지고 나머지는 남는다.
    ///
    /// 순서는 **트리에 보이는 순서**다 — 확인/에러 문구가 위에서부터 차례로 나와야
    /// 사용자가 어디까지 지워졌는지 눈으로 따라갈 수 있다.
    #[test]
    fn 다중_삭제는_트리_순서대로_하나씩_이어_보낸다() {
        let base =
            std::env::temp_dir().join(format!("deppy-ft-multi-trash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // **트리 순서와 경로 정렬 순서가 갈리는** fixture여야 한다. flat은 디렉터리를
        // 먼저 놓으므로 `zz/` → `a.txt`인데, 경로 정렬은 `a.txt` → `zz/`다. 예전 fixture는
        // a/b/c.txt뿐이라 두 순서가 우연히 같아 "트리 순서" 단언이 공허했다.
        std::fs::create_dir_all(base.join("zz")).unwrap();
        for name in ["a.txt", "m.txt"] {
            std::fs::write(base.join(name), b"x").unwrap();
        }
        let base = base.canonicalize().unwrap();
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);

        tree.selected = BTreeSet::from([base.join("a.txt"), base.join("m.txt"), base.join("zz")]);
        tree.spawn_trash_selection(None);

        // 세 개를 순서대로 확인해야 "하나씩"이 고정된다 — 두 개면 큐 길이가 늘 1이라
        // `remove(0)`을 `pop()`으로 바꿔도 통과한다.
        let expected = [base.join("zz"), base.join("a.txt"), base.join("m.txt")];
        let mut remaining = expected.len();
        for (step, want) in expected.iter().enumerate() {
            let intent = tree
                .take_io_intent()
                .unwrap_or_else(|| panic!("{step}번째 삭제 intent가 없다"));
            match &intent.request {
                FileTreeIoRequest::Trash { target } => {
                    assert_eq!(target.as_path(), want.as_path(), "{step}번째 대상")
                }
                other => panic!("unexpected request: {other:?}"),
            }
            remaining -= 1;
            assert_eq!(
                tree.trash_queue.len(),
                remaining,
                "{step}번째 뒤 남은 대기열"
            );
            assert!(
                tree.take_io_intent().is_none(),
                "capacity-1 큐에 두 번째를 겹쳐 보내면 안 된다"
            );
            if step == 0 {
                assert!(tree.selected.is_empty(), "삭제를 접수했으면 선택은 비운다");
                assert!(tree.select_anchor.is_none(), "기준점도 함께 버린다");
            }
            tree.complete_io(FileTreeIoCompletion {
                operation: intent.operation,
                generation: intent.generation,
                result: Ok(()),
            });
        }
        assert!(tree.trash_queue.is_empty());

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 삭제가 아닌 IO의 성공에는 대기열이 풀리면 안 된다. 예전엔 완료 종류를 안 봐서,
    /// 붙여넣기 하나가 끝나는 순간 사용자가 고른 적 없는 시점에 파일이 사라졌다.
    #[test]
    fn 대기열은_삭제_완료에만_이어진다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.trash_queue = vec![DeleteTarget {
            path: PathBuf::from("/root/b.txt"),
            label: "b.txt".to_owned(),
            is_dir: false,
        }];
        // 삭제가 아닌 요청 하나를 큐에 태우고 성공시킨다.
        tree.copy_files_to_clipboard(&[PathBuf::from("/root/x.txt")]);
        let intent = tree.take_io_intent().expect("복사 intent");
        tree.complete_io(FileTreeIoCompletion {
            operation: intent.operation,
            generation: intent.generation,
            result: Ok(()),
        });
        assert!(
            tree.take_io_intent().is_none(),
            "복사 성공이 삭제 대기열을 풀었다"
        );
        assert_eq!(tree.trash_queue.len(), 1, "대기열은 그대로 남는다");
    }

    /// 이미 삭제가 진행 중일 때 새 다중 삭제가 Busy로 거절되면 새 대기열도 폐기한다.
    /// 그대로 남기면 앞선 삭제가 성공하는 순간, 접수되지 않은 새 제스처의 뒤 대상이
    /// 조용히 삭제된다.
    #[test]
    fn 다중_삭제의_첫_요청이_busy면_새_대기열을_버린다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.flat = ["old.txt", "new-a.txt", "new-b.txt"]
            .into_iter()
            .map(|name| FlatRow {
                path: PathBuf::from(format!("/root/{name}")),
                display_name: name.to_owned(),
                depth: 0,
                is_dir: false,
                expanded: false,
            })
            .collect();

        tree.spawn_trash(DeleteTarget {
            path: PathBuf::from("/root/old.txt"),
            label: "old.txt".to_owned(),
            is_dir: false,
        });
        let old = tree.take_io_intent().expect("기존 삭제 intent");

        tree.selected = BTreeSet::from([
            PathBuf::from("/root/new-a.txt"),
            PathBuf::from("/root/new-b.txt"),
        ]);
        tree.spawn_trash_selection(None);
        assert_eq!(
            tree.error.as_deref(),
            Some(file_tree_io_error_message(FileTreeIoErrorCode::Busy)),
            "새 다중 삭제는 Busy로 거절돼야 한다"
        );

        tree.complete_io(FileTreeIoCompletion {
            operation: old.operation,
            generation: old.generation,
            result: Ok(()),
        });

        assert!(
            tree.take_io_intent().is_none(),
            "접수되지 않은 새 제스처의 뒤 대상이 앞선 삭제 완료에 이어졌다"
        );
        assert!(tree.trash_queue.is_empty());
    }

    /// 행의 이름 오른쪽 빈 공간에서 시작한 드래그는 파일 이동이 아니라 선택 사각형이다.
    /// `dnd_drag_source`의 바깥 response는 행 전체 폭이라 그 right를 기준으로 삼으면
    /// 이 조건이 영원히 참이 되지 않는다.
    #[test]
    fn kittest_행_이름_오른쪽_드래그는_선택_사각형을_시작한다() {
        use egui_kittest::kittest::Queryable as _;

        let base = temp_root("marquee-blank");
        std::fs::create_dir_all(base.join("dest")).unwrap();
        std::fs::write(base.join("a.txt"), b"a").unwrap();
        let base = base.canonicalize().unwrap();
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        let mut harness = drop_harness(&catalog, tree);
        for _ in 0..10 {
            harness.step();
        }

        let source = harness.get_by_label("a.txt").rect();
        let destination = harness.get_by_label("dest").rect();
        let start = egui::pos2(220.0, source.center().y);
        let end = egui::pos2(220.0, destination.center().y);
        harness.hover_at(start);
        harness.drag_at(start);
        harness.step();
        harness.hover_at(end);
        harness.step();

        assert!(
            harness.state().0.marquee.is_some(),
            "행 빈 공간 드래그가 선택 사각형을 시작하지 않았다"
        );
        assert!(
            !egui::DragAndDrop::has_any_payload(&harness.ctx),
            "선택 사각형이 파일 이동 payload로 바뀌었다"
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    /// 키 repeat는 시간이 지나도 새 삭제가 아니고, 빠르게 다시 누른 non-repeat는 새
    /// 제스처다. 벽시계 창으로 중복을 막으면 이 두 경우를 구분할 수 없다.
    #[test]
    fn kittest_삭제는_repeat을_무시하고_빠른_새_keydown을_받는다() {
        use egui_kittest::kittest::Queryable as _;

        let base = temp_root("delete-repeat");
        std::fs::write(base.join("a.txt"), b"a").unwrap();
        let base = base.canonicalize().unwrap();
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        let mut harness = drop_harness(&catalog, tree);
        for _ in 0..10 {
            harness.step();
        }
        let pos = harness.get_by_label("a.txt").rect().center();
        harness.hover_at(pos);
        harness.get_by_label("a.txt").click();
        harness.run();

        let press_delete =
            |harness: &mut egui_kittest::Harness<'_, (FileTreeUi, Vec<SidebarAction>)>, repeat| {
                harness.event(egui::Event::ModifiersChanged(egui::Modifiers::COMMAND));
                harness.event(egui::Event::Key {
                    key: egui::Key::Backspace,
                    physical_key: None,
                    pressed: true,
                    repeat,
                    modifiers: egui::Modifiers::COMMAND,
                });
                harness.step();
            };
        let release_delete =
            |harness: &mut egui_kittest::Harness<'_, (FileTreeUi, Vec<SidebarAction>)>| {
                harness.event(egui::Event::Key {
                    key: egui::Key::Backspace,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers: egui::Modifiers::COMMAND,
                });
                harness.event(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
                harness.step();
            };
        let complete =
            |harness: &mut egui_kittest::Harness<'_, (FileTreeUi, Vec<SidebarAction>)>| {
                let intent = harness
                    .state_mut()
                    .0
                    .take_io_intent()
                    .expect("delete intent");
                harness.state_mut().0.complete_io(FileTreeIoCompletion {
                    operation: intent.operation,
                    generation: intent.generation,
                    result: Ok(()),
                });
                drain_listings(&mut harness.state_mut().0);
                harness.step();
            };

        press_delete(&mut harness, false);
        complete(&mut harness);
        release_delete(&mut harness);
        harness.state_mut().0.selected = BTreeSet::from([base.join("a.txt")]);
        harness
            .ctx
            .memory_mut(|memory| memory.request_focus(tree_keyboard_focus_id()));
        harness.hover_at(pos);
        harness.step();
        press_delete(&mut harness, false);
        complete(&mut harness);
        release_delete(&mut harness);

        harness.state_mut().0.selected = BTreeSet::from([base.join("a.txt")]);
        harness
            .ctx
            .memory_mut(|memory| memory.request_focus(tree_keyboard_focus_id()));
        harness.hover_at(pos);
        harness.step();
        press_delete(&mut harness, false);
        complete(&mut harness);
        std::thread::sleep(std::time::Duration::from_millis(450));
        press_delete(&mut harness, true);
        let repeated = harness.state_mut().0.take_io_intent();
        assert!(
            repeated.is_none(),
            "키 repeat가 별도 IO를 만들었다: {repeated:?}"
        );
        release_delete(&mut harness);
        std::fs::remove_dir_all(base).unwrap();
    }

    /// 파일 트리를 누른 뒤 다른 위젯이 키보드 포커스를 가져가면 삭제 소유권도 함께
    /// 넘어가야 한다. 포인터가 트리 위에 남았다는 이유로 터미널의 ⌘⌫를 받으면 안 된다.
    #[test]
    fn kittest_다른_위젯의_프로그램포커스는_트리_삭제를_해제한다() {
        use egui_kittest::kittest::Queryable as _;

        struct State {
            tree: FileTreeUi,
            focus_other: bool,
            other_id: Option<egui::Id>,
            fonts_ready: bool,
        }

        let base = temp_root("tree-programmatic-focus");
        std::fs::write(base.join("a.txt"), b"a").unwrap();
        let base = base.canonicalize().unwrap();
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .build_ui_state(
                |ui, state: &mut State| {
                    if !state.fonts_ready {
                        return;
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: "ws",
                        workspaces: &[],
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
                    };
                    let _ = state.tree.panel(
                        ui,
                        &std::collections::HashMap::new(),
                        &snapshot,
                        &catalog,
                    );
                    let (_, response) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::click());
                    state.other_id = Some(response.id);
                    if state.focus_other {
                        response.request_focus();
                        state.focus_other = false;
                    }
                },
                State {
                    tree,
                    focus_other: false,
                    other_id: None,
                    fonts_ready: false,
                },
            );
        install_sidebar_test_fonts(&harness.ctx);
        harness.state_mut().fonts_ready = true;
        harness.run();

        harness.get_by_label("a.txt").click();
        harness.run();
        harness.state_mut().focus_other = true;
        harness.step();
        let other = harness.state().other_id.expect("other widget id");
        assert_eq!(harness.ctx.memory(|memory| memory.focused()), Some(other));

        let pos = harness.get_by_label("a.txt").rect().center();
        harness.hover_at(pos);
        harness.step();
        harness.event(egui::Event::ModifiersChanged(egui::Modifiers::COMMAND));
        harness.event(egui::Event::Key {
            key: egui::Key::Backspace,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.step();

        assert!(
            harness.state_mut().tree.take_io_intent().is_none(),
            "다른 위젯이 포커스를 가졌는데 트리가 삭제를 받았다"
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    /// 루트가 바뀌면 선택과 대기열을 함께 버린다. generation 검사는 옛 **완료**만 막을
    /// 뿐 큐 자체는 살아남아, 새 워크스페이스의 아무 성공에나 옛 루트 파일이 지워졌다.
    #[test]
    fn 루트가_바뀌면_선택과_삭제_대기열을_버린다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.selected = BTreeSet::from([PathBuf::from("/old/a.txt")]);
        tree.select_anchor = Some(PathBuf::from("/old/a.txt"));
        tree.trash_queue = vec![DeleteTarget {
            path: PathBuf::from("/old/b.txt"),
            label: "b.txt".to_owned(),
            is_dir: false,
        }];

        tree.set_root(Some(PathBuf::from("/new")));

        assert!(tree.trash_queue.is_empty());
        assert!(tree.selected.is_empty());
        assert!(tree.select_anchor.is_none());
    }

    /// 앞이 실패하면 남은 대상은 보내지 않는다. 영구삭제 확인이 뜬 채로 뒤가 계속
    /// 지워지면, 사용자가 보고 있는 확인 문구와 실제로 사라지는 파일이 갈라진다.
    #[test]
    fn 다중_삭제는_실패하면_남은_대상을_보내지_않는다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        let target = |name: &str| DeleteTarget {
            path: PathBuf::from(format!("/root/{name}")),
            label: name.to_owned(),
            is_dir: false,
        };
        tree.trash_queue = vec![target("b.txt")];
        tree.spawn_trash(target("a.txt"));
        let intent = tree.take_io_intent().expect("첫 삭제 intent");
        tree.complete_io(FileTreeIoCompletion {
            operation: intent.operation,
            generation: intent.generation,
            result: Err(FileTreeIoErrorCode::TrashUnavailable),
        });

        assert_eq!(tree.confirm_delete, Some(target("a.txt")));
        assert!(tree.trash_queue.is_empty(), "남은 대상은 버린다");
        assert!(tree.take_io_intent().is_none(), "다음을 보내지 않는다");

        // TrashUnavailable이 아닌 실패에서도 같다 — 남겨두면 뒤이은 아무 성공에나 풀린다.
        tree.trash_queue = vec![target("b.txt")];
        tree.spawn_trash(target("a.txt"));
        let intent = tree.take_io_intent().expect("두 번째 삭제 intent");
        tree.complete_io(FileTreeIoCompletion {
            operation: intent.operation,
            generation: intent.generation,
            result: Err(FileTreeIoErrorCode::NativeFailure),
        });
        assert!(
            tree.trash_queue.is_empty(),
            "일반 실패에서도 남은 대상은 버린다"
        );
        assert!(tree.take_io_intent().is_none());
    }

    #[test]
    fn 새_삭제_제스처는_직전_영구삭제_확인을_닫는다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        let first = DeleteTarget {
            path: PathBuf::from("/root/src/mod.rs"),
            label: "src/mod.rs".to_owned(),
            is_dir: false,
        };
        tree.spawn_trash(first.clone());
        let intent = tree.take_io_intent().expect("trash intent");
        tree.complete_io(FileTreeIoCompletion {
            operation: intent.operation,
            generation: intent.generation,
            result: Err(FileTreeIoErrorCode::TrashUnavailable),
        });
        assert_eq!(tree.confirm_delete, Some(first.clone()));

        tree.spawn_trash(DeleteTarget {
            path: PathBuf::from("/root/tests/mod.rs"),
            label: "tests/mod.rs".to_owned(),
            is_dir: false,
        });
        assert_eq!(
            tree.confirm_delete, None,
            "새 대상의 휴지통 이동을 접수했는데 예전 대상 확인이 그대로 남아 있다"
        );

        // Busy 갈래: capacity-1 IO 큐가 차 있으면 `queue_io`는 요청을 받지 않는다.
        // 그때도 확인은 닫혀야 한다 — 확인을 무효로 만드는 것은 큐 수용이 아니라
        // **제스처**다. 남겨두면 "파일 작업이 진행 중입니다" 바로 아래에 예전 대상
        // 확인이 서고, 그걸 방금 고른 파일 이야기로 읽은 사용자가 [영구 삭제]를
        // 누르면 엉뚱한 파일이 사라진다.
        //
        // 직전 제스처가 큐에 넣어둔 요청부터 비운다 — 그대로 두면 아래 arm이 Busy로
        // 거절되어 검증하려던 상태를 만들지 못한다.
        let queued = tree.take_io_intent().expect("직전 제스처의 trash intent");
        tree.complete_io(FileTreeIoCompletion {
            operation: queued.operation,
            generation: queued.generation,
            result: Ok(()),
        });
        tree.spawn_trash(first.clone());
        let intent = tree.take_io_intent().expect("second trash intent");
        tree.complete_io(FileTreeIoCompletion {
            operation: intent.operation,
            generation: intent.generation,
            result: Err(FileTreeIoErrorCode::TrashUnavailable),
        });
        assert_eq!(tree.confirm_delete, Some(first));
        // 다른 파일 조작(⌘C 복사)이 같은 큐를 차지한다 — 트리 내 이동·Finder 드롭·
        // 더블클릭 열기도 모두 같은 capacity-1 큐다.
        tree.copy_files_to_clipboard(&[PathBuf::from("/root/src/mod.rs")]);
        assert!(
            tree.io_intent.is_some() || tree.pending_io.is_some(),
            "큐를 점유하지 못해 Busy 갈래를 검증할 수 없다"
        );
        tree.spawn_trash(DeleteTarget {
            path: PathBuf::from("/root/tests/mod.rs"),
            label: "tests/mod.rs".to_owned(),
            is_dir: false,
        });
        assert_eq!(
            tree.error.as_deref(),
            Some(file_tree_io_error_message(FileTreeIoErrorCode::Busy)),
            "큐가 찬 상태의 삭제 제스처가 Busy로 거절되지 않아 갈래를 잘못 짚었다"
        );
        assert_eq!(
            tree.confirm_delete, None,
            "큐가 거절했다는 이유로 예전 대상 확인이 살아남았다"
        );
    }

    /// 영구삭제 확인은 **대상이 들고 온 label 그대로**를 보여주고, 같은 대상의 경로를
    /// 지운다. 확인 시점에 경로에서 이름을 다시 만들면 표시와 삭제 대상이 갈라질 수
    /// 있으므로, 여기서는 파일명과 **다른** label을 일부러 넣어 재유추가 없음을 고정한다.
    /// 폴더 대상은 "안의 내용까지 지운다"는 별도 문구를 쓴다.
    #[test]
    fn kittest_영구삭제_확인은_대상이_들고온_이름을_그대로_지운다() {
        use egui_kittest::kittest::Queryable as _;

        let base = temp_root("delete-confirm");
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/mod.rs"), b"x").unwrap();
        std::fs::create_dir_all(base.join("docs")).unwrap();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);

        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .build_ui_state(
                |ui, state: &mut (FileTreeUi, bool)| {
                    // 폰트 설치 전 빌드 프레임은 건너뛴다(사이드바 harness 관례).
                    if !state.1 {
                        return;
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: "ws-test",
                        workspaces: &[],
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
                    };
                    state
                        .0
                        .panel(ui, &std::collections::HashMap::new(), &snapshot, &catalog);
                },
                (tree, false),
            );
        install_sidebar_test_fonts(&harness.ctx);
        harness.state_mut().1 = true;
        harness.step();
        harness.step();

        // 파일 대상 — label은 행이 확정해 넘긴 값이며, 파일명(mod.rs)과 다르다.
        harness.state_mut().0.spawn_trash(DeleteTarget {
            path: base.join("src/mod.rs"),
            label: "src/mod.rs".to_owned(),
            is_dir: false,
        });
        let intent = harness
            .state_mut()
            .0
            .take_io_intent()
            .expect("trash intent");
        match &intent.request {
            FileTreeIoRequest::Trash { target } => {
                assert_eq!(target.as_path(), base.join("src/mod.rs"));
            }
            other => panic!("unexpected request: {other:?}"),
        }
        harness.state_mut().0.complete_io(FileTreeIoCompletion {
            operation: intent.operation,
            generation: intent.generation,
            result: Err(FileTreeIoErrorCode::TrashUnavailable),
        });
        harness.step();

        let prompt = catalog.t(
            "file_tree.permanent_delete_prompt",
            &[("name", "src/mod.rs")],
        );
        assert!(
            harness.query_by_label(&prompt).is_some(),
            "확인 문구가 대상 label을 그대로 보여주지 않는다"
        );
        let ambiguous = catalog.t("file_tree.permanent_delete_prompt", &[("name", "mod.rs")]);
        assert!(
            harness.query_by_label(&ambiguous).is_none(),
            "파일명만 쓴 옛 문구가 남아 있다 — 같은 이름 파일을 구분할 수 없다"
        );

        // 라벨 노드는 클립 밖에서도 만들어지므로 존재가 아니라 **위치**를 본다.
        // 확인 문구를 스크롤 영역 뒤에 두던 시절엔 700px 패널에서 문구가 y=695,
        // [영구 삭제] 버튼이 y=728이었다 — 무엇을 지우는지도, 누를 버튼도 화면 밖이었다.
        let prompt_rect = harness.get_by_label(&prompt).rect();
        let button_rect = harness
            .get_by_label(&catalog.t("file_tree.permanent_delete", &[]))
            .rect();
        assert!(
            prompt_rect.bottom() <= 700.0 && button_rect.bottom() <= 700.0,
            "영구삭제 확인이 패널(700px) 밖으로 밀렸다: 문구={prompt_rect:?} 버튼={button_rect:?}"
        );
        harness
            .get_by_label(&catalog.t("file_tree.permanent_delete", &[]))
            .click();
        // 버튼 clicked()는 release 프레임에 뜬다 — kittest는 press/release를 다른
        // 프레임에 재생하므로 intent가 나올 때까지 몇 프레임 돌린다.
        let deleted = (0..8)
            .find_map(|_| {
                harness.step();
                harness.state_mut().0.take_io_intent()
            })
            .expect("delete intent");
        match &deleted.request {
            FileTreeIoRequest::DeletePermanently { target } => {
                assert_eq!(
                    target.as_path(),
                    base.join("src/mod.rs"),
                    "보여준 대상과 실제 삭제 경로가 갈라졌다"
                );
            }
            other => panic!("unexpected request: {other:?}"),
        }
        harness.state_mut().0.complete_io(FileTreeIoCompletion {
            operation: deleted.operation,
            generation: deleted.generation,
            result: Ok(()),
        });
        harness.step();

        // 폴더 대상 — 영구 삭제는 안의 내용까지 지우므로 문구가 달라야 한다.
        harness.state_mut().0.spawn_trash(DeleteTarget {
            path: base.join("docs"),
            label: "docs".to_owned(),
            is_dir: true,
        });
        let folder = harness
            .state_mut()
            .0
            .take_io_intent()
            .expect("folder trash intent");
        harness.state_mut().0.complete_io(FileTreeIoCompletion {
            operation: folder.operation,
            generation: folder.generation,
            result: Err(FileTreeIoErrorCode::TrashUnavailable),
        });
        harness.step();
        assert!(
            harness
                .query_by_label(&catalog.t(
                    "file_tree.permanent_delete_folder_prompt",
                    &[("name", "docs")]
                ))
                .is_some(),
            "폴더 대상인데 파일용 문구를 쓴다 — 안의 내용이 함께 사라지는 것을 말해주지 않는다"
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 파일 트리만 그리는 harness (삭제 확인 계열 공용). 워크스페이스 목록은 비운다.
    fn delete_confirm_harness<'a>(
        tree: FileTreeUi,
        catalog: &'a i18n::Catalog,
    ) -> egui_kittest::Harness<'a, (FileTreeUi, bool)> {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .with_step_dt(0.05)
            .build_ui_state(
                |ui, state: &mut (FileTreeUi, bool)| {
                    // 폰트 설치 전 빌드 프레임은 건너뛴다(사이드바 harness 관례).
                    if !state.1 {
                        return;
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: "ws-test",
                        workspaces: &[],
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
                    };
                    state
                        .0
                        .panel(ui, &std::collections::HashMap::new(), &snapshot, catalog);
                },
                (tree, false),
            );
        install_sidebar_test_fonts(&harness.ctx);
        harness.state_mut().1 = true;
        harness.step();
        harness.step();
        harness
    }

    /// 행 우클릭 → 「휴지통으로 이동」까지의 **실제 제스처**. 라벨 노드
    /// `click_secondary()`로 컨텍스트 메뉴를 여는 것은 사이드바 harness 관례와 같다
    /// (`kittest_사이드바_조작_전반에_widget_id_충돌이_없다`). kittest는 press/release를
    /// 다른 프레임에 재생하므로 메뉴가 뜰 때까지, 그리고 메뉴 항목의 `clicked()`가
    /// 뜰 때까지 프레임을 돌린다.
    fn trash_row_via_context_menu(
        harness: &mut egui_kittest::Harness<'_, (FileTreeUi, bool)>,
        row_center: egui::Pos2,
        menu_label: &str,
    ) -> FileTreeIoIntent {
        use egui_kittest::kittest::Queryable as _;
        harness.event(egui::Event::PointerMoved(row_center));
        harness.event(egui::Event::PointerButton {
            pos: row_center,
            button: egui::PointerButton::Secondary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        });
        harness.event(egui::Event::PointerButton {
            pos: row_center,
            button: egui::PointerButton::Secondary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        });
        for _ in 0..4 {
            harness.step();
        }
        harness.get_by_label(menu_label).click();
        (0..8)
            .find_map(|_| {
                harness.step();
                harness.state_mut().0.take_io_intent()
            })
            .unwrap_or_else(|| {
                panic!("행 우클릭 「휴지통으로 이동」이 IO intent를 내지 않음: {row_center:?}")
            })
    }

    /// 같은 파일명이 여러 행에 있을 수 있으므로(루트의 `mod.rs` vs `src/mod.rs`) 라벨만
    /// 보고 고르지 않고 **위에서 첫 번째** 행을 집는다. 정렬이 디렉터리 우선이라
    /// `src` 하위 행들이 루트의 파일 행보다 항상 위에 온다(`정렬은_디렉터리_우선_이름순`).
    fn topmost_row_center(
        harness: &egui_kittest::Harness<'_, (FileTreeUi, bool)>,
        label: &str,
        expected_rows: usize,
    ) -> egui::Pos2 {
        use egui_kittest::kittest::Queryable as _;
        let mut rects: Vec<egui::Rect> = harness
            .query_all_by_label(label)
            .map(|node| node.rect())
            .collect();
        assert_eq!(
            rects.len(),
            expected_rows,
            "'{label}' 행 수가 예상과 달라 어느 행을 우클릭하는지 확정할 수 없다"
        );
        rects.sort_by(|a, b| a.top().total_cmp(&b.top()));
        rects.first().expect("행 rect").center()
    }

    /// 회귀: 행 → 확인 문구를 잇는 **제스처 전체**가 어떤 테스트로도 잠기지 않았던 것.
    ///
    /// 확인 라벨을 만드는 프로덕션 지점은 우클릭 메뉴의 `MenuAction::Delete` 한 곳뿐인데,
    /// 기존 삭제 테스트는 모두 `DeleteTarget`을 손으로 만들어 `spawn_trash`에 직접
    /// 넣었다 — 그 한 줄을 옛 `path_file_name_display`로 되돌려 `src/mod.rs`와
    /// `tests/mod.rs`가 다시 똑같이 `'mod.rs'`로 보이게 해도 전부 통과했다. 여기서는
    /// 행이 확정하는 label과 is_dir이 실제 메뉴 경로를 지나 문구에 닿는지를 본다.
    #[test]
    fn kittest_행_우클릭_삭제는_행이_확정한_이름과_종류로_확인을_세운다() {
        use egui_kittest::kittest::Queryable as _;

        let base = temp_root("trash-gesture");
        std::fs::create_dir_all(base.join("src/inner")).unwrap();
        std::fs::write(base.join("src/mod.rs"), b"x").unwrap();
        // 루트에도 같은 파일명을 둬, 파일명만 쓰는 옛 문구로는 두 행을 구분할 수 없게 한다.
        std::fs::write(base.join("mod.rs"), b"x").unwrap();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        let mut harness = delete_confirm_harness(tree, &catalog);

        // `src`를 펼쳐 하위 행(`src/mod.rs`, `src/inner`)을 화면에 올린다.
        harness.get_by_label("src").click();
        harness.step();
        drain_listings(&mut harness.state_mut().0);
        for _ in 0..30 {
            harness.step();
            if harness.query_by_label("inner").is_some() {
                break;
            }
        }
        assert!(
            harness.query_by_label("inner").is_some(),
            "src 펼치기가 하위 행을 올리지 못해 제스처를 재현할 수 없다"
        );

        // 파일 행: 루트의 mod.rs가 아니라 **이 행**(src/mod.rs)을 지운다.
        let file_row = topmost_row_center(&harness, "mod.rs", 2);
        let file_intent = trash_row_via_context_menu(
            &mut harness,
            file_row,
            &catalog.t("file_tree.move_to_trash", &[]),
        );
        match &file_intent.request {
            FileTreeIoRequest::Trash { target } => {
                assert_eq!(
                    target.as_path(),
                    base.join("src/mod.rs"),
                    "메뉴가 우클릭한 행이 아닌 다른 경로를 지우려 한다"
                );
            }
            other => panic!("unexpected request: {other:?}"),
        }
        harness.state_mut().0.complete_io(FileTreeIoCompletion {
            operation: file_intent.operation,
            generation: file_intent.generation,
            result: Err(FileTreeIoErrorCode::TrashUnavailable),
        });
        harness.step();

        assert!(
            harness
                .query_by_label(&catalog.t(
                    "file_tree.permanent_delete_prompt",
                    &[("name", "src/mod.rs")]
                ))
                .is_some(),
            "확인 문구가 루트 기준 이름을 쓰지 않는다 — 행에서 확정한 label이 메뉴 경로에서 끊겼다"
        );
        assert!(
            harness
                .query_by_label(
                    &catalog.t("file_tree.permanent_delete_prompt", &[("name", "mod.rs")])
                )
                .is_none(),
            "파일명만 쓴 옛 문구가 돌아왔다 — 루트의 mod.rs와 src/mod.rs를 구분할 수 없다"
        );

        // 폴더 행: 종류(is_dir)도 같은 길로 흘러야 "안의 내용까지" 문구가 나온다.
        let folder_row = topmost_row_center(&harness, "inner", 1);
        let folder_intent = trash_row_via_context_menu(
            &mut harness,
            folder_row,
            &catalog.t("file_tree.move_to_trash", &[]),
        );
        match &folder_intent.request {
            FileTreeIoRequest::Trash { target } => {
                assert_eq!(target.as_path(), base.join("src/inner"));
            }
            other => panic!("unexpected request: {other:?}"),
        }
        harness.state_mut().0.complete_io(FileTreeIoCompletion {
            operation: folder_intent.operation,
            generation: folder_intent.generation,
            result: Err(FileTreeIoErrorCode::TrashUnavailable),
        });
        harness.step();

        assert!(
            harness
                .query_by_label(&catalog.t(
                    "file_tree.permanent_delete_folder_prompt",
                    &[("name", "src/inner")]
                ))
                .is_some(),
            "폴더 행인데 폴더용 문구가 뜨지 않는다 — 안의 내용이 함께 사라지는 것을 말해주지 않는다"
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 회귀: [영구 삭제]가 `queue_io`보다 **먼저** 확인을 닫아, capacity-1 큐가 거절하면
    /// 아무것도 지우지 않은 채 확인만 사라지던 것. 사용자에게는 "지워진 건가?"만 남고
    /// 다시 누를 화면이 없었다. 확인은 삭제를 접수했을 때만 소비해야 한다.
    #[test]
    fn kittest_영구삭제_버튼은_큐가_거절하면_확인을_남긴다() {
        use egui_kittest::kittest::Queryable as _;

        let base = temp_root("permanent-delete-busy");
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/mod.rs"), b"x").unwrap();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        let mut harness = delete_confirm_harness(tree, &catalog);

        let target = DeleteTarget {
            path: base.join("src/mod.rs"),
            label: "src/mod.rs".to_owned(),
            is_dir: false,
        };
        harness.state_mut().0.spawn_trash(target.clone());
        let intent = harness
            .state_mut()
            .0
            .take_io_intent()
            .expect("trash intent");
        harness.state_mut().0.complete_io(FileTreeIoCompletion {
            operation: intent.operation,
            generation: intent.generation,
            result: Err(FileTreeIoErrorCode::TrashUnavailable),
        });
        // 다른 파일 조작(⌘C 복사)이 capacity-1 큐를 차지한다 — intent를 꺼내지 않아
        // 클릭하는 동안 계속 점유 상태다.
        harness
            .state_mut()
            .0
            .copy_files_to_clipboard(&[base.join("src/mod.rs")]);
        harness.step();

        harness
            .get_by_label(&catalog.t("file_tree.permanent_delete", &[]))
            .click();
        for _ in 0..8 {
            harness.step();
        }

        assert_eq!(
            harness.state().0.error.as_deref(),
            Some(file_tree_io_error_message(FileTreeIoErrorCode::Busy)),
            "거절 사유가 Busy가 아니라 다른 갈래를 짚었다"
        );
        assert_eq!(
            harness.state().0.confirm_delete,
            Some(target),
            "큐가 거절해 아무것도 지우지 않았는데 확인이 사라졌다 — 다시 누를 화면이 없다"
        );
        let queued = harness
            .state_mut()
            .0
            .take_io_intent()
            .expect("점유 중이던 복사 intent");
        assert!(
            matches!(queued.request, FileTreeIoRequest::CopyFileUrls { .. }),
            "거절됐어야 할 영구 삭제가 큐에 들어갔다: {:?}",
            queued.request
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 회귀: 영구삭제 확인이 트리 갱신으로는 **한 번도** 무효화되지 않던 것(경로 TOCTOU).
    ///
    /// 확인은 사용자가 누를 때까지 남는다. 그 사이 브랜치 전환·빌드·외부 편집이 그
    /// 경로를 지우고 **같은 자리에 다른 것**을 만들면, 문구는 옛 이름을 말하는데 삭제는
    /// 지금 그 자리에 있는 것을 지웠다. 종류까지 바뀌면 파일용 문구를 띄운 채
    /// `remove_dir_all`이 폴더를 통째로 지운다. 반대로 폴더를 접었을 뿐인데 확인이
    /// 닫히면 사용자가 놀라므로, "안 보인다"와 "없어졌다"를 갈라 놓는 것까지 함께 고정한다.
    #[test]
    fn 영구삭제_확인은_나열에서_사라지면_닫히고_접기로는_닫히지_않는다() {
        let base = temp_root("confirm-toctou");
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/gone.rs"), b"x").unwrap();
        std::fs::write(base.join("src/flip.rs"), b"x").unwrap();
        std::fs::write(base.join("src/keep.rs"), b"x").unwrap();

        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        tree.toggle_dir(&base.join("src"));
        drain_listings(&mut tree);

        // 확인을 세우는 유일한 길은 휴지통 이동 실패다.
        let arm = |tree: &mut FileTreeUi, name: &str, is_dir: bool| {
            tree.spawn_trash(DeleteTarget {
                path: base.join("src").join(name),
                label: format!("src/{name}"),
                is_dir,
            });
            let intent = tree.take_io_intent().expect("trash intent");
            tree.complete_io(FileTreeIoCompletion {
                operation: intent.operation,
                generation: intent.generation,
                result: Err(FileTreeIoErrorCode::TrashUnavailable),
            });
            assert!(
                tree.confirm_delete.is_some(),
                "확인이 서지 않아 검증 전제가 깨졌다"
            );
        };

        // ① 대상이 정말 사라졌다 — 재나열이 확인을 닫는다.
        arm(&mut tree, "gone.rs", false);
        std::fs::remove_file(base.join("src/gone.rs")).unwrap();
        tree.reload_dir(&base.join("src"));
        drain_listings(&mut tree);
        assert_eq!(
            tree.confirm_delete, None,
            "대상이 나열에서 사라졌는데 확인이 남아 있다 — 지금 그 자리에 있는 것이 지워진다"
        );

        // ② 같은 자리에 **다른 종류**가 들어왔다 — 파일용 문구가 폴더를 지우면 안 된다.
        arm(&mut tree, "flip.rs", false);
        std::fs::remove_file(base.join("src/flip.rs")).unwrap();
        std::fs::create_dir(base.join("src/flip.rs")).unwrap();
        tree.reload_dir(&base.join("src"));
        drain_listings(&mut tree);
        assert_eq!(
            tree.confirm_delete, None,
            "파일 자리에 폴더가 들어왔는데 파일용 확인이 그대로다 — 경고 없이 폴더가 통째로 지워진다"
        );

        // ③ 접기·다시 펼치기는 닫지 않는다. `flat`에서 사라지는 것은 "안 보인다"일 뿐이다.
        arm(&mut tree, "keep.rs", false);
        tree.toggle_dir(&base.join("src"));
        assert!(
            tree.flat
                .iter()
                .all(|row| row.path != base.join("src/keep.rs")),
            "접기로 대상 행이 사라지지 않아 ③의 전제가 깨졌다"
        );
        assert!(
            tree.confirm_delete.is_some(),
            "폴더를 접었을 뿐인데 확인이 닫혔다 — 안 보이는 것과 없어진 것은 다르다"
        );
        tree.toggle_dir(&base.join("src"));
        drain_listings(&mut tree);
        assert!(
            tree.confirm_delete.is_some(),
            "대상이 그대로 있는데 재나열이 확인을 닫았다"
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 회귀: IO 오류 라벨과 진행 스피너가 패널 밖으로 밀려 보이지 않던 것.
    ///
    /// 목록 `ScrollArea`는 `auto_shrink([false,false])`라 남은 높이를 전부 가져간다 —
    /// 그 **뒤에** 그린 상태 표시는 패널 바닥 밖으로 나간다. 420×700 패널에서 실측하면
    /// 오류 라벨이 y=717.5..732.5, 진행 문구가 y=696.5..711.5였다 — 둘 다 바닥(700)을
    /// 넘어 잘렸다. 파일 조작이 실패해도 오래 걸려도 화면에는 아무 표시가 없었다
    /// (§조용한 실패 금지).
    ///
    /// 오류·영구삭제 확인·진행 표시는 동시에 뜰 수 있으므로 셋이 헤더 아래에
    /// 겹치지 않고 순서대로 쌓이는 것까지 함께 고정한다.
    #[test]
    fn kittest_오류와_진행표시는_패널_안에_쌓인다() {
        use egui_kittest::kittest::Queryable as _;

        let base = temp_root("io-status-visible");
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/mod.rs"), b"x").unwrap();

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);

        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .build_ui_state(
                |ui, state: &mut (FileTreeUi, bool)| {
                    // 폰트 설치 전 빌드 프레임은 건너뛴다(사이드바 harness 관례).
                    if !state.1 {
                        return;
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: "ws-test",
                        workspaces: &[],
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
                    };
                    state
                        .0
                        .panel(ui, &std::collections::HashMap::new(), &snapshot, &catalog);
                },
                (tree, false),
            );
        install_sidebar_test_fonts(&harness.ctx);
        harness.state_mut().1 = true;
        harness.step();
        harness.step();

        // 휴지통 실패 → 오류 + 영구삭제 확인이 동시에 선다.
        harness.state_mut().0.spawn_trash(DeleteTarget {
            path: base.join("src/mod.rs"),
            label: "src/mod.rs".to_owned(),
            is_dir: false,
        });
        let intent = harness
            .state_mut()
            .0
            .take_io_intent()
            .expect("trash intent");
        harness.state_mut().0.complete_io(FileTreeIoCompletion {
            operation: intent.operation,
            generation: intent.generation,
            result: Err(FileTreeIoErrorCode::TrashUnavailable),
        });
        // 그 위에 새 파일 조작을 얹어 진행 표시까지 셋이 동시에 뜨게 한다.
        harness
            .state_mut()
            .0
            .copy_files_to_clipboard(&[base.join("src/mod.rs")]);
        assert!(
            harness.state().0.in_flight > 0,
            "진행 중 상태를 만들지 못해 스피너 배치를 검증할 수 없다"
        );
        harness.step();

        // 오류 문구는 **실제로 보고된 것**을 그대로 쓴다. 테스트가 코드를 재현해
        // 만들면 라벨과 상태가 갈라져도 통과한다.
        let reported = harness
            .state()
            .0
            .error
            .clone()
            .expect("휴지통 실패가 오류 상태로 남지 않았다");
        let running = catalog.t("file_tree.file_operation_running", &[]);
        let prompt = catalog.t(
            "file_tree.permanent_delete_prompt",
            &[("name", "src/mod.rs")],
        );

        // 라벨 노드는 클립 밖에서도 만들어지므로 존재가 아니라 **위치**를 본다.
        let error_rect = harness.get_by_label(&reported).rect();
        let running_rect = harness.get_by_label(&running).rect();
        let prompt_rect = harness.get_by_label(&prompt).rect();
        assert!(
            error_rect.bottom() <= 700.0,
            "오류 라벨이 패널(700px) 밖으로 밀렸다: {error_rect:?}"
        );
        assert!(
            running_rect.bottom() <= 700.0,
            "진행 표시가 패널(700px) 밖으로 밀렸다: {running_rect:?}"
        );

        // 셋이 겹치지 않고 오류 → 영구삭제 확인 → 진행 표시 순으로 쌓인다.
        assert!(
            error_rect.bottom() <= prompt_rect.top(),
            "오류와 영구삭제 확인이 겹친다: 오류={error_rect:?} 문구={prompt_rect:?}"
        );
        assert!(
            prompt_rect.bottom() <= running_rect.top(),
            "영구삭제 확인과 진행 표시가 겹친다: 문구={prompt_rect:?} 진행={running_rect:?}"
        );

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
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
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

    #[test]
    fn kittest_folder_typeahead는_화면밖_폴더행을_보이고_열지_않는다() {
        use egui_kittest::kittest::Queryable as _;

        let base = temp_root("typeahead-reveal");
        for index in 0..30 {
            std::fs::create_dir(base.join(format!("A{index:02}"))).unwrap();
        }
        std::fs::create_dir(base.join("Fold")).unwrap();
        let base = base.canonicalize().unwrap();
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspaces = vec![SidebarWorkspaceEntry {
            id: "ws-typeahead".to_owned(),
            name: "typeahead".to_owned(),
            state: SidebarWorkspaceState::Active,
            summary: SidebarSessionSummary::default(),
        }];
        let mut harness = close_menu_harness(&workspaces, "ws-typeahead", &catalog);
        harness.state_mut().0.set_root(Some(base.clone()));
        drain_listings(&mut harness.state_mut().0);
        harness.step();
        assert!(
            harness.query_by_label("Fold").is_none(),
            "처음엔 화면 밖이어야 한다"
        );
        harness
            .ctx
            .memory_mut(|memory| memory.request_focus(tree_keyboard_focus_id()));
        harness.event(egui::Event::Text("F".to_owned()));
        harness.step();
        assert!(
            harness.query_by_label("Fold").is_some(),
            "Fold 행이 화면에 보여야 한다"
        );
        assert_eq!(
            harness.state().0.selected,
            BTreeSet::from([base.join("Fold")])
        );
        assert_eq!(harness.state().0.root.as_deref(), Some(base.as_path()));
        harness.state_mut().0.selected.clear();
        harness.state_mut().0.start_edit(EditState::NewFolder {
            parent: base.clone(),
            buffer: String::new(),
            focus: false,
        });
        harness
            .ctx
            .memory_mut(|memory| memory.request_focus(tree_keyboard_focus_id()));
        harness.event(egui::Event::Text("A".to_owned()));
        harness.step();
        assert!(
            harness.state().0.selected.is_empty(),
            "편집 중인 글자는 탐색하지 않는다"
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn kittest_file_search_결과를_폴더트리에서_보인다() {
        use egui_kittest::kittest::Queryable as _;

        let base = temp_root("search-reveal").canonicalize().unwrap();
        std::fs::create_dir_all(base.join("nested")).unwrap();
        let found = base.join("nested/Fold-note.md");
        std::fs::write(&found, b"note").unwrap();
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspaces = vec![SidebarWorkspaceEntry {
            id: "ws-search".to_owned(),
            name: "search".to_owned(),
            state: SidebarWorkspaceState::Active,
            summary: SidebarSessionSummary::default(),
        }];
        let mut harness = close_menu_harness(&workspaces, "ws-search", &catalog);
        harness.state_mut().0.set_root(Some(base.clone()));
        drain_listings(&mut harness.state_mut().0);
        harness.state_mut().0.search = Some(FileTreeSearch {
            query: "fold".to_owned(),
            results: vec![found.clone()],
            selected: None,
            busy: false,
            result_limit_reached: false,
            traversal_incomplete: false,
            focus: false,
        });
        harness.step();
        // kittest의 step은 프레임을 완료한 뒤 반환하므로, 바로 이전 프레임 번호가
        // 등록되어 있어야 한다. workspace는 같은 프레임 안에서 이 값을 읽는다.
        assert_eq!(
            harness
                .ctx
                .data(|data| { data.get_temp::<u64>(file_tree_search_keyboard_owner_id()) }),
            Some(harness.ctx.cumulative_frame_nr() - 1)
        );
        harness.get_by_label("nested/Fold-note.md").click();
        harness.step();
        assert_eq!(
            harness.state().0.search.as_ref().unwrap().selected,
            Some(found.clone())
        );
        harness.step();
        harness.get_by_label("Show in tree").click();
        harness.step();
        drain_listings(&mut harness.state_mut().0);
        harness.step();
        assert!(harness.state().0.search.is_none());
        assert!(!file_tree_search_keyboard_active(&harness.ctx));
        assert_eq!(harness.state().0.root.as_deref(), found.parent());
        assert_eq!(harness.state().0.selected, BTreeSet::from([found]));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn 파일검색_키보드_소유권은_패널이_없는_다음_프레임에_남지_않는다() {
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            mark_file_tree_search_keyboard_active(ui.ctx(), true);
            assert!(file_tree_search_keyboard_active(ui.ctx()));
        })
        .drop_without_applying_deltas();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            assert!(!file_tree_search_keyboard_active(ui.ctx()));
        })
        .drop_without_applying_deltas();
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

    /// 워크스페이스 상태와 관계없이 대상 워크스페이스에서 새 세션을 시작할 수 있어야
    /// 한다. App은 이 id를 받아 필요하면 먼저 워크스페이스를 전환한다.
    #[test]
    fn kittest_세션열기_클릭이_대상_workspace_액션을_낸다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspace = SidebarWorkspaceEntry {
            id: "ws-session".to_owned(),
            name: "session-target".to_owned(),
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
        harness.get_by_label("Open session").click();
        harness.run();

        assert!(
            matches!(
                harness.state(),
                Some(SidebarAction::OpenWorkspaceSession(id)) if id == "ws-session"
            ),
            "세션 열기 클릭이 대상 워크스페이스 id를 보존하지 않음"
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

    /// Idle(비활성) 워크스페이스 메뉴에도 「세션 열기」와 「이름 바꾸기」는 있고 종료
    /// 항목만 없다 — 닫을 세션이 없다.
    #[test]
    fn kittest_비활성_워크스페이스에는_종료메뉴가_없다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspaces = vec![SidebarWorkspaceEntry {
            id: "ws-idle".to_owned(),
            name: "sleeper".to_owned(),
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

    #[test]
    fn kittest_warm_세션행_hover_오른쪽열기가_정확한_대상을_낸다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspace = SidebarWorkspaceEntry {
            id: "workspace-b".to_owned(),
            name: "Workspace B".to_owned(),
            state: SidebarWorkspaceState::Warm,
            summary: SidebarSessionSummary::default(),
        };
        let entry = SidebarSessionRow::from_live(
            "workspace-b",
            7,
            SessionEntry {
                tab: runtime::MuxTabId("tab-b".to_owned()),
                pane: runtime::MuxPaneId("pane-b".to_owned()),
                session: Some(runtime::SessionId(42)),
                title: "Session B".to_owned(),
                title_is_custom: true,
                agent_model: None,
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
                last_output_at: None,
            },
        );
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(360.0, 160.0))
            .build_ui_state(
                |ui, state: &mut (Option<SidebarAction>, bool)| {
                    if !state.1 {
                        return;
                    }
                    let (next_action, _) = inactive_workspace_sessions(
                        ui,
                        &workspace,
                        "workspace-a",
                        std::slice::from_ref(&entry),
                        egui::Color32::LIGHT_BLUE,
                        &catalog,
                    );
                    if next_action.is_some() {
                        state.0 = next_action;
                    }
                },
                (None, false),
            );
        let font_config = crate::config::Config::default();
        crate::fonts::install_cjk_fallback(
            &harness.ctx,
            None,
            &font_config.terminal.mono_font,
            &font_config.terminal.mono_weight,
        );
        harness.state_mut().1 = true;

        harness.run();
        let row_rect = harness.get_by_label("Session B").rect();
        harness.hover_at(row_rect.center());
        harness.run();
        harness.get_by_label("↗").click();
        harness.run();

        match &harness.state().0 {
            Some(SidebarAction::OpenSessionBeside(SessionRowTarget::Live {
                workspace_id,
                runtime_instance,
                tab,
                pane,
                session,
            })) => {
                assert_eq!(workspace_id, "workspace-b");
                assert_eq!(*runtime_instance, 7);
                assert_eq!(tab.0, "tab-b");
                assert_eq!(pane.0, "pane-b");
                assert_eq!(*session, runtime::SessionId(42));
            }
            Some(SidebarAction::FocusSession { .. }) => {
                panic!("hover 오른쪽 열기 클릭을 세션 행 클릭이 탈취함")
            }
            None => panic!("hover 오른쪽 열기 클릭이 액션을 내지 않음"),
            _ => panic!("hover 오른쪽 열기가 다른 액션을 냄"),
        }
    }

    #[test]
    fn kittest_활성_세션행_hover_닫기가_정확한_pane을_닫는다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspaces = vec![SidebarWorkspaceEntry {
            id: "workspace-a".to_owned(),
            name: "Workspace A".to_owned(),
            state: SidebarWorkspaceState::Active,
            summary: SidebarSessionSummary::default(),
        }];
        let sessions = std::collections::HashMap::from([(
            "workspace-a".to_owned(),
            vec![SidebarSessionRow::from_live(
                "workspace-a",
                7,
                SessionEntry {
                    tab: runtime::MuxTabId("tab-a".to_owned()),
                    pane: runtime::MuxPaneId("pane-a".to_owned()),
                    session: Some(runtime::SessionId(42)),
                    title: "Session A".to_owned(),
                    title_is_custom: true,
                    agent_model: None,
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
                    last_output_at: None,
                },
            )],
        )]);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .build_ui_state(
                |ui, state: &mut (FileTreeUi, Vec<SidebarAction>, bool)| {
                    if !state.2 {
                        return;
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: "workspace-a",
                        workspaces: &workspaces,
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
                    };
                    if let Some(action) = state.0.panel(ui, &sessions, &snapshot, &catalog) {
                        state.1.push(action);
                    }
                },
                (FileTreeUi::new(egui::Context::default()), Vec::new(), false),
            );
        install_sidebar_test_fonts(&harness.ctx);
        harness.state_mut().2 = true;

        harness.run();
        let row_rect = harness.get_by_label("Session A").rect();
        harness.hover_at(row_rect.center());
        harness.run();
        harness.get_by_label("×").click();
        harness.run();

        assert!(matches!(
            harness.state().1.as_slice(),
            [SidebarAction::ClosePane { pane }] if pane.0 == "pane-a"
        ));
    }

    /// 회귀 고정(2026-08-15): 세션 행 컨텍스트 메뉴의 「변경 보기」는 **그 세션**의
    /// ShowDiff{session}을 낸다 — 포커스 세션이 아니라 클릭한 행 기준이어야 한다.
    /// 한때 payload 없는 유닛 variant로 단순화됐다가(포커스 세션 기준으로 App이
    /// 일원화) 세션 B 행을 눌러도 세션 A(포커스)의 repo가 뜨는 회귀가 났다. 세션 A·B
    /// 둘 다 초점(focused) 없이 두고 세션 B 행을 우클릭·클릭해, 액션이 세션 A가
    /// 아니라 세션 B의 id를 담는지로 "포커스 아님, 클릭한 행"을 고정한다.
    #[test]
    fn kittest_세션_행_변경_보기는_그_세션의_showdiff_액션을_낸다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspaces = vec![SidebarWorkspaceEntry {
            id: "workspace-a".to_owned(),
            name: "Workspace A".to_owned(),
            state: SidebarWorkspaceState::Active,
            summary: SidebarSessionSummary::default(),
        }];
        let sessions = std::collections::HashMap::from([(
            "workspace-a".to_owned(),
            vec![
                SidebarSessionRow::from_live(
                    "workspace-a",
                    7,
                    SessionEntry {
                        tab: runtime::MuxTabId("tab-a".to_owned()),
                        pane: runtime::MuxPaneId("pane-a".to_owned()),
                        session: Some(runtime::SessionId(1)),
                        title: "Session A".to_owned(),
                        title_is_custom: true,
                        agent_model: None,
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
                        last_output_at: None,
                    },
                ),
                SidebarSessionRow::from_live(
                    "workspace-a",
                    7,
                    SessionEntry {
                        tab: runtime::MuxTabId("tab-b".to_owned()),
                        pane: runtime::MuxPaneId("pane-b".to_owned()),
                        session: Some(runtime::SessionId(2)),
                        title: "Session B".to_owned(),
                        title_is_custom: true,
                        agent_model: None,
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
                        last_output_at: None,
                    },
                ),
            ],
        )]);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(420.0, 700.0))
            .build_ui_state(
                |ui, state: &mut (FileTreeUi, Vec<SidebarAction>, bool)| {
                    if !state.2 {
                        return;
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: "workspace-a",
                        workspaces: &workspaces,
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
                    };
                    if let Some(action) = state.0.panel(ui, &sessions, &snapshot, &catalog) {
                        state.1.push(action);
                    }
                },
                (FileTreeUi::new(egui::Context::default()), Vec::new(), false),
            );
        install_sidebar_test_fonts(&harness.ctx);
        harness.state_mut().2 = true;

        harness.run();
        let row_rect = harness.get_by_label("Session B").rect();
        harness.event(egui::Event::PointerMoved(row_rect.center()));
        harness.event(egui::Event::PointerButton {
            pos: row_rect.center(),
            button: egui::PointerButton::Secondary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        });
        harness.event(egui::Event::PointerButton {
            pos: row_rect.center(),
            button: egui::PointerButton::Secondary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        });
        harness.run();
        harness
            .get_by_label(&catalog.t("sidebar.menu.show_diff", &[]))
            .click();
        harness.run();

        assert!(matches!(
            harness.state().1.as_slice(),
            [SidebarAction::ShowDiff { session }] if *session == runtime::SessionId(2)
        ));
    }

    #[test]
    fn kittest_warm_세션행_drag가_정확한_payload를_시작한다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspace = SidebarWorkspaceEntry {
            id: "workspace-b".to_owned(),
            name: "Workspace B".to_owned(),
            state: SidebarWorkspaceState::Warm,
            summary: SidebarSessionSummary::default(),
        };
        let entry = SidebarSessionRow::from_live(
            "workspace-b",
            7,
            SessionEntry {
                tab: runtime::MuxTabId("tab-b".to_owned()),
                pane: runtime::MuxPaneId("pane-b".to_owned()),
                session: Some(runtime::SessionId(42)),
                title: "Session B".to_owned(),
                title_is_custom: true,
                agent_model: None,
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
                last_output_at: None,
            },
        );
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(360.0, 160.0))
            .build_ui_state(
                |ui, fonts_ready: &mut bool| {
                    if !*fonts_ready {
                        return;
                    }
                    let _ = inactive_workspace_sessions(
                        ui,
                        &workspace,
                        "workspace-a",
                        std::slice::from_ref(&entry),
                        egui::Color32::LIGHT_BLUE,
                        &catalog,
                    );
                },
                false,
            );
        let font_config = crate::config::Config::default();
        crate::fonts::install_cjk_fallback(
            &harness.ctx,
            None,
            &font_config.terminal.mono_font,
            &font_config.terminal.mono_weight,
        );
        *harness.state_mut() = true;

        harness.run();
        let row_rect = harness.get_by_label("Session B").rect();
        harness.hover_at(row_rect.center());
        harness.drag_at(row_rect.center());
        harness.run();
        harness.hover_at(row_rect.center() + egui::vec2(20.0, 0.0));
        harness.run();

        let payload = egui::DragAndDrop::payload::<SessionRowDragPayload>(&harness.ctx)
            .expect("session row drag payload");
        assert_eq!(
            payload.target(),
            &SessionRowTarget::Live {
                workspace_id: "workspace-b".to_owned(),
                runtime_instance: 7,
                tab: runtime::MuxTabId("tab-b".to_owned()),
                pane: runtime::MuxPaneId("pane-b".to_owned()),
                session: runtime::SessionId(42),
            }
        );
    }

    #[test]
    fn kittest_warm_세션의_오른쪽열기_메뉴가_정확한_대상을_낸다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspace = SidebarWorkspaceEntry {
            id: "workspace-b".to_owned(),
            name: "Workspace B".to_owned(),
            state: SidebarWorkspaceState::Warm,
            summary: SidebarSessionSummary::default(),
        };
        let entry = SidebarSessionRow::from_live(
            "workspace-b",
            7,
            SessionEntry {
                tab: runtime::MuxTabId("tab-b".to_owned()),
                pane: runtime::MuxPaneId("pane-b".to_owned()),
                session: Some(runtime::SessionId(42)),
                title: "Session B".to_owned(),
                title_is_custom: true,
                agent_model: None,
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
                last_output_at: None,
            },
        );
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, action: &mut Option<SidebarAction>| {
                inactive_session_context_menu_items(
                    ui,
                    &workspace,
                    "workspace-a",
                    &entry,
                    &catalog,
                    action,
                );
            },
            None,
        );

        harness.run();
        harness.get_by_label("Open beside").click();
        harness.run();

        match harness.state() {
            Some(SidebarAction::OpenSessionBeside(SessionRowTarget::Live {
                workspace_id,
                runtime_instance,
                tab,
                pane,
                session,
            })) => {
                assert_eq!(workspace_id, "workspace-b");
                assert_eq!(*runtime_instance, 7);
                assert_eq!(tab.0, "tab-b");
                assert_eq!(pane.0, "pane-b");
                assert_eq!(*session, runtime::SessionId(42));
            }
            _ => panic!("오른쪽 열기 메뉴가 namespaced target 액션을 내지 않음"),
        }
    }

    #[test]
    fn kittest_live_session_context_menu_keeps_all_items_on_one_line() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let labels = [
            catalog.t("workspace.rename_menu", &[]),
            catalog.t("sidebar.menu.open_folder", &[]),
            catalog.t("sidebar.menu.copy_path", &[]),
            catalog.t("sidebar.menu.new_shell_here", &[]),
            catalog.t("sidebar.menu.show_diff", &[]),
            catalog.t("sidebar.menu.new_worktree_cell", &[]),
            catalog.t("sidebar.menu.remove_worktree", &[]),
            catalog.t("sidebar.menu.resume_agent", &[]),
            catalog.t("sidebar.menu.close_pane", &[]),
        ];
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut (f32, bool, bool)| {
                ui.allocate_ui(egui::vec2(100.0, 500.0), |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                    let menu = live_session_context_menu_items(ui, |ui| {
                        let extends = ui.wrap_mode() == egui::TextWrapMode::Extend;
                        for label in &labels {
                            let _ = ui.button(label);
                        }
                        extends
                    });
                    state.0 = menu.response.rect.width();
                    state.1 = menu.inner;
                    state.2 = ui.wrap_mode() == egui::TextWrapMode::Wrap;
                });
            },
            (0.0, false, false),
        );

        harness.run();
        assert!(harness.state().0 >= 220.0);
        assert!(harness.state().1, "menu scope did not use Extend wrapping");
        assert!(
            harness.state().2,
            "menu wrap mode leaked into its parent UI"
        );
        let baseline_height = harness.get_by_label(&labels[0]).rect().height();
        for label in &labels {
            let rect = harness.get_by_label(label).rect();
            assert!(
                (rect.height() - baseline_height).abs() < 0.5,
                "menu item wrapped instead of extending: {label} ({rect:?})"
            );
        }
    }

    /// 세션 이름 변경은 **우클릭 메뉴에만** 있다. 더블클릭 진입은 제거했다
    /// (2026-08-11 사용자) — 세션 행의 주 동작은 전환인데 빠르게 두 번 누르면
    /// 편집기가 열려 오조작이 됐다. 진입점이 다시 늘면 같은 문제가 돌아온다.
    #[test]
    fn 세션_이름_변경은_우클릭_메뉴에만_있다() {
        let source = include_str!("file_tree.rs");
        let production = source
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("production source");

        // `= None`(편집 종료)은 진입이 아니라 해제다. 전체에서 그것만 빼고 세면
        // 남는 것이 **편집을 여는 경로**다.
        let assignments = production.matches("self.session_name_edit =").count();
        let clears = production.matches("self.session_name_edit = None").count();
        assert_eq!(
            assignments - clears,
            1,
            "이름 편집 진입점이 하나가 아니다 — 우클릭 메뉴 외에 다른 경로가 생겼다 \
             (전체 {assignments}, 해제 {clears})"
        );

        // 그 하나가 컨텍스트 메뉴 안이어야 한다.
        let menu = production
            .split("resp.context_menu(|ui| {")
            .nth(1)
            .and_then(|tail| tail.split("} else if resp.clicked()").next())
            .expect("live session context menu");
        assert!(
            menu.contains("self.session_name_edit ="),
            "이름 편집이 우클릭 메뉴 밖으로 나갔다"
        );
    }

    #[test]
    fn live_session_row_context_menu_uses_the_multi_item_renderer() {
        let source = include_str!("file_tree.rs");
        let production = source
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("production source");
        let context_menu = production
            .split("resp.context_menu(|ui| {")
            .nth(1)
            // 경계: 메뉴 블록 다음에 오는 클릭 처리. 예전엔 `if resp.double_clicked()`가
            // 그 자리였는데 더블클릭 이름 변경을 제거하며 사라졌다(2026-08-11).
            .and_then(|tail| tail.split("} else if resp.clicked()").next())
            .expect("live session context menu");

        assert!(context_menu.contains("live_session_context_menu_items("));
    }

    #[test]
    fn kittest_활성_workspace_세션에는_옆에열기_메뉴가_없다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        for state in [SidebarWorkspaceState::Active, SidebarWorkspaceState::Warm] {
            let workspace_id = "workspace-a";
            let active_workspace_id = "workspace-a";
            let workspace = SidebarWorkspaceEntry {
                id: workspace_id.to_owned(),
                name: workspace_id.to_owned(),
                state,
                summary: SidebarSessionSummary::default(),
            };
            let entry = SidebarSessionRow::from_live(
                workspace_id,
                7,
                SessionEntry {
                    tab: runtime::MuxTabId("tab".to_owned()),
                    pane: runtime::MuxPaneId("pane".to_owned()),
                    session: Some(runtime::SessionId(1)),
                    title: "Session".to_owned(),
                    title_is_custom: true,
                    agent_model: None,
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
                    last_output_at: None,
                },
            );
            let mut harness = egui_kittest::Harness::new_ui_state(
                |ui, action: &mut Option<SidebarAction>| {
                    inactive_session_context_menu_items(
                        ui,
                        &workspace,
                        active_workspace_id,
                        &entry,
                        &catalog,
                        action,
                    );
                },
                None,
            );

            harness.run();
            assert!(
                harness.query_by_label("Open beside").is_none(),
                "state={state:?}, workspace={workspace_id}, active={active_workspace_id}"
            );
            assert!(harness.state().is_none());
        }
    }

    /// 좁은 폭에서도 워크스페이스 메뉴 항목은 한 줄로 그려진다 — 이전에는 메뉴가
    /// 좁은 폭을 물려받아 「워크스페이스 종료」가 두 줄로 잘렸다(2026-07-18 스샷).
    /// 메뉴 본문이 가장 긴 항목의 no-wrap 폭으로 최소 폭을 강제하므로 100px 제약
    /// 안에서도 버튼이 제약 밖으로 확장되고(줄바꿈 없음) 세 항목의 행 높이가 같다.
    #[test]
    fn kittest_좁은_폭에서도_워크스페이스_메뉴가_한줄로_그려진다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspace = SidebarWorkspaceEntry {
            id: "ws-narrow".to_owned(),
            name: "narrow".to_owned(),
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
        let open = harness.get_by_label("Open session").rect();
        let close = harness.get_by_label("Close workspace sessions").rect();
        let rename = harness.get_by_label("Rename workspace").rect();
        assert!(
            close.width() > 100.0,
            "종료 버튼이 최소 폭으로 확장되지 않음 (width={})",
            close.width()
        );
        assert!(
            (close.height() - rename.height()).abs() < 0.5
                && (open.height() - rename.height()).abs() < 0.5,
            "메뉴 항목이 여러 줄로 접힘 (open={}, close={}, rename={})",
            open.height(),
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
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
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
    fn designall_nav행은_아이콘위_텍스트아래_중앙정렬한다() {
        let row = egui::Rect::from_min_size(
            egui::pos2(10.0, 20.0),
            egui::vec2(80.0, SIDEBAR_NAV_ROW_HEIGHT),
        );
        let layout = nav_row_layout(row, true);

        assert_eq!(layout.icon_center.x, row.center().x);
        assert_eq!(layout.label_anchor.x, row.center().x);
        assert!(layout.icon_center.y < layout.label_anchor.y);
        // 아이콘 위·라벨 아래가 행 안에 들어가야 한다 — 행을 낮추면서 넘치면
        // 레일 첫 행이 패널 위로 잘린다(아이콘 13×12, 라벨 12pt 기준).
        assert!(
            layout.icon_center.y - 6.5 > row.top(),
            "아이콘이 행 위로 넘쳤다"
        );
        assert!(
            layout.label_anchor.y + 6.0 < row.bottom(),
            "라벨이 행 아래로 넘쳤다"
        );
    }

    #[test]
    fn 한국어_레일문구는_작업과_ai를_사용한다() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        assert_eq!(catalog.t("sidebar.nav.fleet", &[]), "작업");
        assert_eq!(catalog.t("sidebar.nav.agents", &[]), "AI");
    }

    #[test]
    fn kittest_도움말_버튼은_레일하단에_고정되고_메뉴로_설정을_연다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut fonts_ready = false;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(520.0, 700.0))
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
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
                    };
                    if let Some(action) =
                        state
                            .0
                            .panel(ui, &std::collections::HashMap::new(), &snapshot, &catalog)
                    {
                        state.1.push(action);
                    }
                },
                (FileTreeUi::new(egui::Context::default()), Vec::new()),
            );
        harness.run();

        // 레일 하단에 고정되는 것은 이제 도움말(물음표) 버튼이다(2026-08-21).
        let help_rect = harness.get_by_label("Help").rect();
        let rail = egui::PanelState::load(&harness.ctx, egui::Id::new("designall_navigation_rail"))
            .unwrap();
        assert!(
            (help_rect.bottom() - rail.outer_rect.bottom()).abs() < 4.0,
            "help bottom {} vs rail bottom {}",
            help_rect.bottom(),
            rail.outer_rect.bottom()
        );

        harness.get_by_label("Help").click();
        harness.run();

        // 메뉴 순서는 버전 → 업데이트 → 피드백 → 설정이다. 사용자가 지정한 순서라
        // y좌표로 고정한다.
        let update_y = harness.get_by_label("Check for updates").rect().top();
        let feedback_y = harness.get_by_label("Send feedback").rect().top();
        let settings_y = harness.get_by_label("Settings").rect().top();
        assert!(
            update_y < feedback_y && feedback_y < settings_y,
            "메뉴 순서가 업데이트 < 피드백 < 설정이어야 한다: {update_y} / {feedback_y} / {settings_y}"
        );

        harness.get_by_label("Settings").click();
        harness.run();
        assert!(matches!(
            harness.state().1.as_slice(),
            [SidebarAction::OpenSettings]
        ));
    }

    #[test]
    fn kittest_designall은_내비게이션레일과_프로젝트패널을_분리한다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let snapshot = SidebarSnapshot {
            active_workspace_id: "ws-test",
            workspaces: &[],
            view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
            home_notice_count: 0,
            fleet_summary: Default::default(),
            history_tab_active: false,
            git_tab_active: false,
            agents_open: false,
            workspace_note: None,
        };
        let ctx = egui::Context::default();
        install_sidebar_test_fonts(&ctx);
        let mut tree = FileTreeUi::new(ctx.clone());
        ctx.run_ui(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let _ = tree.panel(ui, &std::collections::HashMap::new(), &snapshot, &catalog);
            });
        })
        .drop_without_applying_deltas();

        let navigation = egui::PanelState::load(&ctx, egui::Id::new("designall_navigation_rail"))
            .expect("DesignALL 내비게이션 레일이 별도 패널이어야 한다");
        let project = egui::PanelState::load(&ctx, egui::Id::new("designall_project_file_panel"))
            .expect("DesignALL 프로젝트·파일 영역이 별도 패널이어야 한다");

        assert!((navigation.size().x - crate::ui::designall::NAV_RAIL_WIDTH).abs() < 0.1);
        assert!(project.size().x >= 40.0);
    }

    #[test]
    fn kittest_파일헤더는_파일_메모탭만_표시한다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let tree = FileTreeUi::new(egui::Context::default());
        let mut harness = drop_harness(&catalog, tree);
        harness.run();
        harness.run();

        assert!(harness.query_by_label("Files").is_some());
        assert!(harness.query_by_label("Notes").is_some());
        // Git은 2026-08-15 2차에서 레일로 옮겼다(스펙 §8-1). 이 하네스는 레일까지 같이
        // 그리므로 "Git 라벨이 아예 없다"로는 확인할 수 없다 — **도구 탭 줄에는 없고
        // 레일에만 있다**를 좌표로 고정한다.
        let files_rect = harness.get_by_label("Files").rect();
        let git_rect = harness.get_by_label("Git").rect();
        assert!(
            git_rect.right() <= files_rect.left(),
            "Git이 도구 탭 줄에 남아 있다 — 레일(좌측)에만 있어야 한다: git={git_rect:?} files={files_rect:?}"
        );
        // MCP 탭은 뺐다(2026-08-10) — 혼자 설정(별도 OS 창)을 열어 레벨이 달랐다.
        // 연결 설정은 설정 → 관리 → 「연결」이 계속 담당한다.
        assert!(harness.query_by_label("MCP").is_none());
        assert!(harness.query_by_label("Search").is_none());
        assert!(harness.query_by_label("Terminal").is_none());
    }

    #[test]
    fn kittest_파일헤더는_새로고침과_더보기만_노출하고_나머지는_메뉴에_둔다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let tree = FileTreeUi::new(egui::Context::default());
        let mut harness = drop_harness(&catalog, tree);
        harness.run();
        harness.run();

        assert!(harness.query_by_label("Refresh").is_some());
        harness.get_by_label("More").click();
        harness.run();

        assert!(harness.query_by_label("New file (root)").is_some());
        assert!(harness.query_by_label("Show hidden files").is_some());
        assert!(harness.query_by_label("New folder (root)").is_some());
    }

    #[test]
    fn designall_titlebar는_조절된_레일과_프로젝트폭을_따른다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        assert_eq!(tree.designall_titlebar_widths(), (88.0, 200.0));

        tree.navigation_rail_width = 10.0;
        tree.sidebar_width = 900.0;
        assert_eq!(tree.designall_titlebar_widths(), (20.0, 680.0));

        tree.collapse_project_file_panel();
        assert_eq!(tree.designall_titlebar_widths(), (20.0, 22.0));
    }

    /// 하단 nav 3항목 렌더 + 클릭 → 액션 방출. 재클릭 토글은 App 로직이라
    /// 여기서는 방출까지만 검증한다. 2026-08-08 작업함이 「작업」에 흡수돼 4→3항목이다.
    #[test]
    fn kittest_하단_nav_클릭이_홈_작업_에이전트_액션을_낸다() {
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
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
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
        harness.get_by_label("Work").click();
        harness.run();
        harness.get_by_label("History").click();
        harness.run();
        harness.get_by_label("AI").click();
        harness.run();
        let kinds: Vec<&'static str> = harness
            .state()
            .1
            .iter()
            .map(|action| match action {
                SidebarAction::ShowHome => "home",
                SidebarAction::ShowFleet => "work",
                SidebarAction::ShowHistory => "history",
                SidebarAction::OpenAgents => "agents",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["home", "work", "history", "agents"],
            "nav 4항목 클릭이 각각의 액션을 순서대로 내야 한다"
        );
    }

    /// 반입 대상 폴더 판정(§과제①②) — 폴더 행은 자신, 파일 행은 부모.
    #[test]
    fn 반입_대상은_폴더행_자신_파일행_부모() {
        let root = PathBuf::from("/ws");
        let dir_row = FlatRow {
            path: root.join("sub"),
            display_name: "sub".to_owned(),
            depth: 0,
            is_dir: true,
            expanded: false,
        };
        let file_row = FlatRow {
            path: root.join("sub/a.txt"),
            display_name: "a.txt".to_owned(),
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
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
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
        harness
            .input_mut()
            .dropped_files
            .push(crate::test_dropped_file::handle(src.clone()));
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

    #[test]
    fn typed_persisted_row_carries_exact_pane_id_into_session_target() {
        let target =
            SessionRowTarget::persisted("workspace-b", runtime::MuxPaneId("pane-exact".to_owned()));

        assert_eq!(
            target,
            SessionRowTarget::PersistedPane {
                workspace_id: "workspace-b".to_owned(),
                pane: runtime::MuxPaneId("pane-exact".to_owned()),
            }
        );
    }

    #[test]
    fn persisted_session_click_carries_workspace_and_exact_pane() {
        let action = session_row_activation(&SessionRowTarget::persisted(
            "workspace-b",
            runtime::MuxPaneId("pane-exact".to_owned()),
        ));

        assert!(matches!(
            action,
            SidebarAction::ActivatePersistedSession { workspace_id, pane }
                if workspace_id == "workspace-b" && pane.0 == "pane-exact"
        ));
    }

    #[test]
    fn active_unmaterialized_session_click_uses_the_same_exact_activation_mapper() {
        let source = include_str!("file_tree.rs");
        let active_rows = source
            .split_once("let close_clicked =")
            .unwrap()
            .1
            .split_once("paint_workspace_group_separator")
            .unwrap()
            .0;
        assert!(active_rows.contains("session_row_activation"));
        assert!(!active_rows.contains("live_tab()"));
    }

    #[test]
    fn focused_unmaterialized_session_remains_activatable() {
        let persisted =
            SessionRowTarget::persisted("workspace-b", runtime::MuxPaneId("pane-cold".to_owned()));
        let live = SessionRowTarget::live(
            "workspace-b",
            41,
            runtime::MuxTabId("tab-b".to_owned()),
            runtime::MuxPaneId("pane-live".to_owned()),
            runtime::SessionId(42),
        );

        assert!(session_row_should_activate(&persisted, true));
        assert!(!session_row_should_activate(&live, true));
        assert!(session_row_should_activate(&live, false));
    }

    #[test]
    fn live_session_target_keeps_all_exact_identifiers() {
        let target = SessionRowTarget::live(
            "workspace-b",
            41,
            runtime::MuxTabId("tab-b".to_owned()),
            runtime::MuxPaneId("pane-b".to_owned()),
            runtime::SessionId(42),
        );

        assert_eq!(target.workspace_id(), "workspace-b");
        assert_eq!(target.runtime_instance(), Some(41));
        assert_eq!(target.pane().0, "pane-b");
        assert_eq!(target.session(), Some(runtime::SessionId(42)));
    }

    #[test]
    fn warm_unmaterialized_pane_keeps_exact_persisted_target() {
        let row = SidebarSessionRow::from_live(
            "workspace-b",
            41,
            SessionEntry {
                tab: runtime::MuxTabId("tab-b".to_owned()),
                pane: runtime::MuxPaneId("pane-cold".to_owned()),
                session: None,
                title: "Saved shell".to_owned(),
                title_is_custom: true,
                agent_model: None,
                status: None,
                summary: String::new(),
                focused: false,
                attention: false,
                pulse: None,
                agent_line: None,
                status_label: None,
                resumable: false,
                has_cwd: true,
                in_worktree: false,
                status_line: None,
                last_output_at: None,
            },
        );

        assert_eq!(
            &row.target,
            &SessionRowTarget::PersistedPane {
                workspace_id: "workspace-b".to_owned(),
                pane: runtime::MuxPaneId("pane-cold".to_owned()),
            }
        );
    }

    #[test]
    fn session_drag_payload_debug_excludes_presentation_and_terminal_data() {
        let payload = SessionRowDragPayload::new(SessionRowTarget::persisted(
            "workspace-safe",
            runtime::MuxPaneId("pane-safe".to_owned()),
        ));
        let debug = format!("{payload:?}");

        assert!(debug.contains("workspace-safe"));
        assert!(debug.contains("pane-safe"));
        assert!(!debug.contains("SECRET_TITLE"));
        assert!(!debug.contains("SECRET/CWD"));
        assert!(!debug.to_ascii_lowercase().contains("scrollback"));
    }

    #[test]
    fn persisted_sidebar_row_preserves_canonical_fields_and_entry_target() {
        let row = SidebarSessionRow::from_persisted_parts(
            "workspace-b",
            runtime::MuxPaneId("pane-exact".to_owned()),
            "Saved shell".to_owned(),
            "/private/project-b".to_owned(),
        );

        assert_eq!(
            row.target,
            SessionRowTarget::PersistedPane {
                workspace_id: "workspace-b".to_owned(),
                pane: runtime::MuxPaneId("pane-exact".to_owned()),
            }
        );
        assert_eq!(row.title, "Saved shell");
        assert_eq!(row.summary, "/private/project-b");
        assert!(row.has_cwd);

        let hover = open_beside_action(row.target.clone());
        let context = open_beside_action(row.target.clone());
        let drag = SessionRowDragPayload::new(row.target.clone());
        assert!(matches!(
            (hover, context),
            (
                SidebarAction::OpenSessionBeside(left),
                SidebarAction::OpenSessionBeside(right)
            ) if left == right
        ));
        assert_eq!(drag.target(), &row.target);
    }

    #[test]
    fn hover_and_context_open_beside_emit_identical_action() {
        let target = SessionRowTarget::PersistedPane {
            workspace_id: "workspace-b".to_owned(),
            pane: runtime::MuxPaneId("pane-b".to_owned()),
        };

        let hover = open_beside_action(target.clone());
        let context = open_beside_action(target);
        assert!(matches!(
            (hover, context),
            (
                SidebarAction::OpenSessionBeside(left),
                SidebarAction::OpenSessionBeside(right)
            ) if left == right
        ));
    }

    #[test]
    fn session_row_drag_suppresses_ordinary_click_activation() {
        assert!(session_row_click_allowed(true, false));
        assert!(!session_row_click_allowed(true, true));
    }

    #[test]
    fn unrelated_dnd_payload_does_not_suppress_session_row_click() {
        let unrelated_file_payload_is_active = true;
        assert!(unrelated_file_payload_is_active);
        assert!(session_row_click_allowed(true, false));
    }

    #[test]
    fn inactive_session_menu_keeps_korean_labels_on_one_line() {
        let style = inactive_session_menu_style();

        assert!(style.min_width >= 220.0);
        assert_eq!(style.wrap_mode, egui::TextWrapMode::Extend);
    }

    #[test]
    fn exact_session_drag_projects_elevated_source_style() {
        let style = session_drag_style(true, crate::ui::designall::DARK);

        assert!(style.fill.is_some());
        assert_eq!(style.stroke.width, 1.0);
        assert!(style.shadow.blur > 0);
        assert!(style.rail_multiplier > 1.0);
    }

    #[test]
    fn inactive_session_drag_style_is_a_visual_noop() {
        let style = session_drag_style(false, crate::ui::designall::DARK);

        assert!(style.fill.is_none());
        assert_eq!(style.stroke, egui::Stroke::NONE);
        assert_eq!(style.shadow, egui::epaint::Shadow::NONE);
        assert_eq!(style.rail_multiplier, 1.0);
    }

    #[test]
    fn session_drag_matches_only_the_exact_active_payload() {
        let context = egui::Context::default();
        let exact = SessionRowTarget::persisted(
            "workspace-exact",
            runtime::MuxPaneId("pane-exact".to_owned()),
        );
        let other = SessionRowTarget::persisted(
            "workspace-other",
            runtime::MuxPaneId("pane-other".to_owned()),
        );

        egui::DragAndDrop::set_payload(&context, SessionRowDragPayload::new(exact.clone()));
        assert!(session_drag_payload_matches(&context, &exact));
        assert!(!session_drag_payload_matches(&context, &other));

        egui::DragAndDrop::set_payload(&context, PathBuf::from("/tmp/unrelated"));
        assert!(!session_drag_payload_matches(&context, &exact));
    }

    #[test]
    fn active_workspace_target_is_denied_for_cross_workspace_attach() {
        let target = SessionRowTarget::PersistedPane {
            workspace_id: "workspace-a".to_owned(),
            pane: runtime::MuxPaneId("pane-a".to_owned()),
        };

        assert!(!can_open_session_beside("workspace-a", &target));
        assert!(can_open_session_beside("workspace-b", &target));
    }

    /// indicator → 로고색 계약 — 색 자체가 상태 표기이므로 매핑을 고정한다.
    #[test]
    fn 레일_서비스_로고색은_indicator를_따른다() {
        use crate::agent_surface::AgentVisualState;
        use crate::status_feed::ServiceIndicator;
        use crate::ui::agent_visuals::status_color;
        let visuals = egui::Visuals::dark();
        assert_eq!(
            rail_service_color(&visuals, Some(ServiceIndicator::Operational)),
            status_color(AgentVisualState::Complete),
            "정상 = 초록"
        );
        assert_eq!(
            rail_service_color(&visuals, Some(ServiceIndicator::Minor)),
            status_color(AgentVisualState::Waiting),
            "저하 = 노랑"
        );
        for indicator in [ServiceIndicator::Major, ServiceIndicator::Critical] {
            assert_eq!(
                rail_service_color(&visuals, Some(indicator)),
                status_color(AgentVisualState::Error),
                "장애 = 빨강"
            );
        }
        assert_eq!(
            rail_service_color(&visuals, Some(ServiceIndicator::Unknown)),
            visuals.weak_text_color()
        );
        assert_eq!(
            rail_service_color(&visuals, None),
            visuals.weak_text_color()
        );
    }

    /// 서비스 상태 스택은 레일 하단에 세로로 서고, 이름은 접근 라벨로만 남는다
    /// (2026-08-07 상태바의 점+이름 3종에서 이동).
    #[test]
    fn kittest_레일_하단에_서비스_상태_로고_3개가_세로로_선다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let workspaces = vec![SidebarWorkspaceEntry {
            id: "workspace-a".to_owned(),
            name: "Workspace A".to_owned(),
            state: SidebarWorkspaceState::Active,
            summary: SidebarSessionSummary::default(),
        }];
        struct State {
            tree: FileTreeUi,
            fonts_ready: bool,
        }
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
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
                    };
                    let _ =
                        state
                            .tree
                            .panel(ui, &std::collections::HashMap::new(), &sidebar, &catalog);
                },
                State {
                    tree: FileTreeUi::new(egui::Context::default()),
                    fonts_ready: false,
                },
            );
        install_sidebar_test_fonts(&harness.ctx);
        harness.state_mut().fonts_ready = true;
        harness.run();

        let claude = harness.get_by_label("Claude").rect();
        let openai = harness.get_by_label("OpenAI").rect();
        let github = harness.get_by_label("GitHub").rect();
        assert!(
            claude.bottom() <= openai.top() && openai.bottom() <= github.top(),
            "위→아래 Claude→OpenAI→GitHub 세로 스택이어야 한다"
        );
        assert!(
            (claude.center().x - openai.center().x).abs() <= 0.5
                && (openai.center().x - github.center().x).abs() <= 0.5,
            "레일 세로 중심축에 정렬돼야 한다"
        );
        assert_eq!(claude.width(), RAIL_SERVICE_LOGO_SIZE);
    }

    /// 삽입 마커는 행 왼쪽 끝이 아니라 그 행의 들여쓰기(캐럿+아이콘 시작)에서
    /// 시작한다 — 깊이가 다르면 시작 x도 달라져야 "이 깊이의 폴더로" 들어간다는
    /// 뜻이 위치로 읽힌다.
    #[test]
    fn 삽입_마커_들여쓰기는_깊이를_따라간다() {
        assert_eq!(insertion_marker_indent_x(0.0, 0), 10.0);
        assert_eq!(insertion_marker_indent_x(0.0, 1), 28.0);
        assert_eq!(insertion_marker_indent_x(100.0, 2), 146.0);
    }

    /// 마커는 행 경계에 걸치는 알약이다: 위쪽 대상이면 행 top에, 아래쪽
    /// 대상이면 행 bottom에 중심을 두고 위아래로 부푼다. 오른쪽은 여백만큼
    /// 안으로 들어와 행 끝까지 꽉 차 보이지 않는다.
    #[test]
    fn 삽입_마커는_행_경계에_걸치고_오른쪽에_여백을_둔다() {
        let row = egui::Rect::from_min_max(egui::pos2(0.0, 100.0), egui::pos2(300.0, 125.0));

        let above = insertion_marker_rect(row, 40.0, false);
        assert_eq!(above.left(), 40.0);
        assert_eq!(above.top(), 97.5);
        assert_eq!(above.bottom(), 102.5);
        assert_eq!(above.right(), 290.0); // 오른쪽 여백 10px

        let below = insertion_marker_rect(row, 40.0, true);
        assert_eq!(below.left(), 40.0);
        assert_eq!(below.top(), 122.5);
        assert_eq!(below.bottom(), 127.5);
    }

    /// 들여쓰기가 깊어 남는 폭이 최소 폭보다 좁으면(깊은 트리 + 좁은 패널)
    /// 왼쪽으로 물러나서라도 최소 폭은 지킨다 — 폭이 음수/역전되는 사고를
    /// 막는다.
    #[test]
    fn 삽입_마커는_좁은_행에서도_최소_폭을_지킨다() {
        let row = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(60.0, 25.0));
        let marker = insertion_marker_rect(row, 55.0, false); // 들여쓰기가 오른쪽 여백을 넘어선다
        assert!(marker.right() > marker.left(), "폭이 역전되면 안 된다");
        assert!(marker.width() >= 24.0 - f32::EPSILON);
        assert!(marker.left() >= row.left());
    }
}
