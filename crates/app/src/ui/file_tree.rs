//! 폴더 트리 사이드바 (docs/file-tree-design.md).
//!
//! 리소스 3원칙(§3): lazy `read_dir`(펼친 노드만), flat 평탄화 + `show_rows` 가상화,
//! IO는 상호작용 시점만 — 유휴 시 repaint를 유발하지 않는다. 로컬 파일 IO는
//! config/DB처럼 앱 소관이라 `std::fs` 직접 사용(§2, remote는 후속 trait 추상화 지점).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

/// 사이드바 세션 목록 항목 (§6 확장 — 좌측 패널은 트리+세션의 workspace 사이드바다,
/// 2026-07-05). App이 WorkspaceUi 스냅샷에서 조립해 넘긴다.
pub struct SessionEntry {
    pub tab: runtime::MuxTabId,
    pub pane: runtime::MuxPaneId,
    pub title: String,
    /// agent 감지 상태 (Running/Waiting/NeedsApproval/Done/Error). 셸은 항상 None —
    /// status 감지는 agent만(§PR-12). 세션 행 좌측 상태 레일 색으로 그린다.
    pub status: Option<runtime::SessionStatus>,
    /// 최신 화면 요약 (마지막 비어있지 않은 행 — 2026-07-05)
    pub summary: String,
    pub focused: bool,
}

/// 사이드바에서 App으로 올라가는 액션.
pub enum SidebarAction {
    /// 경로를 포커스된 터미널에 삽입 (FT-3)
    InsertPath(PathBuf),
    /// 세션 목록에서 선택 — 해당 tab/pane으로 전환
    FocusSession {
        tab: runtime::MuxTabId,
        pane: runtime::MuxPaneId,
    },
    /// 새 셸 생성 (세션 섹션의 + 버튼)
    NewShell,
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
    /// 사이드바 접힘 (Panel 폭만 줄인다 — 상태/캐시는 유지).
    collapsed: bool,
    /// 마지막 조작 에러 (하단 빨간 라벨, §4).
    error: Option<String>,
    /// 백그라운드 파일 조작(EXDEV copy 등 §9-3)의 완료/에러 채널.
    ops_tx: Sender<OpOutcome>,
    ops_rx: Receiver<OpOutcome>,
    /// 백그라운드 디렉터리 listing 결과 채널. read_dir/sort는 worker에서 수행한다.
    listing_tx: Sender<ListingOutcome>,
    listing_rx: Receiver<ListingOutcome>,
    /// root 전환 generation. 이전 root의 late result는 epoch mismatch로 폐기한다.
    listing_epoch: u64,
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

enum ListingResult {
    Chunk { nodes: Vec<TreeNode>, done: bool },
    Error(String),
}

struct PendingListing {
    token: u64,
    /// refresh/reload 시작 시점의 펼침 상태. 결과 적용 직전의 현재 상태가 없을 때 fallback.
    preserve_expanded: Arc<HashSet<PathBuf>>,
    /// 첫 chunk 적용 시점의 현재 펼침 상태. 이후 chunk는 같은 기준으로 append한다.
    apply_expanded: Option<Arc<HashSet<PathBuf>>>,
    started: bool,
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
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
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
}

impl FileTreeUi {
    pub fn new(egui_ctx: egui::Context) -> Self {
        let (ops_tx, ops_rx) = std::sync::mpsc::channel();
        let (listing_tx, listing_rx) = std::sync::mpsc::channel();
        Self {
            root: None,
            root_error: None,
            children: None,
            flat: Vec::new(),
            show_hidden: false,
            collapsed: false,
            error: None,
            ops_tx,
            ops_rx,
            listing_tx,
            listing_rx,
            listing_epoch: 0,
            next_listing_token: 0,
            pending_listings: HashMap::new(),
            in_flight: 0,
            egui_ctx,
            edit: None,
            confirm_delete: None,
            watcher: None,
            watched_dirs: std::collections::HashSet::new(),
            watch_rx: None,
            measured_row_height: None,
            watch_ignore: std::sync::Arc::new(Vec::new()),
            ignore_cache: GitIgnoreCache::default(),
            watch_show_hidden: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_watch: BTreeSet::new(),
            env_warning_candidates: BTreeSet::new(),
            last_watch_reload: std::time::Instant::now(),
        }
    }

