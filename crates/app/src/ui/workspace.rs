//! 워크스페이스 뷰 (설계문서 PR-10): tab bar + split pane 렌더.
//! Runtime Boundary(2장) 준수 — 명령 전송/이벤트 수신/스냅샷 렌더만.
//! mux 배치는 MuxUpdated 스냅샷이 유일한 근거, active tab visible pane만 live render (14.4).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use runtime::{
    LayoutNode, MuxSnapshot, RuntimeClient, RuntimeCommand, RuntimeEvent, SessionId, SessionStatus,
    SpawnKind, SplitDirection,
};
use terminal::{TerminalViewportSnapshot, input_mapper, renderer_egui};

use super::format_bytes;
use crate::config::TerminalConfig;

/// 경로 해석 캐시 TTL (path_click_cache · session_cwd_cache 공통).
const PATH_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

/// 세션 셸 cwd 캐시 항목 — hover 경로 해석용. lsof(수십 ms)는 백그라운드 스레드가
/// 채우고 UI 스레드는 stale 값(있으면)으로 즉시 응답한다.
struct CwdCacheEntry {
    cwd: Option<std::path::PathBuf>,
    fetched_at: std::time::Instant,
    /// 백그라운드 재해석 진행 중 — 중복 spawn을 막는다. lsof는 항상 종료하므로
    /// 완료 기록이 반드시 이 플래그를 내린다.
    inflight: bool,
}

pub struct WorkspaceUi {
    mux: Option<Arc<MuxSnapshot>>,
    sessions: HashMap<SessionId, SessionView>,
    /// Runtime이 per-session shell metadata를 제공하기 전까지 path insert quoting에 쓰는
    /// workspace 기본 shell kind.
    shell_kind: crate::ui::file_tree::ShellKind,
    /// IME 조합 중 텍스트 (focused pane 전용)
    preedit: String,
    /// 이번 UI 프레임 직전에 AppKit local monitor가 본 ASCII 문장부호/숫자/공백
    /// key-down. IME Commit/Text와 대조한 뒤 누락된 문자만 복구하고 프레임 끝에 버린다.
    native_printable_key_downs: Vec<crate::native_key_monitor::NativePrintableKeyDown>,
    /// egui-winit이 이미지-only clipboard에서 Event::Paste 없이 소비하는 macOS Command+V
    /// 원본 key-down. 터미널 입력 소유권을 확인한 pane에서만 1회 소비한다.
    native_clipboard_paste_requested: bool,
    /// 세션 → 셸 pid (App이 ResourceUsage에서 매 프레임 갱신). 터미널 경로 더블클릭의
    /// 상대경로를 그 셸의 실제 cwd로 해석하는 데 쓴다 (2026-07-14).
    session_pids: HashMap<SessionId, u32>,
    /// 경로 해석 캐시: (세션, 단어) → 해석 결과. hover가 매 프레임 도는 경로라
    /// 같은 단어의 재해석(metadata/lsof)을 막는다. TTL PATH_CACHE_TTL.
    path_click_cache: Option<(SessionId, String, Option<PathClick>, std::time::Instant)>,
    /// 세션별 셸 cwd 캐시 — lsof는 수십 ms라 UI 스레드에서 돌리면 hover 중 프레임이
    /// 멈춘다. 만료 시 백그라운드 스레드가 재해석하고 그동안 stale 값을 쓴다
    /// (stale-while-revalidate). 세션별 항목이라 분할 pane 간 hover 이동이 서로
    /// 캐시를 밀어내지 않는다. 우리가 cd를 보낼 때 해당 세션 항목을 즉시 무효화.
    session_cwd_cache: Arc<Mutex<HashMap<SessionId, CwdCacheEntry>>>,
    /// 직전 폴더 클릭 (경로, 시각) — 더블클릭이 clicked를 두 번 발화시켜 같은 cd가
    /// 연속 주입되는 것을 막는다.
    last_dir_click: Option<(std::path::PathBuf, std::time::Instant)>,
    /// 직전 URL 클릭 (주소, 시각) — last_dir_click과 같은 이중 발화 방지.
    last_url_click: Option<(String, std::time::Instant)>,
    /// 마지막으로 egui Event::Paste 텍스트를 직접 전송한 시각. ⌘V는 press에서
    /// Event::Paste가, release에서 키 이벤트가 **다른 프레임으로** 도착할 수 있다 —
    /// 그때 release 쪽 이미지 태스크가 클립보드 텍스트 fallback으로 같은 내용을
    /// 한 번 더 보내 이중 붙여넣기가 됐다 (2026-07-14 사용자). 같은 제스처로 보고
    /// release 태스크를 건너뛰기 위한 표식.
    last_text_paste: Option<std::time::Instant>,
    /// AppKit Command+V key-down에서 이미지 paste 태스크를 시작한 시각. 뒤이어 오는
    /// egui V key-up fallback이 같은 clipboard를 다시 붙이지 않게 하는 제스처 표식.
    last_native_paste: Option<std::time::Instant>,
    /// 세션별 마지막 전송한 (cols, rows) — 변화 시에만 Resize 전송
    sent_sizes: HashMap<SessionId, (u16, u16)>,
    /// 트랙패드 미세 스크롤 누적 (focused pane 기준)
    scroll_residual: f32,
    /// 드래그 선택 오토스크롤 행 누적 — 경계 초과 속도(행/초)×dt의 소수부 보관 (T4)
    drag_autoscroll_residual: f32,
    /// 이번 프레임에 명령을 보냈다 — 응답 이벤트 폴링을 위해 repaint 예약
    command_sent: bool,
    /// mux focused_pane 변경 추적
    last_focused_pane: Option<runtime::MuxPaneId>,
    /// 세션별 pane 강조 플래시 만료 시각 — 포커스 이동·입력요청·작업완료 시 now+PANE_FLASH로
    /// 세팅해 pane 전체 테두리를 잠깐 포인트색으로 그린다(2026-07-12 사용자). 탑라인은 별도로
    /// 포커스 동안 항상 유지된다.
    session_flash: HashMap<SessionId, std::time::Instant>,
    /// egui 포커스 동기화와 입력 대상 전환 대기. Runtime의 `FocusPane` 반영은 비동기라,
    /// 클릭·검색 닫힘 직후에도 이 pane을 먼저 입력 대상으로 삼아 첫 문자를 잃지 않는다.
    pending_focus: Option<runtime::MuxPaneId>,
    /// 이 프레임에 사용자가 터미널을 직접 클릭해 키보드 소유권을 요청했다. App이
    /// 뒤이어 렌더하는 Agents TextEdit의 지연 autofocus를 취소하는 one-shot 신호다.
    terminal_focus_claimed: bool,
    /// 응답(Spawned/Failed)을 아직 못 받은 셸 spawn 수 — 0이 될 때까지 계속 폴링
    pending_spawns: u32,
    /// split 경계 드래그 중 로컬 미리보기 (path, ratio). 드래그 동안은 명령을 보내지
    /// 않고(매 프레임 DB 저장 방지) 릴리즈 시 1회 ResizeSplit을 보낸다.
    split_drag: Option<(Vec<u8>, f32)>,
    /// 닫기 확인 대기 중인 pane — 실행 중 세션이 있는 pane 닫기는 확인을 거친다
    /// (2026-07-05 사용자 보고: 닫기 실수로 셸 전체 즉사 방지).
    confirm_close: Option<runtime::MuxPaneId>,
    /// '같은 폴더에서 새 셀'(사이드바) — 다음 ShellSpawned에 cd로 주입할 폴더.
    pending_spawn_cd: Option<String>,
    /// pane 우클릭 → "환경변수·API 설정" 요청 (E4 ⑥). App이 프레임에서 take해
    /// 설정 창을 Environment 카테고리로 연다.
    open_environment_requested: bool,
    /// 터미널 마우스 선택 (session, anchor 셀, head 셀 — 드래그 방향 그대로,
    /// 렌더/복사 시 정규화). 새 출력(Viewport)이 오면 그 세션의 선택은 해제한다.
    selection: Option<(SessionId, usize, usize)>,
    /// 활성 workspace의 프로젝트명(폴더명 ≈ 깃 레포명, 없으면 "~"). 세션 기본 제목이
    /// "셀 134" 대신 이걸로 표시된다. rename한 세션은 그대로 둔다. App이 매 프레임 세팅.
    project_name: Option<String>,
    /// UI 텍스트 배율(App이 매 프레임 set). 터미널은 zoom_factor로 같이 커지므로 font_size를
    /// 이 값으로 역보정해 물리 크기를 유지한다(UI만 스케일, 터미널 독립 — 2026-07-13).
    ui_scale: f32,
    /// 세션별 현재 작업 폴더(App이 매 프레임 set) — 1행 제목 폴더명/프로젝트명 원천.
    session_cwds: std::collections::HashMap<SessionId, String>,
    /// 세션 위치 표시명 스타일(설정 미러 — set_session_cwds가 매 프레임 갱신).
    session_name_style: crate::config::SessionNameStyle,
    /// 세션별 에이전트 표시정보(model/effort/context — App이 병합해 set) — 3줄 행 2/3행.
    agent_info: std::collections::HashMap<SessionId, crate::agent_detect::AgentDisplay>,
    /// 진행 중인 백그라운드 클립보드 paste(이미지 PNG 인코딩을 UI 밖으로 — 2026-07-07).
    /// show()가 매 프레임 폴링해 완료 시 해당 세션에 삽입한다. 새 ⌘V는 이전 것을 대체.
    paste_task: Option<PendingPaste>,
    error: Option<String>,
    /// 현재 error 배너가 input backpressure 경고인지 — 해소 이벤트(queued=0)가
    /// 무관한 오류(spawn 실패 등)를 지우지 않게 구분한다(codex 2026-07-09).
    error_is_pressure: bool,
    /// 터미널 텍스트 검색 상태 (T3). Cmd+F로 열리고, 열려 있으면 focused pane 우상단에
    /// 검색 바를 그린다. 한 번에 한 세션만 검색한다.
    search: Option<TerminalSearch>,
    /// 이번 프레임에 그린 pane들의 렌더 카운터 합 (B1 실측). show() 시작에서 리셋하고
    /// pane마다 누적한다 — 정수 덧셈뿐이라 게이트 없이 항상 집계한다.
    frame_counters: renderer_egui::RenderCounters,
}

/// 터미널 검색 매치 수 상한 (T3) — worker에 보내는 요청 상한. 도달 시 결과가 잘린다.
const SEARCH_MAX_MATCHES: u32 = 1000;

/// 터미널 텍스트 검색 세션 상태 (T3).
struct TerminalSearch {
    /// 검색 대상 세션 — 이 세션의 pane에만 검색 바를 그린다.
    session: SessionId,
    /// 현재 입력된 쿼리.
    query: String,
    /// worker에 마지막으로 요청한 쿼리 — 바뀔 때만 재검색한다(매 프레임 재검색 금지).
    requested: Option<String>,
    /// 최근 검색 결과(화면 최하단 우선 정렬).
    matches: Vec<terminal::ScrollbackMatch>,
    /// 검색 시점의 전체 라인 수 — 스크롤 목표 클램프에 쓴다.
    total_lines: u32,
    /// 매치 상한 도달로 결과가 잘렸는지.
    capped: bool,
    /// 현재 매치 인덱스(0 = 최신). matches가 비면 무의미.
    current: usize,
    /// 이번 프레임에 입력창 포커스를 요청해야 하는지.
    focus_input: bool,
    /// 다음 렌더에서 current 매치가 화면에 보이도록 스크롤해야 하는지.
    scroll_to_current: bool,
}

/// 백그라운드 paste 1건의 컨텍스트 — 요청 시점의 세션/모드를 캡처해 완료 시 그대로 쓴다.
struct PendingPaste {
    rx: std::sync::mpsc::Receiver<anyhow::Result<Option<Vec<std::path::PathBuf>>>>,
    session: SessionId,
    bracketed: bool,
    shell_kind: crate::ui::file_tree::ShellKind,
    /// egui Event::Paste로 이미 받은 텍스트(있으면) — 이미지가 없을 때의 fallback.
    text_fallback: Option<Vec<u8>>,
    /// 요청 시각 — 워크스페이스가 warm으로 물러났다 돌아온 뒤 도착한 옛 paste가
    /// 살아있는 세션(바뀐 프롬프트)에 뒤늦게 꽂히지 않게 만료시킨다(codex Medium).
    requested_at: std::time::Instant,
}

/// 백그라운드 paste 결과의 수명 — 이보다 오래된 완료는 버린다.
const PASTE_TASK_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// 세션별 화면 캐시. hidden tab 세션의 스냅샷은 `MuxUpdated`에서 버린다.
#[derive(Default)]
struct SessionView {
    snapshot: Option<Arc<TerminalViewportSnapshot>>,
    /// 스냅샷 세대 — 새 스냅샷을 받을 때마다 +1. 렌더러가 "이 스냅샷의 dirty를 이미
    /// 소비했는지" 판정해, 새 출력이 없는 repaint에서 전 행을 다시 shaping하지 않게 한다
    /// (2026-07-14 실측: idle repaint마다 rows_rebuilt≈전체 행 — 캐시 무효화 버그).
    snapshot_gen: u64,
    /// freeze(선택 중) 동안 도착한 최신 snapshot을 보관 — 선택 해제 시 이걸로 catch-up해
    /// 화면이 선택 당시에 머무는 것을 막는다(codex). 프레임 시작 시 프로모트한다.
    pending_snapshot: Option<Arc<TerminalViewportSnapshot>>,
    render_cache: renderer_egui::TerminalRenderCache,
    bracketed_paste: bool,
    /// 사이드바 세션 목록에 보여줄 최신 화면 요약 (마지막 비어있지 않은 행, ≤48자)
    summary: String,
    exit_code: Option<Option<u32>>,
    /// status detector 감지 상태 (agent만, PR-12)
    status: Option<SessionStatus>,
    status_view: Option<runtime::SessionStatusView>,
    input_pressure: Option<runtime::PtyInputPressure>,
}

impl WorkspaceUi {
    pub fn new() -> Self {
        Self {
            mux: None,
            sessions: HashMap::new(),
            shell_kind: crate::ui::file_tree::default_shell_kind(),
            preedit: String::new(),
            native_printable_key_downs: Vec::new(),
            native_clipboard_paste_requested: false,
            sent_sizes: HashMap::new(),
            scroll_residual: 0.0,
            drag_autoscroll_residual: 0.0,
            command_sent: false,
            last_focused_pane: None,
            session_flash: HashMap::new(),
            pending_focus: None,
            terminal_focus_claimed: false,
            pending_spawns: 0,
            split_drag: None,
            confirm_close: None,
            pending_spawn_cd: None,
            open_environment_requested: false,
            selection: None,
            project_name: None,
            ui_scale: 1.0,
            session_pids: HashMap::new(),
            path_click_cache: None,
            session_cwd_cache: Arc::new(Mutex::new(HashMap::new())),
            last_dir_click: None,
            last_url_click: None,
            last_text_paste: None,
            last_native_paste: None,
            session_cwds: std::collections::HashMap::new(),
            session_name_style: crate::config::SessionNameStyle::default(),
            agent_info: std::collections::HashMap::new(),
            paste_task: None,
            error: None,
            error_is_pressure: false,
            search: None,
            frame_counters: renderer_egui::RenderCounters::default(),
        }
    }

    /// 이번 프레임에 이 워크스페이스가 그린 터미널 렌더 카운터 합 (B1 실측).
    pub fn frame_counters(&self) -> renderer_egui::RenderCounters {
        self.frame_counters
    }

    /// Consume the one-frame terminal keyboard ownership claim. This is kept
    /// separate from runtime pane focus because Agents is rendered after the
    /// workspace and can otherwise re-request its deferred TextEdit focus.
    pub fn take_terminal_focus_claimed(&mut self) -> bool {
        std::mem::take(&mut self.terminal_focus_claimed)
    }

    /// 세션 스냅샷이 하나라도 도착했는가 — 워크스페이스 생성 burst의 `first_snapshot`
    /// 단계 판정용 (B1). 세션이 없으면 false.
    pub fn any_snapshot(&self) -> bool {
        self.sessions.values().any(|view| view.snapshot.is_some())
    }

    /// Cmd+F 등으로 focused 터미널에서 검색 바를 연다 (T3). 이미 같은 세션에 열려 있으면
    /// 입력창 포커스만 다시 준다. focused pane에 세션이 없으면 무시한다.
    pub fn open_search(&mut self) {
        let Some(session) = self.focused_session() else {
            return;
        };
        match &mut self.search {
            Some(search) if search.session == session => {
                search.focus_input = true;
            }
            _ => {
                self.search = Some(TerminalSearch {
                    session,
                    query: String::new(),
                    requested: None,
                    matches: Vec::new(),
                    total_lines: 0,
                    capped: false,
                    current: 0,
                    focus_input: true,
                    scroll_to_current: false,
                });
            }
        }
    }

    /// 검색 바를 닫고 원래 터미널로 포커스를 되돌린다 (T3).
    fn close_search(&mut self) {
        self.search = None;
        // 실제 refocus는 다음 프레임 render_pane에서 pending_focus로 소비된다.
        if let Some(pane) = self.mux.as_ref().and_then(|mux| mux.focused_pane.clone()) {
            self.begin_terminal_refocus(pane);
        }
    }

