//! 폴더 트리 사이드바 (docs/file-tree-design.md).
//!
//! 리소스 3원칙(§3): lazy `read_dir`(펼친 노드만), flat 평탄화 + `show_rows` 가상화,
//! IO는 상호작용 시점만 — 유휴 시 repaint를 유발하지 않는다. 로컬 파일 IO는
//! config/DB처럼 앱 소관이라 `std::fs` 직접 사용(§2, remote는 후속 trait 추상화 지점).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

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
    /// 워처 이벤트로 재나열할 부모 디렉터리 채널 (워처 스레드 → UI).
    watch_rx: Option<Receiver<PathBuf>>,
    /// 워처가 무시할 경로 prefix들 — 앱 자신의 data/log 디렉터리 등. 자기 로그 쓰기가
    /// 이벤트로 돌아와 리페인트를 유발하는 자기-루프 차단 (리페인트 원인 조사 2026-07-04).
    watch_ignore: std::sync::Arc<Vec<PathBuf>>,
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
    pending_watch: std::collections::HashSet<PathBuf>,
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
            in_flight: 0,
            egui_ctx,
            edit: None,
            confirm_delete: None,
            watcher: None,
            watched_dirs: std::collections::HashSet::new(),
            watch_rx: None,
            measured_row_height: None,
            watch_ignore: std::sync::Arc::new(Vec::new()),
            watch_show_hidden: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_watch: std::collections::HashSet::new(),
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

    pub fn set_root(&mut self, root: Option<PathBuf>) {
        self.root = root.map(|r| r.canonicalize().unwrap_or(r));
        self.root_error = None;
        self.children = None;
        self.flat.clear();
        self.error = None;
        self.edit = None;
        self.confirm_delete = None;
        self.pending_watch.clear();
        self.refresh();
        // 루트가 유효할 때만 감시 시작 (FT-4). 실패는 경고 로그 — 수동 새로고침으로 동작.
        self.start_watcher();
    }

    /// 워처 일괄 재나열 최소 간격(ms) — 이벤트·프레임이 동시에 폭주해도 재나열은 ~3.3Hz.
    const WATCH_RELOAD_MS: u64 = 300;

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
        let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
        let ctx = self.egui_ctx.clone();
        let ignore = std::sync::Arc::clone(&self.watch_ignore);
        let show_hidden = std::sync::Arc::clone(&self.watch_show_hidden);
        let watch_root = root.clone();
        let handler = move |res: Result<notify::Event, notify::Error>| match res {
            Ok(event) => {
                if !relevant_fs_event(&event.kind) {
                    return;
                }
                let mut sent = false;
                for path in &event.paths {
                    // 자기-루프/잡음 차단: 앱 data dir 등 무시 prefix 하위는 버린다.
                    if ignore.iter().any(|p| path.starts_with(p)) {
                        continue;
                    }
                    // 숨김 경로는 트리에 표시되지 않으므로(토글 off) 이벤트도 무의미 —
                    // 홈 루트 감시 시 ~/Library, ~/.* 의 대량 이벤트를 여기서 거른다.
                    if !show_hidden.load(std::sync::atomic::Ordering::Relaxed)
                        && has_hidden_component(&watch_root, path)
                    {
                        continue;
                    }
                    if let Some(parent) = path.parent() {
                        let _ = tx.send(parent.to_path_buf());
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
        desired.insert(root);
        for row in &self.flat {
            if row.is_dir && row.expanded {
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
            while let Ok(dir) = rx.try_recv() {
                self.pending_watch.insert(dir);
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
        let dirty: Vec<PathBuf> = self.pending_watch.drain().collect();
        for dir in dirty {
            self.reload_dir(&dir);
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
        let old = self.children.take().unwrap_or_default();
        match reread(&root, &old) {
            Ok(children) => {
                self.children = Some(children);
                self.root_error = None;
            }
            Err(e) => {
                self.children = None;
                self.root_error = Some(format!("루트 나열 실패: {e}"));
            }
        }
        self.rebuild_flat();
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
        let Some(node) = self.children.as_mut().and_then(|c| node_mut(c, rel)) else {
            return;
        };
        if node.expanded {
            node.expanded = false;
            node.children = None; // 접힌 노드 캐시 해제 (§3 메모리 상한)
        } else {
            match read_children(path) {
                Ok(children) => {
                    node.children = Some(children);
                    node.expanded = true;
                }
                Err(e) => self.error = Some(format!("{} 나열 실패: {e}", node.name)),
            }
        }
        self.rebuild_flat();
    }

    /// 좌측 사이드바 렌더 (§6 — `egui::Panel::left`, CentralPanel 앞에서 호출할 것 §9-1).
    /// 반환: "터미널에 경로 삽입" 요청 경로 (호출측 App이 WriteInput으로 전달 — §6
    /// 유일한 runtime 접점을 App에 남긴다).
    pub fn panel(&mut self, ui: &mut egui::Ui, title: &str) -> Option<PathBuf> {
        // 접힘 여부와 무관하게 배경 채널을 소비한다 (codex Med-2 — 접힌 채로 워처/조작
        // 채널이 무한 누적되거나 op 완료(in_flight/에러/영구삭제 확인)가 방치되는 것 방지).
        self.pump_watch_events(ui.ctx());
        self.pump_ops();
        if self.collapsed {
            egui::Panel::left("file_tree_panel_collapsed")
                .resizable(false)
                .exact_size(22.0)
                .show(ui, |ui| {
                    if ui
                        .small_button("▸")
                        .on_hover_text("폴더 트리 펼치기")
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
            .show(ui, |ui| self.contents(ui, title))
            .inner
    }

    fn contents(&mut self, ui: &mut egui::Ui, title: &str) -> Option<PathBuf> {
        // (워처/백그라운드 채널 수거는 panel()이 접힘 여부와 무관하게 이미 수행했다)

        // 헤더: workspace 이름 + 새로고침/숨김 토글/접기 (§6). 헤더 전체가
        // 루트로의 드롭 대상이다 (§4 — 루트 영역 dnd_drop_zone).
        let (header, root_drop) = ui.dnd_drop_zone::<PathBuf, ()>(egui::Frame::default(), |ui| {
            ui.horizontal(|ui| {
                ui.strong(title);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("◂")
                        .on_hover_text("사이드바 접기")
                        .clicked()
                    {
                        self.collapsed = true;
                    }
                    if ui.small_button("⟳").on_hover_text("새로고침").clicked() {
                        self.refresh();
                    }
                    let hidden = ui
                        .selectable_label(self.show_hidden, "숨김")
                        .on_hover_text("숨김(.) 항목 표시");
                    if hidden.clicked() {
                        self.show_hidden = !self.show_hidden;
                        // 워처 콜백 스레드와 동기화 (숨김 이벤트 필터)
                        self.watch_show_hidden
                            .store(self.show_hidden, std::sync::atomic::Ordering::Relaxed);
                        self.rebuild_flat();
                    }
                });
            });
        });
        if let (Some(payload), Some(root)) = (root_drop, self.root.clone()) {
            self.start_move((*payload).clone(), root);
        }
        ui.separator();

        if self.root.is_none() {
            // path 미설정 (§9-2 backfill 강제 없음) — 트리 대신 안내
            ui.weak("프로젝트 경로를 설정하세요");
            ui.weak("(워크스페이스 창 → 경로 편집)");
            return None;
        }
        if let Some(err) = &self.root_error {
            ui.colored_label(ui.visuals().error_fg_color, err);
            return None;
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
                ui.label("새 폴더:");
                let resp = ui.add(
                    egui::TextEdit::singleline(buffer)
                        .hint_text("이름")
                        .desired_width(120.0),
                );
                if *focus {
                    resp.request_focus(); // §9-8 — 키가 터미널로 새지 않게 즉시 포커스
                    *focus = false;
                }
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.small_button("생성").clicked() || enter {
                    edit_done = Some(true);
                } else if ui.small_button("취소").clicked()
                    || ui.input(|i| i.key_pressed(egui::Key::Escape))
                {
                    edit_done = Some(false);
                }
            });
            ui.weak(format!("위치: {}", parent.display()));
        }

        // 헤더 우클릭: 루트에 새 폴더 (FT-3)
        if let Some(root) = self.root.clone() {
            header.response.context_menu(|ui| {
                if ui.button("새 폴더 (루트)").clicked() {
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

                    // 행 전체 = 드래그 소스 (payload = 절대 경로, §4). Id는 path 기반(§9-6).
                    let drag_id = egui::Id::new(("file_tree_row", &row.path));
                    let egui::InnerResponse {
                        inner: label_resp,
                        response,
                    } = ui.dnd_drag_source(drag_id, row.path.clone(), |ui| {
                        ui.horizontal(|ui| {
                            ui.add_space(row.depth as f32 * 12.0);
                            let text = if row.is_dir {
                                let caret = if row.expanded { "▾" } else { "▸" };
                                format!("{caret} {}", row.name)
                            } else {
                                format!("\u{2003}{}", row.name) // 파일은 아이콘 없이 이름만(들여쓰기 정렬용 공백)
                            };
                            ui.add(
                                egui::Label::new(text)
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
                    let row_resp = ui.interact(row_rect, drag_id.with("row"), egui::Sense::click());
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
                                "새 폴더 (이 안에)"
                            } else {
                                "새 폴더 (같은 위치)"
                            };
                            if ui.button(label).clicked() {
                                menu_action = Some(MenuAction::NewFolder(parent));
                                ui.close();
                            }
                        }
                        if ui.button("이름 변경").clicked() {
                            menu_action = Some(MenuAction::Rename(row.path.clone()));
                            ui.close();
                        }
                        if ui.button("휴지통으로 삭제").clicked() {
                            menu_action = Some(MenuAction::Delete(row.path.clone()));
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("경로 복사").clicked() {
                            menu_action = Some(MenuAction::CopyPath(row.path.clone()));
                            ui.close();
                        }
                        if ui.button("터미널에 경로 삽입").clicked() {
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
        let mut insert_path: Option<PathBuf> = None;
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
            Some(MenuAction::InsertPath(path)) => insert_path = Some(path),
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
                format!("휴지통 이동 실패 — '{name}' 영구 삭제?"),
            );
            ui.horizontal(|ui| {
                if ui.button("영구 삭제").clicked() {
                    self.confirm_delete = None;
                    let refresh: Vec<PathBuf> =
                        path.parent().map(Path::to_path_buf).into_iter().collect();
                    let target = path.clone();
                    // 디렉터리 삭제는 느릴 수 있다 — 백그라운드 (§9-3)
                    self.spawn_op(refresh, move || {
                        remove_all(&target).map_err(|e| format!("영구 삭제 실패: {e}"))
                    });
                }
                if ui.button("취소").clicked() {
                    self.confirm_delete = None;
                }
            });
        }

        if self.in_flight > 0 {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                ui.weak("파일 조작 중…");
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
        insert_path
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
        if let Some(node) = self.children.as_mut().and_then(|c| node_mut(c, rel))
            && node.expanded
        {
            let old = node.children.take().unwrap_or_default();
            match reread(dir, &old) {
                Ok(children) => node.children = Some(children),
                Err(_) => {
                    // 디렉터리가 사라짐(이동/삭제) — 접고 캐시 해제
                    node.expanded = false;
                    node.children = None;
                }
            }
        }
        self.rebuild_flat();
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

/// 터미널 삽입용 최소 셸 인용: 안전 문자만이면 그대로, 아니면 작은따옴표 감싸기.
pub fn shell_quote(path: &Path) -> String {
    let s = path.display().to_string();
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-~".contains(c));
    if safe {
        s
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
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
fn read_children(path: &Path) -> std::io::Result<Vec<TreeNode>> {
    let mut nodes = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        nodes.push(TreeNode::new(
            entry.file_name().to_string_lossy().into_owned(),
            is_dir,
        ));
    }
    sort_nodes(&mut nodes);
    Ok(nodes)
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

/// 디렉터리를 다시 나열하되, 이전 트리의 펼침 상태를 이월한다 (펼친 하위만 재귀 —
/// 접근 불가/사라진 하위는 접는다). 새로고침·부분 재나열의 공통 코어.
fn reread(base: &Path, old: &[TreeNode]) -> std::io::Result<Vec<TreeNode>> {
    let mut fresh = read_children(base)?;
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
    fn 워처_이벤트_경로의_부모만_부분_재나열된다() {
        // 워처 콜백이 보내는 "부모 디렉터리" 재나열 경로를 OS 워처 없이 검증한다
        // (실제 FSEvents 왕복은 타이밍 의존이라 단위 테스트에서 제외 — 수동 스모크).
        let base = temp_root("watch-reload");
        std::fs::create_dir_all(base.join("watched")).unwrap();
        std::fs::create_dir_all(base.join("other")).unwrap();
        std::fs::write(base.join("other/o.txt"), b"o").unwrap();

        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        tree.toggle_dir(&base.join("watched"));
        tree.toggle_dir(&base.join("other"));
        assert!(tree.flat.iter().any(|r| r.name == "o.txt"));
        assert!(!tree.flat.iter().any(|r| r.name == "new.txt"));

        // 디스크 변경 후 watched만 재나열 → 새 파일 반영, other는 캐시 유지 확인
        std::fs::write(base.join("watched/new.txt"), b"n").unwrap();
        std::fs::write(base.join("other/late.txt"), b"l").unwrap();
        tree.reload_dir(&base.join("watched"));

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
        tree.toggle_dir(&base.join("d"));

        // 실제 OS 워처 대신 채널을 주입해 스로틀 로직만 검증한다
        let (tx, rx) = std::sync::mpsc::channel();
        tree.watch_rx = Some(rx);
        std::fs::write(base.join("d/a.txt"), b"a").unwrap();
        tx.send(base.join("d")).unwrap();
        tx.send(base.join("d")).unwrap(); // 폭주 중 중복 이벤트

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
        tx.send(base.join("d")).unwrap();
        tree.pump_watch_events(&ctx);
        assert!(!tree.flat.iter().any(|r| r.name == "a.txt"));

        // 창 경과 → 일괄 재나열 1회, pending 소진
        tree.last_watch_reload = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(WATCH_TEST_ELAPSED_MS))
            .expect("테스트 프로세스 기동 후라 언더플로 없음");
        tree.pump_watch_events(&ctx);
        assert!(
            tree.flat.iter().any(|r| r.name == "a.txt"),
            "창 경과 후 반영"
        );
        assert!(tree.pending_watch.is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 스로틀 창(300ms)보다 확실히 큰 경과값.
    const WATCH_TEST_ELAPSED_MS: u64 = FileTreeUi::WATCH_RELOAD_MS + 50;

    #[test]
    fn 접힘_상태에서도_panel이_채널을_소비한다() {
        let base = temp_root("collapsed-drain");
        let mut tree = FileTreeUi::new(egui::Context::default());
        tree.set_root(Some(base.clone()));
        tree.collapsed = true;

        // 워처 채널 주입 + 새 파일 이벤트 (창 경과 상태)
        let (tx, rx) = std::sync::mpsc::channel();
        tree.watch_rx = Some(rx);
        std::fs::write(base.join("new.txt"), b"n").unwrap();
        tx.send(base.clone()).unwrap();
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
        egui::__run_test_ui(|ui| {
            assert_eq!(tree.panel(ui, "ws"), None);
        });

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