    /// 루트 교체 (workspace 전환/경로 변경). 캐시를 버리고 루트만 다시 나열한다.
    /// 루트는 canonicalize해 보관한다 — 트리의 모든 행 경로가 canonical 기준이 되어
    /// 이동 가드(§9-4)·부분 재나열의 경로 비교가 일관된다.
    /// 워처 무시 prefix 설정 (앱 data dir 등). set_root 이전에 호출.
    pub fn set_watch_ignore(&mut self, prefixes: Vec<PathBuf>) {
        self.watch_ignore = std::sync::Arc::new(prefixes);
    }

    /// 워처가 감지한 `.env*` 변경 후보를 꺼낸다. Project Environment UI 경고 배선은
    /// 후속 PR에서 붙이더라도, PR-U16에서는 이 state가 테스트 가능한 signal이다.
    #[allow(dead_code)]
    pub fn take_env_warning_candidates(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.env_warning_candidates)
            .into_iter()
            .collect()
    }

    pub fn set_root(&mut self, root: Option<PathBuf>) {
        self.listing_epoch = self.listing_epoch.wrapping_add(1);
        self.pending_listings.clear();
        self.root = root.map(|r| r.canonicalize().unwrap_or(r));
        self.root_error = None;
        self.children = None;
        self.flat.clear();
        self.error = None;
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
        self.watcher = None;
        self.watch_rx = None;
        self.watched_dirs.clear();
        let Some(root) = self.root.clone() else {
            return;
        };
        if self.root_error.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel::<WatchEvent>();
        let ctx = self.egui_ctx.clone();
        let ignore = std::sync::Arc::clone(&self.watch_ignore);
        let ignore_cache = self.ignore_cache.clone();
        let show_hidden = std::sync::Arc::clone(&self.watch_show_hidden);
        let watch_root = root.clone();
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
                        let _ = tx.send(event);
                        sent = true;
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
            Err(e) => tracing::warn!("파일 감시자 생성 실패 (수동 새로고침으로 동작): {e}"),
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
    /// **끝까지 비워** pending 집합에 흡수하고(백로그 방지), 실제 재나열(reread 재귀)은
    /// 마지막 일괄 후 WATCH_RELOAD_MS 경과 시에만 수행한다 — 터미널 출력으로 프레임이
    /// 계속 돌면서 파일 이벤트가 쏟아져도 재나열은 최대 ~3.3Hz.
    fn pump_watch_events(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.watch_rx {
            while let Ok(event) = rx.try_recv() {
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
            .default_size(240.0)
            // 최소폭 확보 — 너무 좁히면 세션 행(점·글리프·요약)이 깨져 보였다
            // (2026-07-06 사용자 화면). 접기는 별도 토글(◂)로 처리, 폭은 160px까지만.
            .size_range(egui::Rangef::new(160.0, f32::INFINITY))
            .show(ui, |ui| self.contents(ui, sessions, catalog))
            .inner
    }

    /// 루트 경로 표시용 — 홈은 `~`로 축약.
    fn display_root(&self) -> String {
        let Some(root) = &self.root else {
            return String::new();
        };
        let home = std::env::var_os("HOME").map(PathBuf::from);
        match home.as_deref().and_then(|h| root.strip_prefix(h).ok()) {
            Some(rel) if rel.as_os_str().is_empty() => "~".to_owned(),
            Some(rel) => format!("~/{}", rel.display()),
            None => root.display().to_string(),
        }
    }

    fn contents(
        &mut self,
        ui: &mut egui::Ui,
        sessions: &[SessionEntry],
        catalog: &i18n::Catalog,
    ) -> Option<SidebarAction> {
        // (워처/백그라운드 채널 수거는 panel()이 접힘 여부와 무관하게 이미 수행했다)
        let mut action: Option<SidebarAction> = None;

        // ── 세션 목록 (workspace 사이드바 §6 확장, 2026-07-05) ──
        // 현재 workspace의 셸/에이전트를 나열하고 클릭으로 전환한다.
        if !sessions.is_empty() {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.weak(catalog.t("file_tree.sessions", &[]));
                if ui
                    .small_button("+")
                    .on_hover_text(catalog.t("workspace.new_shell", &[]))
                    .clicked()
                {
                    action = Some(SidebarAction::NewShell);
                }
            });
            // 세션이 많으면 목록이 패널을 다 먹고 아래로 넘쳐 잘렸다 (2026-07-05 사용자
            // 보고). 세션 목록은 패널 높이의 절반까지만 쓰고 그 안에서 스크롤, 나머지는
            // 아래 파일 트리가 갖는다. auto_shrink[_, true]로 세션이 적으면 줄어든다.
            let session_max_h = (ui.available_height() * 0.5).max(80.0);
            egui::ScrollArea::vertical()
                .id_salt("session_list_scroll")
                .max_height(session_max_h)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for entry in sessions {
                        let resp = session_row(ui, entry);
                        if resp.clicked() && !entry.focused {
                            action = Some(SidebarAction::FocusSession {
                                tab: entry.tab.clone(),
                                pane: entry.pane.clone(),
                            });
                        }
                    }
                });
            ui.add_space(4.0);
            crate::ui::hairline_full(ui);
        }