    /// 터미널 검색 UI (T3): 뷰포트에 보이는 매치 하이라이트 + 우상단 검색 바 + 이동 스크롤.
    /// 검색은 backend(worker)가 수행하고, UI는 결과(라인 오프셋)를 받아 그리고 이동만 한다.
    #[allow(clippy::too_many_arguments)]
    fn render_terminal_search(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        term_rect: egui::Rect,
        origin: egui::Pos2,
        cell_size: egui::Vec2,
        snapshot: &TerminalViewportSnapshot,
        client: &dyn RuntimeClient,
        catalog: &i18n::Catalog,
    ) {
        // 이 pane의 세션에 대한 검색만 그린다.
        if self.search.as_ref().map(|s| s.session) != Some(session) {
            return;
        }

        // 1) 뷰포트에 보이는 매치 하이라이트. 뷰포트 행 = scroll_offset + rows-1 - line_from_bottom
        //    (total_lines와 무관 — 현재 스냅샷 스크롤 위치만으로 매핑된다).
        if let Some(search) = self.search.as_ref() {
            let rows = snapshot.rows as i32;
            let normal = egui::Color32::from_rgba_unmultiplied(0xE5, 0xC0, 0x7B, 70);
            let current = egui::Color32::from_rgba_unmultiplied(0xF2, 0x8C, 0x28, 140);
            for (i, m) in search.matches.iter().enumerate() {
                let vrow = snapshot.scroll_offset + rows - 1 - m.line_from_bottom as i32;
                if vrow < 0 || vrow >= rows {
                    continue;
                }
                let x0 = origin.x + m.col_start as f32 * cell_size.x;
                let x1 = origin.x + m.col_end as f32 * cell_size.x;
                let y0 = origin.y + vrow as f32 * cell_size.y;
                let rect = egui::Rect::from_min_size(
                    egui::pos2(x0, y0),
                    egui::vec2((x1 - x0).max(cell_size.x), cell_size.y),
                );
                let color = if i == search.current { current } else { normal };
                ui.painter().rect_filled(rect, 0.0, color);
            }
        }

        // 2) current 매치가 화면에 보이도록 스크롤(Scroll delta 양수 = 과거로 = display_offset↑).
        let scroll_delta = {
            let Some(search) = self.search.as_mut() else {
                return;
            };
            if search.scroll_to_current {
                search.scroll_to_current = false;
                search.matches.get(search.current).and_then(|m| {
                    let rows = snapshot.rows as i32;
                    let total = search.total_lines as i32;
                    let b = m.line_from_bottom as i32;
                    // 매치를 화면 중앙 근처에 두되, 유효 범위 [0, history]로 클램프.
                    let desired = (b - rows / 2).clamp(0, (total - rows).max(0));
                    let delta = desired - snapshot.scroll_offset;
                    (delta != 0).then_some(delta)
                })
            } else {
                None
            }
        };
        if let Some(delta) = scroll_delta {
            self.send(client, RuntimeCommand::Scroll { session, delta });
        }

        // 3) 우상단 검색 바.
        let (mut query, focus_input, match_count, current, capped) = {
            let Some(search) = self.search.as_ref() else {
                return;
            };
            (
                search.query.clone(),
                search.focus_input,
                search.matches.len(),
                search.current,
                search.capped,
            )
        };
        let mut do_next = false;
        let mut do_prev = false;
        let mut do_close = false;

        let bar_width = 280.0;
        let pos = egui::pos2(
            (term_rect.right() - bar_width - 8.0).max(term_rect.left() + 4.0),
            term_rect.top() + 8.0,
        );
        egui::Area::new(egui::Id::new(("terminal_search", session)))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .constrain_to(term_rect)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(bar_width);
                    ui.horizontal(|ui| {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut query)
                                .desired_width(120.0)
                                .hint_text(catalog.t("workspace.search.hint", &[])),
                        );
                        if focus_input {
                            resp.request_focus();
                        }
                        // 매치 카운트 n/m (쿼리 없으면 공백, 매치 없으면 "없음").
                        let label = if query.trim().is_empty() {
                            String::new()
                        } else if match_count == 0 {
                            catalog.t("workspace.search.no_match", &[])
                        } else {
                            format!("{}/{}", current + 1, match_count)
                        };
                        ui.label(label);
                        if capped {
                            ui.label(catalog.t("workspace.search.capped", &[]));
                        }
                        if ui
                            .button("‹")
                            .on_hover_text(catalog.t("workspace.search.prev", &[]))
                            .clicked()
                        {
                            do_prev = true;
                        }
                        if ui
                            .button("›")
                            .on_hover_text(catalog.t("workspace.search.next", &[]))
                            .clicked()
                        {
                            do_next = true;
                        }
                        if ui
                            .button("×")
                            .on_hover_text(catalog.t("workspace.search.close", &[]))
                            .clicked()
                        {
                            do_close = true;
                        }
                        // 키 입력은 입력창이 포커스일 때만 소비(터미널로 안 흘러감).
                        // Enter=다음, Shift+Enter=이전, Esc=닫기.
                        if resp.has_focus() {
                            let (enter, esc, shift) = ui.input(|i| {
                                (
                                    i.key_pressed(egui::Key::Enter),
                                    i.key_pressed(egui::Key::Escape),
                                    i.modifiers.shift,
                                )
                            });
                            if esc {
                                do_close = true;
                            } else if enter {
                                if shift {
                                    do_prev = true;
                                } else {
                                    do_next = true;
                                }
                            }
                        }
                    });
                });
            });

        // 4) 검색 바 조작 반영.
        {
            let Some(search) = self.search.as_mut() else {
                return;
            };
            search.focus_input = false;
            if query != search.query {
                search.query = query;
            }
            let m = search.matches.len();
            if !do_close && m > 0 {
                if do_next {
                    search.current = (search.current + 1) % m;
                    search.scroll_to_current = true;
                }
                if do_prev {
                    search.current = (search.current + m - 1) % m;
                    search.scroll_to_current = true;
                }
            }
        }
        if do_close {
            self.close_search();
            return;
        }

        // 5) 쿼리가 바뀌었을 때만 재검색을 요청한다(매 프레임 금지).
        let pending = self.search.as_ref().and_then(|s| {
            (s.requested.as_deref() != Some(s.query.as_str())).then(|| (s.session, s.query.clone()))
        });
        if let Some((sess, q)) = pending {
            let empty = q.trim().is_empty();
            if let Some(s) = self.search.as_mut() {
                s.requested = Some(q.clone());
                if empty {
                    s.matches.clear();
                    s.total_lines = 0;
                    s.capped = false;
                    s.current = 0;
                }
            }
            if !empty {
                self.send(
                    client,
                    RuntimeCommand::SearchScrollback {
                        session: sess,
                        query: q,
                        max_matches: SEARCH_MAX_MATCHES,
                    },
                );
            }
        }
    }

    /// 활성 workspace의 프로젝트명을 세팅한다(App이 매 프레임). 세션 기본 제목("셀 N")을
    /// 이 이름으로 표시한다.
    pub fn set_project_name(&mut self, name: Option<String>) {
        self.project_name = name;
    }

    /// UI 텍스트 배율을 세팅한다(App이 매 프레임). 터미널 font_size 역보정에 쓴다.
    pub fn set_ui_scale(&mut self, scale: f32) {
        self.ui_scale = if scale.is_finite() && scale > 0.1 {
            scale
        } else {
            1.0
        };
    }

    /// 세션 셸 pid (ResourceUsage 스냅샷 기준) — 사이드바 메뉴의 cwd 일회성 조회용.
    pub fn session_pid(&self, session: SessionId) -> Option<u32> {
        self.session_pids.get(&session).copied()
    }

    /// 세션 → 셸 pid를 세팅한다(App이 매 프레임, ResourceUsage 스냅샷 기준).
    /// 터미널 경로 더블클릭의 상대경로 해석(lsof cwd 1회 조회)에 쓴다.
    pub fn set_session_pids(&mut self, pids: &[(SessionId, u32)]) {
        self.session_pids.clear();
        self.session_pids.extend(pids.iter().copied());
        // 죽은 세션의 cwd 캐시 정리 — pid 스냅샷이 최신 live 집합이다.
        self.session_cwd_cache
            .lock()
            .expect("cwd cache lock")
            .retain(|session, _| self.session_pids.contains_key(session));
    }

    /// (세션, 단어) 캐시를 거친 경로 해석. hover가 매 프레임 부르므로 같은 단어는
    /// 재해석하지 않고, 셸 cwd(lsof)는 세션별 백그라운드 캐시로 조회한다 — hover
    /// 경로는 UI 스레드에서 lsof를 직접 돌리지 않는다(프레임 스톨 방지, 2026-07-16).
    fn resolve_path_cached(
        &mut self,
        ctx: &egui::Context,
        session: SessionId,
        word: &str,
    ) -> Option<PathClick> {
        if let Some((s, w, res, at)) = &self.path_click_cache
            && *s == session
            && w == word
            && at.elapsed() < PATH_CACHE_TTL
        {
            return res.clone();
        }
        let (cwd, cwd_pending) = self.hover_cwd(ctx, session);
        let res = resolve_path_click(word, cwd.as_deref());
        // cwd가 아직 오는 중이면 부정 결과를 굳히지 않는다 — 도착 즉시 다음 프레임에 재해석.
        if !cwd_pending {
            self.path_click_cache = Some((
                session,
                word.to_owned(),
                res.clone(),
                std::time::Instant::now(),
            ));
        }
        res
    }

    /// hover용 셸 cwd — 캐시가 신선하면 그 값을, 만료/부재면 백그라운드 재해석을 걸고
    /// stale 값(있으면)을 돌려준다. 반환 (cwd, pending): pending은 "값이 아직 없어
    /// 해석 대기 중"이라는 뜻이다.
    fn hover_cwd(
        &mut self,
        ctx: &egui::Context,
        session: SessionId,
    ) -> (Option<std::path::PathBuf>, bool) {
        let Some(pid) = self.session_pids.get(&session).copied() else {
            // 자원 스냅샷(pid)이 아직/원래 없으면 감지 워커의 cwd(에이전트 세션)로 폴백 —
            // pid 지연이 hover 커서를 통째로 죽이지 않게 한다.
            return (
                self.session_cwds
                    .get(&session)
                    .map(std::path::PathBuf::from),
                false,
            );
        };
        let mut cache = self.session_cwd_cache.lock().expect("cwd cache lock");
        if let Some(entry) = cache.get(&session)
            && (entry.inflight || entry.fetched_at.elapsed() < PATH_CACHE_TTL)
        {
            return (entry.cwd.clone(), entry.cwd.is_none() && entry.inflight);
        }
        // 만료/부재 — 백그라운드 재해석을 걸고 stale 값으로 즉시 응답한다.
        let stale = cache.get(&session).and_then(|entry| entry.cwd.clone());
        cache.insert(
            session,
            CwdCacheEntry {
                cwd: stale.clone(),
                fetched_at: std::time::Instant::now(),
                inflight: true,
            },
        );
        drop(cache);
        let shared = Arc::clone(&self.session_cwd_cache);
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("cwd-resolve".to_owned())
            .spawn(move || {
                let cwd = platform::process_cwd(pid);
                shared.lock().expect("cwd cache lock").insert(
                    session,
                    CwdCacheEntry {
                        cwd,
                        fetched_at: std::time::Instant::now(),
                        inflight: false,
                    },
                );
                // 마우스가 정지 상태면 자연 repaint가 없다 — 결과가 다음 프레임에
                // 커서/메뉴에 반영되게 명시 요청.
                ctx.request_repaint();
            })
            .expect("cwd resolve thread spawn");
        let pending = stale.is_none();
        (stale, pending)
    }

    /// 클릭 실행용 신선 해석 — hover 캐시를 거치지 않고 lsof를 동기 1회 실행한다
    /// (사용자 클릭 시점의 일회성 조회라 수십 ms를 감수 — platform::process_cwd 관례).
    /// 결과는 hover 캐시에도 반영한다.
    fn resolve_path_fresh(&mut self, session: SessionId, word: &str) -> Option<PathClick> {
        let cwd = self
            .session_pids
            .get(&session)
            .copied()
            .and_then(platform::process_cwd);
        self.session_cwd_cache
            .lock()
            .expect("cwd cache lock")
            .insert(
                session,
                CwdCacheEntry {
                    cwd: cwd.clone(),
                    fetched_at: std::time::Instant::now(),
                    inflight: false,
                },
            );
        resolve_path_click(word, cwd.as_deref())
    }

    /// cd 주입 등으로 셸 cwd가 바뀌었을 때 해당 세션의 cwd 캐시를 버린다.
    fn invalidate_session_cwd(&self, session: SessionId) {
        self.session_cwd_cache
            .lock()
            .expect("cwd cache lock")
            .remove(&session);
    }

    /// 세션별 현재 작업 폴더를 세팅한다(App이 매 프레임, 감지 워커 lsof 결과).
    /// `style`은 위치 표시명 스타일(설정 — 현재 폴더명 vs 저장소명)을 함께 나른다.
    pub fn set_session_cwds(
        &mut self,
        cwds: std::collections::HashMap<SessionId, String>,
        style: crate::config::SessionNameStyle,
    ) {
        self.session_cwds = cwds;
        self.session_name_style = style;
    }

    /// 세션별 에이전트 표시정보를 세팅한다(App이 병합한 최종본 — 3줄 행 렌더용).
    pub fn set_agent_info(
        &mut self,
        info: std::collections::HashMap<SessionId, crate::agent_detect::AgentDisplay>,
    ) {
        self.agent_info = info;
    }

    /// 세션의 에이전트 요약 줄("Codex · gpt-5.6-sol · max")을 돌려준다 — 없으면 셸/미감지.
    /// 워크스페이스가 대기(warm)로 내려가도 이 맵은 마지막 감지값을 유지하므로(전환 시
    /// 안 지움), 활동 패널·PWA가 비활성 워크스페이스의 에이전트 정보를 보여줄 수 있다
    /// (2026-07-13 방안①). 절전되면 workspace_ui째로 사라져 자연히 표시 안 된다.
    pub fn agent_line_for(&self, session: SessionId) -> Option<String> {
        self.agent_info.get(&session).map(agent_info_line)
    }

    pub fn agent_providers(
        &self,
    ) -> std::collections::HashMap<SessionId, crate::agent_surface::AgentProvider> {
        self.agent_info
            .iter()
            .map(|(session, display)| (*session, display.kind.into()))
            .collect()
    }

    /// 세션 표시 제목. 우선순위: ① 사용자 rename(기본 제목이 아니면) → 그대로,
    /// ② OSC 0/2 동적 제목(프로그램이 설정, 예: cwd/명령) → 그 제목, ③ 프로젝트 폴더명(≈깃
    /// 레포명), ④ 원 표기. osc는 이 세션 터미널의 현재 OSC 제목.
    /// 세션 1행 제목 우선순위(2026-07-08 사용자): 수동 rename > 현재 작업 폴더명(git
    /// 프로젝트명) > OSC 타이틀 > "~". 폴더명이 기본이라 어느 폴더에서 작업 중인지 보인다.
    fn resolve_session_title(
        &self,
        raw: &str,
        session: Option<SessionId>,
        osc: Option<&str>,
        catalog: &i18n::Catalog,
    ) -> String {
        if !is_default_session_title(raw) {
            return display_pane_title(raw, catalog); // 사용자 rename — 그대로 고정
        }
        // 현재 작업 폴더명(git 프로젝트명) — 세션별 cwd(App이 매 프레임 set).
        if let Some(n) = session
            .and_then(|s| self.session_cwds.get(&s))
            .and_then(|c| crate::agent_detect::project_display_name(c, self.session_name_style))
            .filter(|t| !t.trim().is_empty())
        {
            return n;
        }
        // cwd 미탐지(pid 없음/lsof 지연) 폴백: OSC 타이틀 > 프로젝트명 > 기본.
        if let Some(t) = osc.map(str::trim).filter(|t| !t.is_empty()) {
            return t.to_owned();
        }
        if let Some(project) = self.project_name.as_deref() {
            return project.to_owned();
        }
        display_pane_title(raw, catalog)
    }

    /// 세션의 현재 OSC 제목(있으면).
    fn session_osc_title(&self, session: Option<SessionId>) -> Option<String> {
        session
            .and_then(|s| self.sessions.get(&s))
            .and_then(|v| v.snapshot.as_ref())
            .and_then(|s| s.title.clone())
    }

    /// 모든 세션의 터미널 렌더 캐시를 비운다 — 테마 변경 시 stale galley(옛 폰트
    /// 아틀라스/색)가 재사용돼 글자가 깨지던 문제 해결(#7). 다음 프레임에 전 행 재구성.
    /// 이 세션에 걸린 선택을 해제한다 — 선택 중엔 화면이 freeze되므로, WorkspaceUi::send를
    /// 우회해 직접 WriteInput을 보내는 경로(app.rs의 트리 드롭·resume 주입)에서 화면이 멈춘
    /// 듯 보이지 않게 호출한다(codex).
    pub fn clear_selection(&mut self, session: SessionId) {
        if self.selection.is_some_and(|(s, _, _)| s == session) {
            self.selection = None;
        }
    }

    pub fn clear_render_caches(&mut self) {
        for view in self.sessions.values_mut() {
            view.render_cache.clear();
        }
    }

    fn handle_events(
        &mut self,
        client: &dyn RuntimeClient,
        events: &[RuntimeEvent],
        catalog: &i18n::Catalog,
    ) {
        for event in events {
            match event {
                RuntimeEvent::MuxUpdated { snapshot } => {
                    // 사라진 세션의 캐시 정리
                    let alive = mux_sessions(snapshot);
                    self.sessions.retain(|id, _| alive.contains(id));
                    self.sent_sizes.retain(|id, _| alive.contains(id));
                    // 검색 중인 세션이 사라지면 검색 바를 닫는다.
                    if self
                        .search
                        .as_ref()
                        .is_some_and(|s| !alive.contains(&s.session))
                    {
                        self.search = None;
                    }
                    // hidden(active tab 밖) 세션의 마지막 스냅샷은 버린다 —
                    // §14.4 hidden render cache drop. tab 복귀 시 worker가
                    // 전환 즉시 push하므로(emit_mux_and_watched) 공백은 짧다 (codex 리뷰)
                    let visible = visible_mux_sessions(snapshot);
                    // 선택된 세션이 hidden 되면 선택을 해제한다 — 안 그러면 freeze(선택 중
                    // snapshot 갱신 스킵)가 재표시돼도 안 풀려 stuck 된다(codex).
                    if self
                        .selection
                        .is_some_and(|(s, _, _)| !visible.contains(&s))
                    {
                        self.selection = None;
                    }
                    for (id, view) in self.sessions.iter_mut() {
                        if !visible.contains(id) {
                            view.snapshot = None;
                            view.snapshot_gen = view.snapshot_gen.wrapping_add(1);
                            view.pending_snapshot = None;
                            view.render_cache.clear();
                        }
                    }
                    // 드래그 중 tab 전환/분할 구조 변경이면 미리보기가 다른 split에
                    // 잘못 적용될 수 있다 — 구조가 바뀌는 지점에서 정리 (codex 리뷰).
                    // 리사이즈 자신의 MuxUpdated는 drag_stopped 이후라 잃을 상태가 없다.
                    if self.mux.as_ref().map(|m| (&m.active_tab, &m.tabs))
                        != Some((&snapshot.active_tab, &snapshot.tabs))
                    {
                        self.split_drag = None;
                    }
                    self.mux = Some(Arc::clone(snapshot));
                }
                RuntimeEvent::Viewport {
                    session,
                    snapshot,
                    bracketed_paste,
                } => {
                    // hidden 전환 뒤 도착한 stale Viewport가 캐시를 되살리지 않도록
                    // 현재 active tab의 visible 세션만 snapshot을 저장한다.
                    if self.session_visible(*session) {
                        // 이 세션에 선택이 걸려 있으면 화면(snapshot)을 얼린다 — claude/codex
                        // 작업 중엔 화면이 매 프레임 갱신돼 예전엔 선택이 즉시 무효화됐다(#3).
                        // 좌표 기준 선택이라 그냥 유지만 하면 갱신된 화면의 '다른 텍스트'를
                        // 복사할 수 있어(codex), 선택 중엔 뷰를 정지시켜 선택·복사·표시가 항상
                        // 일치하게 한다(표준 터미널 동작). 클릭으로 선택 해제하면 최신으로 갱신.
                        let frozen = self.selection.is_some_and(|(s, _, _)| s == *session);
                        let view = self.sessions.entry(*session).or_default();
                        view.bracketed_paste = *bracketed_paste;
                        if frozen {
                            // 선택 중엔 표시 snapshot을 얼리되, 최신본은 pending에 보관해
                            // 해제 시 catch-up한다(codex — 안 그러면 화면이 선택 당시에 멈춤).
                            view.pending_snapshot = Some(Arc::clone(snapshot));
                        } else {
                            view.snapshot = Some(Arc::clone(snapshot));
                            view.snapshot_gen = view.snapshot_gen.wrapping_add(1);
                            view.pending_snapshot = None;
                            // 사이드바 세션 요약 — 마지막 비어있지 않은 행 (2026-07-05)
                            view.summary = last_line_summary(snapshot);
                        }
                    }
                }
                // SessionExited(런타임 종료) / SessionRestored(재시작 시 아카이브 복원,
                // PR-A2)는 UI 부기가 동일하다 — exit_code + 결과 상태 배지를 채운다.
                // 완료 알림 차이(복원은 재발화 안 함)는 process_ws_notifications 몫.
                RuntimeEvent::SessionExited { session, exit_code }
                | RuntimeEvent::SessionRestored { session, exit_code } => {
                    if self.session_alive(*session) {
                        let view = self.sessions.entry(*session).or_default();
                        view.exit_code = Some(*exit_code);
                        // 진행형 상태(⏳/✋)는 종료와 함께 무효. 결과 상태(✅/❌)는
                        // 유지하고, 없으면 exit code로 채운다 — 알림(on_exit)과
                        // tab 아이콘이 같은 결과를 보여주도록 (codex 리뷰 반영).
                        if !matches!(
                            view.status,
                            Some(SessionStatus::Done) | Some(SessionStatus::Error)
                        ) {
                            view.status = Some(if *exit_code == Some(0) {
                                SessionStatus::Done
                            } else {
                                SessionStatus::Error
                            });
                        }
                    }
                }
                RuntimeEvent::SpawnFailed { kind, message } => {
                    if *kind == SpawnKind::Shell {
                        self.pending_spawns = self.pending_spawns.saturating_sub(1);
                    }
                    self.error_is_pressure = false;
                    self.error = Some(crate::ui::render_message(catalog, message));
                }
                RuntimeEvent::SessionStatusChanged { session, status } => {
                    if self.session_alive(*session) {
                        self.note_status_flash(*session, *status);
                        self.sessions.entry(*session).or_default().status = Some(*status);
                    }
                }
                RuntimeEvent::SessionStatusViewChanged { session, view } => {
                    if self.session_alive(*session) {
                        self.note_status_flash(*session, view.status);
                        let entry = self.sessions.entry(*session).or_default();
                        entry.status = Some(view.status);
                        entry.status_view = Some(view.clone());
                    }
                }
                RuntimeEvent::PtyInputPressure { session, pressure } => {
                    // queued=0은 해소 신호(2026-07-09) — 경고 대신 상태를 걷어낸다.
                    if pressure.queued_messages == 0 && pressure.queued_bytes == 0 {
                        if let Some(entry) = self.sessions.get_mut(session) {
                            entry.input_pressure = None;
                        }
                        // 압력 경고일 때만 배너를 걷는다 — spawn 실패 등 무관 오류 보존.
                        if self.error_is_pressure {
                            self.error = None;
                            self.error_is_pressure = false;
                        }
                        continue;
                    }
                    if self.session_alive(*session) {
                        self.sessions.entry(*session).or_default().input_pressure =
                            Some(pressure.clone());
                    }
                    self.error_is_pressure = true;
                    self.error = Some(catalog.t(
                        "workspace.input_pressure",
                        &[
                            ("queued", &format_bytes(pressure.queued_bytes as u64)),
                            ("max", &format_bytes(pressure.max_bytes as u64)),
                        ],
                    ));
                }
                RuntimeEvent::ShellSpawned { session } => {
                    self.pending_spawns = self.pending_spawns.saturating_sub(1);
                    // '같은 폴더에서 새 셀' — 스폰 완료 시 cd 1회 주입(spawn_shell_at).
                    if let Some(cwd) = self.pending_spawn_cd.take() {
                        let bytes = cd_paste_bytes(
                            std::path::Path::new(&cwd),
                            self.session_shell_kind(*session),
                            false,
                        );
                        self.send(
                            client,
                            RuntimeCommand::WriteInput {
                                session: *session,
                                bytes,
                            },
                        );
                    }
                }
                RuntimeEvent::AgentSpawned { .. } => {}
                RuntimeEvent::ResourceUsage { .. } => {}
                RuntimeEvent::ScrollbackSearchResult {
                    session,
                    query,
                    result,
                } => {
                    // 늦게 도착한 stale 결과(쿼리가 이미 바뀜)는 버린다.
                    if let Some(search) = self.search.as_mut()
                        && search.session == *session
                        && search.query == *query
                    {
                        search.matches = result.matches.clone();
                        search.total_lines = result.total_lines;
                        search.capped = result.capped;
                        search.current = 0;
                        search.scroll_to_current = !search.matches.is_empty();
                    }
                }
            }
        }
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
        events: &[RuntimeEvent],
        catalog: &i18n::Catalog,
    ) {
        self.frame_counters = renderer_egui::RenderCounters::default();
        self.terminal_focus_claimed = false;
        // AppKit local monitor는 winit/egui가 IME 처리 중 숨길 수 있는 원본 key-down을
        // 보존한다. 매 프레임 먼저 비워 두어 검색창/설정창에서 친 키가 나중에 터미널로
        // 이월되지 않게 하고, 실제 전송은 terminal_keyboard_active pane만 수행한다.
        let native_key_downs = crate::native_key_monitor::drain();
        self.native_printable_key_downs = native_key_downs.printable;
        self.native_clipboard_paste_requested = native_key_downs.clipboard_paste;
        self.handle_events(client, events, catalog);
        self.poll_paste_task(client);

        // 탭바 제거 (2026-07-05): 셸 전환은 좌측 사이드바 세션 목록이 담당하고,
        // 새 셸/분할/닫기는 각 pane 헤더가 담당한다 — 셸 수만큼 탭이 늘어나
        // 상단이 넘치던 문제 해소.
        // 에러 바가 있을 때만 pane과 분리하는 헤어라인을 둔다 — 평소엔 top_bar 하단
        // 헤어라인이 이미 구분선이라 여기 무조건 그리면 라인이 두 줄로 겹쳤다(#64 사용자).
        if let Some(error) = self.error.clone() {
            ui.horizontal(|ui| {
                ui.colored_label(ui.visuals().error_fg_color, error);
                if ui.small_button("×").clicked() {
                    self.error = None;
                }
            });
            crate::ui::hairline(ui);
        }

        let Some(mux) = self.mux.clone() else {
            ui.centered_and_justified(|ui| {
                if ui
                    .button(catalog.t("workspace.new_shell", &[]))
                    .on_hover_text(catalog.t("workspace.start_shell_prompt", &[]))
                    .clicked()
                {
                    self.send(
                        client,
                        RuntimeCommand::SpawnShell {
                            cols: 80,
                            rows: 24,
                            scrollback_lines: config.scrollback_lines as usize,
                        },
                    );
                }
            });
            self.flush_command_repaint(ui.ctx());
            return;
        };
        // mux 포커스가 바뀐 프레임: stale 조합/스크롤 잔여분 리셋 (세션 간 이월 방지)
        if self.last_focused_pane != mux.focused_pane {
            self.last_focused_pane = mux.focused_pane.clone();
            self.pending_focus = mux.focused_pane.clone();
            self.preedit.clear();
            self.scroll_residual = 0.0;
            // 포커스가 옮겨간 pane 세션을 잠깐 강조(pane 전체 2초 플래시).
            if let Some(session) = mux
                .focused_pane
                .as_ref()
                .and_then(|pid| {
                    mux.tabs
                        .iter()
                        .flat_map(|tab| &tab.panes)
                        .find(|p| &p.id == pid)
                })
                .and_then(|pane| pane.session_id)
            {
                self.session_flash
                    .insert(session, std::time::Instant::now() + PANE_FLASH);
            }
        }
        // 만료된 플래시 정리(무한 성장 방지).
        let now = std::time::Instant::now();
        self.session_flash.retain(|_, until| now < *until);
        let Some(active_tab) = mux
            .active_tab
            .as_ref()
            .and_then(|id| mux.tabs.iter().find(|tab| &tab.id == id))
        else {
            ui.centered_and_justified(|ui| {
                if ui
                    .button(catalog.t("workspace.new_shell", &[]))
                    .on_hover_text(catalog.t("workspace.start_shell_prompt", &[]))
                    .clicked()
                {
                    self.send(
                        client,
                        RuntimeCommand::SpawnShell {
                            cols: 80,
                            rows: 24,
                            scrollback_lines: config.scrollback_lines as usize,
                        },
                    );
                }
            });
            self.flush_command_repaint(ui.ctx());
            return;
        };

        // (출력/상태 폴링 제거 — 2026-07-04 상시 리페인트 원인 조사)
        // 예전엔 "가시+실행 세션 = 50ms 폴링"으로 출력을 끌어왔다(wake가 Viewport를
        // 깨우지 않던 시절의 안전망) → 가시 idle에서 20fps 리페인트로 CPU ~10%를 상시
        // 소모했다. 이제 worker의 wake가 Viewport(dirty 게이트)·상태 이벤트 모두를
        // 깨우므로 폴링이 불필요하다: 출력/상태가 있을 때만 프레임이 돈다.

        self.close_confirm_dialog(ui.ctx(), client, catalog);

        let rect = ui.available_rect_before_wrap();
        let layout = active_tab.layout.clone();
        let tab_id = active_tab.id.clone();
        let mut split_path = Vec::new();
        self.render_node(
            ui,
            rect,
            &layout,
            &mux,
            config,
            client,
            &tab_id,
            &mut split_path,
            catalog,
        );

        // 응답(MuxUpdated/Viewport)을 다음 프레임에서 수신하도록 보장
        self.flush_command_repaint(ui.ctx());
    }

    /// layout 트리를 rect 분할로 재귀 렌더한다.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn render_node(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        node: &LayoutNode,
        mux: &MuxSnapshot,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
        tab_id: &runtime::MuxTabId,
        path: &mut Vec<u8>,
        catalog: &i18n::Catalog,
    ) {
        match node {
            LayoutNode::Pane(pane_id) => {
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                // max_rect는 배치만 제한한다 — 이전 크기의 스냅샷이 이웃 pane을
                // 덮어 그리지 않게 페인터 클립도 pane 영역으로 줄인다
                child.set_clip_rect(rect.intersect(ui.clip_rect()));
                self.render_pane(&mut child, pane_id, mux, config, client, catalog);
                // 포커스된 pane 표시는 pane 헤더의 accent 하이라이트가 담당한다 —
                // 상단 2px 강조선은 헤더 색과 겹쳐 라인만 늘어 제거(#66 사용자).
            }
            LayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                // 목업처럼 pane을 붙이고 1px 구분선만 둔다 (기존 4px 투명 gap 제거).
                // 리사이즈 잡기는 split_handle이 히트영역을 ±2px 확장해 보장한다.
                let gap = 1.0;
                // 드래그 중이면 로컬 미리보기 ratio 사용 (릴리즈 시에만 명령 전송)
                let ratio = match &self.split_drag {
                    Some((drag_path, preview)) if drag_path == path => *preview,
                    _ => *ratio,
                };
                let (first_rect, second_rect, gap_rect) = match direction {
                    SplitDirection::Horizontal => {
                        // 좌/우 분할
                        let split_x = rect.min.x + (rect.width() - gap) * ratio;
                        (
                            egui::Rect::from_min_max(rect.min, egui::pos2(split_x, rect.max.y)),
                            egui::Rect::from_min_max(
                                egui::pos2(split_x + gap, rect.min.y),
                                rect.max,
                            ),
                            egui::Rect::from_min_max(
                                egui::pos2(split_x, rect.min.y),
                                egui::pos2(split_x + gap, rect.max.y),
                            ),
                        )
                    }
                    SplitDirection::Vertical => {
                        // 상/하 분할
                        let split_y = rect.min.y + (rect.height() - gap) * ratio;
                        (
                            egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, split_y)),
                            egui::Rect::from_min_max(
                                egui::pos2(rect.min.x, split_y + gap),
                                rect.max,
                            ),
                            egui::Rect::from_min_max(
                                egui::pos2(rect.min.x, split_y),
                                egui::pos2(rect.max.x, split_y + gap),
                            ),
                        )
                    }
                };
                path.push(0);
                self.render_node(
                    ui, first_rect, first, mux, config, client, tab_id, path, catalog,
                );
                path.pop();
                path.push(1);
                self.render_node(
                    ui,
                    second_rect,
                    second,
                    mux,
                    config,
                    client,
                    tab_id,
                    path,
                    catalog,
                );
                path.pop();
                // 핸들은 자식 pane들 **뒤에** 등록 — egui 히트테스트는 나중 등록이
                // 우선이라, ±2px 확장 히트영역이 터미널 선택 드래그에 밀리지 않는다
                // (codex 리뷰: 가장자리에서 리사이즈 대신 선택이 잡히는 문제).
                self.split_handle(ui, rect, gap_rect, *direction, gap, client, tab_id, path);
            }
        }
    }

    /// split 경계 드래그 핸들 — 드래그 중엔 split_drag로 로컬 미리보기만 갱신하고,
    /// 릴리즈 시 1회 ResizeSplit을 보낸다 (드래그 내내 DB 저장/이벤트 폭주 방지).
    #[allow(clippy::too_many_arguments)]
    fn split_handle(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        gap_rect: egui::Rect,
        direction: SplitDirection,
        gap: f32,
        client: &dyn RuntimeClient,
        tab_id: &runtime::MuxTabId,
        path: &[u8],
    ) {
        // 4px 경계는 잡기 어려우니 히트 영역만 양쪽 2px씩 확장 (시각 폭은 그대로)
        let hit_rect = gap_rect.expand2(match direction {
            SplitDirection::Horizontal => egui::vec2(2.0, 0.0),
            SplitDirection::Vertical => egui::vec2(0.0, 2.0),
        });
        let id = egui::Id::new(("split_handle", tab_id, path));
        let resp = ui.interact(hit_rect, id, egui::Sense::drag());
        let cursor = match direction {
            SplitDirection::Horizontal => egui::CursorIcon::ResizeHorizontal,
            SplitDirection::Vertical => egui::CursorIcon::ResizeVertical,
        };
        let resp = resp.on_hover_cursor(cursor);
        // 항상 1px 구분선(다크 헤어라인)을 그린다 — hover/drag 시 accent로 강조.
        if resp.hovered() || resp.dragged() {
            ui.painter()
                .rect_filled(gap_rect, 0.0, ui.visuals().selection.bg_fill);
        } else {
            ui.painter()
                .rect_filled(gap_rect, 0.0, egui::Color32::from_rgb(0x3a, 0x3a, 0x42));
        }
        if resp.dragged()
            && let Some(pointer) = resp.interact_pointer_pos()
        {
            let ratio = match direction {
                SplitDirection::Horizontal => (pointer.x - rect.min.x) / (rect.width() - gap),
                SplitDirection::Vertical => (pointer.y - rect.min.y) / (rect.height() - gap),
            }
            .clamp(0.1, 0.9);
            self.split_drag = Some((path.to_vec(), ratio));
        }
        if resp.drag_stopped()
            && let Some((drag_path, ratio)) = self.split_drag.take()
            && drag_path == path
        {
            self.send(
                client,
                RuntimeCommand::ResizeSplit {
                    tab: tab_id.clone(),
                    path: drag_path,
                    ratio,
                },
            );
        }
    }

    fn render_pane(
        &mut self,
        ui: &mut egui::Ui,
        pane_id: &runtime::MuxPaneId,
        mux: &MuxSnapshot,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
        catalog: &i18n::Catalog,
    ) {
        // 헤더-터미널 사이 기본 item_spacing 틈에 패널색이 비쳤다(#81) — 0으로.
        ui.spacing_mut().item_spacing.y = 0.0;
        let Some(pane) = mux
            .tabs
            .iter()
            .flat_map(|tab| &tab.panes)
            .find(|pane| &pane.id == pane_id)
        else {
            return;
        };
        let focused = mux.focused_pane.as_ref() == Some(pane_id);
        // pane 전체 배경 interact — 터미널 위젯보다 먼저 등록해 터미널 밖 영역과
        // "세션 없음"/"연결 중"(스냅샷 지연) 상태에서도 우클릭 메뉴·드롭이 동작한다
        // (codex P2). 터미널 위에서는 나중에 등록되는 터미널 위젯이 입력을 받는다.
        let pane_rect = ui.max_rect();
        let pane_resp = ui.interact(
            pane_rect,
            egui::Id::new(("pane_bg", pane_id)),
            egui::Sense::click(),
        );
        if pane_resp.clicked() && !focused {
            self.request_pane_focus(client, pane_id.clone());
        }
        self.pane_context_menu(&pane_resp, pane_id, config, client, catalog);

        // pane 헤더 바 (2026-07-05): [상태 제목] [×] ... [+셸] [분할│] [분할─]
        // 닫기/분할 대상이 "이 pane"임이 시각적으로 자명하다 — 탭바 제거의 대체 UI.
        // pane 헤더는 터미널과 동일한 다크 배경(별도 틴트 없음 — 사용자 요청 2026-07-07).
        // 아래 hairline 한 줄로만 최소 분리한다. 포커스는 제목/글리프 accent 색으로 표시.
        let status = pane
            .session_id
            .and_then(|s| self.sessions.get(&s))
            .and_then(|v| v.status);
        let is_agent = status.is_some();
        let accent = ui.visuals().selection.bg_fill;
        // 터미널 렌더러의 default_bg와 동일 (renderer_egui) — 헤더가 터미널로 이어져 보이게.
        let header_fill = egui::Color32::from_rgb(0x18, 0x18, 0x1c);
        let header = egui::Frame::new()
            .fill(header_fill)
            // 좌측 여백 축소 — ◆ 아이콘이 왼쪽 가까이 붙게(사용자 요청).
            .inner_margin(egui::Margin {
                left: 3,
                right: 8,
                top: 4,
                bottom: 4,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // 아이콘-제목 간격을 좁힌다(사용자 요청). 글리프 박스도 축소.
                    ui.spacing_mut().item_spacing.x = 4.0;
                    // 타입 글리프(◆/▸)를 도형으로 — 상태 있으면 상태색, 없으면 focus/dim
                    let (grect, _) =
                        ui.allocate_exact_size(egui::vec2(11.0, 14.0), egui::Sense::hover());
                    let glyph_color = if let Some(s) = status {
                        crate::ui::file_tree::session_status_color(Some(s), ui.visuals())
                    } else if focused {
                        accent
                    } else {
                        egui::Color32::from_rgb(0x8b, 0x8f, 0x98)
                    };
                    crate::ui::file_tree::paint_type_glyph(
                        ui.painter(),
                        grect.center(),
                        is_agent,
                        glyph_color,
                    );
                    // 타이틀 — 다크 헤더 위이므로 밝은 색 명시 (theme text는 라이트
                    // 테마에서 어두워 안 보인다). focused는 accent.
                    let title_color = if focused {
                        accent
                    } else {
                        egui::Color32::from_rgb(0xc8, 0xcc, 0xd2)
                    };
                    // 우측 컨트롤 폭 예약 — 긴 제목이 닫기/분할 버튼을 밀어내지
                    // 않게 truncate 최대폭 제한 (codex, 사이드바 헤더와 동일 패턴)
                    let osc = self.session_osc_title(pane.session_id);
                    let pane_title = self.resolve_session_title(
                        &pane.title,
                        pane.session_id,
                        osc.as_deref(),
                        catalog,
                    );
                    let title_resp = ui
                        .scope(|ui| {
                            ui.set_max_width((ui.available_width() - 120.0).max(30.0));
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(pane_title)
                                        .size(13.0)
                                        .strong()
                                        .color(title_color),
                                )
                                .sense(egui::Sense::click())
                                .truncate(),
                            )
                        })
                        .inner;
                    if title_resp.clicked() && !focused {
                        self.request_pane_focus(client, pane_id.clone());
                    }
                    ui.add_space(4.0); // × 앞 여백 (목업 §pane-head)
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("×")
                                    .color(egui::Color32::from_rgb(0x8b, 0x8f, 0x98)),
                            )
                            .frame(false),
                        )
                        .on_hover_text(catalog.t("workspace.close_pane", &[]))
                        .clicked()
                    {
                        self.request_close_pane(client, pane_id.clone());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if crate::ui::file_tree::paint_split(ui, true)
                            .on_hover_text(catalog.t("workspace.split_vertical_short", &[]))
                            .clicked()
                        {
                            self.send(
                                client,
                                RuntimeCommand::SplitPane {
                                    pane: pane_id.clone(),
                                    direction: SplitDirection::Vertical,
                                    scrollback_lines: config.scrollback_lines as usize,
                                },
                            );
                        }
                        if crate::ui::file_tree::paint_split(ui, false)
                            .on_hover_text(catalog.t("workspace.split_horizontal_short", &[]))
                            .clicked()
                        {
                            self.send(
                                client,
                                RuntimeCommand::SplitPane {
                                    pane: pane_id.clone(),
                                    direction: SplitDirection::Horizontal,
                                    scrollback_lines: config.scrollback_lines as usize,
                                },
                            );
                        }
                        let (pr, plus_resp) =
                            ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
                        let pcol = if plus_resp.hovered() {
                            egui::Color32::from_rgb(0xc8, 0xcc, 0xd2)
                        } else {
                            egui::Color32::from_rgb(0x8b, 0x8f, 0x98)
                        };
                        ui.painter().text(
                            pr.center(),
                            egui::Align2::CENTER_CENTER,
                            "+",
                            egui::FontId::proportional(15.0),
                            pcol,
                        );
                        if plus_resp
                            .on_hover_text(catalog.t("workspace.new_shell", &[]))
                            .clicked()
                        {
                            self.send(
                                client,
                                RuntimeCommand::SpawnShell {
                                    cols: 80,
                                    rows: 24,
                                    scrollback_lines: config.scrollback_lines as usize,
                                },
                            );
                        }
                    });
                });
            });
        // 헤더-터미널 최소 구분선(1px hairline) — 배경색이 같아 경계가 없어지므로.
        let hr = header.response.rect;
        ui.painter().hline(
            hr.x_range(),
            hr.bottom() - 0.5,
            egui::Stroke::new(1.0, egui::Color32::from_rgb(0x2a, 0x2a, 0x30)),
        );
        if pane.session_id.is_some() {
            if pane_resp
                .dnd_hover_payload::<std::path::PathBuf>()
                .is_some()
                || pane_resp
                    .dnd_hover_payload::<TerminalTextDragPayload>()
                    .is_some()
            {
                ui.painter().rect_stroke(
                    pane_rect,
                    2.0,
                    egui::Stroke::new(1.5, ui.visuals().selection.stroke.color),
                    egui::StrokeKind::Inside,
                );
            }
            if let Some(session) = pane.session_id {
                if let Some(path) = pane_resp.dnd_release_payload::<std::path::PathBuf>() {
                    let bytes = path_insert_paste_bytes(
                        &path,
                        self.session_shell_kind(session),
                        self.session_bracketed_paste(session),
                    );
                    self.send(client, RuntimeCommand::WriteInput { session, bytes });
                }
                if let Some(text) = pane_resp.dnd_release_payload::<TerminalTextDragPayload>() {
                    let bytes = terminal_text_paste_bytes(
                        &text.text,
                        self.session_bracketed_paste(session),
                    );
                    self.send(client, RuntimeCommand::WriteInput { session, bytes });
                }
            }
        }
        let Some(session) = pane.session_id else {
            ui.label(catalog.t("workspace.no_session", &[]));
            return;
        };

        // 터미널 폰트는 UI 배율(zoom_factor)로 같이 커지므로 font_size를 배율로 역보정해
        // 물리 크기를 유지한다(UI만 스케일, 터미널 독립 — 2026-07-13). cell_size·draw가
        // 같은 값을 써야 격자/선택이 일치한다.
        let metrics = renderer_egui::CellMetrics {
            font_size: config.font_size / self.ui_scale,
            line_height: config.line_height,
        };
        // pane 크기 → cols/rows. visible pane 전부 대상 — split 직후 기존 pane의
        // PTY 크기가 틀어지는 문제 방지 (runtime도 visible 세션을 모두 push한다)
        let cell = renderer_egui::cell_size(ui.ctx(), metrics);
        let avail = ui.available_size();
        let cols =
            ((renderer_egui::grid_width_for_available(avail.x) / cell.x) as u16).clamp(10, 500);
        let rows = renderer_egui::grid_rows_for_available(avail.y, cell.y);
        if self.sent_sizes.get(&session) != Some(&(cols, rows)) {
            self.sent_sizes.insert(session, (cols, rows));
            self.send(
                client,
                RuntimeCommand::Resize {
                    session,
                    cols,
                    rows,
                },
            );
        }

        let selected = self.selection.is_some_and(|(s, _, _)| s == session);
        let (exit_code, bracketed, snapshot) = {
            let view = self.sessions.entry(session).or_default();
            // 선택이 없으면(freeze 해제) freeze 중 보관한 최신본으로 catch-up한다 — 새
            // Viewport가 안 와도 화면이 선택 당시에 멈추지 않게(codex).
            if !selected && let Some(pending) = view.pending_snapshot.take() {
                view.summary = last_line_summary(&pending);
                view.snapshot = Some(pending);
                view.snapshot_gen = view.snapshot_gen.wrapping_add(1);
            }
            let Some(snapshot) = view.snapshot.clone() else {
                ui.label(catalog.t("workspace.connecting", &[]));
                return;
            };
            (view.exit_code, view.bracketed_paste, snapshot)
        };

        // 런타임의 focused pane과 현재 native UI의 논리적 키보드 소유 상태를 draw 전에
        // 확정한다. renderer가 이 값을 바탕으로 같은 프레임에 egui 공식 IME 소유권까지
        // 동기화하므로, 기존 egui owner를 “요청할지”의 선행 조건으로 쓰지 않는다.
        let terminal_refocus_pending = self.pending_focus.as_ref() == Some(pane_id);
        let terminal_input_owner =
            terminal_input_owner(pane_id, focused, self.pending_focus.as_ref());
        let any_blocking_window_visible = ui.ctx().memory(|mem| {
            mem.areas()
                .visible_layer_ids()
                .iter()
                .any(is_blocking_terminal_window)
        });
        let terminal_keyboard_active = terminal_input_owner
            && terminal_keyboard_input_allowed(
                ui.ctx().text_edit_focused(),
                ui.ctx().any_popup_open(),
                any_blocking_window_visible,
                terminal_refocus_pending,
            );
        let preedit =
            (terminal_keyboard_active && !self.preedit.is_empty()).then_some(self.preedit.as_str());
        // 이 세션의 선택 영역 (정규화)
        let selection_range = self
            .selection
            .and_then(|(s, a, b)| (s == session).then_some((a.min(b), a.max(b))));
        let output = {
            let view = self.sessions.entry(session).or_default();
            renderer_egui::draw(
                ui,
                &snapshot,
                metrics,
                &mut view.render_cache,
                preedit,
                terminal_keyboard_active,
                selection_range,
                view.snapshot_gen,
            )
        };
        // B1 실측: 이 프레임에 그린 pane들의 렌더 비용을 합산한다 (visible pane 전부).
        self.frame_counters += output.counters;

        // 선택된 텍스트 위에서 시작한 드래그는 terminal-internal DnD payload가 된다.
        // 그 외의 마우스 드래그는 기존 셀 선택 동작을 유지한다.
        if !egui::DragAndDrop::has_any_payload(ui.ctx()) {
            let cell_at = |pos: egui::Pos2| -> usize {
                let col = ((pos.x - output.origin.x) / output.cell_size.x)
                    .floor()
                    .clamp(0.0, snapshot.cols.saturating_sub(1) as f32)
                    as usize;
                let row = ((pos.y - output.origin.y) / output.cell_size.y)
                    .floor()
                    .clamp(0.0, snapshot.rows.saturating_sub(1) as f32)
                    as usize;
                row * snapshot.cols as usize + col
            };
            // 폴더/URL hover·단일 클릭 — 이동 가능한 폴더 단어 위에서는 커서를 손가락으로
            // 바꾸고 클릭하면 그 폴더로 cd, URL 위에서는 클릭하면 기본 브라우저로 연다
            // (2026-07-14/2026-07-17 사용자). 파일은 커서를 바꾸지 않는다(복사용 선택과
            // 혼동 방지 — 열기는 우클릭 메뉴). alt screen(TUI)은 cd 주입 금지라 통째로 비활성.
            if !snapshot.is_alt_screen
                && let Some(pos) = output.response.hover_pos()
                && let Some((s, e)) = word_range_at(&snapshot, cell_at(pos))
            {
                let word = renderer_egui::selection_text(&snapshot, s, e);
                // URL은 cwd 해석이 필요 없는 문자열 판정이라 폴더보다 먼저 본다.
                if let Some(url) = extract_url(&word) {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    if output.response.clicked() && focused {
                        // 더블클릭이 clicked를 두 번 발화 — 같은 URL 연속 열기를 막는다
                        // (last_dir_click과 동일 관례).
                        let duplicate = self.last_url_click.as_ref().is_some_and(|(u, at)| {
                            u == url && at.elapsed() < std::time::Duration::from_millis(800)
                        });
                        if !duplicate {
                            self.last_url_click = Some((url.to_owned(), std::time::Instant::now()));
                            if let Err(err) = auth::open_in_browser(url) {
                                self.error_is_pressure = false;
                                self.error = Some(format!("{err:#}"));
                            }
                        }
                    }
                } else if matches!(
                    self.resolve_path_cached(ui.ctx(), session, &word),
                    Some(PathClick::Dir(_))
                ) {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    // 포커스된 pane에서만 cd — 비포커스 pane을 포커스하려는 클릭이
                    // cd까지 주입하면 안 된다 (codex 리뷰 MEDIUM). 첫 클릭은 포커스만,
                    // 포커스된 뒤의 클릭이 이동한다.
                    if output.response.clicked() && focused {
                        // 클릭은 캐시를 거치지 않는다 — 사용자가 방금 손으로 cd를
                        // 타이핑했으면 2s TTL 캐시가 옛 cwd 기준 경로를 줄 수 있다
                        // (codex 리뷰 MEDIUM). hover 커서는 캐시(성능), 실행은 신선 해석.
                        self.path_click_cache = None;
                        if let Some(PathClick::Dir(path)) = self.resolve_path_fresh(session, &word)
                        {
                            // 더블클릭은 clicked를 두 번 발화 — 같은 경로 연속 cd를 막는다.
                            let duplicate = self.last_dir_click.as_ref().is_some_and(|(p, at)| {
                                *p == path && at.elapsed() < std::time::Duration::from_millis(800)
                            });
                            if !duplicate {
                                self.last_dir_click =
                                    Some((path.clone(), std::time::Instant::now()));
                                // file tree의 "이 폴더로 이동"과 같은 헬퍼 — 셸별
                                // cd 문법/인용(PowerShell -LiteralPath, cmd /d)을 공유.
                                let bytes = cd_paste_bytes(
                                    &path,
                                    self.session_shell_kind(session),
                                    bracketed,
                                );
                                self.send(client, RuntimeCommand::WriteInput { session, bytes });
                                // cd로 셸 cwd가 바뀐다 — 방금 만든 해석 캐시도 무효.
                                self.invalidate_session_cwd(session);
                                self.path_click_cache = None;
                            }
                        }
                    }
                }
            }
            if output.response.double_clicked()
                && let Some(pos) = output.response.interact_pointer_pos()
            {
                // 더블클릭 → 커서 아래 단어(공백 구분) 선택 (복사용). URL 열기는 단일
                // 클릭(위 hover/click 블록)으로 이동 — 여기서도 열면 이중 발화된다
                // (2026-07-17). 파일 열기는 우클릭 메뉴, 폴더 진입은 단일 클릭 담당.
                if let Some((s, e)) = word_range_at(&snapshot, cell_at(pos)) {
                    self.selection = Some((session, s, e));
                }
            } else if output.response.drag_started()
                && let Some(pos) = output.response.interact_pointer_pos()
            {
                let idx = cell_at(pos);
                if let Some((start, end)) = selection_range
                    && selection_range_contains(start, end, idx)
                {
                    let text = renderer_egui::selection_text(&snapshot, start, end);
                    if !text.is_empty() {
                        output
                            .response
                            .dnd_set_drag_payload(TerminalTextDragPayload { text });
                    }
                } else {
                    self.selection = Some((session, idx, idx));
                    self.drag_autoscroll_residual = 0.0;
                }
            } else if output.response.dragged()
                && let Some(pos) = output.response.interact_pointer_pos()
                && let Some((s, anchor, _)) = self.selection
                && s == session
            {
                // 드래그 중 스크롤로 도착한 새 화면은 freeze로 pending에 보관돼 있다 —
                // 즉시 반영하고, 화면 좌표 기반 선택 앵커를 스크롤량만큼 이동시켜
                // 같은 텍스트를 계속 가리키게 한다. 끝점은 포인터(화면 위치)를 따른다.
                let mut anchor = anchor;
                {
                    let view = self.sessions.entry(session).or_default();
                    if view.pending_snapshot.as_ref().is_some_and(|p| {
                        (p.cols, p.rows) == (snapshot.cols, snapshot.rows)
                            && p.scroll_offset != snapshot.scroll_offset
                    }) && let Some(pending) = view.pending_snapshot.take()
                    {
                        // scroll_offset 증가(과거로) = 내용이 아래로 이동 → 앵커도 아래로
                        let delta_rows = pending.scroll_offset - snapshot.scroll_offset;
                        anchor = shift_selection_cell(
                            anchor,
                            delta_rows,
                            snapshot.cols as usize,
                            snapshot.rows as usize,
                        );
                        view.summary = last_line_summary(&pending);
                        view.snapshot = Some(pending);
                        view.snapshot_gen = view.snapshot_gen.wrapping_add(1);
                    }
                }
                self.selection = Some((session, anchor, cell_at(pos)));
                // 포인터가 pane 세로 경계를 벗어나면 초과 거리에 비례한 속도로
                // 오토스크롤한다 (iTerm2 관례 — T4). cell_at의 clamp가 끝점을
                // 마지막/첫 행(좌우는 열 경계)에 붙잡아 선택이 계속 확장된다.
                let rect = output.response.rect;
                let rate = drag_autoscroll_rate(pos.y, rect.top(), rect.bottom(), cell.y);
                if rate != 0.0 {
                    // 속도(행/초)×dt 누적 → 정수 행만 전송. dt는 정지 프레임 폭주 대비 clamp.
                    let dt = ui.input(|i| i.stable_dt).min(0.1);
                    self.drag_autoscroll_residual += rate * dt;
                    let step = self.drag_autoscroll_residual.trunc() as i32;
                    if step != 0 {
                        self.drag_autoscroll_residual -= step as f32;
                        // send()가 아니라 선택 보존 경로 — send의 Scroll 선택 해제(휠 UX)와
                        // 충돌하면 첫 스크롤 직후 선택·오토스크롤이 함께 죽는다(A3 버그 #1).
                        self.send_keep_selection(
                            client,
                            RuntimeCommand::Scroll {
                                session,
                                delta: step,
                            },
                        );
                    }
                    // egui는 이벤트 드리븐 — 포인터가 안 움직여도 매 프레임 이어가도록
                    // 예약한다. 버튼 릴리즈 시 이 분기에 안 들어와 예약이 끊긴다(idle 0).
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(30));
                } else {
                    self.drag_autoscroll_residual = 0.0;
                }
            } else if output.response.clicked() {
                self.selection = None; // 단순 클릭은 선택 해제 (더블클릭 아님)
            }
        }

        // 포커스 pane 파란 테두리는 사용자 요청으로 제거(2026-07-04) — 단일 pane 사용 시
        // 항상 보여 거슬림. 다중 pane에서 포커스 식별이 다시 필요해지면 "pane 2개 이상일
        // 때만 표시" 조건으로 복원할 것.
        // (egui 포커스는 클릭 시에만 요청한다 — 매 프레임 request_focus는 다른 창의 입력 포커스를 뺏는다.)
        // pending 포커스는 pane이 실제로 그려진 이 시점에 1회 소비한다 —
        // 매 프레임 요청은 다른 창 입력을 뺏고, "연결 중" 단계에서 소비하면
        // 요청이 유실된다 (리뷰 반영).
        if focused && self.pending_focus.as_ref() == Some(pane_id) {
            self.pending_focus = None;
            request_terminal_focus(&output.response);
        }
        if output.response.clicked() {
            self.terminal_focus_claimed = true;
            request_terminal_focus(&output.response);
            // 이미 runtime focus인 pane을 다시 클릭해도 stale TextEdit focus를 누르고
            // 다음 keydown부터 터미널로 받도록 refocus를 예약한다.
            if !terminal_refocus_pending {
                self.begin_terminal_refocus(pane_id.clone());
            }
            // pane 배경이 같은 클릭을 먼저 받았다면 이미 FocusPane을 보냈다.
            if !focused && !terminal_refocus_pending {
                self.request_pane_focus(client, pane_id.clone());
            }
        }

        // 파일 트리에서 드래그한 경로를 터미널 위에 드롭 → 입력으로 삽입 (2026-07-05).
        // hover 테두리는 위 pane 배경 경로가 pane_rect에 그린다.
        if let Some(path) = output.response.dnd_release_payload::<std::path::PathBuf>() {
            let bytes = path_insert_paste_bytes(&path, self.session_shell_kind(session), bracketed);
            self.send(client, RuntimeCommand::WriteInput { session, bytes });
            if !focused {
                self.request_pane_focus(client, pane_id.clone());
            }
        }
        if let Some(text) = output
            .response
            .dnd_release_payload::<TerminalTextDragPayload>()
        {
            let bytes = terminal_text_paste_bytes(&text.text, bracketed);
            self.send(client, RuntimeCommand::WriteInput { session, bytes });
            if !focused {
                self.request_pane_focus(client, pane_id.clone());
            }
        }
        // 터미널 위 우클릭도 같은 메뉴 (터미널 위젯이 topmost라 배경 interact가 못 받음)
        self.pane_context_menu(&output.response, pane_id, config, client, catalog);

        // 터미널 텍스트 검색 (T3): 매치 하이라이트 + 우상단 검색 바 + 스크롤 이동.
        self.render_terminal_search(
            ui,
            session,
            output.response.rect,
            output.origin,
            output.cell_size,
            &snapshot,
            client,
            catalog,
        );

        // 검색 TextEdit/팝업 같은 overlay가 renderer 뒤에서 포커스를 가져갈 수도 있으므로
        // 이벤트를 소비하는 바로 이 시점에 egui의 공식 IME 소유권을 다시 확인한다.
        let terminal_owns_ime_events = terminal_keyboard_active
            && ui
                .ctx()
                .memory(|memory| memory.owns_ime_events(output.response.id));
        if terminal_owns_ime_events {
            let mut pending: Vec<u8> = Vec::new();
            // macOS/winit은 한 번의 IME 종료 키를 `Ime::Commit`과 일반 `Text` 양쪽으로
            // 전달하거나, 반대로 `Text`를 생략할 수 있다. AppKit/egui에서 관찰한 실제
            // printable key-down 수를 한도 삼아 두 텍스트 경로와 fallback을 한 번에
            // 조정해야 공백·쉼표의 중복과 첫 문장부호 누락을 동시에 막을 수 있다.
            let preedit_active_before_input = !self.preedit.is_empty();
            let native_key_downs = std::mem::take(&mut self.native_printable_key_downs);
            let native_clipboard_paste_requested =
                std::mem::take(&mut self.native_clipboard_paste_requested);
            let ime_reconciliation = ui.input(|input| {
                reconcile_ime_text_events(
                    &native_key_downs,
                    &input.raw.events,
                    preedit_active_before_input,
                )
            });
            let mut copy_text: Option<String> = None;
            let mut image_paste_trigger =
                native_clipboard_paste_requested.then_some(ClipboardPasteTrigger::NativeKeyDown);
            let mut text_paste_bytes: Option<Vec<u8>> = None;
            ui.input(|input| {
                let modifiers = input.modifiers;
                for (event_index, event) in input.raw.events.iter().enumerate() {
                    if let egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) = event {
                        self.preedit = text.clone();
                        continue;
                    }
                    if let egui::Event::Ime(egui::ImeEvent::Commit(_)) = event {
                        self.preedit.clear();
                    }
                    if let Some(text) = ime_reconciliation.event_text[event_index].as_ref() {
                        pending.extend(text.as_bytes());
                        continue;
                    }
                    if matches!(event, egui::Event::Paste(_)) {
                        text_paste_bytes = input_mapper::map_event(event, bracketed, &modifiers);
                        continue;
                    }
                    // Cmd+C(macOS)/Ctrl+C(그 외)의 Copy 이벤트: 선택이 있으면 복사가
                    // 우선 — 이벤트를 소비해 ^C 전송(비macOS 매핑)을 막는다 (2026-07-05)
                    if matches!(event, egui::Event::Copy)
                        && let Some((sel_session, a, b)) = self.selection
                        && sel_session == session
                    {
                        copy_text =
                            Some(renderer_egui::selection_text(&snapshot, a.min(b), a.max(b)));
                        continue;
                    }
                    if is_clipboard_paste_shortcut(event) {
                        image_paste_trigger.get_or_insert(ClipboardPasteTrigger::EguiShortcut);
                        continue;
                    }
                    // Shift+화살표 → 마우스 드래그처럼 선택 확장. 터미널로는 안 보낸다.
                    // alt-screen(vim/less 등 TUI)에선 앱이 shift+화살표를 쓰므로 가로채지
                    // 않고 그대로 통과시킨다.
                    if !snapshot.is_alt_screen
                        && let egui::Event::Key {
                            key,
                            pressed: true,
                            modifiers: m,
                            ..
                        } = event
                        && m.shift
                        && matches!(
                            key,
                            egui::Key::ArrowLeft
                                | egui::Key::ArrowRight
                                | egui::Key::ArrowUp
                                | egui::Key::ArrowDown
                        )
                    {
                        let cols = snapshot.cols as usize;
                        let max_idx = (cols * snapshot.rows as usize).saturating_sub(1);
                        // 앵커: 기존 선택이 있으면 유지, 없으면 커서 위치에서 시작.
                        let (anchor, end) = match self.selection {
                            Some((s, a, e)) if s == session => (a, e),
                            _ => {
                                let cur = snapshot.cursor.row as usize * cols
                                    + snapshot.cursor.col as usize;
                                (cur, cur)
                            }
                        };
                        let new_end = match key {
                            egui::Key::ArrowRight => (end + 1).min(max_idx),
                            egui::Key::ArrowLeft => end.saturating_sub(1),
                            egui::Key::ArrowDown => (end + cols).min(max_idx),
                            egui::Key::ArrowUp => end.saturating_sub(cols),
                            _ => end,
                        };
                        self.selection = Some((session, anchor, new_end));
                        continue;
                    }
                    // Shift+Enter → 줄바꿈(LF). claude/codex는 \n을 입력 줄바꿈으로, \r을
                    // 제출로 구분한다(claude /terminal-setup 관례). Enter(\r)는 그대로 제출.
                    if let egui::Event::Key {
                        key: egui::Key::Enter,
                        pressed: true,
                        modifiers: m,
                        ..
                    } = event
                        && m.shift
                    {
                        pending.push(b'\n');
                        continue;
                    }
                    if let Some(bytes) = input_mapper::map_event(event, bracketed, &modifiers) {
                        pending.extend(bytes);
                    }
                }
            });
            pending.extend(&ime_reconciliation.fallback_bytes);
            if let Some(text) = copy_text {
                ui.ctx().copy_text(text);
            }
            if let Some(paste_trigger) = image_paste_trigger {
                if should_skip_paste_task(
                    paste_trigger,
                    text_paste_bytes.is_some(),
                    self.last_text_paste,
                    self.last_native_paste,
                ) {
                    // 같은 ⌘V 제스처의 native key-down 또는 Event::Paste에서 이미 처리했다.
                    // 뒤늦은 key-up fallback까지 돌리면 이미지/텍스트가 한 번 더 붙는다.
                    self.last_text_paste = None;
                    self.last_native_paste = None;
                } else {
                    // 파일/이미지 판별 + PNG 인코딩은 백그라운드로(UI 딜레이 제거 — 2026-07-07).
                    // 완료는 show()의 poll_paste_task가 소비한다. 연타 ⌘V는 최신 것으로 대체.
                    if paste_trigger == ClipboardPasteTrigger::NativeKeyDown {
                        self.last_native_paste = Some(std::time::Instant::now());
                    }
                    let text_fallback = text_paste_bytes.take();
                    self.paste_task = Some(PendingPaste {
                        rx: crate::ui::clipboard_image::paste_clipboard_paths_or_image_background(
                            ui.ctx().clone(),
                            text_fallback.is_some(),
                        ),
                        session,
                        bracketed,
                        shell_kind: self.session_shell_kind(session),
                        text_fallback,
                        requested_at: std::time::Instant::now(),
                    });
                }
            } else if let Some(bytes) = text_paste_bytes {
                self.last_text_paste = Some(std::time::Instant::now());
                pending.extend(bytes);
            }
            if !pending.is_empty() {
                // 선택 해제는 send()가 WriteInput 공통 지점에서 처리한다.
                self.send(
                    client,
                    RuntimeCommand::WriteInput {
                        session,
                        bytes: pending,
                    },
                );
            }
        }

        // 마우스 휠 → 스크롤백 (focused pane만 — 비활성 pane은 Viewport가
        // push되지 않아(14.4) 스크롤해도 화면이 안 바뀐다)
        if focused && output.response.hovered() {
            let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
            self.scroll_residual += scroll_y / cell.y;
            let whole_rows = self.scroll_residual.trunc() as i32;
            if whole_rows != 0 {
                self.scroll_residual -= whole_rows as f32;
                self.send(
                    client,
                    RuntimeCommand::Scroll {
                        session,
                        delta: whole_rows,
                    },
                );
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        if let Some(code) = exit_code {
            let code = code
                .map(|c| c.to_string())
                .unwrap_or_else(|| catalog.t("workspace.exit_unknown", &[]));
            // renderer가 pane 최하단까지 쓰므로 상태를 새 행으로 배치하지 않고 overlay한다.
            // 종료 표시는 유지하면서 하단에 다시 한 행짜리 빈 띠가 생기는 회귀를 막는다.
            ui.painter().text(
                output.response.rect.left_bottom() + egui::vec2(6.0, -4.0),
                egui::Align2::LEFT_BOTTOM,
                catalog.t("workspace.exited", &[("code", code.as_str())]),
                egui::FontId::proportional(12.0),
                ui.visuals().weak_text_color(),
            );
        }

        // 포커스 pane 탑라인 — 포인트색 1px 라인을 상단에 항상 유지(단일/다중 pane 동일).
        if focused {
            ui.painter().hline(
                pane_rect.x_range(),
                pane_rect.top() + 0.5,
                egui::Stroke::new(1.0, accent),
            );
        }
        // pane 전체 강조 플래시 — 포커스 이동·입력요청·작업완료 시 2초 페이드(2026-07-12 사용자).
        if let Some(session) = pane.session_id
            && let Some(&until) = self.session_flash.get(&session)
        {
            let now = std::time::Instant::now();
            if now < until {
                let remain = (until - now).as_secs_f32() / PANE_FLASH.as_secs_f32();
                let alpha = (remain.clamp(0.0, 1.0) * 255.0) as u8;
                let color = egui::Color32::from_rgba_unmultiplied(
                    accent.r(),
                    accent.g(),
                    accent.b(),
                    alpha,
                );
                ui.painter().rect_stroke(
                    pane_rect.shrink(1.0),
                    2.0,
                    egui::Stroke::new(2.0, color),
                    egui::StrokeKind::Inside,
                );
                ui.ctx().request_repaint(); // 페이드 애니메이션
            }
        }
    }

    /// 명령을 보냈거나 spawn 응답 대기 중이면 repaint를 예약한다 —
    /// 느린 spawn(keyring 등)도 응답 이벤트가 올 때까지 폴링이 끊기지 않는다.
    /// show()의 모든 return 경로에서 호출할 것.
    /// pane 닫기 요청 — 실행 중 세션이면 확인을 거치고, 아니면 즉시 닫는다.
    /// (세션 상태를 모르면 보수적으로 확인을 띄운다 — 실수 즉사 방지가 목적.)
    /// 사이드바 컨텍스트 메뉴(App 경유)도 같은 경로를 쓴다.
    pub fn request_close_pane(&mut self, client: &dyn RuntimeClient, pane: runtime::MuxPaneId) {
        let running = self
            .mux
            .as_ref()
            .and_then(|mux| {
                mux.tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .find(|p| p.id == pane)
            })
            .and_then(|p| p.session_id)
            .map(|s| {
                self.sessions
                    .get(&s)
                    .is_none_or(|view| view.exit_code.is_none())
            })
            .unwrap_or(false);
        if running {
            self.confirm_close = Some(pane);
            // 다이얼로그는 다음 프레임에 그려진다(show 초입 호출 순서) — 리페인트를
            // 예약해 × 클릭 직후 확인창이 바로 뜨게 한다 (codex).
            self.command_sent = true;
        } else {
            self.send(client, RuntimeCommand::ClosePane { pane });
        }
    }

    /// 닫기 확인 다이얼로그 (request_close_pane이 세팅) — 실행 중 세션 종료 경고.
    fn close_confirm_dialog(
        &mut self,
        ctx: &egui::Context,
        client: &dyn RuntimeClient,
        catalog: &i18n::Catalog,
    ) {
        let Some(pane) = self.confirm_close.clone() else {
            return;
        };
        // 대상 pane이 그 사이 사라졌으면(셸 exit 등) 조용히 정리
        let alive = self
            .mux
            .as_ref()
            .is_some_and(|m| m.tabs.iter().flat_map(|t| &t.panes).any(|p| p.id == pane));
        if !alive {
            self.confirm_close = None;
            return;
        }
        let mut open = true;
        egui::Window::new(catalog.t("workspace.close_confirm.title", &[]))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(catalog.t("workspace.close_confirm.body", &[]));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(catalog.t("action.close", &[])).clicked() {
                        self.send(client, RuntimeCommand::ClosePane { pane: pane.clone() });
                        self.confirm_close = None;
                    }
                    if ui.button(catalog.t("action.cancel", &[])).clicked() {
                        self.confirm_close = None;
                    }
                });
            });
        if !open {
            self.confirm_close = None;
        }
    }

    /// pane 우클릭 메뉴 — 분할/닫기 (2026-07-05, 선택한 pane 단위 제어).
    fn pane_context_menu(
        &mut self,
        resp: &egui::Response,
        pane_id: &runtime::MuxPaneId,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
        catalog: &i18n::Catalog,
    ) {
        resp.context_menu(|ui| {
            let session = self.mux.as_ref().and_then(|mux| {
                mux.tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .find(|pane| &pane.id == pane_id)
                    .and_then(|pane| pane.session_id)
            });
            // 선택 텍스트(파일명 드래그)가 열 수 있는 파일이면 "열기" 항목을 맨 위에
            // (2026-07-14 사용자: 더블클릭 열기는 복사와 겹쳐 우클릭 메뉴로). 메뉴가
            // 열려 있는 동안만 평가되고, 해석은 resolve_path_cached의 TTL 캐시를 탄다.
            if let Some(sel_session) = session
                && let Some((s, a, b)) = self.selection
                && s == sel_session
            {
                let text = self
                    .sessions
                    .get(&sel_session)
                    .and_then(|view| view.snapshot.as_ref())
                    .map(|snap| renderer_egui::selection_text(snap, a.min(b), a.max(b)));
                if let Some(text) = text
                    && !text.trim().is_empty()
                    && !text.contains('\n')
                    && let Some(PathClick::OpenFile(path)) =
                        self.resolve_path_cached(ui.ctx(), sel_session, text.trim())
                {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.to_string_lossy().into_owned());
                    if ui
                        .button(catalog.t("workspace.open_file", &[("name", name.as_str())]))
                        .clicked()
                    {
                        platform::open_path(&path);
                        ui.close();
                    }
                    ui.separator();
                }
            }
            // 복사: 선택 텍스트가 있으면 표시 ("열기" 항목의 selection 판별 코드를 재사용).
            if let Some(sel_session) = session
                && let Some((s, a, b)) = self.selection
                && s == sel_session
            {
                let text = self
                    .sessions
                    .get(&sel_session)
                    .and_then(|view| view.snapshot.as_ref())
                    .map(|snap| renderer_egui::selection_text(snap, a.min(b), a.max(b)));
                if let Some(text) = text
                    && !text.trim().is_empty()
                    && ui.button(catalog.t("workspace.menu.copy", &[])).clicked()
                {
                    ui.ctx().copy_text(text);
                    ui.close();
                }
            }
            // 붙여넣기: 세션이 있으면 항상 표시. 드래그앤드롭 텍스트 붙여넣기(위 dnd_release_payload
            // 처리)와 동일한 경로(terminal_text_paste_bytes + session_bracketed_paste)로 주입한다.
            // send()가 WriteInput 공통 지점에서 선택 해제를 처리하므로 별도 clear_selection 불필요.
            if let Some(paste_session) = session
                && ui.button(catalog.t("workspace.menu.paste", &[])).clicked()
            {
                match crate::ui::clipboard_image::read_clipboard_text() {
                    Some(text) => {
                        let bytes = terminal_text_paste_bytes(
                            &text,
                            self.session_bracketed_paste(paste_session),
                        );
                        self.send(
                            client,
                            RuntimeCommand::WriteInput {
                                session: paste_session,
                                bytes,
                            },
                        );
                    }
                    None => tracing::warn!("컨텍스트 메뉴 붙여넣기: 클립보드에 텍스트 없음"),
                }
                ui.close();
            }
            if ui
                .button(catalog.t("workspace.split_horizontal", &[]))
                .clicked()
            {
                self.send(
                    client,
                    RuntimeCommand::SplitPane {
                        pane: pane_id.clone(),
                        direction: SplitDirection::Horizontal,
                        scrollback_lines: config.scrollback_lines as usize,
                    },
                );
                ui.close();
            }
            if ui
                .button(catalog.t("workspace.split_vertical", &[]))
                .clicked()
            {
                self.send(
                    client,
                    RuntimeCommand::SplitPane {
                        pane: pane_id.clone(),
                        direction: SplitDirection::Vertical,
                        scrollback_lines: config.scrollback_lines as usize,
                    },
                );
                ui.close();
            }
            ui.separator();
            // 스크롤백에서 맨 아래(라이브 화면)로 복귀 — 세션이 있는 pane에서만 노출.
            if let Some(session) = session
                && ui
                    .button(catalog.t("workspace.menu.scroll_bottom", &[]))
                    .clicked()
            {
                self.send(client, RuntimeCommand::ScrollToBottom { session });
                ui.close();
            }
            // (수동 상태 지정 U17b 서브메뉴는 사이드바와 함께 제거 — hook 감지 정착,
            // 2026-07-17 사용자. wire 명령 SetUserStatusOverride는 계약상 유지.)
            if ui.button(catalog.t("workspace.close_pane", &[])).clicked() {
                self.request_close_pane(client, pane_id.clone());
                ui.close();
            }
            ui.separator();
            // E4 ⑥: 프로젝트 화면에서 바로 환경변수·API 설정 진입 (에이전트에게 줄
            // 환경변수를 작업 중 즉시 등록하는 동선 — 사용자 시나리오).
            if ui
                .button(catalog.t("workspace.open_environment", &[]))
                .clicked()
            {
                self.open_environment_requested = true;
                ui.close();
            }
        });
    }

    /// pane 우클릭의 환경설정 진입 요청을 소비한다 (E4 ⑥ — App이 프레임마다 확인).
    pub fn take_open_environment(&mut self) -> bool {
        std::mem::take(&mut self.open_environment_requested)
    }

    fn flush_command_repaint(&mut self, ctx: &egui::Context) {
        if self.command_sent || self.pending_spawns > 0 {
            self.command_sent = false;
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// 최신 mux 스냅샷 (알림 센터가 pane 조회·제목에 사용).
    /// 사이드바 세션 목록용 항목 조립 (2026-07-05 — workspace 사이드바).
    pub fn session_entries(
        &self,
        catalog: &i18n::Catalog,
        agent_activity: &std::collections::HashMap<
            runtime::SessionId,
            crate::agent_transcript::AgentActivity,
        >,
        needs_input: &std::collections::HashSet<runtime::SessionId>,
        turn_done: &std::collections::HashMap<runtime::SessionId, i64>,
    ) -> Vec<crate::ui::file_tree::SessionEntry> {
        let Some(mux) = &self.mux else {
            return Vec::new();
        };
        mux.tabs
            .iter()
            .flat_map(|tab| tab.panes.iter().map(move |pane| (tab, pane)))
            .map(|(tab, pane)| {
                let regex_status = pane
                    .session_id
                    .and_then(|s| self.sessions.get(&s))
                    .and_then(|v| v.status);
                // transcript 기반 활동(옵션2)이 있으면 regex 상태를 덮어쓴다 — 단 '주의'
                // 상태(승인/오류/완료)는 transcript에 없는 신호라 regex를 우선한다.
                let activity = pane
                    .session_id
                    .and_then(|s| agent_activity.get(&s).copied());
                // hook이 보고한 needsInput = 가장 신뢰도 높은 승인 신호(최우선).
                let waiting = pane.session_id.is_some_and(|s| needs_input.contains(&s));
                let done = pane.session_id.is_some_and(|s| turn_done.contains_key(&s));
                let merged = merge_agent_status(regex_status, activity, waiting, done);
                // U17b: 수동 오버라이드가 있으면 최우선(status view의 user_override).
                let view = pane
                    .session_id
                    .and_then(|s| self.sessions.get(&s))
                    .and_then(|v| v.status_view.as_ref());
                let user_override = view.and_then(|v| v.user_override);
                let status = user_override.or(merged);
                let status_hint = view.map(status_view_hint_text(catalog));
                let summary = pane
                    .session_id
                    .and_then(|s| self.sessions.get(&s))
                    .map(|v| v.summary.clone())
                    .unwrap_or_default();
                let osc = self.session_osc_title(pane.session_id);
                // 에이전트 정보(2/3행) — 있으면 3줄 렌더. codex/claude 병합본(App).
                let info = pane.session_id.and_then(|s| self.agent_info.get(&s));
                let (agent_line, status_line) = match info {
                    Some(d) => (
                        Some(agent_info_line(d)),
                        Some(status_ctx_line(status, d.context_pct, catalog)),
                    ),
                    None => (None, None),
                };
                crate::ui::file_tree::SessionEntry {
                    tab: tab.id.clone(),
                    pane: pane.id.clone(),
                    session: pane.session_id,
                    resumable: false, // App이 restore_agents 기준으로 채운다
                    status_hint,
                    title: self.resolve_session_title(
                        &pane.title,
                        pane.session_id,
                        osc.as_deref(),
                        catalog,
                    ),
                    status,
                    summary,
                    focused: mux.focused_pane.as_ref() == Some(&pane.id),
                    attention: false, // App의 alert 추적이 채운다 (update_session_alerts)
                    pulse: None,
                    agent_line,
                    status_line,
                }
            })
            .collect()
    }

    /// 응답(Spawned/Failed)을 아직 못 받은 셸 spawn 수 — App의 suspend 보호가
    /// "spawn 진행 중 = live"로 판정하는 데 쓴다 (codex High race).
    pub fn pending_spawns(&self) -> u32 {
        self.pending_spawns
    }

    pub fn spawn_shell(&mut self, client: &dyn RuntimeClient, scrollback_lines: usize) {
        self.send(
            client,
            RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines,
            },
        );
    }

    /// 새 셸 + 스폰 완료 시 해당 폴더로 cd 1회 주입 — 사이드바 '같은 폴더에서 새 셀'.
    /// SpawnShell wire에 cwd 필드를 더하는 대신(계약 변경) ShellSpawned 응답에서
    /// cd를 주입한다 (자동 resume의 cd prefix와 같은 관례). cwd가 None이면 일반 스폰.
    pub fn spawn_shell_at(
        &mut self,
        client: &dyn RuntimeClient,
        scrollback_lines: usize,
        cwd: Option<String>,
    ) {
        self.pending_spawn_cd = cwd;
        self.spawn_shell(client, scrollback_lines);
    }

    /// 단축키용 현재 pane 닫기. 실행 중인 세션은 마우스 ×와 동일하게 확인창을 거친다.
    pub fn close_focused_pane(&mut self, client: &dyn RuntimeClient) {
        if let Some(pane) = self.mux.as_ref().and_then(|mux| mux.focused_pane.clone()) {
            self.request_close_pane(client, pane);
        }
    }

    /// 단축키(⌘↓)용 포커스된 pane을 스크롤백 맨 아래로 되돌린다. close_focused_pane과
    /// 동일 구조 — 호출부(app crate의 단축키 처리부) 배선은 workspace.rs 밖이라 이 PR
    /// 범위 밖이다 (호출부가 없어 현재는 미사용).
    pub fn scroll_focused_to_bottom(&mut self, client: &dyn RuntimeClient) {
        let session = self.mux.as_ref().and_then(|mux| {
            mux.focused_pane.as_ref().and_then(|pane| {
                mux.tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .find(|p| &p.id == pane)
                    .and_then(|p| p.session_id)
            })
        });
        if let Some(session) = session {
            self.send(client, RuntimeCommand::ScrollToBottom { session });
        }
    }

    /// 단축키용 현재 pane 분할. UI 버튼과 같은 runtime 명령을 사용한다.
    pub fn split_focused_pane(
        &mut self,
        client: &dyn RuntimeClient,
        direction: SplitDirection,
        scrollback_lines: usize,
    ) {
        // focused_pane이 없으면(복원 직후·pane 미클릭·단일 pane) 활성 탭의 첫 pane으로
        // 폴백한다 — 안 그러면 분할 단축키/버튼이 조용히 아무 것도 안 해 "고장난 것처럼"
        // 보인다(사용자 보고 2026-07-12).
        if let Some(pane) = self.mux.as_deref().and_then(split_target_pane) {
            self.send(
                client,
                RuntimeCommand::SplitPane {
                    pane,
                    direction,
                    scrollback_lines,
                },
            );
        }
    }

    /// 활성 탭의 pane 벡터 순서로 포커스를 순환한다. 끝에서는 반대편으로 이어진다.
    pub fn focus_relative_pane(&mut self, client: &dyn RuntimeClient, delta: isize) {
        let next = self.mux.as_ref().and_then(|mux| {
            let active = mux.active_tab.as_ref()?;
            let tab = mux.tabs.iter().find(|tab| &tab.id == active)?;
            if tab.panes.len() < 2 {
                return None;
            }
            let focused = mux.focused_pane.as_ref()?;
            let current = tab.panes.iter().position(|pane| &pane.id == focused)?;
            let next = (current as isize + delta).rem_euclid(tab.panes.len() as isize) as usize;
            Some(tab.panes[next].id.clone())
        });
        if let Some(pane) = next {
            self.request_pane_focus(client, pane);
        }
    }

    pub fn mux(&self) -> Option<&Arc<MuxSnapshot>> {
        self.mux.as_ref()
    }

    /// 포커스된 pane의 세션 id — 워크스페이스 이름(현재 작업 폴더) 추적용.
    pub fn focused_session(&self) -> Option<SessionId> {
        let mux = self.mux.as_ref()?;
        let focused = mux.focused_pane.as_ref()?;
        mux.tabs
            .iter()
            .flat_map(|t| &t.panes)
            .find(|p| &p.id == focused)
            .and_then(|p| p.session_id)
    }

    pub fn session_bracketed_paste(&self, session: SessionId) -> bool {
        self.sessions
            .get(&session)
            .is_some_and(|view| view.bracketed_paste)
    }

    pub fn session_shell_kind(&self, _session: SessionId) -> crate::ui::file_tree::ShellKind {
        self.shell_kind
    }

    /// 백그라운드 클립보드 paste 완료를 폴링해 요청 시점 세션에 삽입한다(2026-07-07).
    /// 스레드가 끝나면 repaint를 깨우므로 유휴 중에도 다음 프레임에 소비된다.
    fn poll_paste_task(&mut self, client: &dyn RuntimeClient) {
        let Some(task) = &self.paste_task else { return };
        // 만료: warm으로 물러났다 돌아온 뒤 옛 paste가 뒤늦게 꽂히는 것 방지(codex Medium).
        if task.requested_at.elapsed() > PASTE_TASK_TTL {
            self.paste_task = None;
            return;
        }
        let result = match task.rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.paste_task = None;
                return;
            }
        };
        let task = self.paste_task.take().expect("위에서 Some 확인");
        let bytes = match result {
            Ok(paths) => {
                // 이미지/파일이 없으면: egui Event::Paste 텍스트 → 클립보드 텍스트 순 fallback
                // (터미널 위젯엔 Event::Paste가 안 올 수 있음 — #4와 동일 규칙).
                clipboard_terminal_paste_bytes(
                    paths.as_deref(),
                    task.text_fallback,
                    task.shell_kind,
                    task.bracketed,
                )
                .or_else(|| {
                    paths.is_none().then(|| {
                        crate::ui::clipboard_image::read_clipboard_text()
                            .map(|t| terminal_text_paste_bytes(&t, task.bracketed))
                    })?
                })
            }
            Err(e) => {
                self.error_is_pressure = false;
                self.error_is_pressure = false;
                self.error = Some(format!("{e:#}"));
                None
            }
        };
        if let Some(bytes) = bytes {
            self.send(
                client,
                RuntimeCommand::WriteInput {
                    session: task.session,
                    bytes,
                },
            );
        }
    }

    /// 상태가 강조 대상(입력요청/작업종료)으로 **새로** 전이하면 그 세션 pane을 플래시한다.
    /// 반드시 sessions의 status를 갱신하기 **전에** 불러 직전 상태를 읽는다.
    fn note_status_flash(&mut self, session: SessionId, new_status: SessionStatus) {
        let prev = self.sessions.get(&session).and_then(|view| view.status);
        if is_flash_status(new_status) && prev != Some(new_status) {
            self.session_flash
                .insert(session, std::time::Instant::now() + PANE_FLASH);
        }
    }

    fn session_alive(&self, session: SessionId) -> bool {
        self.mux.as_ref().is_some_and(|mux| {
            mux.tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .any(|pane| pane.session_id == Some(session))
        })
    }

    fn session_visible(&self, session: SessionId) -> bool {
        self.mux.as_ref().is_some_and(|mux| {
            mux.active_tab
                .as_ref()
                .and_then(|active| mux.tabs.iter().find(|tab| &tab.id == active))
                .is_some_and(|tab| {
                    tab.panes
                        .iter()
                        .any(|pane| pane.session_id == Some(session))
                })
        })
    }

    fn send(&mut self, client: &dyn RuntimeClient, command: RuntimeCommand) {
        // 터미널에 입력/스크롤을 보내면 그 세션 선택을 해제한다 — 선택 중엔 화면이 freeze돼
        // (선택 정확성) 있어, 안 지우면 타이핑·스크롤해도 화면이 멈춘 듯 보인다(사용자:
        // 드래그 선택 후 스크롤이 안 내려감). 공통 지점이라 여기서 한 번에 처리한다.
        // 예외: 드래그 오토스크롤의 Scroll은 send_keep_selection으로 보낸다 — 이 해제와
        // 정면 충돌해 첫 스크롤 직후 선택·오토스크롤이 함께 죽었다(감사 A3 버그 #1).
        let touched = match &command {
            RuntimeCommand::WriteInput { session, .. } => Some(*session),
            RuntimeCommand::Scroll { session, .. } => Some(*session),
            _ => None,
        };
        if let Some(session) = touched
            && self.selection.is_some_and(|(s, _, _)| s == session)
        {
            self.selection = None;
        }
        self.send_keep_selection(client, command);
    }

    /// Runtime snapshot을 기다리지 않는 터미널 refocus를 시작한다. egui의 TextEdit state가
    /// 사라지는 데 한 프레임 더 걸려도, 그 사이 첫 printable key를 잃지 않는다.
    fn begin_terminal_refocus(&mut self, pane: runtime::MuxPaneId) {
        self.pending_focus = Some(pane);
        self.preedit.clear();
    }

    /// Runtime의 mux snapshot이 도착하기 전에도 입력을 새 pane으로 보낸다. 그렇지 않으면
    /// pane을 클릭하거나 검색을 닫은 직후의 첫 `.`, 공백, 한글 조합이 버려질 수 있다.
    fn request_pane_focus(&mut self, client: &dyn RuntimeClient, pane: runtime::MuxPaneId) {
        self.begin_terminal_refocus(pane.clone());
        self.send(client, RuntimeCommand::FocusPane { pane });
    }

    /// 선택을 해제하지 않는 send — 드래그 오토스크롤 전용(선택을 유지·확장하며
    /// 스크롤해야 한다). 휠/타이핑은 반드시 [`Self::send`]를 쓴다.
    fn send_keep_selection(&mut self, client: &dyn RuntimeClient, command: RuntimeCommand) {
        let is_spawn = matches!(
            command,
            RuntimeCommand::SpawnShell { .. } | RuntimeCommand::SplitPane { .. }
        );
        if let Err(e) = client.send_command(command) {
            self.error_is_pressure = false;
            self.error = Some(format!("{e:#}"));
        } else {
            self.command_sent = true;
            if is_spawn {
                self.pending_spawns += 1;
            }
        }
    }
}

