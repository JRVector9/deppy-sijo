//! 워크스페이스 뷰 (설계문서 PR-10): tab bar + split pane 렌더.
//! Runtime Boundary(2장) 준수 — 명령 전송/이벤트 수신/스냅샷 렌더만.
//! mux 배치는 MuxUpdated 스냅샷이 유일한 근거, active tab visible pane만 live render (14.4).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use runtime::{
    LayoutNode, MuxSnapshot, RuntimeClient, RuntimeCommand, RuntimeEvent, SessionId, SessionStatus,
    SpawnKind, SplitDirection,
};
use terminal::{TerminalViewportSnapshot, input_mapper, renderer_egui};

use crate::config::TerminalConfig;

pub struct WorkspaceUi {
    mux: Option<Arc<MuxSnapshot>>,
    sessions: HashMap<SessionId, SessionView>,
    /// Runtime이 per-session shell metadata를 제공하기 전까지 path insert quoting에 쓰는
    /// workspace 기본 shell kind.
    shell_kind: crate::ui::file_tree::ShellKind,
    /// IME 조합 중 텍스트 (focused pane 전용)
    preedit: String,
    /// 세션별 마지막 전송한 (cols, rows) — 변화 시에만 Resize 전송
    sent_sizes: HashMap<SessionId, (u16, u16)>,
    /// 트랙패드 미세 스크롤 누적 (focused pane 기준)
    scroll_residual: f32,
    /// 이번 프레임에 명령을 보냈다 — 응답 이벤트 폴링을 위해 repaint 예약
    command_sent: bool,
    /// mux focused_pane 변경 추적
    last_focused_pane: Option<runtime::MuxPaneId>,
    /// egui 포커스 동기화 대기 — 해당 pane이 실제로 그려질 때 소비된다
    /// (MuxUpdated가 Viewport보다 먼저 오는 프레임에 요청이 유실되지 않게)
    pending_focus: Option<runtime::MuxPaneId>,
    /// 응답(Spawned/Failed)을 아직 못 받은 셸 spawn 수 — 0이 될 때까지 계속 폴링
    pending_spawns: u32,
    /// split 경계 드래그 중 로컬 미리보기 (path, ratio). 드래그 동안은 명령을 보내지
    /// 않고(매 프레임 DB 저장 방지) 릴리즈 시 1회 ResizeSplit을 보낸다.
    split_drag: Option<(Vec<u8>, f32)>,
    /// 닫기 확인 대기 중인 pane — 실행 중 세션이 있는 pane 닫기는 확인을 거친다
    /// (2026-07-05 사용자 보고: 닫기 실수로 셸 전체 즉사 방지).
    confirm_close: Option<runtime::MuxPaneId>,
    /// 터미널 마우스 선택 (session, anchor 셀, head 셀 — 드래그 방향 그대로,
    /// 렌더/복사 시 정규화). 새 출력(Viewport)이 오면 그 세션의 선택은 해제한다.
    selection: Option<(SessionId, usize, usize)>,
    /// 활성 workspace의 프로젝트명(폴더명 ≈ 깃 레포명, 없으면 "~"). 세션 기본 제목이
    /// "셀 134" 대신 이걸로 표시된다. rename한 세션은 그대로 둔다. App이 매 프레임 세팅.
    project_name: Option<String>,
    /// 세션별 현재 작업 폴더(App이 매 프레임 set) — 1행 제목 폴더명/프로젝트명 원천.
    session_cwds: std::collections::HashMap<SessionId, String>,
    /// 세션별 에이전트 표시정보(model/effort/context — App이 병합해 set) — 3줄 행 2/3행.
    agent_info: std::collections::HashMap<SessionId, crate::agent_detect::AgentDisplay>,
    /// 진행 중인 백그라운드 클립보드 paste(이미지 PNG 인코딩을 UI 밖으로 — 2026-07-07).
    /// show()가 매 프레임 폴링해 완료 시 해당 세션에 삽입한다. 새 ⌘V는 이전 것을 대체.
    paste_task: Option<PendingPaste>,
    error: Option<String>,
    /// 현재 error 배너가 input backpressure 경고인지 — 해소 이벤트(queued=0)가
    /// 무관한 오류(spawn 실패 등)를 지우지 않게 구분한다(codex 2026-07-09).
    error_is_pressure: bool,
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
            sent_sizes: HashMap::new(),
            scroll_residual: 0.0,
            command_sent: false,
            last_focused_pane: None,
            pending_focus: None,
            pending_spawns: 0,
            split_drag: None,
            confirm_close: None,
            selection: None,
            project_name: None,
            session_cwds: std::collections::HashMap::new(),
            agent_info: std::collections::HashMap::new(),
            paste_task: None,
            error: None,
            error_is_pressure: false,
        }
    }

    /// 활성 workspace의 프로젝트명을 세팅한다(App이 매 프레임). 세션 기본 제목("셀 N")을
    /// 이 이름으로 표시한다.
    pub fn set_project_name(&mut self, name: Option<String>) {
        self.project_name = name;
    }

    /// 세션별 현재 작업 폴더를 세팅한다(App이 매 프레임, 감지 워커 lsof 결과).
    pub fn set_session_cwds(&mut self, cwds: std::collections::HashMap<SessionId, String>) {
        self.session_cwds = cwds;
    }

    /// 세션별 에이전트 표시정보를 세팅한다(App이 병합한 최종본 — 3줄 행 렌더용).
    pub fn set_agent_info(
        &mut self,
        info: std::collections::HashMap<SessionId, crate::agent_detect::AgentDisplay>,
    ) {
        self.agent_info = info;
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
            .and_then(|c| crate::agent_detect::project_display_name(c))
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

    fn handle_events(&mut self, events: &[RuntimeEvent], catalog: &i18n::Catalog) {
        for event in events {
            match event {
                RuntimeEvent::MuxUpdated { snapshot } => {
                    // 사라진 세션의 캐시 정리
                    let alive = mux_sessions(snapshot);
                    self.sessions.retain(|id, _| alive.contains(id));
                    self.sent_sizes.retain(|id, _| alive.contains(id));
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
                        self.sessions.entry(*session).or_default().status = Some(*status);
                    }
                }
                RuntimeEvent::SessionStatusViewChanged { session, view } => {
                    if self.session_alive(*session) {
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
                RuntimeEvent::ShellSpawned { .. } => {
                    self.pending_spawns = self.pending_spawns.saturating_sub(1);
                }
                RuntimeEvent::AgentSpawned { .. } => {}
                RuntimeEvent::ResourceUsage { .. } => {}
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
        self.handle_events(events, catalog);
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
        }
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
            self.send(
                client,
                RuntimeCommand::FocusPane {
                    pane: pane_id.clone(),
                },
            );
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
                        self.send(
                            client,
                            RuntimeCommand::FocusPane {
                                pane: pane_id.clone(),
                            },
                        );
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

        // pane 크기 → cols/rows. visible pane 전부 대상 — split 직후 기존 pane의
        // PTY 크기가 틀어지는 문제 방지 (runtime도 visible 세션을 모두 push한다)
        let cell = renderer_egui::cell_size(ui.ctx(), config.font_size);
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
            }
            let Some(snapshot) = view.snapshot.clone() else {
                ui.label(catalog.t("workspace.connecting", &[]));
                return;
            };
            (view.exit_code, view.bracketed_paste, snapshot)
        };

        let preedit = (focused && !self.preedit.is_empty()).then_some(self.preedit.as_str());
        // 이 세션의 선택 영역 (정규화)
        let selection_range = self
            .selection
            .and_then(|(s, a, b)| (s == session).then_some((a.min(b), a.max(b))));
        let output = {
            let view = self.sessions.entry(session).or_default();
            renderer_egui::draw(
                ui,
                &snapshot,
                config.font_size,
                &mut view.render_cache,
                preedit,
                selection_range,
            )
        };

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
            if output.response.double_clicked()
                && let Some(pos) = output.response.interact_pointer_pos()
            {
                // 더블클릭 → 커서 아래 단어(공백 구분) 선택. 단어가 URL이면 기본 브라우저로
                // 연다(claude/codex/셸 화면의 링크를 바로 열기).
                if let Some((s, e)) = word_range_at(&snapshot, cell_at(pos)) {
                    self.selection = Some((session, s, e));
                    let word = renderer_egui::selection_text(&snapshot, s, e);
                    if let Some(url) = extract_url(&word)
                        && let Err(err) = auth::open_in_browser(url)
                    {
                        self.error_is_pressure = false;
                        self.error = Some(format!("{err:#}"));
                    }
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
                }
            } else if output.response.dragged()
                && let Some(pos) = output.response.interact_pointer_pos()
                && let Some((s, anchor, _)) = self.selection
                && s == session
            {
                self.selection = Some((session, anchor, cell_at(pos)));
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
            request_terminal_focus(&output.response);
            if !focused {
                self.send(
                    client,
                    RuntimeCommand::FocusPane {
                        pane: pane_id.clone(),
                    },
                );
            }
        }

        // 파일 트리에서 드래그한 경로를 터미널 위에 드롭 → 입력으로 삽입 (2026-07-05).
        // hover 테두리는 위 pane 배경 경로가 pane_rect에 그린다.
        if let Some(path) = output.response.dnd_release_payload::<std::path::PathBuf>() {
            let bytes = path_insert_paste_bytes(&path, self.session_shell_kind(session), bracketed);
            self.send(client, RuntimeCommand::WriteInput { session, bytes });
            if !focused {
                self.send(
                    client,
                    RuntimeCommand::FocusPane {
                        pane: pane_id.clone(),
                    },
                );
            }
        }
        if let Some(text) = output
            .response
            .dnd_release_payload::<TerminalTextDragPayload>()
        {
            let bytes = terminal_text_paste_bytes(&text.text, bracketed);
            self.send(client, RuntimeCommand::WriteInput { session, bytes });
            if !focused {
                self.send(
                    client,
                    RuntimeCommand::FocusPane {
                        pane: pane_id.clone(),
                    },
                );
            }
        }
        // 터미널 위 우클릭도 같은 메뉴 (터미널 위젯이 topmost라 배경 interact가 못 받음)
        self.pane_context_menu(&output.response, pane_id, config, client, catalog);

        // 입력은 focused pane으로만. egui focus가 세션 목록/버튼으로 튀어도 Claude/vim
        // 같은 terminal TUI 입력은 계속 terminal에 보내야 한다. 단 TextEdit/팝업/별도
        // Window가 열려 있으면 그 UI가 키보드를 소유한다.
        // top_layer_id()는 닫힌 Window의 layer가 areas order에 남아 계속 Some을
        // 반환한다 — 설정 창을 한 번 열면 터미널 입력이 영구 차단됐다(2026-07-06 사용자).
        // "이번 프레임에 실제로 보이는" Middle(Window) layer 존재로 판정한다.
        let any_window_visible = ui.ctx().memory(|mem| {
            mem.areas()
                .visible_layer_ids()
                .iter()
                .any(|layer| layer.order == egui::Order::Middle)
        });
        let terminal_keyboard_active = focused
            && terminal_keyboard_input_allowed(
                ui.ctx().text_edit_focused(),
                ui.ctx().any_popup_open(),
                any_window_visible,
            );
        if terminal_keyboard_active && !output.response.has_focus() {
            request_terminal_focus(&output.response);
        }
        if terminal_keyboard_active {
            let mut pending: Vec<u8> = Vec::new();
            let mut copy_text: Option<String> = None;
            let mut image_paste_requested = false;
            let mut text_paste_bytes: Option<Vec<u8>> = None;
            ui.input(|input| {
                let modifiers = input.modifiers;
                for event in &input.raw.events {
                    if let egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) = event {
                        self.preedit = text.clone();
                        continue;
                    }
                    if let egui::Event::Ime(egui::ImeEvent::Commit(_)) = event {
                        self.preedit.clear();
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
                        image_paste_requested = true;
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
            if let Some(text) = copy_text {
                ui.ctx().copy_text(text);
            }
            if image_paste_requested {
                // 파일/이미지 판별 + PNG 인코딩은 백그라운드로(UI 딜레이 제거 — 2026-07-07).
                // 완료는 show()의 poll_paste_task가 소비한다. 연타 ⌘V는 최신 것으로 대체.
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
            } else if let Some(bytes) = text_paste_bytes {
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
    }

    /// 명령을 보냈거나 spawn 응답 대기 중이면 repaint를 예약한다 —
    /// 느린 spawn(keyring 등)도 응답 이벤트가 올 때까지 폴링이 끊기지 않는다.
    /// show()의 모든 return 경로에서 호출할 것.
    /// pane 닫기 요청 — 실행 중 세션이면 확인을 거치고, 아니면 즉시 닫는다.
    /// (세션 상태를 모르면 보수적으로 확인을 띄운다 — 실수 즉사 방지가 목적.)
    fn request_close_pane(&mut self, client: &dyn RuntimeClient, pane: runtime::MuxPaneId) {
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
            if let Some(session) = session {
                ui.menu_button(catalog.t("status.override.menu", &[]), |ui| {
                    // mark_waiting은 제거 — 대기는 입력대기(NeedsApproval)로 통합(2026-07-07).
                    for (key, status) in [
                        ("status.override.mark_running", SessionStatus::Running),
                        (
                            "status.override.mark_needs_approval",
                            SessionStatus::NeedsApproval,
                        ),
                        ("status.override.mark_idle", SessionStatus::Idle),
                        ("status.override.mark_done", SessionStatus::Done),
                        ("status.override.mark_error", SessionStatus::Error),
                    ] {
                        if ui.button(catalog.t(key, &[])).clicked() {
                            self.send(
                                client,
                                RuntimeCommand::SetUserStatusOverride {
                                    session,
                                    override_: runtime::UserStatusOverride::Mark(status),
                                },
                            );
                            ui.close();
                        }
                    }
                    if ui.button(catalog.t("status.override.clear", &[])).clicked() {
                        self.send(
                            client,
                            RuntimeCommand::SetUserStatusOverride {
                                session,
                                override_: runtime::UserStatusOverride::Clear,
                            },
                        );
                        ui.close();
                    }
                });
                ui.separator();
            }
            if ui.button(catalog.t("workspace.close_pane", &[])).clicked() {
                self.request_close_pane(client, pane_id.clone());
                ui.close();
            }
        });
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
                    user_override,
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

    /// 단축키용 현재 pane 닫기. 실행 중인 세션은 마우스 ×와 동일하게 확인창을 거친다.
    pub fn close_focused_pane(&mut self, client: &dyn RuntimeClient) {
        if let Some(pane) = self.mux.as_ref().and_then(|mux| mux.focused_pane.clone()) {
            self.request_close_pane(client, pane);
        }
    }

    /// 단축키용 현재 pane 분할. UI 버튼과 같은 runtime 명령을 사용한다.
    pub fn split_focused_pane(
        &mut self,
        client: &dyn RuntimeClient,
        direction: SplitDirection,
        scrollback_lines: usize,
    ) {
        if let Some(pane) = self.mux.as_ref().and_then(|mux| mux.focused_pane.clone()) {
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
            self.send(client, RuntimeCommand::FocusPane { pane });
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

fn terminal_text_paste_bytes(text: &str, bracketed_paste: bool) -> Vec<u8> {
    input_mapper::paste_bytes(text.as_bytes(), bracketed_paste)
}

/// 세션 행 2행: "Codex · gpt-5.5 · xhigh" (빈 부분은 생략). 2026-07-08.
fn agent_info_line(d: &crate::agent_detect::AgentDisplay) -> String {
    use crate::agent_detect::AgentKind;
    let name = match d.kind {
        AgentKind::Claude => "Claude",
        AgentKind::Codex => "Codex",
    };
    let mut parts = vec![name.to_owned()];
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
        // macOS는 Cmd+V의 key PRESS를 앱에 전달하지 않고 release(pressed=false)만 준다
        // (실측: Event::Paste도 안 옴). 그래서 press로는 감지가 안 돼 붙여넣기가 무시됐다 —
        // release로 감지한다. 프레임당 image_paste_requested 불리언 1회로 합쳐진다.
        !pressed && modifiers.command && !modifiers.ctrl
    } else {
        *pressed && modifiers.ctrl && modifiers.shift
    }
}

/// 셀 idx 아래의 단어(공백 구분 비어있지 않은 셀 연속) 범위를 [start, end]로 돌려준다.
/// 공백 위를 더블클릭하면 None. 더블클릭 단어 선택에 쓴다.
/// 단어가 URL이면 (뒤따르는 구두점 제거 후) 그 URL을 반환한다. http/https만 연다.
fn extract_url(word: &str) -> Option<&str> {
    let trimmed = word.trim_end_matches(|c: char| ".,;:!?)]}>\"'".contains(c));
    (trimmed.starts_with("http://") || trimmed.starts_with("https://")).then_some(trimmed)
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
        snapshot
            .visible_cells
            .get(base + c)
            .is_some_and(|cell| !cell.c.is_whitespace() && cell.c != '\0')
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
) -> bool {
    !(text_edit_focused || popup_open || top_window_open)
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

fn format_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{MuxPaneId, MuxTabId, PaneSnapshot, TabSnapshot};
    use terminal::{CursorShape, CursorSnapshot, TerminalCell};

    fn pane_id(name: &str) -> MuxPaneId {
        MuxPaneId(name.to_owned())
    }

    fn tab_id(name: &str) -> MuxTabId {
        MuxTabId(name.to_owned())
    }

    fn pane(id: &str, session: SessionId) -> PaneSnapshot {
        PaneSnapshot {
            id: pane_id(id),
            session_id: Some(session),
            title: id.to_owned(),
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

        // macOS는 Cmd+V의 key PRESS를 앱에 안 주고 release만 준다(실측) — release로 감지한다.
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
            assert!(!is_clipboard_paste_shortcut(&cmd_v)); // press는 무시(release만)
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
        assert!(terminal_keyboard_input_allowed(false, false, false));
        assert!(!terminal_keyboard_input_allowed(true, false, false));
        assert!(!terminal_keyboard_input_allowed(false, true, false));
        assert!(!terminal_keyboard_input_allowed(false, false, true));
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