        // 헤더: 현재 루트 경로(~ 축약) + 새로고침/숨김 토글/접기 (§6). 헤더 전체가
        // 루트로의 드롭 대상이다 (§4 — 루트 영역 dnd_drop_zone).
        let display_root = self.display_root();
        // 헤더는 dnd_drop_zone을 쓰지 않는다 — 그 API는 항상 inactive.bg_stroke로
        // 프레임 박스를 그려 네모 라인이 보였다(#74). 수동 rect 기반 드롭으로 대체.
        let header_scope = ui.scope(|ui| {
            ui.horizontal(|ui| {
                // 루트 폴더 아이콘 — 도형 (이모지 □ 깨짐 회피)
                let (fr, _) = ui.allocate_exact_size(egui::vec2(18.0, 16.0), egui::Sense::hover());
                paint_folder(ui.painter(), fr.center(), ui.visuals().weak_text_color());
                // 우측 컨트롤(접기/새로고침/숨김) 폭을 예약 — 긴 경로가 버튼을
                // 밀어내지 않게 truncate 라벨의 최대폭을 제한한다 (codex P2).
                ui.scope(|ui| {
                    ui.set_max_width((ui.available_width() - 80.0).max(40.0));
                    ui.add(
                        egui::Label::new(egui::RichText::new(&display_root).strong()).truncate(),
                    )
                    .on_hover_text(&display_root);
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // 아이콘 3종 전부 18x18 painter 셀로 통일 (#74)
                    // 접기: ◂는 폰트에 없어 □로 깨진다 — 도형 캐럿
                    let (cr, collapse) =
                        ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
                    let ccol = if collapse.hovered() {
                        ui.visuals().text_color()
                    } else {
                        ui.visuals().weak_text_color()
                    };
                    {
                        let c = cr.center();
                        let d = 4.0;
                        ui.painter().add(egui::Shape::convex_polygon(
                            vec![
                                egui::pos2(c.x + d * 0.6, c.y - d),
                                egui::pos2(c.x + d * 0.6, c.y + d),
                                egui::pos2(c.x - d * 0.8, c.y),
                            ],
                            ccol,
                            egui::Stroke::NONE,
                        ));
                    }
                    if collapse
                        .on_hover_text(catalog.t("file_tree.collapse_sidebar", &[]))
                        .clicked()
                    {
                        self.collapsed = true;
                    }
                    // 새로고침 ⟳ — 18x18 셀 중앙
                    let (rr, refresh) =
                        ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
                    let rcol = if refresh.hovered() {
                        ui.visuals().text_color()
                    } else {
                        ui.visuals().weak_text_color()
                    };
                    ui.painter().text(
                        rr.center(),
                        egui::Align2::CENTER_CENTER,
                        "⟳",
                        egui::FontId::proportional(14.0),
                        rcol,
                    );
                    if refresh
                        .on_hover_text(catalog.t("file_tree.refresh", &[]))
                        .clicked()
                    {
                        self.refresh();
                    }
                    // 숨김 토글 — 텍스트 대신 눈 아이콘, 켜짐이면 accent (#74)
                    let (er, eye) =
                        ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
                    let ecol = if self.show_hidden {
                        ui.visuals().selection.bg_fill
                    } else if eye.hovered() {
                        ui.visuals().text_color()
                    } else {
                        ui.visuals().weak_text_color()
                    };
                    paint_eye(ui.painter(), er.center(), ecol);
                    if eye
                        .on_hover_text(catalog.t("file_tree.show_hidden", &[]))
                        .clicked()
                    {
                        self.show_hidden = !self.show_hidden;
                        // 워처 콜백 스레드와 동기화 (숨김 이벤트 필터)
                        self.watch_show_hidden
                            .store(self.show_hidden, std::sync::atomic::Ordering::Relaxed);
                        self.rebuild_flat();
                    }
                });
            });
        });
        // 헤더 전체 폭 = 루트로의 드롭 대상 (§4). 드래그 중 hover면 강조 스트로크.
        let header_rect = egui::Rect::from_min_max(
            egui::pos2(ui.max_rect().left(), header_scope.response.rect.min.y),
            egui::pos2(ui.max_rect().right(), header_scope.response.rect.max.y),
        );
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
        crate::ui::hairline_full(ui);