/// File-tree/sidebar path insertion uses the same paste byte semantics as clipboard paste.
pub(crate) fn path_insert_paste_bytes(
    path: &Path,
    shell_kind: crate::ui::file_tree::ShellKind,
    bracketed_paste: bool,
) -> Vec<u8> {
    let raw = crate::ui::file_tree::shell_path_insert_bytes_for(path, shell_kind);
    input_mapper::paste_bytes(&raw, bracketed_paste)
}

/// 포커스 터미널에서 이 폴더로 이동 — `cd <quoted-path>` + 실행(Enter).
/// 붙여넣기(bracketed)로 명령을 넣은 뒤 CR을 브라켓 밖에 붙여 실행되게 한다(2026-07-08).
pub(crate) fn cd_paste_bytes(
    path: &Path,
    shell_kind: crate::ui::file_tree::ShellKind,
    bracketed_paste: bool,
) -> Vec<u8> {
    use crate::ui::file_tree::ShellKind;
    let quoted = crate::ui::file_tree::shell_quote_for(path, shell_kind);
    // 셸별 cd 문법 — PowerShell은 -LiteralPath로 와일드카드(`foo[bar]`) 해석을 막고,
    // cmd는 `/d`로 드라이브 변경까지 처리한다(codex Medium 2026-07-08).
    let cmd = match shell_kind {
        ShellKind::PowerShell => format!("Set-Location -LiteralPath {quoted}"),
        ShellKind::Cmd => format!("cd /d {quoted}"),
        ShellKind::Posix | ShellKind::Fish => format!("cd {quoted}"),
    };
    let mut bytes = input_mapper::paste_bytes(cmd.as_bytes(), bracketed_paste);
    bytes.push(b'\r'); // 실행 — bracketed paste 종료 뒤의 CR
    bytes
}

