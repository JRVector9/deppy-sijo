//! 폴더 트리 사이드바 (docs/file-tree-design.md).
//!
//! 리소스 3원칙(§3): lazy `read_dir`(펼친 노드만), flat 평탄화 + `show_rows` 가상화,
//! IO는 상호작용 시점만 — 유휴 시 repaint를 유발하지 않는다. 로컬 파일 IO는
//! config/DB처럼 앱 소관이라 `std::fs` 직접 사용(§2, remote는 후속 trait 추상화 지점).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};

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
    /// 저장된 에이전트 세션이 있고 지금 실행 중이 아님 — 컨텍스트 메뉴 '이어가기' 노출.
    pub resumable: bool,
    /// 상태 hover 힌트 — 감지 출처/신뢰도 또는 '수동 지정'(U17b).
    pub status_hint: Option<String>,
    /// 세션 cwd가 감지 캐시에 있음 — 워크트리 메뉴 노출 조건. App이 채운다(PR-W).
    pub has_cwd: bool,
    /// 세션 cwd가 `.deppy/worktrees/` 하위 — 「워크트리 삭제」 메뉴 노출 조건.
    /// App이 채운다(2026-07-18).
    pub in_worktree: bool,
    /// 에이전트 3행: "실행 중 · ctx 69%" (상태 라벨 + 남은 컨텍스트).
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
            // 0은 "등록됐지만 복원 세션도 없는 비활성 워크스페이스" 표식으로 1칸 유지.
            // 단독 표시는 개수를 노출하지 않고 항상 "비활성"이라 사용자에게 과장되지 않는다.
            inactive: count.max(1),
            ..Self::default()
        }
    }
}

pub struct SidebarSnapshot<'a> {
    pub active_workspace_id: &'a str,
    pub workspaces: &'a [SidebarWorkspaceEntry],
    pub view: super::agent_terminal::AgentTerminalView,
    pub inbox_count: usize,
}

/// 사이드바에서 App으로 올라가는 액션.
pub enum SidebarAction {
    SwitchWorkspace(String),
    ShowHome,
    ShowTerminal,
    OpenInbox,
    OpenSettings,
    OpenAgents,
    /// 경로를 포커스된 터미널에 삽입 (FT-3)
    InsertPath(PathBuf),
    /// 포커스된 터미널에서 이 폴더로 cd 실행 (디렉터리 컨텍스트 메뉴, 2026-07-08)
    CdPath(PathBuf),
    /// 파일 행 더블클릭 — OS 연결 프로그램으로 연다 (터미널 「열기」와 동일 판정, 2026-07-18)
    OpenExternal(PathBuf),
    /// 세션 목록에서 선택 — 해당 tab/pane으로 전환
    FocusSession {
        tab: runtime::MuxTabId,
        pane: runtime::MuxPaneId,
    },
    /// 새 셸 생성 (세션 섹션의 + 버튼)
    NewShell,
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

pub struct FileTreeUi {
    /// workspace 루트. None = path 미설정 → 안내 표시(§9-2).
    root: Option<PathBuf>,
    /// 루트 나열 실패 사유 (invalid root — 에러 라벨 + 트리 비활성, §9-2).
    root_error: Option<String>,
    /// 루트 디렉터리의 자식들. 루트 자체는 행으로 그리지 않는다.
    children: Option<Vec<TreeNode>>,
    /// 가시 행 평탄화 캐시 — 펼침/접힘/조작 시에만 재계산(§3).
    flat: Vec<FlatRow>,
    show_hidden: bool,
    file_search_open: bool,
    file_search: String,
    /// 사이드바 접힘 (Panel 폭만 줄인다 — 상태/캐시는 유지).
    collapsed: bool,
    /// 마지막 조작 에러 (하단 빨간 라벨, §4).
    error: Option<String>,
    /// macOS/TCC 등에서 나열 권한이 거부된 디렉터리. 전역 오류로 승격하지 않고
    /// 해당 행만 비활성화해 상위 탐색과 나머지 트리를 계속 사용할 수 있게 한다.
    inaccessible_paths: HashSet<PathBuf>,
    /// 백그라운드 파일 조작(EXDEV copy 등 §9-3)의 완료/에러 채널.
    ops_tx: SyncSender<OpOutcome>,
    ops_rx: Receiver<OpOutcome>,
    /// 백그라운드 디렉터리 listing 결과 채널. read_dir/sort는 worker에서 수행한다.
    listing_tx: SyncSender<ListingOutcome>,
    listing_rx: Receiver<ListingOutcome>,
    /// 프로세스 전역 고정 크기 listing worker pool 입력 큐. FileTreeUi를 반복 생성해도
    /// 요청마다/인스턴스마다 OS thread를 만들지 않는다.
    listing_jobs: SyncSender<ListingJob>,
    /// FileTreeUi drop/root 교체 시 worker가 send/read loop를 중단하게 하는 플래그.
    listing_shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// job 큐 포화 시 다수 요청을 한 번의 root refresh로 축약한다.
    listing_refresh_deferred: bool,
    /// root 전환 generation. 이전 root의 late result는 epoch mismatch로 폐기한다.
    listing_epoch: u64,
    /// 워커와 공유하는 현재 epoch — 워커가 나열 전/청크 전송 중에 확인해 stale 작업을
    /// **송신 전에** 중단한다(구 epoch 청크가 unbounded 채널에 쌓이는 것 방지 —
    /// 안정성 감사 High #2).
    listing_epoch_shared: Arc<std::sync::atomic::AtomicU64>,
    /// per-directory listing token 발급용 monotonic counter.
    next_listing_token: u64,
    /// 현재 유효한 per-directory listing 요청. collapse/refresh/root switch 시 제거한다.
    pending_listings: HashMap<PathBuf, PendingListing>,
    /// 진행 중인 백그라운드 조작 수 (>0이면 스피너 표시).
    in_flight: usize,
    /// 백그라운드 완료 시 UI를 깨우기 위한 컨텍스트.
    egui_ctx: egui::Context,
    /// 인라인 편집 상태 (이름 변경/새 폴더, FT-3).
    edit: Option<EditState>,
    /// 휴지통 이동 실패 → 영구삭제 확인 대기 중인 경로 (§9-7).
    confirm_delete: Option<PathBuf>,
    /// FSEvents 워처 (FT-4). Drop이 감시 스레드를 정리한다 — OFF 토글/workspace
    /// 전환/앱 종료 시 FileTreeUi가 drop되며 함께 정리된다.
    watcher: Option<notify::RecommendedWatcher>,
    /// 현재 감시 중인 디렉터리 집합 = 루트 + 펼친 디렉터리 (각각 **비재귀**). 펼침/접힘에
    /// 맞춰 sync_watches가 delta로 watch/unwatch한다 — 크고 바쁜 루트(홈, node_modules
    /// 있는 프로젝트 등)를 재귀 감시할 때 FSEvents firehose로 앱이 유휴에도 3~5fps로
    /// 영영 안 쉬던 문제를 구조적으로 제거(설계 §3 "펼치는 디렉터리만", 2026-07-04 조사).
    watched_dirs: std::collections::HashSet<PathBuf>,
    /// 워처 이벤트 채널 (워처 스레드 → UI). 콜백에서 기본 ignore/.env 분류를 끝내고
    /// UI 스레드는 dedup+debounce된 dirty dir만 재나열한다.
    watch_rx: Option<Receiver<WatchEvent>>,
    /// watcher 채널 포화 — 개별 이벤트를 더 쌓지 않고 root refresh 한 건으로 축약.
    watch_overflowed: Arc<std::sync::atomic::AtomicBool>,
    /// 워처가 무시할 경로 prefix들 — 앱 자신의 data/log 디렉터리 등. 자기 로그 쓰기가
    /// 이벤트로 돌아와 리페인트를 유발하는 자기-루프 차단 (리페인트 원인 조사 2026-07-04).
    watch_ignore: std::sync::Arc<Vec<PathBuf>>,
    /// `.gitignore`/`.git/info/exclude`/global excludes matcher. listing worker와 watcher가
    /// 공유하고, 디렉터리를 펼칠 때만 ancestor ignore 파일을 lazy 로드한다.
    ignore_cache: GitIgnoreCache,
    /// 콜백 스레드와 공유하는 show_hidden — 숨김 경로 이벤트는 트리에 보이지도 않으므로
    /// 무시한다 (홈 디렉터리 루트에서 ~/Library 등 잡음 이벤트 대량 차단).
    watch_show_hidden: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 실측 행높이 (show_rows 자기보정). show_rows는 "모든 행 = 선언 높이" 계약인데
    /// 실제 행높이는 폰트 메트릭(한글 폰트 라인높이 등)에 따라 선언값과 어긋날 수 있고,
    /// 어긋나면 스크롤 위치·가시 범위가 리빌드마다 밀려 클릭이 다른 행에 떨어진다
    /// (2026-07-05 사용자 보고: 펼침 간헐 실패/재클릭 접힘 안 됨/위치 점프). 첫 프레임에
    /// 실제 그린 행높이를 재서 다음 프레임부터 그 값을 쓴다.
    measured_row_height: Option<f32>,
    /// 스로틀 창 안에 도착해 아직 재나열하지 않은 디렉터리 (dedup 집합 — codex Med-1).
    pending_watch: BTreeSet<PathBuf>,
    /// `.env*` 파일 변경 후보. 숨김 파일 필터와 무관하게 기록해 env-warning 후보로 쓸 수 있다.
    env_warning_candidates: BTreeSet<PathBuf>,
    /// 마지막 워처 일괄 재나열 시각 — WATCH_RELOAD_MS 미만이면 흡수만 하고 건너뛴다.
    last_watch_reload: std::time::Instant,
    /// 세션 목록 이름 인라인 편집 중 (pane, 편집 버퍼). 우클릭/더블클릭으로 시작.
    session_name_edit: Option<(runtime::MuxPaneId, String)>,
    /// 활성 워크스페이스의 세션 트리 접힘 상태. 접혀도 요약 수치는 워크스페이스 행에 남긴다.
    workspace_sessions_expanded: bool,
}

/// 백그라운드 파일 조작 결과 — 완료 후 재나열할 부모 디렉터리 + 에러(있으면).
struct OpOutcome {
    refresh: Vec<PathBuf>,
    error: Option<String>,
    /// 휴지통 이동 실패 시 영구삭제 확인을 띄울 경로 (§9-7 폴백).
    confirm_delete: Option<PathBuf>,
}

/// 디렉터리 listing worker 결과. 큰 디렉터리 apply 비용도 쪼개기 위해 chunk로 전달한다.
struct ListingOutcome {
    epoch: u64,
    token: u64,
    path: PathBuf,
    result: ListingResult,
}

struct ListingJob {
    tx: SyncSender<ListingOutcome>,
    ctx: egui::Context,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    epoch: ListingEpochGuard,
    token: u64,
    root: Option<PathBuf>,
    path: PathBuf,
    ignore_cache: GitIgnoreCache,
}

enum ListingResult {
    Chunk {
        nodes: Vec<TreeNode>,
        done: bool,
    },
    Error {
        message: String,
        kind: std::io::ErrorKind,
    },
}

struct PendingListing {
    token: u64,
    /// refresh/reload 시작 시점의 펼침 상태. 결과 적용 직전의 현재 상태가 없을 때 fallback.
    preserve_expanded: Arc<HashSet<PathBuf>>,
    /// 첫 chunk 적용 시점의 현재 펼침 상태. 이후 chunk는 같은 기준으로 append한다.
    apply_expanded: Option<Arc<HashSet<PathBuf>>>,
    started: bool,
    /// 워커와 공유하는 취소 플래그 — 같은 경로 재요청(reload_dir token 교체)이나 부분
    /// 무효화 시 구 워커가 청크를 **송신하기 전에** 멈추게 한다(codex High 2026-07-08).
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WatchEvent {
    DirtyDir(PathBuf),
    EnvFileChanged(PathBuf),
}

#[derive(Clone)]
struct GitIgnoreCache {
    inner: Arc<Mutex<GitIgnoreCacheInner>>,
}

#[derive(Default)]
struct GitIgnoreCacheInner {
    root: Option<PathBuf>,
    base_rules: Vec<IgnoreRule>,
    dir_rules: HashMap<PathBuf, Vec<IgnoreRule>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IgnoreRule {
    base: PathBuf,
    pattern: String,
    negated: bool,
    directory_only: bool,
    anchored: bool,
    has_slash: bool,
}

impl Default for GitIgnoreCache {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(GitIgnoreCacheInner::default())),
        }
    }
}

impl GitIgnoreCache {
    fn reset(&self, root: Option<&Path>) {
        let mut inner = self.inner.lock().expect("gitignore cache lock");
        inner.root = root.map(Path::to_path_buf);
        inner.base_rules.clear();
        inner.dir_rules.clear();
        if let Some(root) = root {
            inner.base_rules.extend(load_global_ignore_rules(root));
            inner
                .base_rules
                .extend(load_ignore_file(&root.join(".gitignore"), root));
            inner
                .base_rules
                .extend(load_ignore_file(&root.join(".git/info/exclude"), root));
        }
    }

    fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let mut inner = self.inner.lock().expect("gitignore cache lock");
        let Some(root) = inner.root.clone() else {
            return false;
        };
        if path == root {
            return false;
        }
        let Ok(rel) = path.strip_prefix(&root) else {
            return false;
        };
        if rel.as_os_str().is_empty() {
            return false;
        }
        let dir = path.parent().unwrap_or(&root);
        let rules = inner.rules_for_dir(&root, dir);
        let mut ignored = false;
        for rule in &rules {
            if rule.matches(path, is_dir) {
                ignored = !rule.negated;
            }
        }
        ignored
    }
}