        if self.root.is_none() {
            // path 미설정 (§9-2 backfill 강제 없음) — 트리 대신 안내
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
        let row_height = self
            .measured_row_height
            .unwrap_or_else(|| ui.text_style_height(&egui::TextStyle::Body));
        let total = self.flat.len();
        let mut toggle: Option<PathBuf> = None;
        let mut drop_action: Option<(PathBuf, PathBuf)> = None; // (src, dst_dir)
        let mut observed_row_height: Option<f32> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, row_height, total, |ui, range| {
                for row in &self.flat[range] {
                    // 이름 변경 중인 행은 인라인 TextEdit로 대체 (FT-3, §9-8)
                    if let Some(EditState::Rename {
                        path,
                        buffer,
                        focus,
                    }) = &mut edit
                        && path == &row.path
                    {
                        ui.horizontal(|ui| {
                            ui.add_space(row.depth as f32 * 12.0);
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
                            3.0,
                            ui.visuals().widgets.hovered.weak_bg_fill,
                        );
                    }

                    // 행 전체 = 드래그 소스 (payload = 절대 경로, §4). Id는 path 기반(§9-6).
                    let drag_id = egui::Id::new(("file_tree_row", &row.path));
                    let egui::InnerResponse {
                        inner: label_resp,
                        response,
                    } = ui.dnd_drag_source(drag_id, row.path.clone(), |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 5.0;
                            ui.add_space(row.depth as f32 * 12.0);
                            // 캐럿+폴더/파일 아이콘을 도형으로 (이모지 □ 깨짐 회피, 목업 §트리)
                            let icon_col = ui.visuals().weak_text_color();
                            let carve = ui.visuals().extreme_bg_color;
                            let (cr, _) = ui
                                .allocate_exact_size(egui::vec2(10.0, 16.0), egui::Sense::hover());
                            if row.is_dir {
                                paint_caret(ui.painter(), cr.center(), row.expanded, icon_col);
                            }
                            let (ir, _) = ui
                                .allocate_exact_size(egui::vec2(17.0, 16.0), egui::Sense::hover());
                            if row.is_dir {
                                paint_folder(ui.painter(), ir.center(), icon_col);
                            } else {
                                let fc = if row.name.starts_with('.') {
                                    ui.visuals().weak_text_color()
                                } else {
                                    egui::Color32::from_rgb(0xc8, 0xcc, 0xd2)
                                };
                                paint_file(ui.painter(), ir.center(), fc, carve);
                            }
                            let mut rich = egui::RichText::new(&row.name);
                            if row.name.starts_with('.') {
                                rich = rich.weak().italics();
                            }
                            ui.add(
                                egui::Label::new(rich)
                                    .sense(egui::Sense::click())
                                    .truncate(),
                            )
                        })
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
                    if row_resp.drag_started() {
                        row_resp.dnd_set_drag_payload(row.path.clone());
                    }
                    // 행높이 실측 (드래그 중엔 행이 tooltip 레이어로 빠져 rect가 다름 — 제외)
                    if observed_row_height.is_none()
                        && !egui::DragAndDrop::has_any_payload(ui.ctx())
                    {
                        observed_row_height = Some(response.rect.height());
                    }
                    if row.is_dir {
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
                        if row_resp.clicked() || label_resp.clicked() {
                            toggle = Some(row.path.clone());
                        }
                    }
                    // 우클릭 컨텍스트 메뉴 (FT-3) — 행 전체에서 열리게 row_resp에 단다
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
                    });
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
        if let Some(path) = toggle {
            self.toggle_dir(&path);
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
        self.pending_listings.insert(
            path.clone(),
            PendingListing {
                token,
                preserve_expanded,
                apply_expanded: None,
                started: false,
            },
        );
        spawn_listing_worker(
            self.listing_tx.clone(),
            self.egui_ctx.clone(),
            self.listing_epoch,
            token,
            self.root.clone(),
            path,
            self.ignore_cache.clone(),
        );
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
        self.pending_listings
            .retain(|pending_path, _| !pending_path.starts_with(path));
    }

    fn pump_listings(&mut self) {
        let mut processed = 0;
        while processed < LISTING_RESULTS_PER_FRAME {
            let Ok(outcome) = self.listing_rx.try_recv() else {
                return;
            };
            self.apply_listing_outcome(outcome);
            processed += 1;
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
            ListingResult::Error(error) => {
                self.pending_listings.remove(&outcome.path);
                self.apply_listing_error(&outcome.path, error);
            }
        }
    }

    fn apply_listing_chunk(&mut self, path: PathBuf, nodes: Vec<TreeNode>, done: bool) {
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

    fn apply_listing_error(&mut self, path: &Path, error: String) {
        let Some(root) = self.root.clone() else {
            return;
        };
        if path == root {
            self.children = None;
            self.root_error = Some(format!("루트 나열 실패: {error}"));
            self.rebuild_flat();
            return;
        }
        if let Ok(rel) = path.strip_prefix(&root)
            && let Some(node) = self.children.as_mut().and_then(|c| node_mut(c, rel))
        {
            node.expanded = false;
            node.children = None;
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
fn session_row(ui: &mut egui::Ui, entry: &SessionEntry) -> egui::Response {
    let has_summary = !entry.summary.is_empty();
    let row_h = if has_summary { 38.0 } else { 24.0 };
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), row_h),
        egui::Sense::click(),
    );
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let visuals = ui.visuals();
    let accent = visuals.selection.bg_fill;
    let dot = session_status_color(entry.status, visuals);
    let painter = ui.painter();

    // 선택/hover 배경
    if entry.focused {
        painter.rect_filled(rect, 4.0, accent.gamma_multiply(0.18));
    } else if resp.hovered() {
        painter.rect_filled(rect, 4.0, visuals.widgets.hovered.bg_fill);
    }
    // 좌측 상태 레일(2px) — 항상 표시, 상태 색으로 세로로 훑어 파악 (목업 §세션).
    // 점·타입 글리프는 제거하고 레일이 유일한 상태 표시다 (2026-07-06 사용자).
    let rail = egui::Rect::from_min_size(
        egui::pos2(rect.left(), rect.top() + 4.0),
        egui::vec2(2.0, row_h - 8.0),
    );
    painter.rect_filled(rail, 1.0, dot);
    let mid_y = rect.top() + if has_summary { 13.0 } else { row_h / 2.0 };
    // 타이틀
    let title_color = if entry.focused {
        accent
    } else {
        visuals.text_color()
    };
    painter.text(
        egui::pos2(rect.left() + 16.0, mid_y),
        egui::Align2::LEFT_CENTER,
        &entry.title,
        egui::FontId::proportional(13.0),
        title_color,
    );
    // 요약 (dim, mono, 길면 잘림)
    if has_summary {
        painter.text(
            egui::pos2(rect.left() + 16.0, rect.top() + 27.0),
            egui::Align2::LEFT_CENTER,
            truncate_chars(&entry.summary, 40),
            egui::FontId::monospace(10.5),
            visuals.weak_text_color().gamma_multiply(0.9),
        );
    }
    resp
}

/// 타입 글리프를 도형으로 그린다: agent=마름모(◆), shell=삼각형(▸). 폰트에 없는
/// 글리프(□ 깨짐)를 피하려 painter로 직접 그린다. `center` 중심, `d`≈3.5 반경.
pub(crate) fn paint_type_glyph(
    painter: &egui::Painter,
    center: egui::Pos2,
    is_agent: bool,
    color: egui::Color32,
) {
    let d = 3.5;
    let pts = if is_agent {
        vec![
            egui::pos2(center.x, center.y - d),
            egui::pos2(center.x + d, center.y),
            egui::pos2(center.x, center.y + d),
            egui::pos2(center.x - d, center.y),
        ]
    } else {
        vec![
            egui::pos2(center.x - 2.5, center.y - d),
            egui::pos2(center.x - 2.5, center.y + d),
            egui::pos2(center.x + 3.0, center.y),
        ]
    };
    painter.add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
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

/// pane 헤더 분할 아이콘 — 작은 사각형 + 가운데 분할선. horizontal=가로선(위/아래
/// 분할 표현), false=세로선. 클릭 Response 반환 (이모지 □ 깨짐 회피, 목업 §pane-head).
pub(crate) fn paint_split(ui: &mut egui::Ui, horizontal: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
    // pane 헤더는 테마 무관 항상 다크 — theme 색을 쓰면 light 테마에서 어두운 아이콘이
    // 다크 헤더에 묻힌다(codex Step4). ×와 같은 고정 밝은 회색을 쓴다.
    let col = if resp.hovered() {
        egui::Color32::from_rgb(0xc8, 0xcc, 0xd2)
    } else {
        egui::Color32::from_rgb(0x8b, 0x8f, 0x98)
    };
    let p = ui.painter();
    let sq = egui::Rect::from_center_size(rect.center(), egui::vec2(11.0, 11.0));
    p.rect_stroke(
        sq,
        1.5,
        egui::Stroke::new(1.0, col),
        egui::StrokeKind::Inside,
    );
    if horizontal {
        p.hline(sq.x_range(), sq.center().y, egui::Stroke::new(1.0, col));
    } else {
        p.vline(sq.center().x, sq.y_range(), egui::Stroke::new(1.0, col));
    }
    resp
}

/// 눈 아이콘 — 숨김 파일 토글 (#74). 아몬드형 윤곽 + 동공.
fn paint_eye(p: &egui::Painter, c: egui::Pos2, col: egui::Color32) {
    let s = egui::Stroke::new(1.3, col);
    let w = 6.5;
    let h = 4.2;
    // 위/아래 눈꺼풀 곡선을 짧은 선분으로 근사
    let mut top = Vec::new();
    let mut bot = Vec::new();
    for i in 0..=8 {
        let t = i as f32 / 8.0;
        let x = c.x - w + 2.0 * w * t;
        let dy = h * (std::f32::consts::PI * t).sin();
        top.push(egui::pos2(x, c.y - dy));
        bot.push(egui::pos2(x, c.y + dy));
    }
    p.add(egui::Shape::line(top, s));
    p.add(egui::Shape::line(bot, s));
    p.circle_filled(c, 1.8, col);
}

/// 폴더 아이콘 — 탭 + 본체 (채움).
fn paint_folder(p: &egui::Painter, c: egui::Pos2, col: egui::Color32) {
    let w = 15.0;
    let h = 11.0;
    let body = egui::Rect::from_center_size(egui::pos2(c.x, c.y + 1.0), egui::vec2(w, h));
    let tab = egui::Rect::from_min_size(
        egui::pos2(body.left(), body.top() - 3.0),
        egui::vec2(w * 0.45, 4.0),
    );
    p.rect_filled(tab, 1.5, col);
    p.rect_filled(body, 2.0, col);
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

/// 세션 상태 → 상태 점 색 (목업 상태 색과 일치). 유휴(None)는 흐린 회색.
pub(crate) fn session_status_color(
    status: Option<runtime::SessionStatus>,
    _visuals: &egui::Visuals,
) -> egui::Color32 {
    use runtime::SessionStatus as S;
    match status {
        Some(S::Running) => egui::Color32::from_rgb(0x43, 0xb8, 0xcd), // 실행(시안)
        Some(S::Waiting) => egui::Color32::from_rgb(0xd9, 0xb2, 0x6a), // 대기(노랑)
        Some(S::NeedsApproval) => egui::Color32::from_rgb(0xe0, 0xa8, 0x3e), // 승인(주황)
        Some(S::Done) => egui::Color32::from_rgb(0x6c, 0xc2, 0x6c),    // 완료(초록)
        Some(S::Error) => egui::Color32::from_rgb(0xe0, 0x5c, 0x53),   // 오류(빨강)
        // 유휴(작업완료·프롬프트 복귀, 살아있음) — 차분한 회색 (실행중 시안과 구분).
        Some(S::Idle) => egui::Color32::from_rgb(0x8b, 0x8f, 0x98),
        // status 미보고(첫 평가 전) — Idle과 같은 회색 fallback.
        None => egui::Color32::from_rgb(0x8b, 0x8f, 0x98),
    }
}

/// UTF-8 char 경계 기준 말줄임 (요약 표시용 — painter.text는 자동 truncate가 없다).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
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

fn spawn_listing_worker(
    tx: Sender<ListingOutcome>,
    ctx: egui::Context,
    epoch: u64,
    token: u64,
    root: Option<PathBuf>,
    path: PathBuf,
    ignore_cache: GitIgnoreCache,
) {
    std::thread::spawn(
        move || match read_children(&path, root.as_deref(), &ignore_cache) {
            Ok(nodes) => send_listing_chunks(tx, &ctx, epoch, token, path, nodes),
            Err(e) => {
                let _ = tx.send(ListingOutcome {
                    epoch,
                    token,
                    path,
                    result: ListingResult::Error(e.to_string()),
                });
                ctx.request_repaint();
            }
        },
    );
}

fn send_listing_chunks(
    tx: Sender<ListingOutcome>,
    ctx: &egui::Context,
    epoch: u64,
    token: u64,
    path: PathBuf,
    nodes: Vec<TreeNode>,
) {
    let mut iter = nodes.into_iter().peekable();
    if iter.peek().is_none() {
        let _ = tx.send(ListingOutcome {
            epoch,
            token,
            path,
            result: ListingResult::Chunk {
                nodes: Vec::new(),
                done: true,
            },
        });
        ctx.request_repaint();
        return;
    }

    while iter.peek().is_some() {
        let mut chunk = Vec::with_capacity(LISTING_CHUNK_SIZE);
        for _ in 0..LISTING_CHUNK_SIZE {
            let Some(node) = iter.next() else {
                break;
            };
            chunk.push(node);
        }
        let done = iter.peek().is_none();
        let sent = tx.send(ListingOutcome {
            epoch,
            token,
            path: path.clone(),
            result: ListingResult::Chunk { nodes: chunk, done },
        });
        ctx.request_repaint();
        if sent.is_err() {
            break;
        }
        std::thread::yield_now();
    }
}

fn read_children(
    path: &Path,
    root: Option<&Path>,
    ignore_cache: &GitIgnoreCache,
) -> std::io::Result<Vec<TreeNode>> {
    let mut nodes = Vec::new();
    for entry in std::fs::read_dir(path)? {
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
        egui::__run_test_ui(|ui| {
            assert!(tree.panel(ui, &[], &catalog).is_none());
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
}