pub(crate) fn paths_insert_paste_bytes(
    paths: &[std::path::PathBuf],
    shell_kind: crate::ui::file_tree::ShellKind,
    bracketed_paste: bool,
) -> Vec<u8> {
    let mut raw = Vec::new();
    for path in paths {
        raw.extend(crate::ui::file_tree::shell_path_insert_bytes_for(
            path, shell_kind,
        ));
    }
    input_mapper::paste_bytes(&raw, bracketed_paste)
}

/// raw 제목이 아직 rename 안 된 기본 셸/에이전트 제목("workspace.spawn.shell 134" 등)인가.
/// 기본 제목이면 프로젝트명으로 대체 표시한다(resolve_session_title / 활동 패널의
/// warm·유휴 워크스페이스 행도 같은 규칙을 쓴다 — App::activity_session_name).
pub(crate) fn is_default_session_title(raw: &str) -> bool {
    let Some((prefix, suffix)) = raw.rsplit_once(' ') else {
        return false;
    };
    matches!(
        prefix,
        "workspace.spawn.shell" | "셸" | "workspace.spawn.agent" | "에이전트"
    ) && suffix.parse::<u64>().is_ok()
}

pub(crate) fn display_pane_title(raw: &str, catalog: &i18n::Catalog) -> String {
    let Some((prefix, suffix)) = raw.rsplit_once(' ') else {
        return match raw {
            "workspace.spawn.shell" | "셸" => catalog.t("workspace.spawn.shell", &[]),
            "workspace.spawn.agent" | "에이전트" => catalog.t("workspace.spawn.agent", &[]),
            _ => raw.to_owned(),
        };
    };
    let key = match prefix {
        "workspace.spawn.shell" | "셸" => Some("workspace.spawn.shell"),
        "workspace.spawn.agent" | "에이전트" => Some("workspace.spawn.agent"),
        _ => None,
    };
    if let Some(key) = key
        && suffix.parse::<u64>().is_ok()
    {
        return format!("{} {suffix}", catalog.t(key, &[]));
    }
    raw.to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalTextDragPayload {
    text: String,
}

fn selection_range_contains(start: usize, end: usize, idx: usize) -> bool {
    start <= idx && idx <= end
}

/// 드래그 선택 오토스크롤 속도(행/초, RuntimeCommand::Scroll delta 부호 —
/// 양수=과거로). 포인터가 pane 세로 경계를 벗어난 거리에 비례해 빨라진다:
/// 1셀 초과당 8행/초, 최대 60행/초. 경계 안이면 0 (T4).
fn drag_autoscroll_rate(pointer_y: f32, top: f32, bottom: f32, cell_h: f32) -> f32 {
    // 위로 벗어남 = 양수(과거로), 아래로 벗어남 = 음수(최신으로)
    let overshoot = if pointer_y < top {
        top - pointer_y
    } else if pointer_y > bottom {
        bottom - pointer_y
    } else {
        return 0.0;
    };
    (overshoot / cell_h.max(1.0) * 8.0).clamp(-60.0, 60.0)
}

/// 스크롤로 화면이 delta_rows행 이동했을 때(양수=과거로 → 내용이 아래로 이동)
/// 화면 좌표 기반 선택 셀 인덱스를 같은 텍스트로 보정한다. 화면 밖으로 나가면
/// 선형 경계(첫 행 첫 열 / 마지막 행 마지막 열)로 clamp — 스냅샷이 가시 영역만
/// 담으므로 화면 밖 선택은 표현할 수 없다 (T4).
fn shift_selection_cell(idx: usize, delta_rows: i32, cols: usize, rows: usize) -> usize {
    let max_idx = (cols * rows).saturating_sub(1) as i64;
    (idx as i64 + delta_rows as i64 * cols as i64).clamp(0, max_idx) as usize
}

fn terminal_text_paste_bytes(text: &str, bracketed_paste: bool) -> Vec<u8> {
    input_mapper::paste_bytes(text.as_bytes(), bracketed_paste)
}

/// 세션 행 2행: "[PTY] Codex · gpt-5.5 · xhigh" (빈 부분은 생략).
fn agent_info_line(d: &crate::agent_detect::AgentDisplay) -> String {
    use crate::agent_surface::{AgentProvider, AgentTransport};

    let provider = AgentProvider::from(d.kind);
    let mut parts = vec![format!(
        "[{}] {}",
        AgentTransport::Pty.badge(),
        provider.label()
    )];
    if let Some(m) = d.model.as_deref().filter(|s| !s.is_empty()) {
        parts.push(m.to_owned());
    }
    if let Some(e) = d.effort.as_deref().filter(|s| !s.is_empty()) {
        parts.push(e.to_owned());
    }
    parts.join(" · ")
}

/// 세션 행 3행: "실행 중 · ctx 69%" (상태 라벨 + 남은 컨텍스트%). 상태 없으면 ctx만.
/// 상태 view의 hover 힌트 — 출처(감지 방법)와 신뢰도, 수동 지정 여부(U17b).
fn status_view_hint_text(
    catalog: &i18n::Catalog,
) -> impl Fn(&runtime::SessionStatusView) -> String + '_ {
    move |view| {
        if view.user_override.is_some() {
            return catalog.t("status.hint.user_override", &[]);
        }
        let source_key = match view.source {
            runtime::StatusSource::ProcessExit => "status.hint.source.process_exit",
            runtime::StatusSource::StreamRegex => "status.hint.source.stream_regex",
            runtime::StatusSource::ScreenText => "status.hint.source.screen_text",
            runtime::StatusSource::IdleHeuristic => "status.hint.source.idle_heuristic",
            runtime::StatusSource::UserOverride => "status.hint.user_override",
        };
        let mut out = catalog.t(source_key, &[]);
        if let Some(conf) = &view.confidence {
            out.push_str(" · ");
            out.push_str(&catalog.t(
                "status.hint.confidence",
                &[("score", &format!("{:.0}%", conf.score * 100.0))],
            ));
        }
        out
    }
}