impl GitIgnoreCacheInner {
    /// 디렉터리별 캐시 항목 상한 — 초과 시 통째로 비운다(다음 조회가 lazy 재구축).
    /// reset()은 워크스페이스 전환에서만 불리므로, 한 워크스페이스 안에서 대형
    /// 모노레포를 오래 탐색하면 방문 디렉터리 수만큼 무한히 자라는 것을 막는다.
    const DIR_RULES_CAP: usize = 4096;

    fn rules_for_dir(&mut self, root: &Path, dir: &Path) -> Vec<IgnoreRule> {
        let dir = if dir.starts_with(root) { dir } else { root };
        if let Some(rules) = self.dir_rules.get(dir) {
            return rules.clone();
        }

        let mut rules = self.base_rules.clone();
        if let Ok(rel) = dir.strip_prefix(root) {
            let mut current = root.to_path_buf();
            for component in rel.components() {
                let std::path::Component::Normal(name) = component else {
                    continue;
                };
                current.push(name);
                rules.extend(load_ignore_file(&current.join(".gitignore"), &current));
            }
        }
        if self.dir_rules.len() >= Self::DIR_RULES_CAP {
            self.dir_rules.clear();
        }
        self.dir_rules.insert(dir.to_path_buf(), rules.clone());
        rules
    }
}

impl IgnoreRule {
    fn parse(base: &Path, raw: &str) -> Option<Self> {
        let mut pattern = raw.trim();
        if pattern.is_empty() {
            return None;
        }
        if let Some(rest) = pattern.strip_prefix("\\#") {
            pattern = rest;
        } else if pattern.starts_with('#') {
            return None;
        }

        let negated = if let Some(rest) = pattern.strip_prefix("\\!") {
            pattern = rest;
            false
        } else if let Some(rest) = pattern.strip_prefix('!') {
            pattern = rest.trim_start();
            true
        } else {
            false
        };
        if pattern.is_empty() {
            return None;
        }

        let directory_only = pattern.ends_with('/');
        pattern = pattern.trim_end_matches('/');
        let anchored = pattern.starts_with('/');
        pattern = pattern.trim_start_matches('/');
        if pattern.is_empty() {
            return None;
        }
        let pattern = pattern.replace("\\#", "#").replace("\\!", "!");
        let has_slash = pattern.contains('/');
        Some(Self {
            base: base.to_path_buf(),
            pattern,
            negated,
            directory_only,
            anchored,
            has_slash,
        })
    }

    fn matches(&self, path: &Path, is_dir: bool) -> bool {
        let Ok(rel) = path.strip_prefix(&self.base) else {
            return false;
        };
        let components = path_components(rel);
        if components.is_empty() {
            return false;
        }

        if self.has_slash || self.anchored {
            let rel_text = components.join("/");
            if self.directory_only {
                return path_prefixes(&components, is_dir)
                    .iter()
                    .any(|prefix| glob_match(&self.pattern, prefix));
            }
            return glob_match(&self.pattern, &rel_text)
                || path_prefixes(&components, is_dir)
                    .iter()
                    .any(|prefix| glob_match(&self.pattern, prefix));
        }

        let check_components: &[String] = if self.directory_only && !is_dir {
            components
                .get(..components.len().saturating_sub(1))
                .unwrap_or(&[])
        } else {
            &components
        };
        check_components
            .iter()
            .any(|component| glob_match(&self.pattern, component))
    }
}

fn load_global_ignore_rules(root: &Path) -> Vec<IgnoreRule> {
    global_ignore_files()
        .into_iter()
        .flat_map(|path| load_ignore_file(&path, root))
        .collect()
}

fn global_ignore_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(home) = crate::paths::home_dir() {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
            files.push(xdg.join("git/ignore"));
        } else {
            files.push(home.join(".config/git/ignore"));
        }
        files.push(home.join(".gitignore_global"));
    }
    files
}

fn load_ignore_file(path: &Path, base: &Path) -> Vec<IgnoreRule> {
    let Ok(source) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    source
        .lines()
        .filter_map(|line| IgnoreRule::parse(base, line))
        .collect()
}

fn path_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