fn status_ctx_line(
    status: Option<runtime::SessionStatus>,
    context_pct: Option<u8>,
    catalog: &i18n::Catalog,
) -> String {
    use runtime::SessionStatus as S;
    let label = status.map(|s| {
        let key = match s {
            S::Running => "status.running",
            S::Waiting | S::NeedsApproval => "status.needs_approval",
            S::Done => "status.done",
            S::Error => "status.error",
            S::Idle => "status.idle",
        };
        catalog.t(key, &[])
    });
    match (label, context_pct) {
        (Some(l), Some(p)) => format!("{l} · ctx {p}%"),
        (Some(l), None) => l,
        (None, Some(p)) => format!("ctx {p}%"),
        (None, None) => String::new(),
    }
}

fn request_terminal_focus(response: &egui::Response) {
    response.request_focus();
    response.ctx.memory_mut(|memory| {
        memory.set_focus_lock_filter(response.id, renderer_egui::terminal_focus_lock_filter());
    });
}

fn clipboard_terminal_paste_bytes(
    paths: Option<&[std::path::PathBuf]>,
    text_paste_bytes: Option<Vec<u8>>,
    shell_kind: crate::ui::file_tree::ShellKind,
    bracketed_paste: bool,
) -> Option<Vec<u8>> {
    if let Some(paths) = paths {
        Some(paths_insert_paste_bytes(paths, shell_kind, bracketed_paste))
    } else {
        text_paste_bytes
    }
}

/// ⌘V press의 Event::Paste와 release 키 이벤트 사이 간격 상한 — 이 안이면 같은
/// 붙여넣기 제스처로 본다. 보통 탭의 press→release는 50~200ms. 너무 길면 별개의
/// 두 붙여넣기를 오인한다 — 메뉴 텍스트 붙여넣기 직후의 ⌘V 이미지 붙여넣기가
/// 스킵되는 구멍 (codex 리뷰 MEDIUM, 3s→600ms 축소). 600ms 이상 키를 누르고
/// 있는 경우는 OS 키 반복이 Event::Paste를 다시 보내 타임스탬프가 갱신된다.
const PASTE_GESTURE_WINDOW: std::time::Duration = std::time::Duration::from_millis(600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipboardPasteTrigger {
    /// AppKit local monitor가 winit/egui보다 먼저 본 신뢰 가능한 Command+V key-down.
    NativeKeyDown,
    /// egui가 전달한 플랫폼 shortcut. macOS에서는 V key-up fallback이다.
    EguiShortcut,
}

/// clipboard shortcut이 띄우는 이미지 paste 태스크를 건너뛸지 판정한다.
///
/// NativeKeyDown은 새 제스처의 시작이므로 이전 paste 이력 때문에 건너뛰지 않는다.
/// macOS key-up fallback만 같은 제스처의 native key-down 또는 press Event::Paste가
/// 최근 처리됐고 같은 프레임 text fallback도 없을 때 건너뛴다.
fn should_skip_paste_task(
    trigger: ClipboardPasteTrigger,
    has_text_fallback: bool,
    last_text_paste: Option<std::time::Instant>,
    last_native_paste: Option<std::time::Instant>,
) -> bool {
    trigger == ClipboardPasteTrigger::EguiShortcut
        && !has_text_fallback
        && [last_text_paste, last_native_paste]
            .into_iter()
            .flatten()
            .any(|at| at.elapsed() < PASTE_GESTURE_WINDOW)
}

fn is_clipboard_paste_shortcut(event: &egui::Event) -> bool {
    let egui::Event::Key {
        key: egui::Key::V,
        pressed,
        modifiers,
        ..
    } = event
    else {
        return false;
    };
    if cfg!(target_os = "macos") {
        // 이미지-only clipboard의 Cmd+V PRESS는 egui-winit이 Event::Paste/Key 없이
        // 소비한다. AppKit monitor의 native key-down이 주 경로이고, 이 release는 monitor
        // 설치 실패·포커스 경계 누락을 위한 fallback이다. 제스처 상태가 중복을 제거한다.
        !pressed && modifiers.command && !modifiers.ctrl
    } else {
        *pressed && modifiers.ctrl && modifiers.shift
    }
}

/// 셀 idx 아래의 단어(공백 구분 비어있지 않은 셀 연속) 범위를 [start, end]로 돌려준다.
/// 공백 위를 더블클릭하면 None. 더블클릭 단어 선택에 쓴다.
/// 단어가 URL이면 (뒤따르는 구두점 제거 후) 그 URL을 반환한다. http/https만 연다.
fn extract_url(word: &str) -> Option<&str> {
    // 머리의 여는 괄호/따옴표도 벗긴다 — "(https://…)" 꼴이 흔하다 (resolve_path_click 관례).
    let trimmed = word
        .trim_start_matches(|c: char| "\"'`([{<".contains(c))
        .trim_end_matches(|c: char| ".,;:!?)]}>\"'".contains(c));
    (trimmed.starts_with("http://") || trimmed.starts_with("https://")).then_some(trimmed)
}

/// 터미널 텍스트가 가리키는 파일시스템 대상 (2026-07-14 사용자 요청).
#[derive(Debug, Clone, PartialEq)]
enum PathClick {
    /// 디렉터리 — 셸에 cd를 보낸다 (alt screen이 아닐 때만)
    Dir(std::path::PathBuf),
    /// 외부 프로그램으로 여는 파일 (OPENABLE_EXTS 허용목록)
    OpenFile(std::path::PathBuf),
}

/// 더블클릭으로 외부 프로그램에 넘겨도 안전한 확장자 — 문서/이미지/미디어/아카이브.
/// 실행파일·스크립트는 제외한다: macOS `open`은 실행 가능한 대상을 **실행**하므로
/// 더블클릭 오조작이 코드 실행이 되면 안 된다.
const OPENABLE_EXTS: &[&str] = &[
    "pdf", "html", "htm", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "csv", "hwp", "txt", "md",
    "rtf", "png", "jpg", "jpeg", "gif", "webp", "svg", "heic", "tiff", "mp4", "mov", "mp3", "wav",
    "zip", "numbers", "pages", "key",
];

/// 더블클릭된 단어를 파일시스템 경로로 해석한다. 절대(`/`)·홈(`~/`)·상대(cwd 기준)
/// 순으로 시도하고, `path.py:33`처럼 줄번호가 붙은 꼴은 `:` 뒤를 떼고 재시도한다.
/// 존재하지 않거나(오탈자·일반 단어) 허용 확장자가 아닌 파일이면 None — 이 함수가
/// None이면 더블클릭은 기존 동작(단어 선택)만 한다.
fn resolve_path_click(word: &str, cwd: Option<&Path>) -> Option<PathClick> {
    // 꼬리는 따옴표와 문말 부호가 섞여 올 수 있어("'docs',") 통합 집합으로 벗긴다.
    // 머리도 여는 괄호류가 붙어 올 수 있다("(docs/report.pdf)" — codex 리뷰).
    let token = word
        .trim_start_matches(|c: char| "\"'`([{<".contains(c))
        .trim_end_matches(|c: char| "\"'`.,;!?)]}>".contains(c));
    if token.is_empty() {
        return None;
    }
    // "path.py:33" / "src/main.rs:12:34" → 숫자 suffix를 반복해서 벗긴 후보도 시도
    // (Claude/컴파일러 출력 관례 — rustc는 :행:칸 두 개가 붙는다, codex 리뷰).
    let mut candidates = vec![token];
    let mut head = token;
    while let Some((rest, tail)) = head.rsplit_once(':')
        && !rest.is_empty()
        && !tail.is_empty()
        && tail.chars().all(|c| c.is_ascii_digit())
    {
        candidates.push(rest);
        head = rest;
    }
    for cand in candidates {
        let path = if let Some(rest) = cand.strip_prefix("~/") {
            crate::paths::home_dir().map(|home| home.join(rest))
        } else if cand.starts_with('/') {
            Some(std::path::PathBuf::from(cand))
        } else {
            cwd.map(|c| c.join(cand))
        };
        let Some(path) = path else { continue };
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            return Some(PathClick::Dir(path));
        }
        let ext_openable = |p: &Path| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| OPENABLE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        };
        // 허용목록은 **실체(canonical) 경로의 확장자**로도 검사한다 — "safe.pdf"가
        // 실행파일을 가리키는 심링크면 open이 실행해버린다 (codex 리뷰 하드닝).
        if meta.is_file()
            && ext_openable(&path)
            && std::fs::canonicalize(&path).is_ok_and(|real| ext_openable(&real))
        {
            return Some(PathClick::OpenFile(path));
        }
    }
    None
}