fn path_prefixes(components: &[String], is_dir: bool) -> Vec<String> {
    let limit = if is_dir {
        components.len()
    } else {
        components.len().saturating_sub(1)
    };
    (1..=limit).map(|end| components[..end].join("/")).collect()
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let mut dp = vec![vec![false; text.len() + 1]; pattern.len() + 1];
    dp[0][0] = true;
    for i in 1..=pattern.len() {
        if pattern[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=pattern.len() {
        for j in 1..=text.len() {
            dp[i][j] = match pattern[i - 1] {
                '*' => dp[i - 1][j] || dp[i][j - 1],
                '?' => dp[i - 1][j - 1],
                ch => ch == text[j - 1] && dp[i - 1][j - 1],
            };
        }
    }
    dp[pattern.len()][text.len()]
}

/// 인라인 편집 (FT-3). focus는 첫 프레임에 TextEdit에 포커스를 1회 요청하는 플래그 —
/// 편집 중 키 입력이 터미널로 새지 않게 한다(§9-8: 터미널은 자기 response가
/// 포커스를 가질 때만 입력을 소비한다).
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
    pub fn new(egui_ctx: egui::Context) -> Self {
        let (ops_tx, ops_rx) = sync_channel(FILE_OP_RESULT_QUEUE_CAP);
        let (listing_tx, listing_rx) = sync_channel(LISTING_RESULT_QUEUE_CAP);
        let listing_jobs = global_listing_pool().clone();
        let listing_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Self {
            root: None,
            root_error: None,
            children: None,
            flat: Vec::new(),
            show_hidden: false,
            file_search_open: false,
            file_search: String::new(),
            collapsed: false,
            error: None,
            inaccessible_paths: HashSet::new(),
            ops_tx,
            ops_rx,
            listing_tx,
            listing_rx,
            listing_jobs,
            listing_shutdown,
            listing_refresh_deferred: false,
            listing_epoch: 0,
            listing_epoch_shared: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            next_listing_token: 0,
            pending_listings: HashMap::new(),
            in_flight: 0,
            egui_ctx,
            edit: None,
            confirm_delete: None,
            watcher: None,
            watched_dirs: std::collections::HashSet::new(),
            watch_rx: None,
            watch_overflowed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            measured_row_height: None,
            watch_ignore: std::sync::Arc::new(Vec::new()),
            ignore_cache: GitIgnoreCache::default(),
            watch_show_hidden: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_watch: BTreeSet::new(),
            env_warning_candidates: BTreeSet::new(),
            last_watch_reload: std::time::Instant::now(),
            session_name_edit: None,
            workspace_sessions_expanded: true,
        }
    }

    /// 루트 교체 (workspace 전환/경로 변경). 캐시를 버리고 루트만 다시 나열한다.
    /// 루트는 canonicalize해 보관한다 — 트리의 모든 행 경로가 canonical 기준이 되어
    /// 이동 가드(§9-4)·부분 재나열의 경로 비교가 일관된다.
    /// 워처 무시 prefix 설정 (앱 data dir 등). set_root 이전에 호출.
    pub fn set_watch_ignore(&mut self, prefixes: Vec<PathBuf>) {
        self.watch_ignore = std::sync::Arc::new(prefixes);
    }

    /// 워처가 감지한 `.env*` 변경 후보를 꺼낸다. App이 매 프레임 소비해 .env 변경/삭제
    /// 시 dotenv 재동기화를 트리거한다(2026-07-08 — stale secret 주입 방지, codex High).
    pub fn take_env_warning_candidates(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.env_warning_candidates)
            .into_iter()
            .collect()
    }

    pub fn set_root(&mut self, root: Option<PathBuf>) {
        self.listing_epoch = self.listing_epoch.wrapping_add(1);
        self.listing_epoch_shared
            .store(self.listing_epoch, std::sync::atomic::Ordering::Relaxed);
        self.pending_listings.clear();
        self.root = root.map(|r| r.canonicalize().unwrap_or(r));
        self.root_error = None;
        self.children = None;
        self.flat.clear();
        self.file_search.clear();
        self.file_search_open = false;
        self.error = None;
        self.inaccessible_paths.clear();
        self.edit = None;
        self.confirm_delete = None;
        self.pending_watch.clear();
        self.env_warning_candidates.clear();
        self.ignore_cache.reset(self.root.as_deref());
        self.refresh();
        // 루트가 유효할 때만 감시 시작 (FT-4). 실패는 경고 로그 — 수동 새로고침으로 동작.
        self.start_watcher();
    }

    /// 워처 일괄 재나열 최소 간격(ms) — 이벤트·프레임이 동시에 폭주해도 재나열은 ~3.3Hz.
    const WATCH_RELOAD_MS: u64 = 300;
    /// 한 debounce window에서 reload_dir을 요청할 최대 dirty directory 수. 대량 이벤트가
    /// 여러 펼친 디렉터리에 흩어져도 한 프레임에 listing worker를 과도하게 만들지 않는다.
    const WATCH_RELOAD_DIRS_PER_BATCH: usize = 8;

    /// 감시자 생성 (FT-4 — FSEvents/notify, 스레드 1개). 실제 감시 대상 디렉터리는
    /// sync_watches가 루트+펼친 디렉터리로 **비재귀** 등록한다. 이벤트 도착 시 해당 부모
    /// 디렉터리만 채널로 보내고 ~300ms 디바운스로 repaint를 예약한다(폭주 시 일괄 처리).
    /// idle에는 이벤트가 없어 repaint를 유발하지 않는다 (리소스 계약).
    fn start_watcher(&mut self) {
        #[cfg(not(test))]
        if let Some(watcher) = self.watcher.take() {
            retire_watcher(watcher);
        }
        #[cfg(test)]
        {
            self.watcher = None;
        }
        self.watch_rx = None;
        self.watched_dirs.clear();
        // 단위 테스트는 아래 watcher 변환/스로틀 로직에 채널을 직접 주입한다. macOS
        // FSEvents backend를 실제로 만들면 Drop이 OS latency만큼(실측 60s+) 기다려 테스트가
        // 느려지므로 platform watcher 생성만 제외한다.
        #[cfg(test)]
        return;
        #[cfg(not(test))]
        self.start_platform_watcher();
    }

    #[cfg(not(test))]
    fn start_platform_watcher(&mut self) {
        let Some(root) = self.root.clone() else {
            return;
        };
        if self.root_error.is_some() {
            return;
        }
        if !try_acquire_watcher_slot() {
            tracing::warn!("파일 감시자 정리 대기 상한 — 수동 새로고침으로 동작");
            return;
        }
        self.watch_overflowed
            .store(false, std::sync::atomic::Ordering::Release);
        let (tx, rx) = sync_channel::<WatchEvent>(WATCH_EVENT_QUEUE_CAP);
        let ctx = self.egui_ctx.clone();
        let ignore = std::sync::Arc::clone(&self.watch_ignore);
        let ignore_cache = self.ignore_cache.clone();
        let show_hidden = std::sync::Arc::clone(&self.watch_show_hidden);
        let watch_root = root.clone();
        let overflowed = Arc::clone(&self.watch_overflowed);
        let handler = move |res: Result<notify::Event, notify::Error>| match res {
            Ok(event) => {
                if !relevant_fs_event(&event.kind) {
                    return;
                }
                let show_hidden_now = show_hidden.load(std::sync::atomic::Ordering::Relaxed);
                let mut sent = false;
                for path in &event.paths {
                    for event in watch_events_for_path_with_ignore(
                        &watch_root,
                        path,
                        show_hidden_now,
                        ignore.as_ref(),
                        &ignore_cache,
                    ) {
                        match tx.try_send(event) {
                            Ok(()) => sent = true,
                            Err(TrySendError::Full(_)) => {
                                // 개별 경로를 계속 쌓지 않고 UI가 root refresh 한 건으로
                                // 복구하게 한다. 플래그는 coalesced라 burst 크기와 무관하게 유계.
                                overflowed.store(true, std::sync::atomic::Ordering::Release);
                                sent = true;
                            }
                            Err(TrySendError::Disconnected(_)) => return,
                        }
                    }
                }
                if !sent {
                    return; // 전부 걸러졌으면 리페인트도 깨우지 않는다 (유휴 유지)
                }
                // 디바운스 ~300ms: request_repaint_after는 가장 이른 예약만 유지되므로
                // 이벤트 폭주 중에도 UI는 최대 ~3Hz로 일괄 재나열한다.
                ctx.request_repaint_after(std::time::Duration::from_millis(300));
            }
            Err(e) => tracing::warn!("파일 감시 이벤트 오류: {e}"),
        };
        match notify::recommended_watcher(handler) {
            Ok(watcher) => {
                self.watcher = Some(watcher);
                self.watch_rx = Some(rx);
                self.sync_watches(); // 루트(+현재 펼침) 비재귀 등록
            }
            Err(e) => {
                release_watcher_slot();
                tracing::warn!("파일 감시자 생성 실패 (수동 새로고침으로 동작): {e}");
            }
        }
    }

    /// 감시 대상을 현재 트리 상태(루트 + 펼친 디렉터리)와 동기화한다 — 각 디렉터리를
    /// **비재귀**로 watch/unwatch(delta만). flat이 바뀔 때(펼침/접힘/재나열)마다 호출한다.
    /// 재귀 감시를 피해 크고 바쁜 서브트리(예: 홈의 ~/Library)의 이벤트 firehose를 차단한다.
    fn sync_watches(&mut self) {
        use notify::Watcher as _;
        let Some(root) = self.root.clone() else {
            return;
        };
        // desired = 루트 + 그 직속 항목이 화면에 보이는(펼친) 디렉터리들.
        let mut desired: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        desired.insert(root.clone());
        for row in &self.flat {
            if row.is_dir
                && row.expanded
                && !has_default_watch_ignore_component(&root, &row.path)
                && !self.ignore_cache.is_ignored(&row.path, true)
            {
                desired.insert(row.path.clone());
            }
        }
        if desired == self.watched_dirs {
            return; // 변화 없음 — watch/unwatch 호출 자체를 생략(유휴 무비용)
        }
        let Some(watcher) = self.watcher.as_mut() else {
            return; // 감시자 미생성 상태(refresh 중) — start_watcher가 이후 동기화한다
        };
        for dir in desired.difference(&self.watched_dirs) {
            if let Err(e) = watcher.watch(dir, notify::RecursiveMode::NonRecursive) {
                tracing::warn!("파일 감시 추가 실패 {}: {e}", dir.display());
            }
        }
        for dir in self.watched_dirs.difference(&desired) {
            let _ = watcher.unwatch(dir); // 접힌 디렉터리 — 실패는 무시(이미 사라졌을 수 있음)
        }
        self.watched_dirs = desired;
    }

    /// 워처 이벤트 수거 + 시간 스로틀 재나열 (FT-4, codex Med-1). 채널은 매 프레임
    /// 프레임 예산만큼 비워 pending 집합에 흡수하고, 실제 재나열(reread 재귀)은
    /// 마지막 일괄 후 WATCH_RELOAD_MS 경과 시에만 수행한다 — 터미널 출력으로 프레임이
    /// 계속 돌면서 파일 이벤트가 쏟아져도 재나열은 최대 ~3.3Hz.
    fn pump_watch_events(&mut self, ctx: &egui::Context) {
        if self
            .watch_overflowed
            .swap(false, std::sync::atomic::Ordering::AcqRel)
            && let Some(root) = &self.root
        {
            // 포화 burst는 root 한 건으로 축약한다.
            insert_pending_watch_dir(&mut self.pending_watch, root.clone());
        }
        let mut drained = 0usize;
        if let Some(rx) = &self.watch_rx {
            while drained < WATCH_EVENTS_PER_FRAME {
                let Ok(event) = rx.try_recv() else {
                    break;
                };
                drained += 1;
                match event {
                    WatchEvent::DirtyDir(dir) => {
                        insert_pending_watch_dir(&mut self.pending_watch, dir)
                    }
                    WatchEvent::EnvFileChanged(path) => {
                        self.env_warning_candidates.insert(path);
                    }
                }
            }
        }
        if drained == WATCH_EVENTS_PER_FRAME {
            ctx.request_repaint();
        }
        if self.pending_watch.is_empty() {
            return;
        }
        let window = std::time::Duration::from_millis(Self::WATCH_RELOAD_MS);
        let elapsed = self.last_watch_reload.elapsed();
        if elapsed < window {
            // 창 안 — 처리를 미룬다. 워처가 예약한 repaint가 이 프레임에 이미 소비됐을 수
            // 있으므로 남은 창만큼 뒤 프레임을 직접 예약해 pending이 방치되지 않게 한다.
            ctx.request_repaint_after(window - elapsed);
            return;
        }
        self.last_watch_reload = std::time::Instant::now();
        let dirty =
            take_pending_watch_batch(&mut self.pending_watch, Self::WATCH_RELOAD_DIRS_PER_BATCH);
        for dir in dirty {
            self.reload_dir(&dir);
        }
        if !self.pending_watch.is_empty() {
            ctx.request_repaint_after(window);
        }
    }

    /// 백그라운드 조작 완료 수거 (§9-3 — 완료/에러를 채널로 받아 부모만 재나열).
    fn pump_ops(&mut self) {
        while let Ok(outcome) = self.ops_rx.try_recv() {
            self.in_flight = self.in_flight.saturating_sub(1);
            if let Some(e) = outcome.error {
                self.error = Some(e);
            }
            if let Some(path) = outcome.confirm_delete {
                self.confirm_delete = Some(path); // 휴지통 실패 → 영구삭제 확인 (§9-7)
            }
            for dir in &outcome.refresh {
                self.reload_dir(dir);
            }
        }
    }

    /// 펼친 노드 전체를 재나열한다 (수동 새로고침 — 펼침 상태는 이월).
    fn refresh(&mut self) {
        let Some(root) = self.root.clone() else {
            return;
        };
        self.root_error = None;
        // 같은 root 재나열도 전체 교체다 — epoch을 올려 진행 중이던 구 워커가
        // 청크를 **송신하기 전에** 중단되게 한다(안 올리면 token mismatch로 수신측에서만
        // 버려져 unbounded 채널에 stale 청크가 쌓인다 — codex High 2026-07-08).
        self.listing_epoch = self.listing_epoch.wrapping_add(1);
        self.listing_epoch_shared
            .store(self.listing_epoch, std::sync::atomic::Ordering::Relaxed);
        let preserve_expanded = Arc::new(self.collect_expanded_paths());
        self.invalidate_listing_subtree(&root);
        self.request_listing(root, preserve_expanded);
    }

    /// flat 캐시 재계산 (펼침/접힘/숨김 토글/조작 후에만 호출).
    fn rebuild_flat(&mut self) {
        self.flat.clear();
        if let (Some(root), Some(children)) = (&self.root, &self.children) {
            flatten(children, root, 0, self.show_hidden, &mut self.flat);
        }
        // 펼침/접힘/재나열로 가시 트리가 바뀌었으니 감시 대상도 맞춘다(delta, 비재귀).
        self.sync_watches();
    }

    /// 디렉터리 행 클릭: 펼침 ↔ 접힘. 펼칠 때만 read_dir(lazy), 접으면 캐시 해제.
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
            self.invalidate_listing_subtree(path);
        }
        if expand {
            self.request_listing(path.to_path_buf(), Arc::new(HashSet::new()));
        }
        self.rebuild_flat();
    }

    /// 좌측 사이드바 렌더 (§6 — `egui::Panel::left`, CentralPanel 앞에서 호출할 것 §9-1).
    /// 반환: "터미널에 경로 삽입" 요청 경로 (호출측 App이 WriteInput으로 전달 — §6
    /// 유일한 runtime 접점을 App에 남긴다).
    pub fn panel(
        &mut self,
        ui: &mut egui::Ui,
        sessions: &[SessionEntry],
        sidebar: &SidebarSnapshot<'_>,
        catalog: &i18n::Catalog,
    ) -> Option<SidebarAction> {
        // 접힘 여부와 무관하게 배경 채널을 소비한다 (codex Med-2 — 접힌 채로 워처/조작
        // 채널이 무한 누적되거나 op 완료(in_flight/에러/영구삭제 확인)가 방치되는 것 방지).
        self.pump_listings();
        self.pump_watch_events(ui.ctx());
        self.pump_ops();
        if self.collapsed {
            egui::Panel::left("file_tree_panel_collapsed")
                .resizable(false)
                .exact_size(22.0)
                .show(ui, |ui| {
                    if ui
                        .small_button("▸")
                        .on_hover_text(catalog.t("file_tree.expand_sidebar", &[]))
                        .clicked()
                    {
                        self.collapsed = false;
                    }
                });
            return None;
        }
        egui::Panel::left("file_tree_panel")
            .resizable(true)
            .default_size(360.0)
            // 아이콘 레일까지 축소할 수 있도록 최소 폭을 40pt로 둔다. 내부 행은 폭에
            // 따라 제목/요약/도구를 단계적으로 생략해 콘텐츠가 패널을 다시 밀지 않는다.
            .size_range(egui::Rangef::new(40.0, 680.0))
            .show(ui, |ui| {
                let nav_h = 96.0;
                let body_h = (ui.available_height() - nav_h).max(180.0);
                let body = ui
                    .allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), body_h),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| self.contents(ui, sessions, sidebar, catalog),
                    )
                    .inner;
                crate::ui::hairline_full(ui);
                let navigation = self.navigation(ui, sidebar);
                body.or(navigation)
            })
            .inner
    }

    fn contents(
        &mut self,
        ui: &mut egui::Ui,
        sessions: &[SessionEntry],
        sidebar: &SidebarSnapshot<'_>,
        catalog: &i18n::Catalog,
    ) -> Option<SidebarAction> {
        // (워처/백그라운드 채널 수거는 panel()이 접힘 여부와 무관하게 이미 수행했다)
        let mut action: Option<SidebarAction> = None;

        // ── 통합 워크스페이스·세션 계층 ──
        ui.add_space(3.0);
        let compact_sidebar = ui.available_width() < 120.0;
        ui.horizontal(|ui| {
            if !compact_sidebar {
                ui.label(
                    egui::RichText::new("워크스페이스 & 세션")
                        .strong()
                        .size(14.0),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("+")
                    .on_hover_text(catalog.t("workspace.new_shell", &[]))
                    .clicked()
                {
                    action = Some(SidebarAction::NewShell);
                }
            });
        });
        ui.add_space(4.0);
        // DB list_workspaces가 보장하는 created_at 순서를 그대로 그린다. 이전 구현은
        // 활성 workspace를 먼저 뽑아 맨 위에 렌더해 선택할 때마다 행이 이동했다.
        let (before_active, active, after_active) =
            workspace_creation_order_partition(sidebar.workspaces, sidebar.active_workspace_id);
        for workspace in before_active {
            if workspace_row(ui, workspace, false, Some(false)).clicked() {
                action = Some(SidebarAction::SwitchWorkspace(workspace.id.clone()));
            }
        }
        if let Some(active) = active
            && workspace_row(ui, active, true, Some(self.workspace_sessions_expanded)).clicked()
        {
            self.workspace_sessions_expanded = !self.workspace_sessions_expanded;
        }

        // 현재 workspace의 셸/에이전트를 활성 워크스페이스 아래에 들여써 나열한다.
        if self.workspace_sessions_expanded && !sessions.is_empty() {
            // 세션이 많으면 목록이 패널을 다 먹고 아래로 넘쳐 잘렸다 (2026-07-05 사용자
            // 보고). 세션 목록은 패널 높이의 절반까지만 쓰고 그 안에서 스크롤, 나머지는
            // 아래 파일 트리가 갖는다. auto_shrink[_, true]로 세션이 적으면 줄어든다.
            let session_max_h = (ui.available_height() * 0.34).clamp(70.0, 230.0);
            egui::ScrollArea::vertical()
                .id_salt("session_list_scroll")
                .max_height(session_max_h)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add_space(2.0);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for entry in sessions {
                        ui.horizontal(|ui| {
                            ui.add_space(20.0);
                            ui.vertical(|ui| {
                                let editing = matches!(
                                    &self.session_name_edit,
                                    Some((p, _)) if *p == entry.pane
                                );
                                if editing {
                                    // 인라인 이름 편집 — Enter 확정(RenameSession), Esc 취소.
                                    // 행(레일/상태줄) 레이아웃은 유지하고 제목 자리만 편집기로.
                                    let buf = &mut self.session_name_edit.as_mut().unwrap().1;
                                    session_row_editing(ui, entry, buf);
                                    let (enter, esc) = ui.input(|i| {
                                        (
                                            i.key_pressed(egui::Key::Enter),
                                            i.key_pressed(egui::Key::Escape),
                                        )
                                    });
                                    if enter {
                                        if let Some((pane, title)) = self.session_name_edit.take() {
                                            let title = title.trim().to_owned();
                                            if !title.is_empty() {
                                                action = Some(SidebarAction::RenameSession {
                                                    pane,
                                                    title,
                                                });
                                            }
                                        }
                                    } else if esc {
                                        self.session_name_edit = None;
                                    }
                                } else {
                                    // hover 힌트: 상태 감지 출처/신뢰도(U17b)가 있으면 함께.
                                    let hover = match &entry.status_hint {
                                        Some(hint) => {
                                            format!(
                                                "{}\n{}",
                                                hint,
                                                catalog.t("workspace.rename_hint", &[])
                                            )
                                        }
                                        None => catalog.t("workspace.rename_hint", &[]),
                                    };
                                    let resp = session_row(ui, entry).on_hover_text(hover);
                                    // 우클릭 → 컨텍스트 메뉴(이름 변경/폴더/새 셸/이어가기/닫기).
                                    // 더블클릭 → 이름 편집. 단순 클릭 → 세션 전환.
                                    // (수동 상태 지정 U17b는 hook 감지 정착으로 제거 — 2026-07-17 사용자.)
                                    if let Some(session) = entry.session {
                                        resp.context_menu(|ui| {
                                            if ui
                                                .button(catalog.t("workspace.rename_menu", &[]))
                                                .clicked()
                                            {
                                                self.session_name_edit =
                                                    Some((entry.pane.clone(), entry.title.clone()));
                                                ui.close();
                                            }
                                            ui.separator();
                                            if ui
                                                .button(catalog.t("sidebar.menu.open_folder", &[]))
                                                .clicked()
                                            {
                                                action = Some(SidebarAction::OpenSessionFolder {
                                                    session,
                                                });
                                                ui.close();
                                            }
                                            if ui
                                                .button(catalog.t("sidebar.menu.copy_path", &[]))
                                                .clicked()
                                            {
                                                action = Some(SidebarAction::CopySessionPath {
                                                    session,
                                                });
                                                ui.close();
                                            }
                                            if ui
                                                .button(
                                                    catalog.t("sidebar.menu.new_shell_here", &[]),
                                                )
                                                .clicked()
                                            {
                                                action = Some(SidebarAction::NewShellSameFolder {
                                                    session,
                                                });
                                                ui.close();
                                            }
                                            // 변경 보기 — 세션 cwd 레포의 git diff 패널 (PR-D).
                                            if ui
                                                .button(catalog.t("sidebar.menu.show_diff", &[]))
                                                .clicked()
                                            {
                                                action = Some(SidebarAction::ShowDiff { session });
                                                ui.close();
                                            }
                                            // 새 워크트리에서 셸 — cwd를 아는 세션만 (레포 판정은
                                            // dispatch의 백그라운드 repo_root가 한다, PR-W).
                                            if entry.has_cwd
                                                && ui
                                                    .button(
                                                        catalog.t(
                                                            "sidebar.menu.new_worktree_cell",
                                                            &[],
                                                        ),
                                                    )
                                                    .clicked()
                                            {
                                                action = Some(SidebarAction::NewWorktreeCell {
                                                    session,
                                                });
                                                ui.close();
                                            }
                                            // 워크트리 삭제 — 이 세션 cwd가 `.deppy/worktrees/`
                                            // 하위일 때만 노출(2026-07-18 사용자 제안).
                                            if entry.in_worktree
                                                && ui
                                                    .button(
                                                        catalog
                                                            .t("sidebar.menu.remove_worktree", &[]),
                                                    )
                                                    .clicked()
                                            {
                                                action =
                                                    Some(SidebarAction::RemoveWorktree { session });
                                                ui.close();
                                            }
                                            if entry.resumable
                                                && ui
                                                    .button(
                                                        catalog.t("sidebar.menu.resume_agent", &[]),
                                                    )
                                                    .clicked()
                                            {
                                                action = Some(SidebarAction::ResumeAgent {
                                                    pane: entry.pane.clone(),
                                                    session,
                                                    title: entry.title.clone(),
                                                });
                                                ui.close();
                                            }
                                            ui.separator();
                                            if ui
                                                .button(catalog.t("sidebar.menu.close_pane", &[]))
                                                .clicked()
                                            {
                                                action = Some(SidebarAction::ClosePane {
                                                    pane: entry.pane.clone(),
                                                });
                                                ui.close();
                                            }
                                        });
                                    }
                                    if resp.double_clicked() {
                                        self.session_name_edit =
                                            Some((entry.pane.clone(), entry.title.clone()));
                                    } else if resp.clicked() && !entry.focused {
                                        action = Some(SidebarAction::FocusSession {
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

        egui::ScrollArea::vertical()
            .id_salt("workspace_list_scroll")
            .max_height((ui.available_height() * 0.34).clamp(118.0, 232.0))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for workspace in after_active {
                    if workspace_row(ui, workspace, false, Some(false)).clicked() {
                        action = Some(SidebarAction::SwitchWorkspace(workspace.id.clone()));
                    }
                }
            });
        ui.add_space(4.0);
        crate::ui::hairline_full(ui);

        // 독립 「파일」 제목행은 제거하고 현재 경로와 핵심 도구를 한 행에 합친다.
        // 패널이 극단적으로 좁아지면 검색 → 새 폴더 → 숨김 순으로 도구를 남겨
        // 40pt까지 실제로 축소할 수 있게 한다.
        let mut create_folder = false;
        let mut create_file = false;
        let (header_rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 38.0), egui::Sense::hover());
        let visible_tools = (((header_rect.width() - 4.0).max(0.0) / 25.0).floor() as usize).min(4);
        let mut tool_right = header_rect.right() - 4.0;
        if visible_tools >= 2 {
            let rect = egui::Rect::from_min_size(
                egui::pos2(tool_right - 25.0, header_rect.top() + 6.5),
                egui::vec2(25.0, 25.0),
            );
            create_folder =
                file_toolbar_icon_at(ui, rect, "new_folder", FileToolbarIcon::Folder, false)
                    .on_hover_text(catalog.t("file_tree.new_folder_root", &[]))
                    .clicked();
            tool_right -= 25.0;
        }
        if visible_tools >= 1 {
            let rect = egui::Rect::from_min_size(
                egui::pos2(tool_right - 25.0, header_rect.top() + 6.5),
                egui::vec2(25.0, 25.0),
            );
            let search = file_toolbar_icon_at(
                ui,
                rect,
                "search",
                FileToolbarIcon::Search,
                self.file_search_open,
            )
            .on_hover_text("파일 검색");
            if search.clicked() {
                self.file_search_open = !self.file_search_open;
                if !self.file_search_open {
                    self.file_search.clear();
                }
            }
            tool_right -= 25.0;
        }
        if visible_tools >= 3 {
            let rect = egui::Rect::from_min_size(
                egui::pos2(tool_right - 25.0, header_rect.top() + 6.5),
                egui::vec2(25.0, 25.0),
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
                self.watch_show_hidden
                    .store(self.show_hidden, std::sync::atomic::Ordering::Relaxed);
                self.rebuild_flat();
            }
            tool_right -= 25.0;
        }
        // 새 파일 — 툴바 리팩토링(2026-07-18)에서 빠졌던 버튼 복원. EditState::NewFile
        // 소비 흐름(인라인 편집·커밋)은 그대로 살아 있어 생성 지점만 다시 잇는다.
        if visible_tools >= 4 {
            let rect = egui::Rect::from_min_size(
                egui::pos2(tool_right - 25.0, header_rect.top() + 6.5),
                egui::vec2(25.0, 25.0),
            );
            create_file = file_toolbar_icon_at(ui, rect, "new_file", FileToolbarIcon::File, false)
                .on_hover_text(catalog.t("file_tree.new_file_root", &[]))
                .clicked();
            tool_right -= 25.0;
        }

        let path_left = header_rect.left() + 10.0;
        if tool_right - path_left >= 20.0 {
            let icon_center = egui::pos2(path_left + 8.0, header_rect.center().y);
            paint_folder(ui.painter(), icon_center, ui.visuals().text_color());
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
                let galley = clipped_line(ui, &label, egui::FontId::monospace(11.5), text_width);
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
                    .hint_text("파일 이름 검색")
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
        paint_folder(ui.painter(), parent_icon_center, parent_color);
        ui.painter().text(
            egui::pos2(parent_rect.left() + 47.0, parent_rect.center().y),
            egui::Align2::LEFT_CENTER,
            "..",
            egui::FontId::monospace(12.5),
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
        if let Some(err) = &self.root_error {
            ui.colored_label(ui.visuals().error_fg_color, err);
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
                ui.label("새 파일");
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
        egui::ScrollArea::vertical()
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
                    if ui.rect_contains_pointer(hover_rect) {
                        ui.painter().rect_filled(
                            hover_rect,
                            1.0,
                            ui.visuals().widgets.hovered.weak_bg_fill,
                        );
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
                                    paint_folder(ui.painter(), ir.center(), folder_col);
                                } else {
                                    let file_color = if inaccessible {
                                        ui.visuals().weak_text_color()
                                    } else if row.name.starts_with('.') {
                                        entry_color.gamma_multiply(0.62)
                                    } else {
                                        entry_color
                                    };
                                    paint_file(ui.painter(), ir.center(), file_color, carve);
                                }
                                let text_color = if inaccessible {
                                    ui.visuals().weak_text_color()
                                } else if row.name.starts_with('.') {
                                    entry_color.gamma_multiply(0.62)
                                } else {
                                    entry_color
                                };
                                let rich = egui::RichText::new(&row.name)
                                    .monospace()
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
                    let row_resp =
                        ui.interact(row_rect, drag_id.with("row"), egui::Sense::click_and_drag());
                    let row_resp = if inaccessible {
                        row_resp.on_hover_text("macOS 접근 권한 없음")
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
                    } else if (row_resp.double_clicked() || label_resp.double_clicked())
                        && crate::ui::workspace::openable_file(&row.path)
                    {
                        // 파일 더블클릭 → 연결된 프로그램으로 열기 (2026-07-18 사용자).
                        // 터미널 우클릭 「열기」와 같은 판정(openable_file)을 공유한다 —
                        // 허용 확장자가 아니면(실행파일·스크립트 등) 아무 동작도 안 한다.
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
            action = Some(SidebarAction::OpenExternal(path));
        }
        if let Some((src, dst_dir)) = drop_action {
            self.start_move(src, dst_dir);
        }

        // 인라인 편집 커밋/취소 처리 (실패 시 편집 유지 — 이름을 고칠 수 있게)
        match edit_done {
            Some(false) => edit = None,
            Some(true) => match edit {
                Some(EditState::Rename { path, buffer, .. }) => {
                    match apply_rename(&path, &buffer) {
                        Ok(new_path) => {
                            self.error = None;
                            self.reload_parents(&path, &new_path);
                            edit = None;
                        }
                        Err(msg) => {
                            self.error = Some(msg);
                            edit = Some(EditState::Rename {
                                path,
                                buffer,
                                focus: true,
                            });
                        }
                    }
                }
                Some(EditState::NewFolder { parent, buffer, .. }) => {
                    match apply_new_folder(&parent, &buffer) {
                        Ok(_) => {
                            self.error = None;
                            self.reveal_dir(&parent);
                            edit = None;
                        }
                        Err(msg) => {
                            self.error = Some(msg);
                            edit = Some(EditState::NewFolder {
                                parent,
                                buffer,
                                focus: true,
                            });
                        }
                    }
                }
                Some(EditState::NewFile { parent, buffer, .. }) => {
                    match apply_new_file(&parent, &buffer) {
                        Ok(_) => {
                            self.error = None;
                            self.reveal_dir(&parent);
                            edit = None;
                        }
                        Err(msg) => {
                            self.error = Some(msg);
                            edit = Some(EditState::NewFile {
                                parent,
                                buffer,
                                focus: true,
                            });
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
            Some(MenuAction::CopyPath(path)) => ui.ctx().copy_text(path.display().to_string()),
            Some(MenuAction::InsertPath(path)) => action = Some(SidebarAction::InsertPath(path)),
            Some(MenuAction::CdPath(path)) => action = Some(SidebarAction::CdPath(path)),
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
                    let target = path.clone();
                    // 디렉터리 삭제는 느릴 수 있다 — 백그라운드 (§9-3)
                    self.spawn_op(refresh, move || {
                        remove_all(&target).map_err(|e| format!("영구 삭제 실패: {e}"))
                    });
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
        if !self.pending_listings.is_empty() {
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

    fn navigation(
        &mut self,
        ui: &mut egui::Ui,
        sidebar: &SidebarSnapshot<'_>,
    ) -> Option<SidebarAction> {
        let mut action = None;
        let selected = ui.visuals().selection.bg_fill.gamma_multiply(0.16);
        let labels = [
            (
                "⌂  홈",
                sidebar.view == super::agent_terminal::AgentTerminalView::Home,
                SidebarAction::ShowHome,
            ),
            (
                "▣  터미널",
                sidebar.view == super::agent_terminal::AgentTerminalView::Terminal,
                SidebarAction::ShowTerminal,
            ),
            ("▤  작업함", false, SidebarAction::OpenInbox),
            ("⚙  설정", false, SidebarAction::OpenSettings),
        ];
        for pair in labels.chunks(2) {
            ui.columns(2, |columns| {
                for (column, (label, active, next)) in columns.iter_mut().zip(pair) {
                    let label =
                        if matches!(next, SidebarAction::OpenInbox) && sidebar.inbox_count > 0 {
                            format!("{label}  {}", sidebar.inbox_count)
                        } else {
                            (*label).to_owned()
                        };
                    let button = egui::Button::new(label)
                        .selected(*active)
                        .fill(if *active {
                            selected
                        } else {
                            egui::Color32::TRANSPARENT
                        })
                        .stroke(egui::Stroke::NONE)
                        .corner_radius(egui::CornerRadius::same(1))
                        .min_size(egui::vec2(column.available_width(), 34.0));
                    if column.add(button).clicked() {
                        action = Some(match next {
                            SidebarAction::ShowHome => SidebarAction::ShowHome,
                            SidebarAction::ShowTerminal => SidebarAction::ShowTerminal,
                            SidebarAction::OpenInbox => SidebarAction::OpenInbox,
                            SidebarAction::OpenSettings => SidebarAction::OpenSettings,
                            _ => unreachable!("fixed navigation action"),
                        });
                    }
                }
            });
        }
        // 에이전트 관리 표면은 터미널/홈과 별도 창이므로 작은 보조 진입점으로 유지한다.
        if ui
            .add(
                egui::Button::new("Agents")
                    .frame(false)
                    .corner_radius(egui::CornerRadius::same(1)),
            )
            .clicked()
        {
            action = Some(SidebarAction::OpenAgents);
        }
        action
    }

    /// 새 폴더 생성 후 부모를 화면에 반영: 펼쳐져 있으면 재나열, 접혀 있으면 펼친다.
    fn reveal_dir(&mut self, dir: &Path) {
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
        let expanded = self
            .children
            .as_mut()
            .and_then(|c| node_mut(c, rel))
            .map(|n| n.expanded);
        match expanded {
            Some(true) => self.reload_dir(dir),
            Some(false) => self.toggle_dir(dir),
            None => self.refresh(), // 노드 미발견 (드묾) — 안전하게 전체 새로고침
        }
    }

    /// 드롭 → 이동 시작: 가드(§9-4) → 같은 볼륨 rename(§9-5) → EXDEV면 백그라운드
    /// copy+delete(§9-3). 성공 시 src/dst 부모만 재나열한다(§4).
    fn start_move(&mut self, src: PathBuf, dst_dir: PathBuf) {
        let Some(root) = self.root.clone() else {
            return;
        };
        match plan_move(&root, &src, &dst_dir) {
            Err(e) => self.error = Some(e),
            Ok(MovePlan::Noop) => {}
            Ok(MovePlan::Move { src, dst }) => match rename_no_replace(&src, &dst) {
                Ok(()) => {
                    self.error = None;
                    self.reload_parents(&src, &dst);
                }
                Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
                    // 크로스 볼륨: UI 프레임을 막지 않게 백그라운드로 (§9-3)
                    let refresh = parent_dirs(&src, &dst);
                    let (src, dst_dir, dst) = (src.clone(), dst_dir.clone(), dst.clone());
                    self.spawn_op(refresh, move || move_cross_volume(&src, &dst_dir, &dst));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    self.error = Some(format!(
                        "같은 이름이 이미 있습니다 — 덮어쓰지 않습니다: {}",
                        dst.display()
                    ));
                }
                Err(e) => self.error = Some(format!("이동 실패: {e}")),
            },
        }
    }

    /// 백그라운드 파일 조작 실행 — 완료/에러는 채널로 UI에 전달되고 repaint를 깨운다(§9-3).
    fn spawn_op(
        &mut self,
        refresh: Vec<PathBuf>,
        job: impl FnOnce() -> Result<(), String> + Send + 'static,
    ) {
        if self.in_flight >= FILE_OP_WORKER_CAP {
            self.error =
                Some("파일 작업이 너무 많습니다 — 진행 중인 작업을 기다려 주세요".to_owned());
            return;
        }
        self.in_flight += 1;
        let tx = self.ops_tx.clone();
        let ctx = self.egui_ctx.clone();
        std::thread::spawn(move || {
            let error = job().err();
            let _ = tx.send(OpOutcome {
                refresh,
                error,
                confirm_delete: None,
            });
            ctx.request_repaint();
        });
    }

    /// 휴지통 이동 (§5/§9-7). 큰 디렉터리도 프레임을 막지 않게 항상 백그라운드.
    /// 실패 시 영구삭제 확인을 UI에 예약한다 (조용한 영구삭제 금지).
    fn spawn_trash(&mut self, path: PathBuf) {
        if self.in_flight >= FILE_OP_WORKER_CAP {
            self.error =
                Some("파일 작업이 너무 많습니다 — 진행 중인 작업을 기다려 주세요".to_owned());
            return;
        }
        self.in_flight += 1;
        let tx = self.ops_tx.clone();
        let ctx = self.egui_ctx.clone();
        std::thread::spawn(move || {
            let refresh: Vec<PathBuf> = path.parent().map(Path::to_path_buf).into_iter().collect();
            let outcome = match trash::delete(&path) {
                Ok(()) => OpOutcome {
                    refresh,
                    error: None,
                    confirm_delete: None,
                },
                Err(e) => OpOutcome {
                    refresh: Vec::new(),
                    error: Some(format!("휴지통 이동 실패: {e}")),
                    confirm_delete: Some(path),
                },
            };
            let _ = tx.send(outcome);
            ctx.request_repaint();
        });
    }

    /// src/dst의 부모 디렉터리만 재나열 (§4 — 전체 리스캔 금지).
    fn reload_parents(&mut self, src: &Path, dst: &Path) {
        for dir in parent_dirs(src, dst) {
            self.reload_dir(&dir);
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
            let preserve_expanded = Arc::new(self.collect_expanded_paths());
            self.invalidate_listing_subtree(dir);
            self.request_listing(dir.to_path_buf(), preserve_expanded);
        }
    }

    fn request_listing(&mut self, path: PathBuf, preserve_expanded: Arc<HashSet<PathBuf>>) {
        self.next_listing_token = self.next_listing_token.wrapping_add(1);
        let token = self.next_listing_token;
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if let Some(old) = self.pending_listings.insert(
            path.clone(),
            PendingListing {
                token,
                preserve_expanded,
                apply_expanded: None,
                started: false,
                cancel: Arc::clone(&cancel),
            },
        ) {
            // 같은 경로 재요청(reload_dir) — 구 워커의 잔여 청크 송신을 중단시킨다.
            old.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let job = ListingJob {
            tx: self.listing_tx.clone(),
            ctx: self.egui_ctx.clone(),
            shutdown: Arc::clone(&self.listing_shutdown),
            epoch: ListingEpochGuard {
                requested: self.listing_epoch,
                current: Arc::clone(&self.listing_epoch_shared),
                cancel,
            },
            token,
            root: self.root.clone(),
            path,
            ignore_cache: self.ignore_cache.clone(),
        };
        match self.listing_jobs.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(job)) => {
                // 요청을 무제한 보관하지 않는다. 이 경로 요청은 취소하고, 큐가 비는
                // 프레임에 root refresh 한 건으로 상태를 재구성한다.
                job.epoch
                    .cancel
                    .store(true, std::sync::atomic::Ordering::Release);
                self.pending_listings.remove(&job.path);
                self.listing_refresh_deferred = true;
                self.egui_ctx.request_repaint();
            }
            Err(TrySendError::Disconnected(job)) => {
                self.pending_listings.remove(&job.path);
                self.error = Some("파일 listing worker가 종료되었습니다".to_owned());
            }
        }
    }

    fn request_listing_if_absent(
        &mut self,
        path: PathBuf,
        preserve_expanded: Arc<HashSet<PathBuf>>,
    ) {
        if self.pending_listings.contains_key(&path) {
            return;
        }
        self.request_listing(path, preserve_expanded);
    }

    fn invalidate_listing_subtree(&mut self, path: &Path) {
        self.pending_listings.retain(|pending_path, pending| {
            let keep = !pending_path.starts_with(path);
            if !keep {
                pending
                    .cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            keep
        });
    }

    fn pump_listings(&mut self) {
        let mut processed = 0;
        while processed < LISTING_RESULTS_PER_FRAME {
            let Ok(outcome) = self.listing_rx.try_recv() else {
                break;
            };
            // stale(구 epoch)은 프레임 예산에 세지 않고 즉시 폐기 — 백로그를
            // 프레임당 4개씩만 비우면 따라잡기가 밀린다(안정성 감사 High #2).
            if outcome.epoch != self.listing_epoch {
                continue;
            }
            self.apply_listing_outcome(outcome);
            processed += 1;
        }
        if self.listing_refresh_deferred && self.pending_listings.len() < LISTING_JOB_QUEUE_CAP / 2
        {
            self.listing_refresh_deferred = false;
            self.refresh();
        }
        if !self.pending_listings.is_empty() {
            self.egui_ctx.request_repaint();
        }
    }

    fn apply_listing_outcome(&mut self, outcome: ListingOutcome) {
        if outcome.epoch != self.listing_epoch {
            return;
        }
        let Some(pending) = self.pending_listings.get(&outcome.path) else {
            return;
        };
        if pending.token != outcome.token {
            return;
        }

        match outcome.result {
            ListingResult::Chunk { nodes, done } => {
                self.apply_listing_chunk(outcome.path, nodes, done);
            }
            ListingResult::Error { message, kind } => {
                self.pending_listings.remove(&outcome.path);
                self.apply_listing_error(&outcome.path, message, kind);
            }
        }
    }

    fn apply_listing_chunk(&mut self, path: PathBuf, nodes: Vec<TreeNode>, done: bool) {
        self.inaccessible_paths.remove(&path);
        let first = self
            .pending_listings
            .get(&path)
            .map(|pending| !pending.started)
            .unwrap_or(false);
        let expanded = if first {
            let expanded = self
                .current_expanded_paths_for_listing(&path)
                .unwrap_or_else(|| {
                    self.pending_listings
                        .get(&path)
                        .map(|pending| (*pending.preserve_expanded).clone())
                        .unwrap_or_default()
                });
            let expanded = Arc::new(expanded);
            if let Some(pending) = self.pending_listings.get_mut(&path) {
                pending.started = true;
                pending.apply_expanded = Some(Arc::clone(&expanded));
            }
            expanded
        } else {
            self.pending_listings
                .get(&path)
                .and_then(|pending| pending.apply_expanded.as_ref().map(Arc::clone))
                .unwrap_or_else(|| Arc::new(HashSet::new()))
        };

        let prepared = prepare_listing_nodes(&path, nodes, &expanded);
        let applied = if first {
            self.replace_listing_children(&path, prepared)
        } else {
            self.append_listing_children(&path, prepared)
        };
        if !applied {
            self.invalidate_listing_subtree(&path);
            return;
        }

        let preserve_expanded = self
            .pending_listings
            .get(&path)
            .map(|pending| Arc::clone(&pending.preserve_expanded));
        if done {
            self.pending_listings.remove(&path);
            if let Some(preserve_expanded) = preserve_expanded {
                for child in self.expanded_direct_child_paths(&path) {
                    self.request_listing_if_absent(child, Arc::clone(&preserve_expanded));
                }
            }
        }
        self.root_error = None;
        self.rebuild_flat();
    }

    fn apply_listing_error(&mut self, path: &Path, error: String, kind: std::io::ErrorKind) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let permission_denied = kind == std::io::ErrorKind::PermissionDenied;
        if path == root {
            self.children = None;
            self.root_error = Some(if permission_denied {
                "이 폴더는 macOS 접근 권한이 없습니다.".to_owned()
            } else {
                format!("루트 나열 실패: {error}")
            });
            self.rebuild_flat();
            return;
        }
        if let Ok(rel) = path.strip_prefix(&root)
            && let Some(node) = self.children.as_mut().and_then(|c| node_mut(c, rel))
        {
            node.expanded = false;
            node.children = None;
        }
        if permission_denied {
            self.inaccessible_paths.insert(path.to_path_buf());
            tracing::info!(path = %path.display(), "macOS가 폴더 나열 권한을 거부함");
            self.rebuild_flat();
            return;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        self.error = Some(format!("{name} 나열 실패: {error}"));
        self.rebuild_flat();
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

    fn append_listing_children(&mut self, path: &Path, mut nodes: Vec<TreeNode>) -> bool {
        let Some(root) = self.root.clone() else {
            return false;
        };
        let target = if path == root {
            self.children.as_mut()
        } else {
            let Ok(rel) = path.strip_prefix(&root) else {
                return false;
            };
            let Some(node) = self.children.as_mut().and_then(|c| node_mut(c, rel)) else {
                return false;
            };
            if !node.expanded {
                return false;
            }
            node.children.as_mut()
        };
        let Some(target) = target else {
            return false;
        };
        target.append(&mut nodes);
        true
    }

    fn current_expanded_paths_for_listing(&self, path: &Path) -> Option<HashSet<PathBuf>> {
        let root = self.root.as_ref()?;
        if path == root {
            let children = self.children.as_ref()?;
            let mut expanded = HashSet::new();
            collect_expanded_paths(children, root, &mut expanded);
            return Some(expanded);
        }
        let rel = path.strip_prefix(root).ok()?;
        let node = self.children.as_ref().and_then(|c| node_ref(c, rel))?;
        let children = node.children.as_ref()?;
        let mut expanded = HashSet::new();
        collect_expanded_paths(children, path, &mut expanded);
        Some(expanded)
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

impl Drop for FileTreeUi {
    fn drop(&mut self) {
        // read_dir가 네트워크 파일시스템에서 오래 막힐 수 있어 UI thread에서 join하지는
        // 않는다. 인스턴스 플래그와 result receiver drop으로 전역 pool의 이 인스턴스 job은
        // 다음 entry/send 경계에서 끝난다. pool 자체는 프로세스 전역 4개로 계속 재사용한다.
        self.listing_shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        // macOS FSEvents watcher Drop은 감시 루트가 외부에서 사라진 경우 OS latency만큼
        // 블록할 수 있다. 고정 1개 reaper + 전역 slot cap으로 UI를 막지 않고 유계 정리한다.
        #[cfg(not(test))]
        if let Some(watcher) = self.watcher.take() {
            retire_watcher(watcher);
        }
    }
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

fn take_pending_watch_batch(pending: &mut BTreeSet<PathBuf>, limit: usize) -> Vec<PathBuf> {
    let selected: Vec<PathBuf> = pending.iter().take(limit).cloned().collect();
    for path in &selected {
        pending.remove(path);
    }
    selected
}

#[cfg(test)]
fn watch_events_for_path(
    root: &Path,
    path: &Path,
    show_hidden: bool,
    ignore_prefixes: &[PathBuf],
) -> Vec<WatchEvent> {
    watch_events_for_path_with_ignore(
        root,
        path,
        show_hidden,
        ignore_prefixes,
        &GitIgnoreCache::default(),
    )
}

fn watch_events_for_path_with_ignore(
    root: &Path,
    path: &Path,
    show_hidden: bool,
    ignore_prefixes: &[PathBuf],
    ignore_cache: &GitIgnoreCache,
) -> Vec<WatchEvent> {
    if ignore_prefixes
        .iter()
        .any(|prefix| path.starts_with(prefix))
    {
        return Vec::new();
    }
    if has_default_watch_ignore_component(root, path) {
        return Vec::new();
    }
    let is_dir = path.is_dir();
    if ignore_cache.is_ignored(path, is_dir) {
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

fn has_default_watch_ignore_component(root: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components().any(|component| {
        matches!(
            component,
            std::path::Component::Normal(name) if default_watch_ignore_name(&name.to_string_lossy())
        )
    })
}

fn default_watch_ignore_name(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".next"
            | ".turbo"
            | "vendor"
            | "logs"
            | ".cache"
            | ".DS_Store"
    )
}

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

fn relevant_fs_event(kind: &notify::EventKind) -> bool {
    !matches!(kind, notify::EventKind::Access(_))
}

/// 우클릭 컨텍스트 메뉴 동작 (FT-3) — flat 순회 밖에서 처리한다.
enum MenuAction {
    NewFolder(PathBuf),
    Rename(PathBuf),
    Delete(PathBuf),
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
/// 요약 한 줄(dim/mono). 반환 Response로 클릭을 처리한다.
fn workspace_row(
    ui: &mut egui::Ui,
    workspace: &SidebarWorkspaceEntry,
    active: bool,
    expanded: Option<bool>,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 46.0), egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    if active {
        ui.painter().rect_filled(
            rect,
            1.0,
            ui.visuals().selection.bg_fill.gamma_multiply(0.12),
        );
    } else if response.hovered() {
        ui.painter()
            .rect_filled(rect, 1.0, ui.visuals().widgets.hovered.bg_fill);
    }
    // 선택/실행 상태와 무관한 프로젝트 고유색. 40pt 아이콘 레일에서도 워크스페이스를
    // 색만으로 빠르게 구분할 수 있게 비활성 행도 같은 색을 유지한다.
    let color = workspace_accent(&workspace.name);
    let avatar = egui::Rect::from_min_size(
        egui::pos2(rect.left() + 8.0, rect.top() + 8.0),
        egui::vec2(30.0, 30.0),
    );
    // 워크스페이스 마크는 별도 테두리 없이 상태색을 채운다(HTML 목업과 같은 규칙).
    ui.painter().rect_filled(
        avatar,
        1.0,
        color.gamma_multiply(if active { 0.48 } else { 0.36 }),
    );
    let bold_family = egui::FontFamily::Name(terminal::MONO_BOLD_FAMILY.into());
    let initial = workspace
        .name
        .chars()
        .next()
        .and_then(|character| character.to_uppercase().next())
        .unwrap_or('W');
    ui.painter().text(
        avatar.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        egui::FontId::new(15.0, bold_family.clone()),
        egui::Color32::WHITE,
    );
    let show_summary = rect.width() >= 270.0;
    let show_disclosure = expanded.is_some() && rect.width() >= 56.0;
    if rect.width() >= 64.0 {
        let reserved_right = if show_summary { 198.0 } else { 8.0 };
        let name_width = (rect.right() - reserved_right - avatar.right() - 9.0).max(0.0);
        if name_width > 4.0 {
            let uppercase_name = workspace.name.to_uppercase();
            let name = clipped_line(
                ui,
                &uppercase_name,
                egui::FontId::new(14.0, bold_family),
                name_width,
            );
            ui.painter().galley(
                egui::pos2(avatar.right() + 9.0, rect.center().y - name.size().y / 2.0),
                name,
                ui.visuals().text_color(),
            );
        }
    }
    if show_summary {
        let right = if show_disclosure {
            rect.right() - 22.0
        } else {
            rect.right() - 8.0
        };
        paint_workspace_summary(ui, right, rect.center().y, workspace.summary);
    }
    if show_disclosure && let Some(expanded) = expanded {
        let center = egui::pos2(rect.right() - 9.0, rect.center().y);
        let points = if expanded {
            vec![
                egui::pos2(center.x - 4.0, center.y - 2.0),
                egui::pos2(center.x + 4.0, center.y - 2.0),
                egui::pos2(center.x, center.y + 3.0),
            ]
        } else {
            vec![
                egui::pos2(center.x - 2.0, center.y - 4.0),
                egui::pos2(center.x - 2.0, center.y + 4.0),
                egui::pos2(center.x + 3.0, center.y),
            ]
        };
        ui.painter().add(egui::Shape::convex_polygon(
            points,
            ui.visuals().weak_text_color(),
            egui::Stroke::NONE,
        ));
    }
    response
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

fn paint_workspace_summary(
    ui: &egui::Ui,
    right: f32,
    center_y: f32,
    summary: SidebarSessionSummary,
) {
    let weak = ui.visuals().weak_text_color();
    let values = workspace_summary_segments(summary, weak);
    let font = egui::FontId::proportional(11.0);
    let mut cursor = right;
    for (text, color) in values.iter().rev() {
        let galley = ui
            .painter()
            .layout_no_wrap(text.clone(), font.clone(), *color);
        cursor -= galley.size().x;
        ui.painter().galley(
            egui::pos2(cursor, center_y - galley.size().y / 2.0),
            galley,
            *color,
        );
    }
}

fn workspace_summary_segments(
    summary: SidebarSessionSummary,
    weak: egui::Color32,
) -> Vec<(String, egui::Color32)> {
    let blue = egui::Color32::from_rgb(0x4c, 0xa8, 0xdf);
    let orange = egui::Color32::from_rgb(0xe7, 0x9a, 0x3b);
    let green = egui::Color32::from_rgb(0x55, 0xc8, 0x79);
    let red = egui::Color32::from_rgb(0xed, 0x5b, 0x61);
    let mut parts = Vec::new();
    let push = |parts: &mut Vec<(String, egui::Color32)>, label: String, color| {
        if !parts.is_empty() {
            parts.push((" · ".to_owned(), weak));
        }
        parts.push((label, color));
    };
    if summary.running > 0 {
        push(&mut parts, format!("{} 실행 중", summary.running), blue);
    }
    if summary.waiting > 0 {
        push(&mut parts, format!("{} 입력 대기", summary.waiting), orange);
    }
    if summary.done > 0 {
        push(&mut parts, format!("완료 {}", summary.done), green);
    }
    if summary.error > 0 {
        push(&mut parts, format!("오류 {}", summary.error), red);
    }
    if summary.idle > 0 {
        if summary.idle == 1 && parts.is_empty() {
            push(&mut parts, "유휴".to_owned(), weak);
        } else {
            push(&mut parts, format!("유휴 {}", summary.idle), weak);
        }
    }
    if summary.inactive > 0 {
        if parts.is_empty() {
            push(&mut parts, "비활성".to_owned(), weak);
        } else {
            push(&mut parts, format!("비활성 {}", summary.inactive), weak);
        }
    }
    if parts.is_empty() {
        parts.push(("유휴".to_owned(), weak));
    }
    parts
}

fn session_row(ui: &mut egui::Ui, entry: &SessionEntry) -> egui::Response {
    session_row_impl(ui, entry, None)
}

/// 이름 인라인 편집 중인 행 — 레일/보조 행(2·3행)은 그대로 유지하고 **제목 자리만**
/// TextEdit로 바꾼다. 행 전체를 편집기로 대체하면 편집 중 레이아웃이 무너진다
/// (2026-07-16 사용자).
fn session_row_editing(ui: &mut egui::Ui, entry: &SessionEntry, buf: &mut String) {
    session_row_impl(ui, entry, Some(buf));
}

fn session_row_impl(
    ui: &mut egui::Ui,
    entry: &SessionEntry,
    edit_buf: Option<&mut String>,
) -> egui::Response {
    // 에이전트면 3줄(제목/에이전트·모델·effort/상태·ctx%), 아니면 2줄(제목/요약).
    // 요약이 없어도(유휴/시작 직후) 2행에 '~'를 표시해 행 높이를 유지한다(2026-07-07).
    let agent = entry.agent_line.is_some();
    let summary_text: &str = if entry.summary.is_empty() {
        "~"
    } else {
        &entry.summary
    };
    let row_h = if agent { 52.0 } else { 38.0 };
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), row_h),
        egui::Sense::click(),
    );
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    // 색을 먼저 복사(Copy)해 visuals 차용을 끝낸 뒤 ui.fonts로 galley를 만든다.
    let accent = ui.visuals().selection.bg_fill;
    let dot = session_status_color(entry.status, ui.visuals());
    let hover_bg = ui.visuals().widgets.hovered.bg_fill;
    // 2·3행(보조 정보): 다크는 기존 weak 톤, 라이트는 weak가 패널 위에서 너무 옅어
    // textSecondary(#444444) 수준으로 진하게 (라이트 테마 회색 흐림, 2026-07-10).
    let sub_color = if ui.visuals().dark_mode {
        ui.visuals().weak_text_color().gamma_multiply(0.9)
    } else {
        egui::Color32::from_rgb(0x44, 0x44, 0x44)
    };
    let title_color = if entry.focused {
        accent
    } else {
        ui.visuals().text_color()
    };
    // 텍스트는 행 폭(좌 16 + 우 여백 8) 안으로 잘라 '…' 처리 — 고정 글자수 truncate는
    // 좁은 사이드바에서 박스 밖으로 삐져나갔다(#91 사용자).
    let max_w = (rect.width() - 16.0 - 8.0).max(10.0);
    let title_galley = clipped_line(ui, &entry.title, egui::FontId::proportional(13.0), max_w);
    // 2행/3행: 에이전트면 agent_line/status_line, 아니면 요약(2행)만.
    let (line2, line3) = if agent {
        (entry.agent_line.as_deref(), entry.status_line.as_deref())
    } else {
        (Some(summary_text), None)
    };
    let line2_galley = line2.map(|t| clipped_line(ui, t, egui::FontId::monospace(10.5), max_w));
    let line3_galley = line3.map(|t| clipped_line(ui, t, egui::FontId::monospace(10.5), max_w));

    let painter = ui.painter();
    // 선택/hover 배경 — 편집 중에는 hover 톤으로 상시 칠해 편집 상태를 표시.
    if edit_buf.is_some() {
        painter.rect_filled(rect, 1.0, hover_bg);
    } else if entry.focused {
        painter.rect_filled(rect, 1.0, accent.gamma_multiply(0.18));
    } else if resp.hovered() {
        painter.rect_filled(rect, 1.0, hover_bg);
    }
    // 좌측 상태 레일 — 항상 표시, 상태 색으로 세로로 훑어 파악 (목업 §세션).
    // 폭 = 두 번째 채널(2026-07-07): 평시 3px, 미확인 완료/입력대기(attention)는 6px로
    // 굵힌다. 알림 도착 시 이미 보고 있던 pane은 6px 대신 1회 펄스(3→6→3px).
    // 자리는 최대 6px 기준으로 상시 예약(텍스트 x=16 고정)이라 폭이 바뀌어도 안 밀린다.
    let (rail_w, rail_color) = if let Some((t, color)) = entry.pulse {
        (3.0 + 3.0 * (t * std::f32::consts::PI).sin(), color)
    } else if entry.attention {
        (6.0, dot)
    } else {
        (3.0, dot)
    };
    let rail = egui::Rect::from_min_size(
        egui::pos2(rect.left(), rect.top() + 4.0),
        egui::vec2(rail_w, row_h - 8.0),
    );
    painter.rect_filled(rail, 0.0, rail_color);
    // 제목(1행) + 2행 + 3행 — 세로 위치는 행 수에 맞춰.
    // 편집 중에는 제목 갤리 대신 같은 자리에 TextEdit를 얹는다 (아래 edit_buf 분기).
    if edit_buf.is_none() {
        painter.galley(
            egui::pos2(
                rect.left() + 16.0,
                rect.top() + 13.0 - title_galley.size().y / 2.0,
            ),
            title_galley,
            title_color,
        );
    }
    if let Some(g) = line2_galley {
        painter.galley(
            egui::pos2(rect.left() + 16.0, rect.top() + 27.0 - g.size().y / 2.0),
            g,
            sub_color,
        );
    }
    if let Some(g) = line3_galley {
        painter.galley(
            egui::pos2(rect.left() + 16.0, rect.top() + 41.0 - g.size().y / 2.0),
            g,
            sub_color,
        );
    }
    if let Some(buf) = edit_buf {
        // 제목 1행 자리에 프레임 없는 TextEdit — 글꼴/x 위치를 제목 갤리와 맞춘다.
        let title_rect = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 16.0, rect.top() + 4.0),
            egui::pos2(rect.right() - 8.0, rect.top() + 22.0),
        );
        let edit_resp = ui.put(
            title_rect,
            egui::TextEdit::singleline(buf)
                .font(egui::FontId::proportional(13.0))
                .frame(egui::Frame::NONE)
                .margin(egui::Margin::ZERO)
                .vertical_align(egui::Align::Center),
        );
        edit_resp.request_focus();
    }
    resp
}

/// 한 줄 텍스트를 max_width 안으로 잘라 '…'로 끝내는 galley (박스 밖 삐짐 방지, #91).
fn clipped_line(
    ui: &egui::Ui,
    text: &str,
    font_id: egui::FontId,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat {
            font_id,
            // PLACEHOLDER여야 painter.galley의 fallback 색이 적용된다 — 기본값
            // Color32::GRAY는 fallback을 무시하고 항상 회색으로 그려졌다(라이트 흐림 원인).
            color: egui::Color32::PLACEHOLDER,
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
            let upper = vec![
                egui::pos2(center.x - 7.0, center.y),
                egui::pos2(center.x - 3.5, center.y - 3.2),
                egui::pos2(center.x, center.y - 4.0),
                egui::pos2(center.x + 3.5, center.y - 3.2),
                egui::pos2(center.x + 7.0, center.y),
            ];
            let lower = vec![
                egui::pos2(center.x - 7.0, center.y),
                egui::pos2(center.x - 3.5, center.y + 3.2),
                egui::pos2(center.x, center.y + 4.0),
                egui::pos2(center.x + 3.5, center.y + 3.2),
                egui::pos2(center.x + 7.0, center.y),
            ];
            ui.painter().add(egui::Shape::line(upper, stroke));
            ui.painter().add(egui::Shape::line(lower, stroke));
            ui.painter().circle_filled(center, 2.0, color);
        }
        FileToolbarIcon::Folder => paint_folder(ui.painter(), rect.center(), color),
        FileToolbarIcon::File => {
            paint_file(ui.painter(), rect.center(), color, ui.visuals().panel_fill)
        }
        FileToolbarIcon::Search => {
            let center = rect.center() + egui::vec2(-2.0, -2.0);
            ui.painter()
                .circle_stroke(center, 5.5, egui::Stroke::new(1.5, color));
            ui.painter().line_segment(
                [center + egui::vec2(4.0, 4.0), center + egui::vec2(8.0, 8.0)],
                egui::Stroke::new(1.5, color),
            );
        }
    }
    response
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

fn workspace_accent(name: &str) -> egui::Color32 {
    match name
        .chars()
        .next()
        .and_then(|character| character.to_uppercase().next())
        .unwrap_or('W')
    {
        'S' => egui::Color32::from_rgb(0x55, 0xc8, 0x79),
        'A' => egui::Color32::from_rgb(0xe7, 0x9a, 0x3b),
        'V' => egui::Color32::from_rgb(0x9a, 0x78, 0xe8),
        'C' => egui::Color32::from_rgb(0x43, 0xb8, 0xcd),
        'P' => egui::Color32::from_rgb(0x4c, 0x84, 0xdf),
        _ => {
            let palette = [
                egui::Color32::from_rgb(0x55, 0xc8, 0x79),
                egui::Color32::from_rgb(0xe7, 0x9a, 0x3b),
                egui::Color32::from_rgb(0x9a, 0x78, 0xe8),
                egui::Color32::from_rgb(0x43, 0xb8, 0xcd),
                egui::Color32::from_rgb(0x4c, 0x84, 0xdf),
                egui::Color32::from_rgb(0xe0, 0x6c, 0x75),
            ];
            let hash = name.bytes().fold(0usize, |acc, byte| {
                acc.wrapping_mul(31).wrapping_add(byte as usize)
            });
            palette[hash % palette.len()]
        }
    }
}

/// 폴더 아이콘 — 참고 시안처럼 탭 + 본체의 얇은 윤곽선.
fn paint_folder(p: &egui::Painter, c: egui::Pos2, col: egui::Color32) {
    let w = 15.0;
    let h = 11.0;
    let body = egui::Rect::from_center_size(egui::pos2(c.x, c.y + 1.0), egui::vec2(w, h));
    let tab = egui::Rect::from_min_size(
        egui::pos2(body.left(), body.top() - 3.0),
        egui::vec2(w * 0.45, 4.0),
    );
    let stroke = egui::Stroke::new(1.2, col);
    p.rect_stroke(tab, 1.0, stroke, egui::StrokeKind::Inside);
    p.rect_stroke(body, 1.0, stroke, egui::StrokeKind::Inside);
}

/// 파일 아이콘 — 문서(접힌 모서리). `carve`는 접힌 모서리를 파낼 배경색.
fn paint_file(p: &egui::Painter, c: egui::Pos2, col: egui::Color32, carve: egui::Color32) {
    let w = 11.0;
    let h = 14.0;
    let fold = 4.0;
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

/// PTY 세션 상태 → 공통 에이전트 상태 색. 기존 호출부(App의 pulse, pane glyph)가
/// 같은 팔레트를 공유하도록 이 wrapper를 유지한다.
pub(crate) fn session_status_color(
    status: Option<runtime::SessionStatus>,
    _visuals: &egui::Visuals,
) -> egui::Color32 {
    crate::ui::agent_visuals::status_color(crate::agent_surface::AgentVisualState::from_pty(status))
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
fn rename_precheck(src: &Path, dst: &Path) -> std::io::Result<()> {
    // symlink 자체도 "존재"로 취급 — try_exists는 링크를 따라가므로 symlink_metadata로 검사
    if std::fs::symlink_metadata(dst).is_ok() {
        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    }
    std::fs::rename(src, dst)
}

/// 크로스 볼륨 이동 (§9-4 확정 순서): `dst_dir/.tmp-<uuid>`에 전체 copy → 최종 이름으로
/// rename → 성공 후에만 원본 delete. 부분 실패 시 tmp 정리, 원본 보존.
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

/// 파일/링크/디렉터리를 삭제한다 (링크는 링크 자체만).
fn remove_all(path: &Path) -> std::io::Result<()> {
    let file_type = std::fs::symlink_metadata(path)?.file_type();
    if file_type.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// 한 디렉터리를 나열한다 (lazy 단위 — 재귀 없음). symlink는 따라가지 않는다(§9-4:
/// `DirEntry::file_type`은 링크를 해석하지 않으므로 링크는 파일처럼 취급 — 펼침 불가).
const LISTING_CHUNK_SIZE: usize = 2048;
const LISTING_RESULTS_PER_FRAME: usize = 4;
const LISTING_WORKER_COUNT: usize = 4;
const LISTING_JOB_QUEUE_CAP: usize = 64;
const LISTING_RESULT_QUEUE_CAP: usize = 16;
const FILE_OP_RESULT_QUEUE_CAP: usize = 32;
const FILE_OP_WORKER_CAP: usize = 4;
#[cfg(not(test))]
const WATCH_EVENT_QUEUE_CAP: usize = 512;
const WATCH_EVENTS_PER_FRAME: usize = 256;

#[cfg(not(test))]
const WATCHER_SLOT_CAP: usize = 4;
#[cfg(not(test))]
static WATCHER_SLOTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(not(test))]
static WATCHER_REAPER: std::sync::OnceLock<SyncSender<notify::RecommendedWatcher>> =
    std::sync::OnceLock::new();

#[cfg(not(test))]
fn try_acquire_watcher_slot() -> bool {
    WATCHER_SLOTS
        .fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |current| (current < WATCHER_SLOT_CAP).then_some(current + 1),
        )
        .is_ok()
}

#[cfg(not(test))]
fn release_watcher_slot() {
    let previous = WATCHER_SLOTS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    debug_assert!(previous > 0, "watcher slot underflow");
}

#[cfg(not(test))]
fn watcher_reaper() -> &'static SyncSender<notify::RecommendedWatcher> {
    WATCHER_REAPER.get_or_init(|| {
        let (tx, rx) = sync_channel::<notify::RecommendedWatcher>(WATCHER_SLOT_CAP);
        if let Err(error) = std::thread::Builder::new()
            .name("file-watcher-reaper".to_owned())
            .spawn(move || {
                while let Ok(watcher) = rx.recv() {
                    drop(watcher);
                    release_watcher_slot();
                }
            })
        {
            // receiver는 spawn 실패와 함께 drop되어 tx가 Disconnected가 된다. retire 호출은
            // 아래 bounded fallback으로 넘어가므로 앱 시작/토글을 panic시키지 않는다.
            tracing::error!("file watcher reaper thread 생성 실패: {error}");
        }
        tx
    })
}

#[cfg(not(test))]
fn retire_watcher(watcher: notify::RecommendedWatcher) {
    match watcher_reaper().try_send(watcher) {
        Ok(()) => {}
        Err(TrySendError::Full(watcher)) | Err(TrySendError::Disconnected(watcher)) => {
            // slot cap 때문에 fallback thread 수도 최대 WATCHER_SLOT_CAP이다. UI thread에서
            // 직접 Drop해 멈추는 것보다 독립 정리를 유지한다.
            let pending = Arc::new(Mutex::new(Some(watcher)));
            let worker_pending = Arc::clone(&pending);
            if std::thread::Builder::new()
                .name("file-watcher-retire-fallback".to_owned())
                .spawn(move || {
                    if let Some(watcher) =
                        worker_pending.lock().expect("watcher fallback lock").take()
                    {
                        drop(watcher);
                    }
                    release_watcher_slot();
                })
                .is_err()
            {
                // thread 생성 실패 시 watcher를 leak해 UI block을 피하되 slot은 점유한 채
                // 남긴다. 최대 WATCHER_SLOT_CAP 이후 새 watcher 생성이 중단되어 유계다.
                std::mem::forget(pending);
                tracing::error!("파일 감시자 정리 thread 생성 실패 — watcher slot 격리");
            }
        }
    }
}

/// listing 워커의 stale 판정 — 요청 시점 epoch과 UI의 현재 epoch(공유 atomic)을 묶어
/// 워커가 나열 전/청크 전송 중에 확인한다(안정성 감사 High #2).
struct ListingEpochGuard {
    requested: u64,
    current: Arc<std::sync::atomic::AtomicU64>,
    /// 이 요청 전용 취소 플래그(같은 경로 token 교체/부분 무효화).
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl ListingEpochGuard {
    fn is_stale(&self) -> bool {
        self.current.load(std::sync::atomic::Ordering::Relaxed) != self.requested
            || self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }
}

static LISTING_POOL: std::sync::OnceLock<SyncSender<ListingJob>> = std::sync::OnceLock::new();

fn global_listing_pool() -> &'static SyncSender<ListingJob> {
    LISTING_POOL.get_or_init(|| {
        let (tx, jobs) = sync_channel::<ListingJob>(LISTING_JOB_QUEUE_CAP);
        let jobs = Arc::new(Mutex::new(jobs));
        let mut spawned = 0usize;
        for index in 0..LISTING_WORKER_COUNT {
            let jobs = Arc::clone(&jobs);
            match std::thread::Builder::new()
                .name(format!("file-listing-{index}"))
                .spawn(move || {
                    loop {
                        let job = {
                            let Ok(rx) = jobs.lock() else {
                                return;
                            };
                            let Ok(job) = rx.recv() else {
                                return;
                            };
                            job
                        };
                        run_listing_job(job);
                    }
                }) {
                Ok(_) => spawned += 1,
                Err(error) => {
                    tracing::error!(worker = index, "file listing worker 생성 실패: {error}")
                }
            }
        }
        if spawned == 0 {
            // jobs Arc가 이 초기화 끝에서 drop되면 tx는 Disconnected가 되고 UI가 오류를
            // 표시한다. thread 자원 부족으로 앱 전체를 panic시키지 않는다.
            tracing::error!("file listing worker를 하나도 만들지 못했습니다");
        }
        tx
    })
}

fn run_listing_job(job: ListingJob) {
    let ListingJob {
        tx,
        ctx,
        shutdown,
        epoch,
        token,
        root,
        path,
        ignore_cache,
    } = job;
    if epoch.is_stale() || shutdown.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    match read_children_guarded(
        &path,
        root.as_deref(),
        &ignore_cache,
        Some((&epoch, shutdown.as_ref())),
    ) {
        Ok(nodes) => {
            send_listing_chunks_bounded(&tx, &ctx, &epoch, shutdown.as_ref(), token, path, nodes)
        }
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted || epoch.is_stale() => {}
        Err(error) => {
            let _ = send_listing_outcome(
                &tx,
                ListingOutcome {
                    epoch: epoch.requested,
                    token,
                    path,
                    result: ListingResult::Error {
                        message: error.to_string(),
                        kind: error.kind(),
                    },
                },
                &epoch,
                shutdown.as_ref(),
            );
            ctx.request_repaint();
        }
    }
}

fn send_listing_outcome(
    tx: &SyncSender<ListingOutcome>,
    mut outcome: ListingOutcome,
    epoch: &ListingEpochGuard,
    shutdown: &std::sync::atomic::AtomicBool,
) -> bool {
    loop {
        if epoch.is_stale() || shutdown.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        match tx.try_send(outcome) {
            Ok(()) => return true,
            Err(TrySendError::Full(returned)) => {
                outcome = returned;
                std::thread::park_timeout(std::time::Duration::from_millis(2));
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

#[cfg(test)]
fn send_listing_chunks(
    tx: SyncSender<ListingOutcome>,
    ctx: &egui::Context,
    epoch: &ListingEpochGuard,
    token: u64,
    path: PathBuf,
    nodes: Vec<TreeNode>,
) {
    let shutdown = std::sync::atomic::AtomicBool::new(false);
    send_listing_chunks_bounded(&tx, ctx, epoch, &shutdown, token, path, nodes);
}

fn send_listing_chunks_bounded(
    tx: &SyncSender<ListingOutcome>,
    ctx: &egui::Context,
    epoch: &ListingEpochGuard,
    shutdown: &std::sync::atomic::AtomicBool,
    token: u64,
    path: PathBuf,
    nodes: Vec<TreeNode>,
) {
    let mut iter = nodes.into_iter().peekable();
    if iter.peek().is_none() {
        if epoch.is_stale() {
            return;
        }
        let sent = send_listing_outcome(
            tx,
            ListingOutcome {
                epoch: epoch.requested,
                token,
                path,
                result: ListingResult::Chunk {
                    nodes: Vec::new(),
                    done: true,
                },
            },
            epoch,
            shutdown,
        );
        if sent {
            ctx.request_repaint();
        }
        return;
    }

    while iter.peek().is_some() {
        // 청크마다 stale 확인 — 구 epoch 결과를 unbounded 채널에 계속 밀어넣지 않는다.
        if epoch.is_stale() {
            return;
        }
        let mut chunk = Vec::with_capacity(LISTING_CHUNK_SIZE);
        for _ in 0..LISTING_CHUNK_SIZE {
            let Some(node) = iter.next() else {
                break;
            };
            chunk.push(node);
        }
        let done = iter.peek().is_none();
        // send 직전 재확인 — 확인과 send를 원자화할 수는 없어 취소 직후 stale 청크가
        // **최대 1개** 들어갈 수 있지만(유계), 수신측 epoch/token 필터가 버린다.
        if epoch.is_stale() {
            return;
        }
        let sent = send_listing_outcome(
            tx,
            ListingOutcome {
                epoch: epoch.requested,
                token,
                path: path.clone(),
                result: ListingResult::Chunk { nodes: chunk, done },
            },
            epoch,
            shutdown,
        );
        if !sent {
            break;
        }
        ctx.request_repaint();
    }
}

#[cfg(test)]
fn read_children(
    path: &Path,
    root: Option<&Path>,
    ignore_cache: &GitIgnoreCache,
) -> std::io::Result<Vec<TreeNode>> {
    read_children_guarded(path, root, ignore_cache, None)
}

fn read_children_guarded(
    path: &Path,
    root: Option<&Path>,
    ignore_cache: &GitIgnoreCache,
    guard: Option<(&ListingEpochGuard, &std::sync::atomic::AtomicBool)>,
) -> std::io::Result<Vec<TreeNode>> {
    let mut nodes = Vec::new();
    for entry in std::fs::read_dir(path)? {
        if guard.is_some_and(|(epoch, shutdown)| {
            epoch.is_stale() || shutdown.load(std::sync::atomic::Ordering::Acquire)
        }) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "listing cancelled",
            ));
        }
        let entry = entry?;
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let entry_path = entry.path();
        if root.is_some_and(|_| ignore_cache.is_ignored(&entry_path, is_dir)) {
            continue;
        }
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

/// 정렬: 디렉터리 우선 + 이름 (단순 유니코드 순 — §3, 로케일 비교는 비목표).
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
    let ignore_cache = GitIgnoreCache::default();
    let mut fresh = read_children(base, None, &ignore_cache)?;
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

        let segments = workspace_summary_segments(summary, egui::Color32::GRAY);
        let text = segments
            .into_iter()
            .map(|(text, _)| text)
            .collect::<String>();
        assert_eq!(text, "1 실행 중 · 2 입력 대기 · 완료 1 · 오류 1 · 유휴 1");
    }

    #[test]
    fn 상태없는_워크스페이스는_유휴_복원세션은_비활성으로_표시한다() {
        let weak = egui::Color32::GRAY;
        assert_eq!(
            workspace_summary_segments(SidebarSessionSummary::default(), weak)[0].0,
            "유휴"
        );
        assert_eq!(
            workspace_summary_segments(SidebarSessionSummary::inactive(3), weak)[0].0,
            "비활성"
        );
    }

    /// 안정성 감사 High #2: 워커가 stale epoch(루트 전환/refresh 후) 청크를
    /// bounded 결과 채널에 stale 청크를 밀어넣지 않는다 — 송신 전에 중단.
    #[test]
    fn stale_epoch_청크는_송신전에_중단된다() {
        let (tx, rx) = sync_channel(8);
        let ctx = egui::Context::default();
        let shared = Arc::new(std::sync::atomic::AtomicU64::new(7)); // 현재 epoch=7
        let mk_nodes = || -> Vec<TreeNode> {
            (0..5000)
                .map(|i| TreeNode::new(format!("f{i}"), false))
                .collect()
        };
        // 구 epoch(6)으로 전송 시도 → 아무 청크도 채널에 없어야 한다.
        send_listing_chunks(
            tx.clone(),
            &ctx,
            &ListingEpochGuard {
                requested: 6,
                current: Arc::clone(&shared),
                cancel: Arc::default(),
            },
            1,
            PathBuf::from("/tmp/x"),
            mk_nodes(),
        );
        assert!(rx.try_recv().is_err(), "stale 청크가 채널에 들어감");
        // 현재 epoch(7)이면 정상 전송.
        send_listing_chunks(
            tx,
            &ctx,
            &ListingEpochGuard {
                requested: 7,
                current: shared,
                cancel: Arc::default(),
            },
            2,
            PathBuf::from("/tmp/x"),
            mk_nodes(),
        );
        assert!(rx.try_recv().is_ok(), "현재 epoch 청크가 전송돼야 함");
    }

    /// codex High(2026-07-08): 같은 경로 재요청(reload_dir token 교체) 시 구 요청의
    /// cancel 플래그가 서면 같은 epoch이어도 송신 전에 중단된다.
    #[test]
    fn cancel_플래그는_같은_epoch에서도_송신을_중단한다() {
        let (tx, rx) = sync_channel(8);
        let ctx = egui::Context::default();
        let shared = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(true)); // 이미 취소됨
        send_listing_chunks(
            tx,
            &ctx,
            &ListingEpochGuard {
                requested: 1,
                current: shared,
                cancel,
            },
            1,
            PathBuf::from("/tmp/x"),
            (0..100)
                .map(|i| TreeNode::new(format!("f{i}"), false))
                .collect(),
        );
        assert!(rx.try_recv().is_err(), "취소된 요청의 청크가 채널에 들어감");
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
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            tree.pump_listings();
            if tree.pending_listings.is_empty() {
                tree.pump_listings();
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for async file-tree listing"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn pump_listings_for(tree: &mut FileTreeUi, duration: std::time::Duration) {
        let deadline = std::time::Instant::now() + duration;
        while std::time::Instant::now() < deadline {
            tree.pump_listings();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        tree.pump_listings();
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
        assert!(
            tree.pending_listings.contains_key(&root_a),
            "set_root은 read_dir 완료를 기다리지 않고 listing 요청만 등록한다"
        );
        assert!(tree.children.is_none());

        tree.set_root(Some(root_b.clone()));
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
    fn collapse_후_도착한_listing_result는_폐기된다() {
        let base = temp_root("async-collapse-stale");
        std::fs::create_dir_all(base.join("d")).unwrap();
        std::fs::write(base.join("d/child.txt"), b"child").unwrap();

        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);

        tree.toggle_dir(&base.join("d"));
        assert!(tree.pending_listings.contains_key(&base.join("d")));
        tree.toggle_dir(&base.join("d"));
        assert!(
            !tree.pending_listings.contains_key(&base.join("d")),
            "collapse는 in-flight dir listing token을 무효화한다"
        );

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

        // 실제 OS 워처 대신 채널을 주입해 스로틀 로직만 검증한다
        let (tx, rx) = std::sync::mpsc::channel();
        tree.watch_rx = Some(rx);
        std::fs::write(base.join("d/a.txt"), b"a").unwrap();
        tx.send(WatchEvent::DirtyDir(base.join("d"))).unwrap();
        tx.send(WatchEvent::DirtyDir(base.join("d"))).unwrap(); // 폭주 중 중복 이벤트

        // 창 안 (방금 재나열한 상태): 흡수만 — 재나열 없음, dedup 확인
        tree.last_watch_reload = std::time::Instant::now();
        let ctx = egui::Context::default();
        tree.pump_watch_events(&ctx);
        assert!(
            !tree.flat.iter().any(|r| r.name == "a.txt"),
            "창 내에는 재나열하지 않는다"
        );
        assert_eq!(tree.pending_watch.len(), 1, "pending은 dedup 집합");

        // 창 내 반복 호출(프레임 폭주 시뮬레이션)에도 여전히 재나열 없음
        tx.send(WatchEvent::DirtyDir(base.join("d"))).unwrap();
        tree.pump_watch_events(&ctx);
        assert!(!tree.flat.iter().any(|r| r.name == "a.txt"));

        // 창 경과 → 재나열 요청 1회, pending 소진. 실제 read_dir 적용은 async.
        tree.last_watch_reload = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(WATCH_TEST_ELAPSED_MS))
            .expect("테스트 프로세스 기동 후라 언더플로 없음");
        tree.pump_watch_events(&ctx);
        assert!(!tree.flat.iter().any(|r| r.name == "a.txt"));
        assert!(tree.pending_watch.is_empty());
        drain_listings(&mut tree);
        assert!(
            tree.flat.iter().any(|r| r.name == "a.txt"),
            "async listing 적용 후 반영"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 스로틀 창(300ms)보다 확실히 큰 경과값.
    const WATCH_TEST_ELAPSED_MS: u64 = FileTreeUi::WATCH_RELOAD_MS + 50;

    #[test]
    fn 접힘_상태에서도_panel이_채널을_소비한다() {
        let base = temp_root("collapsed-drain");
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);
        tree.collapsed = true;

        // 워처 채널 주입 + 새 파일 이벤트 (창 경과 상태)
        let (tx, rx) = std::sync::mpsc::channel();
        tree.watch_rx = Some(rx);
        std::fs::write(base.join("new.txt"), b"n").unwrap();
        tx.send(WatchEvent::DirtyDir(base.clone())).unwrap();
        tree.last_watch_reload = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(WATCH_TEST_ELAPSED_MS))
            .unwrap();
        // 백그라운드 op 결과도 대기 중 (완료 미처리 = in_flight/에러 방치 버그 검증)
        tree.in_flight = 1;
        tree.ops_tx
            .send(OpOutcome {
                refresh: Vec::new(),
                error: Some("op 에러".to_owned()),
                confirm_delete: None,
            })
            .unwrap();

        // 접힘 상태로 panel 호출 — 렌더는 생략돼도 채널은 소비돼야 한다 (codex Med-2)
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let sidebar = SidebarSnapshot {
            active_workspace_id: "default",
            workspaces: &[],
            view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
            inbox_count: 0,
        };
        egui::__run_test_ui(|ui| {
            assert!(tree.panel(ui, &[], &sidebar, &catalog).is_none());
        });
        drain_listings(&mut tree);

        assert!(
            tree.flat.iter().any(|r| r.name == "new.txt"),
            "접힘 중에도 워처 이벤트가 반영된다"
        );
        assert!(tree.pending_watch.is_empty(), "채널 백로그 없음");
        assert_eq!(tree.in_flight, 0, "op 완료가 처리된다");
        assert_eq!(tree.error.as_deref(), Some("op 에러"));
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
    fn watcher_기본_ignore_rules는_generated_경로를_버린다() {
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
                watch_events_for_path(&root, &path, true, &[]).is_empty(),
                "{name} should be ignored"
            );
        }
        assert!(
            watch_events_for_path(&root, &root.join(".DS_Store"), true, &[]).is_empty(),
            ".DS_Store should be ignored"
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
    fn gitignore_matcher는_listing과_nested_rules에_적용된다() {
        let base = temp_root("gitignore-listing");
        std::fs::write(base.join(".gitignore"), "root-ignored.txt\nbuild/\n").unwrap();
        std::fs::create_dir_all(base.join(".git/info")).unwrap();
        std::fs::write(base.join(".git/info/exclude"), "info.log\n").unwrap();
        std::fs::write(base.join("root-ignored.txt"), b"x").unwrap();
        std::fs::write(base.join("info.log"), b"x").unwrap();
        std::fs::create_dir_all(base.join("build")).unwrap();
        std::fs::write(base.join("build/output.txt"), b"x").unwrap();
        std::fs::create_dir_all(base.join("nested")).unwrap();
        std::fs::write(base.join("nested/.gitignore"), "*.tmp\n").unwrap();
        std::fs::write(base.join("nested/keep.rs"), b"k").unwrap();
        std::fs::write(base.join("nested/skip.tmp"), b"s").unwrap();

        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        drain_listings(&mut tree);

        assert!(tree.flat.iter().any(|r| r.name == "nested"));
        assert!(!tree.flat.iter().any(|r| r.name == "root-ignored.txt"));
        assert!(!tree.flat.iter().any(|r| r.name == "info.log"));
        assert!(!tree.flat.iter().any(|r| r.name == "build"));

        tree.toggle_dir(&base.join("nested"));
        drain_listings(&mut tree);
        assert!(tree.flat.iter().any(|r| r.name == "keep.rs"));
        assert!(!tree.flat.iter().any(|r| r.name == "skip.tmp"));

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn watcher_gitignore_rules는_dirty_event를_버린다() {
        let base = temp_root("gitignore-watch");
        std::fs::write(base.join(".gitignore"), "*.tmp\nignored-dir/\n").unwrap();
        std::fs::write(base.join("skip.tmp"), b"x").unwrap();
        std::fs::create_dir_all(base.join("ignored-dir")).unwrap();
        std::fs::write(base.join("ignored-dir/file.rs"), b"x").unwrap();
        std::fs::write(base.join("keep.rs"), b"k").unwrap();

        let ignore_cache = GitIgnoreCache::default();
        ignore_cache.reset(Some(&base));
        assert!(
            watch_events_for_path_with_ignore(
                &base,
                &base.join("skip.tmp"),
                true,
                &[],
                &ignore_cache
            )
            .is_empty()
        );
        assert!(
            watch_events_for_path_with_ignore(
                &base,
                &base.join("ignored-dir/file.rs"),
                true,
                &[],
                &ignore_cache
            )
            .is_empty()
        );
        assert!(
            !watch_events_for_path_with_ignore(
                &base,
                &base.join("keep.rs"),
                true,
                &[],
                &ignore_cache
            )
            .is_empty()
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

        let (tx, rx) = std::sync::mpsc::channel();
        tree.watch_rx = Some(rx);
        for event in watch_events_for_path(&base, &env, false, &[]) {
            tx.send(event).unwrap();
        }
        tree.last_watch_reload = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(WATCH_TEST_ELAPSED_MS))
            .unwrap();
        tree.pump_watch_events(&egui::Context::default());

        assert_eq!(tree.take_env_warning_candidates(), vec![env]);
        assert!(tree.take_env_warning_candidates().is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn watcher_dirty_dir_batch는_한_프레임_invalidation을_제한한다() {
        let mut tree = FileTreeUi::new(egui::Context::default());
        let (tx, rx) = std::sync::mpsc::channel();
        tree.watch_rx = Some(rx);
        let total = FileTreeUi::WATCH_RELOAD_DIRS_PER_BATCH + 3;
        for i in 0..total {
            tx.send(WatchEvent::DirtyDir(PathBuf::from(format!("dir-{i:02}"))))
                .unwrap();
        }

        tree.last_watch_reload = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(WATCH_TEST_ELAPSED_MS))
            .unwrap();
        tree.pump_watch_events(&egui::Context::default());

        assert_eq!(
            tree.pending_watch.len(),
            total - FileTreeUi::WATCH_RELOAD_DIRS_PER_BATCH
        );
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

    /// 파일 행 더블클릭 → 연결 프로그램 열기(OpenExternal) 회귀 (2026-07-18).
    /// 실제 open 실행은 App 쪽 처리라 여기서는 반환 액션만 검증한다 — 허용
    /// 확장자(pdf)는 액션을 내고, 실행 위험군(sh)은 아무것도 내지 않는다.
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
        // step_dt를 더블클릭 판정 한계(0.3s) 아래로 — 클릭 2번이 한 스텝 간격으로 온다.
        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(0.05)
            .build_ui_state(
                |ui, state: &mut (FileTreeUi, Vec<SidebarAction>)| {
                    if let Some(a) = state.0.panel(ui, &[], &catalog) {
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
        // 실행 위험군(.sh) 더블클릭 — 아무 액션도 내지 않는다.
        harness.get_by_label("run.sh").click();
        harness.step();
        harness.get_by_label("run.sh").click();
        harness.step();
        assert!(harness.state().1.is_empty(), "sh 더블클릭이 액션을 냄");
        // 시뮬레이션 시간 경과 — 직전 클릭 연쇄를 끊는다 (egui triple 판정 창 0.6s는
        // 마지막 클릭과의 거리만 보므로, 붙여서 클릭하면 pdf 2번째가 triple로 잡힌다).
        for _ in 0..15 {
            harness.step();
        }
        // 허용 확장자(.pdf) 더블클릭 → OpenExternal(경로).
        harness.get_by_label("a.pdf").click();
        harness.step();
        harness.get_by_label("a.pdf").click();
        harness.step();
        let opened = harness.state().1.iter().find_map(|a| match a {
            SidebarAction::OpenExternal(p) => Some(p.clone()),
            _ => None,
        });
        assert_eq!(opened.as_deref(), Some(base.join("a.pdf").as_path()));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