fn word_range_at(
    snapshot: &terminal::TerminalViewportSnapshot,
    idx: usize,
) -> Option<(usize, usize)> {
    let cols = snapshot.cols as usize;
    if cols == 0 {
        return None;
    }
    let row = idx / cols;
    let col = idx % cols;
    let base = row * cols;
    let is_word = |c: usize| -> bool {
        snapshot.visible_cells.get(base + c).is_some_and(|cell| {
            // wide char(한글 등) 뒤의 자리 채움 셀은 c==' '지만 단어의 일부다 —
            // 공백으로 취급하면 "nant-성과분석.pdf"가 첫 한글에서 끊긴다 (2026-07-14).
            cell.wide_spacer || (!cell.c.is_whitespace() && cell.c != '\0')
        })
    };
    if !is_word(col) {
        return None;
    }
    let mut start = col;
    while start > 0 && is_word(start - 1) {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < cols && is_word(end + 1) {
        end += 1;
    }
    Some((base + start, base + end))
}

/// regex/휴리스틱 상태와 transcript 기반 활동(옵션2)을 병합한다. 승인/오류/완료는
/// transcript에 없는 '주의' 신호라 regex를 우선하고, 그 외(실행/대기/유휴/미보고)는
/// 더 정확한 transcript 활동으로 덮어쓴다.
fn merge_agent_status(
    regex: Option<runtime::SessionStatus>,
    activity: Option<crate::agent_transcript::AgentActivity>,
    needs_input: bool,
    turn_done: bool,
) -> Option<runtime::SessionStatus> {
    use crate::agent_transcript::AgentActivity;
    use runtime::SessionStatus as S;
    // hook needsInput은 가장 신뢰도 높은 승인 대기 신호 — 단, transcript가 Working이면
    // 에이전트가 재개된 것이라(clear hook 지연 대비) 대기로 보지 않는다.
    if needs_input && activity != Some(AgentActivity::Working) {
        return Some(S::NeedsApproval);
    }
    // 명시적 오류는 완료보다 우선 — Stop은 모든 턴 종료에 오므로 turn_done이 error를
    // 가리면 실패한 턴이 '완료(바이올렛)'로 위장된다(codex 리뷰).
    if matches!(regex, Some(S::Error)) {
        return regex;
    }
    // Stop hook = 턴 완료. UserPromptSubmit/PreToolUse가 clear하므로 재개 시 즉시 해제.
    // transcript activity(Stop 직후 잠깐 Working으로 남음)보다 우선한다.
    if turn_done {
        return Some(S::Done);
    }
    match regex {
        // 대기(Waiting)는 입력대기(주황)로 통합 — 별도 팔레트 없음 (2026-07-07 결정).
        Some(S::Waiting) => return Some(S::NeedsApproval),
        Some(S::NeedsApproval | S::Done) => return regex,
        _ => {}
    }
    match activity {
        Some(AgentActivity::Working) => Some(S::Running),
        Some(AgentActivity::Idle) => Some(S::Idle),
        None => regex,
    }
}

fn terminal_keyboard_input_allowed(
    text_edit_focused: bool,
    popup_open: bool,
    top_window_open: bool,
    terminal_refocus_pending: bool,
) -> bool {
    // TextEditState는 widget이 사라진 뒤 한 프레임 더 memory에 남을 수 있다. terminal
    // refocus가 명시적으로 대기 중이면 그 stale 상태는 무시해야 첫 글자가 빠지지 않는다.
    !popup_open && !top_window_open && (terminal_refocus_pending || !text_edit_focused)
}

/// Agents is a floating but non-modal navigator/editor. A terminal click must
/// be able to reclaim focus while it remains open; confirmation/error windows
/// continue to block terminal input as before.
fn is_blocking_terminal_window(layer: &egui::LayerId) -> bool {
    layer.order == egui::Order::Middle && layer.id != crate::ui::agent_sessions::agents_window_id()
}

/// Pending focus가 있으면 runtime snapshot의 이전 focused pane 대신 그것이 유일한 입력
/// 대상이다. 이 규칙이 없으면 pane 전환 직후 문장부호가 이전 pane에 들어가거나 유실된다.
fn terminal_input_owner(
    pane_id: &runtime::MuxPaneId,
    runtime_focused: bool,
    pending_focus: Option<&runtime::MuxPaneId>,
) -> bool {
    match pending_focus {
        Some(pending) => pending == pane_id,
        None => runtime_focused,
    }
}

/// 후보에서 같은 문자 하나만 제거한다. 연속으로 같은 키를 누른 횟수를 보존하려면
/// 집합이 아니라 이 one-for-one 소비가 필요하다.
fn consume_ime_terminator_char(candidates: &mut Vec<char>, character: char) -> bool {
    if let Some(index) = candidates
        .iter()
        .position(|candidate| *candidate == character)
    {
        candidates.remove(index);
        true
    } else {
        false
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ImeTextReconciliation {
    /// raw event와 같은 길이. Text/Commit 위치에는 중복을 제거한 최종 문자열이 있고,
    /// 다른 이벤트 위치에는 None이 있다.
    event_text: Vec<Option<String>>,
    /// IME가 Text/Commit을 생략한 실제 key-down만 event batch 뒤에 보낸다.
    fallback_bytes: Vec<u8>,
}

/// 한 raw input batch의 IME Commit/Text/fallback을 하나의 물리 키 원장으로 조정한다.
///
/// macOS Korean IME는 한 번 누른 Space/Comma를 `Ime::Commit`에 포함한 직후 일반
/// `Text`로 다시 보낼 수 있다. 반대로 조합을 확정한 키의 Text를 완전히 생략하기도
/// 한다. AppKit local monitor와 egui Key는 같은 물리 키의 두 관측값이므로 합산하지
/// 않고 문자별 최대 개수로 병합한다. 그 개수를 넘는 Commit/Text 문자만 버리고,
/// 모자란 개수는 IME가 관여한 batch에서만 fallback으로 보낸다.
fn reconcile_ime_text_events(
    key_downs: &[crate::native_key_monitor::NativePrintableKeyDown],
    events: &[egui::Event],
    preedit_active: bool,
) -> ImeTextReconciliation {
    let ime_involved = preedit_active
        || events.iter().any(|event| {
            matches!(event, egui::Event::Ime(egui::ImeEvent::Commit(_)))
                || matches!(
                    event,
                    egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) if !text.is_empty()
                )
        });

    let native_candidates: Vec<char> = key_downs
        .iter()
        .map(|key_down| key_down.character)
        .collect();
    let mut physical_candidates = native_candidates.clone();
    if ime_involved {
        // AppKit과 egui Key는 같은 key-down을 보는 두 경로다. 먼저 native 후보와
        // one-for-one으로 짝지어, native가 놓친 egui 후보만 원장에 추가한다.
        let mut unmatched_native = native_candidates;
        for character in events
            .iter()
            .filter_map(input_mapper::ime_terminator_key_char)
        {
            if !consume_ime_terminator_char(&mut unmatched_native, character) {
                physical_candidates.push(character);
            }
        }
    }

    let constrained_characters: HashSet<char> = physical_candidates.iter().copied().collect();
    let mut unclaimed_physical = physical_candidates;
    let mut event_text = Vec::with_capacity(events.len());

    for event in events {
        let text = match event {
            egui::Event::Text(text) | egui::Event::Ime(egui::ImeEvent::Commit(text)) => text,
            _ => {
                event_text.push(None);
                continue;
            }
        };

        let mut filtered = String::with_capacity(text.len());
        for character in text.chars() {
            if !constrained_characters.contains(&character) {
                filtered.push(character);
                continue;
            }
            let Some(position) = unclaimed_physical
                .iter()
                .position(|candidate| *candidate == character)
            else {
                // 이 물리 키의 허용 개수는 앞선 Commit/Text에서 이미 모두 전송됐다.
                continue;
            };
            if ime_involved {
                // 앞선 물리 키가 Text 없이 사라졌는데 뒤 키의 Text가 먼저 보인 경우,
                // 사라진 키를 여기 삽입해야 빠른 연타에서도 입력 순서가 뒤집히지 않는다.
                filtered.extend(unclaimed_physical.drain(..position));
                unclaimed_physical.remove(0);
            } else {
                unclaimed_physical.remove(position);
            }
            filtered.push(character);
        }
        event_text.push(Some(filtered));
    }

    let fallback_bytes = if ime_involved {
        unclaimed_physical
            .into_iter()
            .map(|character| character as u8)
            .collect()
    } else {
        Vec::new()
    };

    ImeTextReconciliation {
        event_text,
        fallback_bytes,
    }
}

/// 상태 → tab 제목 아이콘 (PR-12).
/// 사이드바 세션 요약 — 화면의 마지막 비어있지 않은 행 (≤48자, 2026-07-05).
fn last_line_summary(snapshot: &TerminalViewportSnapshot) -> String {
    let cols = snapshot.cols as usize;
    if cols == 0 {
        return String::new();
    }
    for row in (0..snapshot.rows as usize).rev() {
        let line: String = snapshot.visible_cells[row * cols..(row + 1) * cols]
            .iter()
            .filter(|c| !c.wide_spacer)
            .map(|c| c.c)
            .collect();
        let line = line.trim();
        if !line.is_empty() {
            return line.chars().take(48).collect();
        }
    }
    String::new()
}

fn mux_sessions(snapshot: &MuxSnapshot) -> HashSet<SessionId> {
    snapshot
        .tabs
        .iter()
        .flat_map(|tab| &tab.panes)
        .filter_map(|pane| pane.session_id)
        .collect()
}

/// 분할이 대상으로 삼을 pane: 포커스된 pane이 있으면 그것, 없으면 활성 탭(없으면 첫 탭)의
/// 첫 pane. pane이 하나도 없으면 None(빈 워크스페이스 — 분할할 게 없다).
fn split_target_pane(snapshot: &MuxSnapshot) -> Option<runtime::MuxPaneId> {
    if let Some(pane) = &snapshot.focused_pane {
        return Some(pane.clone());
    }
    let tab = snapshot
        .active_tab
        .as_ref()
        .and_then(|id| snapshot.tabs.iter().find(|tab| &tab.id == id))
        .or_else(|| snapshot.tabs.first());
    tab.and_then(|tab| tab.panes.first())
        .map(|pane| pane.id.clone())
}

/// pane 강조 플래시 지속 시간 — 포커스 이동·입력요청·작업완료 시 pane 전체 테두리를 이만큼
/// 포인트색으로 그리고 페이드아웃한다. 탑라인(포커스 지속 표시)은 이와 무관하다.
const PANE_FLASH: std::time::Duration = std::time::Duration::from_secs(2);

/// pane 전체 플래시를 유발하는 상태: 입력요청(Waiting/NeedsApproval)·작업종료(Done/Error).
/// Running(작업 중)·Idle(쉬는 중)은 제외 — 주목이 필요한 순간만 번쩍인다.
fn is_flash_status(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Waiting
            | SessionStatus::NeedsApproval
            | SessionStatus::Done
            | SessionStatus::Error
    )
}

fn visible_mux_sessions(snapshot: &MuxSnapshot) -> HashSet<SessionId> {
    snapshot
        .active_tab
        .as_ref()
        .and_then(|active| snapshot.tabs.iter().find(|tab| &tab.id == active))
        .into_iter()
        .flat_map(|tab| &tab.panes)
        .filter_map(|pane| pane.session_id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{MuxPaneId, MuxTabId, PaneSnapshot, TabSnapshot};
    use std::sync::Mutex;
    use terminal::{CursorShape, CursorSnapshot, TerminalCell};

    /// hover 커서 회귀 가드 — 백그라운드 cwd 해석(stale-while-revalidate) 후
    /// 폴더 단어가 Dir로 잡혀야 한다 (2026-07-17 사용자: 커서가 안 바뀜).
    #[test]
    fn hover_cwd는_백그라운드_해석_후_폴더를_dir로_잡는다() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(1);
        ui.set_session_pids(&[(session, std::process::id())]);
        let ctx = egui::Context::default();
        // 1차 — 백그라운드 해석 시작, 아직 값 없음(pending)
        let (first, pending) = ui.hover_cwd(&ctx, session);
        assert!(first.is_none() && pending);
        // 해석 완료 대기 (lsof 1회)
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let (cwd, still_pending) = ui.hover_cwd(&ctx, session);
            if let Some(cwd) = cwd {
                assert!(!still_pending);
                assert_eq!(cwd, std::env::current_dir().unwrap());
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cwd 해석 결과가 오지 않음"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // 캐시 경유 경로 해석 — 폴더 단어("src")가 Dir이어야 hover 커서가 바뀐다
        assert!(matches!(
            ui.resolve_path_cached(&ctx, session, "src"),
            Some(PathClick::Dir(_))
        ));
    }

    #[derive(Default)]
    struct RecordingRuntime {
        commands: Mutex<Vec<RuntimeCommand>>,
    }

    impl runtime::RuntimeCommandSink for RecordingRuntime {
        fn send_command(&self, command: RuntimeCommand) -> anyhow::Result<()> {
            self.commands.lock().unwrap().push(command);
            Ok(())
        }
    }

    impl runtime::RuntimeEventStream for RecordingRuntime {
        fn subscribe(&self) -> runtime::RuntimeEventReceiver {
            panic!("paste controller test does not subscribe")
        }
    }

    impl runtime::RuntimeClient for RecordingRuntime {}

    fn pane_id(name: &str) -> MuxPaneId {
        MuxPaneId(name.to_owned())
    }

    #[test]
    fn agent_info_line_distinguishes_pty_transport() {
        let display = crate::agent_detect::AgentDisplay {
            kind: crate::agent_detect::AgentKind::Codex,
            model: Some("gpt-test".to_owned()),
            effort: Some("high".to_owned()),
            context_pct: None,
        };

        assert_eq!(agent_info_line(&display), "[PTY] Codex · gpt-test · high");
    }

    #[test]
    fn 한글_파일명은_wide_spacer를_넘어_한_단어로_잡힌다() {
        // "a nant-성과.pdf b" — 한글은 wide+spacer 2셀. 스페이서를 공백 취급하면
        // 단어가 첫 한글에서 끊긴다 (2026-07-14 "nant-성과분석.pdf 안 열림" 원인).
        fn push(cells: &mut Vec<TerminalCell>, c: char, wide: bool, spacer: bool) {
            cells.push(TerminalCell {
                c,
                fg: [255; 3],
                bg: [0; 3],
                wide,
                wide_spacer: spacer,
                attrs: Default::default(),
            });
        }
        let cols = 20usize;
        let mut cells = Vec::new();
        for c in "a nant-".chars() {
            push(&mut cells, c, false, false);
        }
        for c in ['성', '과'] {
            push(&mut cells, c, true, false);
            push(&mut cells, ' ', false, true);
        }
        for c in ".pdf b".chars() {
            push(&mut cells, c, false, false);
        }
        while cells.len() < cols {
            push(&mut cells, ' ', false, false);
        }
        let snap = TerminalViewportSnapshot {
            cols: cols as u16,
            rows: 1,
            cursor: CursorSnapshot {
                col: 0,
                row: 0,
                shape: CursorShape::Block,
                visible: true,
            },
            visible_cells: cells.into(),
            dirty_ranges: Vec::new(),
            title: None,
            scroll_offset: 0,
            is_alt_screen: false,
        };
        // '성'(idx 7) 위를 더블클릭 — 파일명 전체가 한 단어여야 한다
        let (s, e) = word_range_at(&snap, 7).expect("단어");
        assert_eq!(renderer_egui::selection_text(&snap, s, e), "nant-성과.pdf");
        // 공백(idx 1)은 여전히 단어가 아니다
        assert!(word_range_at(&snap, 1).is_none());
    }

    #[test]
    fn 경로_더블클릭은_폴더와_허용_문서만_해석한다() {
        let base = std::env::temp_dir().join(format!("deppy-pathclick-{}", std::process::id()));
        let dir = base.join("docs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(base.join("report.pdf"), b"x").unwrap();
        std::fs::write(base.join("run.sh"), b"x").unwrap();

        // 절대경로 폴더 → Dir
        assert_eq!(
            resolve_path_click(dir.to_str().unwrap(), None),
            Some(PathClick::Dir(dir.clone()))
        );
        // 상대경로는 cwd 기준. 따옴표·문말 부호는 벗긴다.
        assert_eq!(
            resolve_path_click("'docs',", Some(&base)),
            Some(PathClick::Dir(dir.clone()))
        );
        // cwd 없이 상대경로는 해석 불가
        assert_eq!(resolve_path_click("docs", None), None);
        // 허용 확장자 파일 → OpenFile, 줄번호 suffix 제거 (rustc의 :행:칸 이중 포함)
        assert_eq!(
            resolve_path_click("report.pdf:12", Some(&base)),
            Some(PathClick::OpenFile(base.join("report.pdf")))
        );
        assert_eq!(
            resolve_path_click("report.pdf:12:34", Some(&base)),
            Some(PathClick::OpenFile(base.join("report.pdf")))
        );
        // 여는 괄호로 감싼 표기도 해석된다
        assert_eq!(
            resolve_path_click("(report.pdf)", Some(&base)),
            Some(PathClick::OpenFile(base.join("report.pdf")))
        );
        // 스크립트는 열지 않는다 (open은 실행 가능 대상을 실행하므로)
        assert_eq!(resolve_path_click("run.sh", Some(&base)), None);
        // pdf로 위장한 심링크가 스크립트를 가리키면 열지 않는다 (canonical 확장자 검사)
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(base.join("run.sh"), base.join("fake.pdf")).unwrap();
            assert_eq!(resolve_path_click("fake.pdf", Some(&base)), None);
        }
        // 존재하지 않는 일반 단어 → None (기존 더블클릭 선택만)
        assert_eq!(resolve_path_click("hello", Some(&base)), None);

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn 페이스트_fallback은_같은_제스처의_native또는_text처리_직후에만_스킵된다() {
        let now = std::time::Instant::now();
        let old = now.checked_sub(PASTE_GESTURE_WINDOW * 2);
        // native key-down은 이전 이력과 무관하게 새 제스처를 시작한다.
        assert!(!should_skip_paste_task(
            ClipboardPasteTrigger::NativeKeyDown,
            false,
            Some(now),
            Some(now),
        ));
        // press에서 Event::Paste 텍스트를 이미 보냈고 fallback이 없다 → key-up 스킵.
        assert!(should_skip_paste_task(
            ClipboardPasteTrigger::EguiShortcut,
            false,
            Some(now),
            None,
        ));
        // native key-down에서 이미지 작업을 시작한 뒤의 key-up도 스킵.
        assert!(should_skip_paste_task(
            ClipboardPasteTrigger::EguiShortcut,
            false,
            None,
            Some(now),
        ));
        // 같은 프레임에 Paste가 왔다(fallback 보유) → 태스크가 fallback을 쓰므로 진행.
        assert!(!should_skip_paste_task(
            ClipboardPasteTrigger::EguiShortcut,
            true,
            Some(now),
            Some(now),
        ));
        // 처리 이력이 없거나 오래됐다 → fallback이 유일한 감지 경로라 진행.
        assert!(!should_skip_paste_task(
            ClipboardPasteTrigger::EguiShortcut,
            false,
            None,
            None,
        ));
        if let Some(old) = old {
            assert!(!should_skip_paste_task(
                ClipboardPasteTrigger::EguiShortcut,
                false,
                Some(old),
                Some(old),
            ));
        }
    }

    fn tab_id(name: &str) -> MuxTabId {
        MuxTabId(name.to_owned())
    }

    fn pane(id: &str, session: SessionId) -> PaneSnapshot {
        PaneSnapshot {
            id: pane_id(id),
            session_id: Some(session),
            title: id.to_owned(),
            persistent_session_id: None,
        }
    }

    fn tab(id: &str, panes: Vec<PaneSnapshot>, layout: LayoutNode) -> TabSnapshot {
        TabSnapshot {
            id: tab_id(id),
            title: id.to_owned(),
            layout,
            panes,
        }
    }

    fn mux(active: &str, tabs: Vec<TabSnapshot>, focused: &str) -> Arc<MuxSnapshot> {
        Arc::new(MuxSnapshot {
            tabs,
            active_tab: Some(tab_id(active)),
            focused_pane: Some(pane_id(focused)),
        })
    }

    fn snapshot(text: &str) -> Arc<TerminalViewportSnapshot> {
        let cols = 12;
        let rows = 2;
        let mut cells = Vec::with_capacity(cols * rows);
        let chars: Vec<char> = text.chars().collect();
        for idx in 0..cols * rows {
            cells.push(TerminalCell {
                c: chars.get(idx).copied().unwrap_or(' '),
                fg: [255, 255, 255],
                bg: [0, 0, 0],
                wide: false,
                wide_spacer: false,
                attrs: Default::default(),
            });
        }
        Arc::new(TerminalViewportSnapshot {
            cols: cols as u16,
            rows: rows as u16,
            cursor: CursorSnapshot {
                col: 0,
                row: 0,
                shape: CursorShape::Block,
                visible: true,
            },
            visible_cells: cells.into(),
            dirty_ranges: Vec::new(),
            title: None,
            scroll_offset: 0,
            is_alt_screen: false,
        })
    }

    fn catalog() -> i18n::Catalog {
        i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap()
    }

    #[test]
    fn split_target는_focus_없으면_활성탭_첫_pane으로_폴백한다() {
        let tab_a = tab(
            "a",
            vec![pane("pa", SessionId(1))],
            LayoutNode::Pane(pane_id("pa")),
        );
        let tab_b = tab(
            "b",
            vec![pane("pb", SessionId(2))],
            LayoutNode::Pane(pane_id("pb")),
        );
        let tabs = vec![tab_a, tab_b];

        // focus가 있으면 그대로 쓴다.
        let with_focus = MuxSnapshot {
            tabs: tabs.clone(),
            active_tab: Some(tab_id("a")),
            focused_pane: Some(pane_id("pb")),
        };
        assert_eq!(split_target_pane(&with_focus), Some(pane_id("pb")));

        // focus가 없으면 활성 탭(b)의 첫 pane으로 폴백.
        let no_focus = MuxSnapshot {
            tabs: tabs.clone(),
            active_tab: Some(tab_id("b")),
            focused_pane: None,
        };
        assert_eq!(split_target_pane(&no_focus), Some(pane_id("pb")));

        // focus·active_tab 둘 다 없으면 첫 탭의 첫 pane.
        let nothing = MuxSnapshot {
            tabs,
            active_tab: None,
            focused_pane: None,
        };
        assert_eq!(split_target_pane(&nothing), Some(pane_id("pa")));

        // 빈 워크스페이스면 분할 대상이 없다.
        let empty = MuxSnapshot {
            tabs: vec![],
            active_tab: None,
            focused_pane: None,
        };
        assert_eq!(split_target_pane(&empty), None);
    }

    #[test]
    fn muxupdated_뒤_hidden_viewport는_snapshot을_되살리지_않는다() {
        let hidden = SessionId(1);
        let visible = SessionId(2);
        let mut ui = WorkspaceUi::new();
        let catalog = catalog();

        let tab_a = tab(
            "a",
            vec![pane("pa", hidden)],
            LayoutNode::Pane(pane_id("pa")),
        );
        let tab_b = tab(
            "b",
            vec![pane("pb", visible)],
            LayoutNode::Pane(pane_id("pb")),
        );

        ui.handle_events(
            &RecordingRuntime::default(),
            &[
                RuntimeEvent::MuxUpdated {
                    snapshot: mux("a", vec![tab_a.clone(), tab_b.clone()], "pa"),
                },
                RuntimeEvent::Viewport {
                    session: hidden,
                    snapshot: snapshot("old visible"),
                    bracketed_paste: false,
                },
            ],
            &catalog,
        );
        assert!(ui.sessions.get(&hidden).unwrap().snapshot.is_some());

        ui.handle_events(
            &RecordingRuntime::default(),
            &[
                RuntimeEvent::MuxUpdated {
                    snapshot: mux("b", vec![tab_a, tab_b], "pb"),
                },
                RuntimeEvent::Viewport {
                    session: hidden,
                    snapshot: snapshot("stale hidden"),
                    bracketed_paste: false,
                },
            ],
            &catalog,
        );

        assert!(
            ui.sessions
                .get(&hidden)
                .is_none_or(|view| view.snapshot.is_none()),
            "hidden tab의 stale Viewport가 UI snapshot cache를 되살리면 안 된다"
        );
    }

    #[test]
    fn active_tab_split의_visible_pane들은_viewport를_받는다() {
        let left = SessionId(10);
        let right = SessionId(11);
        let mut ui = WorkspaceUi::new();
        let catalog = catalog();
        let split = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::Pane(pane_id("left"))),
            second: Box::new(LayoutNode::Pane(pane_id("right"))),
        };
        let active = tab(
            "active",
            vec![pane("left", left), pane("right", right)],
            split,
        );

        ui.handle_events(
            &RecordingRuntime::default(),
            &[
                RuntimeEvent::MuxUpdated {
                    snapshot: mux("active", vec![active], "left"),
                },
                RuntimeEvent::Viewport {
                    session: left,
                    snapshot: snapshot("left"),
                    bracketed_paste: false,
                },
                RuntimeEvent::Viewport {
                    session: right,
                    snapshot: snapshot("right"),
                    bracketed_paste: true,
                },
            ],
            &catalog,
        );

        assert!(ui.sessions.get(&left).unwrap().snapshot.is_some());
        assert!(ui.sessions.get(&right).unwrap().snapshot.is_some());
        assert!(ui.sessions.get(&right).unwrap().bracketed_paste);
        assert!(!ui.session_bracketed_paste(left));
        assert!(ui.session_bracketed_paste(right));
    }

    #[test]
    fn path_insert_paste_bytes_required_fixtures는_bracketed와_no_enter를_지킨다() {
        use crate::ui::file_tree::ShellKind;

        let fixtures = [
            "src/main.rs",
            "プロジェクト/設定ファイル.rs",
            "项目/配置文件.rs",
            "專案/設定檔.rs",
            "프로젝트/설정파일.rs",
            "project/🚀-deploy/config.json",
        ];

        for fixture in fixtures {
            let path = Path::new(fixture);
            for shell in [
                ShellKind::Posix,
                ShellKind::Fish,
                ShellKind::PowerShell,
                ShellKind::Cmd,
            ] {
                let raw = crate::ui::file_tree::shell_path_insert_bytes_for(path, shell);
                assert_eq!(
                    path_insert_paste_bytes(path, shell, false),
                    raw,
                    "{fixture} {shell:?}"
                );
                assert_eq!(raw.last(), Some(&b' '), "{fixture} {shell:?}");
                assert!(!raw.contains(&b'\n'), "{fixture} {shell:?}");
                assert!(!raw.contains(&b'\r'), "{fixture} {shell:?}");

                let wrapped = path_insert_paste_bytes(path, shell, true);
                assert!(wrapped.starts_with(b"\x1b[200~"), "{fixture} {shell:?}");
                assert!(wrapped.ends_with(b"\x1b[201~"), "{fixture} {shell:?}");
                let inner = &wrapped[b"\x1b[200~".len()..wrapped.len() - b"\x1b[201~".len()];
                assert_eq!(inner, raw.as_slice(), "{fixture} {shell:?}");
                assert_eq!(inner.last(), Some(&b' '), "{fixture} {shell:?}");
                assert!(!inner.contains(&b'\n'), "{fixture} {shell:?}");
                assert!(!inner.contains(&b'\r'), "{fixture} {shell:?}");
            }
        }
    }

    #[test]
    fn paths_insert_paste_bytes는_클립보드_file_list를_다중_path로_삽입한다() {
        use crate::ui::file_tree::ShellKind;

        let paths = vec![
            std::path::PathBuf::from("images/sample image.png"),
            std::path::PathBuf::from("프로젝트/🚀-deploy/설정 파일.png"),
        ];
        for shell in [
            ShellKind::Posix,
            ShellKind::Fish,
            ShellKind::PowerShell,
            ShellKind::Cmd,
        ] {
            let mut expected = Vec::new();
            for path in &paths {
                expected.extend(crate::ui::file_tree::shell_path_insert_bytes_for(
                    path, shell,
                ));
            }
            assert_eq!(
                paths_insert_paste_bytes(&paths, shell, false),
                expected,
                "{shell:?}"
            );
            assert_eq!(expected.last(), Some(&b' '), "{shell:?}");
            assert!(!expected.contains(&b'\n'), "{shell:?}");
            assert!(!expected.contains(&b'\r'), "{shell:?}");

            let wrapped = paths_insert_paste_bytes(&paths, shell, true);
            assert!(wrapped.starts_with(b"\x1b[200~"), "{shell:?}");
            assert!(wrapped.ends_with(b"\x1b[201~"), "{shell:?}");
            let inner = &wrapped[b"\x1b[200~".len()..wrapped.len() - b"\x1b[201~".len()];
            assert_eq!(inner, expected.as_slice(), "{shell:?}");
        }
    }

    #[test]
    fn clipboard_terminal_paste는_file_list를_text_flavor보다_우선한다() {
        use crate::ui::file_tree::ShellKind;

        let paths = vec![std::path::PathBuf::from("images/sample image.png")];
        let text = Some(b"images/sample image.png".to_vec());

        assert_eq!(
            clipboard_terminal_paste_bytes(Some(&paths), text, ShellKind::Posix, false),
            Some(paths_insert_paste_bytes(&paths, ShellKind::Posix, false))
        );
    }

    #[test]
    fn clipboard_terminal_paste는_file_list가_없으면_text_paste로_fallback한다() {
        use crate::ui::file_tree::ShellKind;

        let text = input_mapper::paste_bytes("plain text".as_bytes(), true);

        assert_eq!(
            clipboard_terminal_paste_bytes(None, Some(text.clone()), ShellKind::Posix, true),
            Some(text)
        );
    }

    #[test]
    fn image_clipboard_background_result는_요청_세션에_정확히_한번만_전송된다() {
        use crate::ui::file_tree::ShellKind;

        let mut ui = WorkspaceUi::new();
        let runtime = RecordingRuntime::default();
        let session = SessionId(77);
        let paths = vec![std::path::PathBuf::from("/tmp/clipboard image.png")];
        let (tx, rx) = std::sync::mpsc::channel();
        ui.paste_task = Some(PendingPaste {
            rx,
            session,
            bracketed: true,
            shell_kind: ShellKind::Posix,
            text_fallback: None,
            requested_at: std::time::Instant::now(),
        });
        tx.send(Ok(Some(paths.clone()))).unwrap();

        ui.poll_paste_task(&runtime);
        ui.poll_paste_task(&runtime);

        let commands = runtime.commands.lock().unwrap();
        assert_eq!(commands.len(), 1);
        assert!(matches!(
            &commands[0],
            RuntimeCommand::WriteInput {
                session: target,
                bytes,
            } if *target == session
                && *bytes == paths_insert_paste_bytes(&paths, ShellKind::Posix, true)
        ));
    }

    #[test]
    fn generated_pane_titles_render_through_catalog_and_legacy_korean_titles() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        assert_eq!(
            display_pane_title("workspace.spawn.shell 3", &catalog),
            "Shell 3"
        );
        assert_eq!(
            display_pane_title("workspace.spawn.agent 4", &catalog),
            "Agent 4"
        );
        assert_eq!(display_pane_title("셸 5", &catalog), "Shell 5");
        assert_eq!(display_pane_title("custom title", &catalog), "custom title");
    }

    #[test]
    fn terminal_text_dnd_paste는_raw_text를_보존한다() {
        let fixtures = [
            "src/main.rs",
            "プロジェクト/設定ファイル.rs",
            "项目/配置文件.rs",
            "專案/設定檔.rs",
            "프로젝트/설정파일.rs",
            "project/🚀-deploy/config.json",
            "line one\nline two",
        ];

        for fixture in fixtures {
            assert_eq!(
                terminal_text_paste_bytes(fixture, false),
                fixture.as_bytes(),
                "{fixture}"
            );
            let wrapped = terminal_text_paste_bytes(fixture, true);
            assert!(wrapped.starts_with(b"\x1b[200~"), "{fixture}");
            assert!(wrapped.ends_with(b"\x1b[201~"), "{fixture}");
            let inner = &wrapped[b"\x1b[200~".len()..wrapped.len() - b"\x1b[201~".len()];
            assert_eq!(inner, fixture.as_bytes(), "{fixture}");
        }
    }

    #[test]
    fn terminal_text_dnd는_선택범위_내부에서만_시작한다() {
        assert!(selection_range_contains(3, 7, 3));
        assert!(selection_range_contains(3, 7, 5));
        assert!(selection_range_contains(3, 7, 7));
        assert!(!selection_range_contains(3, 7, 2));
        assert!(!selection_range_contains(3, 7, 8));
    }

    #[test]
    fn 드래그_오토스크롤_속도는_초과거리_비례_부호는_scroll_delta_규약() {
        // pane 세로 범위 [100, 500], 셀 높이 16
        // 경계 안 → 0
        assert_eq!(drag_autoscroll_rate(300.0, 100.0, 500.0, 16.0), 0.0);
        assert_eq!(drag_autoscroll_rate(100.0, 100.0, 500.0, 16.0), 0.0);
        assert_eq!(drag_autoscroll_rate(500.0, 100.0, 500.0, 16.0), 0.0);
        // 위로 벗어남 → 양수(과거로), 1셀 초과 = 8행/초
        assert_eq!(drag_autoscroll_rate(84.0, 100.0, 500.0, 16.0), 8.0);
        // 아래로 벗어남 → 음수(최신으로), 조금 벗어나면 느리게
        assert_eq!(drag_autoscroll_rate(504.0, 100.0, 500.0, 16.0), -2.0);
        // 많이 벗어나면 빠르게, 최대 60행/초로 clamp
        assert_eq!(drag_autoscroll_rate(2000.0, 100.0, 500.0, 16.0), -60.0);
        assert_eq!(drag_autoscroll_rate(-2000.0, 100.0, 500.0, 16.0), 60.0);
    }

    #[test]
    fn 드래그_오토스크롤_앵커는_스크롤량만큼_이동하고_화면_경계에서_clamp() {
        // 10열 × 5행, 앵커 = 2행 3열(idx 23)
        // 과거로 1행(내용 아래로 이동) → 앵커도 1행 아래
        assert_eq!(shift_selection_cell(23, 1, 10, 5), 33);
        // 최신으로 2행 → 앵커 2행 위
        assert_eq!(shift_selection_cell(23, -2, 10, 5), 3);
        // 위로 화면 밖 → 첫 행 첫 열
        assert_eq!(shift_selection_cell(23, -5, 10, 5), 0);
        // 아래로 화면 밖 → 마지막 행 마지막 열
        assert_eq!(shift_selection_cell(23, 5, 10, 5), 49);
    }

    #[test]
    fn image_paste_shortcut은_platform_clipboard_paste만_잡는다() {
        let ctrl_v = egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL,
        };
        let ctrl_shift_v = egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
        };
        let cmd_v = egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                mac_cmd: true,
                command: true,
                ..egui::Modifiers::NONE
            },
        };

        // 이미지-only clipboard의 Cmd+V PRESS는 egui가 소비한다. 이 함수는 native
        // key-down 감시가 없을 때를 위한 release fallback만 잡는다.
        let cmd_v_release = egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers {
                mac_cmd: true,
                command: true,
                ..egui::Modifiers::NONE
            },
        };
        if cfg!(target_os = "macos") {
            assert!(is_clipboard_paste_shortcut(&cmd_v_release));
            assert!(!is_clipboard_paste_shortcut(&cmd_v)); // press는 AppKit monitor가 담당
            assert!(!is_clipboard_paste_shortcut(&ctrl_v));
            assert!(!is_clipboard_paste_shortcut(&ctrl_shift_v));
        } else {
            assert!(!is_clipboard_paste_shortcut(&cmd_v));
            assert!(!is_clipboard_paste_shortcut(&ctrl_v));
            assert!(is_clipboard_paste_shortcut(&ctrl_shift_v));
        }
    }

    #[test]
    fn terminal_keyboard는_textedit_popup_window가_없을때만_활성이다() {
        assert!(terminal_keyboard_input_allowed(false, false, false, false));
        assert!(!terminal_keyboard_input_allowed(true, false, false, false));
        assert!(!terminal_keyboard_input_allowed(false, true, false, false));
        assert!(!terminal_keyboard_input_allowed(false, false, true, false));
        // 검색 닫힘/터미널 클릭 직후에는 사라진 TextEdit의 stale focus보다 terminal refocus가
        // 우선이라 첫 문장부호·한글 조합이 빠지지 않는다.
        assert!(terminal_keyboard_input_allowed(true, false, false, true));
        assert!(!terminal_keyboard_input_allowed(true, true, false, true));
        assert!(!terminal_keyboard_input_allowed(true, false, true, true));
    }

    #[test]
    fn agents_window는_terminal_refocus를_막는_modal_layer가_아니다() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::Window::new("Agents")
                .id(crate::ui::agent_sessions::agents_window_id())
                .show(ui.ctx(), |ui| {
                    ui.label("agent content");
                });
        });
        let agents = egui::LayerId::new(
            egui::Order::Middle,
            crate::ui::agent_sessions::agents_window_id(),
        );
        let confirmation = egui::LayerId::new(egui::Order::Middle, egui::Id::new("confirmation"));
        let background = egui::LayerId::background();

        assert!(ctx.memory(|memory| memory.areas().visible_layer_ids().contains(&agents)));
        assert!(!is_blocking_terminal_window(&agents));
        assert!(is_blocking_terminal_window(&confirmation));
        assert!(!is_blocking_terminal_window(&background));
    }

    #[test]
    fn pending_focus는_runtime_이전_pane보다_먼저_입력_대상이_된다() {
        let old = pane_id("old");
        let next = pane_id("next");
        assert!(terminal_input_owner(&old, true, None));
        assert!(!terminal_input_owner(&old, true, Some(&next)));
        assert!(terminal_input_owner(&next, false, Some(&next)));
        assert!(terminal_input_owner(&next, true, Some(&next)));
    }

    #[test]
    fn terminal_focus_claim은_한_frame에서_한번만_소비된다() {
        let mut workspace = WorkspaceUi::new();
        workspace.terminal_focus_claimed = true;

        assert!(workspace.take_terminal_focus_claimed());
        assert!(!workspace.take_terminal_focus_claimed());
    }

    fn printable_key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn reconciled_text_bytes(
        native: &[crate::native_key_monitor::NativePrintableKeyDown],
        events: &[egui::Event],
        preedit_active: bool,
    ) -> Vec<u8> {
        let reconciliation = reconcile_ime_text_events(native, events, preedit_active);
        let mut bytes = Vec::new();
        for text in reconciliation.event_text.into_iter().flatten() {
            bytes.extend(text.as_bytes());
        }
        bytes.extend(reconciliation.fallback_bytes);
        bytes
    }

    #[test]
    fn 한글_조합직후_text없는_문장부호_key는_pty로_보완된다() {
        let events = [
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "ㄱ".into(),
                active_range_chars: None,
            }),
            printable_key(egui::Key::Period),
            egui::Event::Ime(egui::ImeEvent::Commit("ㄱ".into())),
        ];
        assert_eq!(reconciled_text_bytes(&[], &events, false), "ㄱ.".as_bytes());
    }

    #[test]
    fn 한글_조합직후_정상_text는_특수문자_대체입력을_취소한다() {
        let events = [
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "ㄱ".into(),
                active_range_chars: None,
            }),
            printable_key(egui::Key::Period),
            egui::Event::Text(".".into()),
        ];
        assert_eq!(reconciled_text_bytes(&[], &events, false), b".");
    }

    #[test]
    fn 한글_조합직후_같은_특수문자_연타도_text_수만큼만_중복제거한다() {
        let events = [
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "ㄱ".into(),
                active_range_chars: None,
            }),
            printable_key(egui::Key::Period),
            printable_key(egui::Key::Period),
            egui::Event::Text(".".into()),
        ];
        assert_eq!(reconciled_text_bytes(&[], &events, false), b"..");
    }

    #[test]
    fn 빠른_연타에서_누락된_첫_기호는_뒤_text보다_앞에_복구된다() {
        let native = [
            crate::native_key_monitor::NativePrintableKeyDown::for_test('.'),
            crate::native_key_monitor::NativePrintableKeyDown::for_test(','),
        ];
        let events = [
            egui::Event::Ime(egui::ImeEvent::Commit("ㅁ".into())),
            egui::Event::Text(",".into()),
        ];
        assert_eq!(
            reconciled_text_bytes(&native, &events, true),
            "ㅁ.,".as_bytes()
        );
    }

    #[test]
    fn appkit이_보존한_keydown은_winit이_숨긴_마침표를_commit_frame에서_복구한다() {
        let native = [crate::native_key_monitor::NativePrintableKeyDown::for_test(
            '.',
        )];
        let events = [
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "ㅁ".into(),
                active_range_chars: None,
            }),
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: String::new(),
                active_range_chars: None,
            }),
            egui::Event::Ime(egui::ImeEvent::Commit("ㅁ".into())),
        ];
        assert_eq!(
            reconciled_text_bytes(&native, &events, false),
            "ㅁ.".as_bytes()
        );
    }

    #[test]
    fn appkit_keydown은_commit_text와_egui_fallback을_각각_중복하지_않는다() {
        let native_period = [crate::native_key_monitor::NativePrintableKeyDown::for_test(
            '.',
        )];
        let commit_includes_period = [egui::Event::Ime(egui::ImeEvent::Commit("ㅁ.".into()))];
        assert_eq!(
            reconciled_text_bytes(&native_period, &commit_includes_period, true),
            "ㅁ.".as_bytes()
        );

        let commit_omits_period = [egui::Event::Ime(egui::ImeEvent::Commit("ㅁ".into()))];
        assert_eq!(
            reconciled_text_bytes(&native_period, &commit_omits_period, true),
            "ㅁ.".as_bytes()
        );
    }

    #[test]
    fn alacritty_8079의_commit과_text_이중_space는_한칸만_전달된다() {
        let native_space = [crate::native_key_monitor::NativePrintableKeyDown::for_test(
            ' ',
        )];
        let events = [
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: " ".into(),
                active_range_chars: Some(1..1),
            }),
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: String::new(),
                active_range_chars: None,
            }),
            egui::Event::Ime(egui::ImeEvent::Commit(" ".into())),
            printable_key(egui::Key::Space),
            egui::Event::Text(" ".into()),
        ];
        assert_eq!(reconciled_text_bytes(&native_space, &events, false), b" ");
    }

    #[test]
    fn space를_두번_누르면_commit_text_중복후에도_정확히_두칸이다() {
        let native_spaces = [
            crate::native_key_monitor::NativePrintableKeyDown::for_test(' '),
            crate::native_key_monitor::NativePrintableKeyDown::for_test(' '),
        ];
        let events = [
            egui::Event::Ime(egui::ImeEvent::Commit(" ".into())),
            printable_key(egui::Key::Space),
            egui::Event::Text(" ".into()),
            printable_key(egui::Key::Space),
            egui::Event::Text(" ".into()),
        ];
        assert_eq!(reconciled_text_bytes(&native_spaces, &events, true), b"  ");
    }

    #[test]
    fn comma_commit과_text가_함께_와도_물리키_한번만_전달된다() {
        let native_comma = [crate::native_key_monitor::NativePrintableKeyDown::for_test(
            ',',
        )];
        let events = [
            egui::Event::Ime(egui::ImeEvent::Commit(",".into())),
            printable_key(egui::Key::Comma),
            egui::Event::Text(",".into()),
        ];
        assert_eq!(reconciled_text_bytes(&native_comma, &events, true), b",");
    }

    #[test]
    fn 한글_commit에_포함된_comma는_뒤따른_text와_중복되지_않는다() {
        let native_comma = [crate::native_key_monitor::NativePrintableKeyDown::for_test(
            ',',
        )];
        let events = [
            egui::Event::Ime(egui::ImeEvent::Commit("한,".into())),
            printable_key(egui::Key::Comma),
            egui::Event::Text(",".into()),
        ];
        assert_eq!(
            reconciled_text_bytes(&native_comma, &events, true),
            "한,".as_bytes()
        );
    }

    #[test]
    fn 일반_text도_appkit_물리키보다_많은_comma는_중복제거한다() {
        let native_comma = [crate::native_key_monitor::NativePrintableKeyDown::for_test(
            ',',
        )];
        let events = [egui::Event::Text(",".into()), egui::Event::Text(",".into())];
        assert_eq!(reconciled_text_bytes(&native_comma, &events, false), b",");
    }

    #[test]
    fn ime가_아닌_batch에서는_누락된_네이티브키를_추측삽입하지_않는다() {
        let native = [
            crate::native_key_monitor::NativePrintableKeyDown::for_test('.'),
            crate::native_key_monitor::NativePrintableKeyDown::for_test(','),
        ];
        let events = [egui::Event::Text(",".into())];
        assert_eq!(reconciled_text_bytes(&native, &events, false), b",");
    }

    #[test]
    fn ime가_관여하지_않은_네이티브_keydown은_누락문자를_별도_주입하지_않는다() {
        let native = [crate::native_key_monitor::NativePrintableKeyDown::for_test(
            '.',
        )];
        assert!(reconciled_text_bytes(&native, &[], false).is_empty());
    }

    #[test]
    fn terminal_focus_lock_filter는_app_request_focus와_같은_filter를_쓴다() {
        let filter = renderer_egui::terminal_focus_lock_filter();
        assert!(filter.tab);
        assert!(filter.horizontal_arrows);
        assert!(filter.vertical_arrows);
        assert!(filter.escape);
    }
}
