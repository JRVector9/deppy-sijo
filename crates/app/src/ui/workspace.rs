//! 워크스페이스 뷰 (설계문서 PR-10): tab bar + split pane 렌더.
//! Runtime Boundary(2장) 준수 — 명령 전송/이벤트 수신/스냅샷 렌더만.
//! mux 배치는 MuxUpdated 스냅샷이 유일한 근거, active pane만 live render (14.4).

use std::collections::HashMap;
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
    error: Option<String>,
}

/// 세션별 화면 캐시. 비활성 pane은 마지막 스냅샷을 정적으로 표시한다.
#[derive(Default)]
struct SessionView {
    snapshot: Option<Arc<TerminalViewportSnapshot>>,
    bracketed_paste: bool,
    exit_code: Option<Option<u32>>,
    /// status detector 감지 상태 (agent만, PR-12)
    status: Option<SessionStatus>,
}

impl WorkspaceUi {
    pub fn new() -> Self {
        Self {
            mux: None,
            sessions: HashMap::new(),
            preedit: String::new(),
            sent_sizes: HashMap::new(),
            scroll_residual: 0.0,
            command_sent: false,
            last_focused_pane: None,
            pending_focus: None,
            pending_spawns: 0,
            error: None,
        }
    }

    fn handle_events(&mut self, events: &[RuntimeEvent]) {
        for event in events {
            match event {
                RuntimeEvent::MuxUpdated { snapshot } => {
                    // 사라진 세션의 캐시 정리
                    let alive: Vec<SessionId> = snapshot
                        .tabs
                        .iter()
                        .flat_map(|tab| &tab.panes)
                        .filter_map(|pane| pane.session_id)
                        .collect();
                    self.sessions.retain(|id, _| alive.contains(id));
                    self.sent_sizes.retain(|id, _| alive.contains(id));
                    // hidden(active tab 밖) 세션의 마지막 스냅샷은 버린다 —
                    // §14.4 hidden render cache drop. tab 복귀 시 worker가
                    // 전환 즉시 push하므로(emit_mux_and_watched) 공백은 짧다 (codex 리뷰)
                    let visible: Vec<SessionId> = snapshot
                        .tabs
                        .iter()
                        .filter(|tab| Some(&tab.id) == snapshot.active_tab.as_ref())
                        .flat_map(|tab| &tab.panes)
                        .filter_map(|pane| pane.session_id)
                        .collect();
                    for (id, view) in self.sessions.iter_mut() {
                        if !visible.contains(id) {
                            view.snapshot = None;
                        }
                    }
                    self.mux = Some(Arc::clone(snapshot));
                }
                RuntimeEvent::Viewport {
                    session,
                    snapshot,
                    bracketed_paste,
                } => {
                    // pane 제거 후 도착한 stale Viewport가 캐시를 되살리면
                    // any_running이 영구 repaint를 유발한다 — mux에 살아있는 세션만
                    if self.session_alive(*session) {
                        let view = self.sessions.entry(*session).or_default();
                        view.snapshot = Some(Arc::clone(snapshot));
                        view.bracketed_paste = *bracketed_paste;
                    }
                }
                RuntimeEvent::SessionExited { session, exit_code } => {
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
                    let kind = match kind {
                        SpawnKind::Shell => "셸",
                        SpawnKind::Agent => "에이전트",
                    };
                    self.error = Some(format!("{kind} 시작 실패: {message}"));
                }
                RuntimeEvent::SessionStatusChanged { session, status } => {
                    if self.session_alive(*session) {
                        self.sessions.entry(*session).or_default().status = Some(*status);
                    }
                }
                RuntimeEvent::ShellSpawned { .. } => {
                    self.pending_spawns = self.pending_spawns.saturating_sub(1);
                }
                RuntimeEvent::AgentSpawned { .. } => {}
            }
        }
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
        events: &[RuntimeEvent],
    ) {
        self.handle_events(events);

        self.tab_bar(ui, config, client);
        if let Some(error) = self.error.clone() {
            ui.horizontal(|ui| {
                ui.colored_label(ui.visuals().error_fg_color, error);
                if ui.small_button("×").clicked() {
                    self.error = None;
                }
            });
        }
        ui.separator();

        let Some(mux) = self.mux.clone() else {
            ui.centered_and_justified(|ui| ui.label("+ 새 셸 로 시작하세요"));
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
            ui.centered_and_justified(|ui| ui.label("+ 새 셸 로 시작하세요"));
            self.flush_command_repaint(ui.ctx());
            return;
        };

        // (출력/상태 폴링 제거 — 2026-07-04 상시 리페인트 원인 조사)
        // 예전엔 "가시+실행 세션 = 50ms 폴링"으로 출력을 끌어왔다(wake가 Viewport를
        // 깨우지 않던 시절의 안전망) → 가시 idle에서 20fps 리페인트로 CPU ~10%를 상시
        // 소모했다. 이제 worker의 wake가 Viewport(dirty 게이트)·상태 이벤트 모두를
        // 깨우므로 폴링이 불필요하다: 출력/상태가 있을 때만 프레임이 돈다.

        let rect = ui.available_rect_before_wrap();
        let layout = active_tab.layout.clone();
        self.render_node(ui, rect, &layout, &mux, config, client);

        // 응답(MuxUpdated/Viewport)을 다음 프레임에서 수신하도록 보장
        self.flush_command_repaint(ui.ctx());
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui, config: &TerminalConfig, client: &dyn RuntimeClient) {
        let mux = self.mux.clone();
        ui.horizontal(|ui| {
            if let Some(mux) = &mux {
                for tab in &mux.tabs {
                    let active = mux.active_tab.as_ref() == Some(&tab.id);
                    // tab 내 세션들의 감지 상태 요약 — 가장 주의가 필요한 상태 우선 (PR-12)
                    let icon = tab
                        .panes
                        .iter()
                        .filter_map(|pane| pane.session_id)
                        .filter_map(|session| {
                            self.sessions.get(&session).and_then(|view| view.status)
                        })
                        .max_by_key(|status| status.urgency())
                        .map(status_icon)
                        .unwrap_or("");
                    let title = format!("{icon}{}", tab.title);
                    if ui.selectable_label(active, title).clicked() && !active {
                        self.send(
                            client,
                            RuntimeCommand::SelectTab {
                                tab: tab.id.clone(),
                            },
                        );
                    }
                    if ui.small_button("×").clicked() {
                        self.send(
                            client,
                            RuntimeCommand::CloseTab {
                                tab: tab.id.clone(),
                            },
                        );
                    }
                    ui.separator();
                }
            }
            if ui.button("+ 새 셸").clicked() {
                self.send(
                    client,
                    RuntimeCommand::SpawnShell {
                        cols: 80,
                        rows: 24,
                        scrollback_lines: config.scrollback_lines as usize,
                    },
                );
            }
            // focused pane 대상 분할 (새 pane에 새 셸 attach)
            if let Some(focused) = mux.as_ref().and_then(|m| m.focused_pane.clone()) {
                if ui.button("분할│").clicked() {
                    self.send(
                        client,
                        RuntimeCommand::SplitPane {
                            pane: focused.clone(),
                            direction: SplitDirection::Horizontal,
                            scrollback_lines: config.scrollback_lines as usize,
                        },
                    );
                }
                if ui.button("분할─").clicked() {
                    self.send(
                        client,
                        RuntimeCommand::SplitPane {
                            pane: focused.clone(),
                            direction: SplitDirection::Vertical,
                            scrollback_lines: config.scrollback_lines as usize,
                        },
                    );
                }
                if ui.button("pane 닫기").clicked() {
                    self.send(client, RuntimeCommand::ClosePane { pane: focused });
                }
            }
        });
    }

    /// layout 트리를 rect 분할로 재귀 렌더한다.
    #[allow(clippy::too_many_arguments)]
    fn render_node(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        node: &LayoutNode,
        mux: &MuxSnapshot,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
    ) {
        match node {
            LayoutNode::Pane(pane_id) => {
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                // max_rect는 배치만 제한한다 — 이전 크기의 스냅샷이 이웃 pane을
                // 덮어 그리지 않게 페인터 클립도 pane 영역으로 줄인다
                child.set_clip_rect(rect.intersect(ui.clip_rect()));
                self.render_pane(&mut child, pane_id, mux, config, client);
            }
            LayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let gap = 4.0;
                let (first_rect, second_rect) = match direction {
                    SplitDirection::Horizontal => {
                        // 좌/우 분할
                        let split_x = rect.min.x + (rect.width() - gap) * ratio;
                        (
                            egui::Rect::from_min_max(rect.min, egui::pos2(split_x, rect.max.y)),
                            egui::Rect::from_min_max(
                                egui::pos2(split_x + gap, rect.min.y),
                                rect.max,
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
                        )
                    }
                };
                self.render_node(ui, first_rect, first, mux, config, client);
                self.render_node(ui, second_rect, second, mux, config, client);
            }
        }
    }

    fn render_pane(
        &mut self,
        ui: &mut egui::Ui,
        pane_id: &runtime::MuxPaneId,
        mux: &MuxSnapshot,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
    ) {
        let Some(pane) = mux
            .tabs
            .iter()
            .flat_map(|tab| &tab.panes)
            .find(|pane| &pane.id == pane_id)
        else {
            return;
        };
        let focused = mux.focused_pane.as_ref() == Some(pane_id);
        let Some(session) = pane.session_id else {
            ui.label("(세션 없음)");
            return;
        };

        // pane 크기 → cols/rows. visible pane 전부 대상 — split 직후 기존 pane의
        // PTY 크기가 틀어지는 문제 방지 (runtime도 visible 세션을 모두 push한다)
        let cell = renderer_egui::cell_size(ui.ctx(), config.font_size);
        let avail = ui.available_size();
        let cols = ((avail.x / cell.x) as u16).clamp(10, 500);
        let rows = (((avail.y - cell.y) / cell.y) as u16).clamp(3, 200);
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

        let view = self.sessions.entry(session).or_default();
        let exit_code = view.exit_code;
        let bracketed = view.bracketed_paste;
        let Some(snapshot) = view.snapshot.clone() else {
            ui.label("연결 중…");
            return;
        };

        let preedit = (focused && !self.preedit.is_empty()).then_some(self.preedit.as_str());
        let output = renderer_egui::draw(ui, &snapshot, config.font_size, preedit);

        // 포커스 pane 파란 테두리는 사용자 요청으로 제거(2026-07-04) — 단일 pane 사용 시
        // 항상 보여 거슬림. 다중 pane에서 포커스 식별이 다시 필요해지면 "pane 2개 이상일
        // 때만 표시" 조건으로 복원할 것.
        // (egui 포커스는 클릭 시에만 요청한다 — 매 프레임 request_focus는 다른 창의 입력 포커스를 뺏는다.)
        // pending 포커스는 pane이 실제로 그려진 이 시점에 1회 소비한다 —
        // 매 프레임 요청은 다른 창 입력을 뺏고, "연결 중" 단계에서 소비하면
        // 요청이 유실된다 (리뷰 반영).
        if focused && self.pending_focus.as_ref() == Some(pane_id) {
            self.pending_focus = None;
            output.response.request_focus();
        }
        if output.response.clicked() {
            output.response.request_focus();
            if !focused {
                self.send(
                    client,
                    RuntimeCommand::FocusPane {
                        pane: pane_id.clone(),
                    },
                );
            }
        }

        // 입력은 focused pane으로만
        if focused && output.response.has_focus() {
            let mut pending: Vec<u8> = Vec::new();
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
                    if let Some(bytes) = input_mapper::map_event(event, bracketed, &modifiers) {
                        pending.extend(bytes);
                    }
                }
            });
            if !pending.is_empty() {
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
            ui.label(format!(
                "[종료: exit code {}]",
                code.map_or("알 수 없음".into(), |c| c.to_string())
            ));
        }
    }

    /// 명령을 보냈거나 spawn 응답 대기 중이면 repaint를 예약한다 —
    /// 느린 spawn(keyring 등)도 응답 이벤트가 올 때까지 폴링이 끊기지 않는다.
    /// show()의 모든 return 경로에서 호출할 것.
    fn flush_command_repaint(&mut self, ctx: &egui::Context) {
        if self.command_sent || self.pending_spawns > 0 {
            self.command_sent = false;
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// 최신 mux 스냅샷 (알림 센터가 pane 조회·제목에 사용).
    pub fn mux(&self) -> Option<&Arc<MuxSnapshot>> {
        self.mux.as_ref()
    }

    fn session_alive(&self, session: SessionId) -> bool {
        self.mux.as_ref().is_some_and(|mux| {
            mux.tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .any(|pane| pane.session_id == Some(session))
        })
    }

    fn send(&mut self, client: &dyn RuntimeClient, command: RuntimeCommand) {
        let is_spawn = matches!(
            command,
            RuntimeCommand::SpawnShell { .. } | RuntimeCommand::SplitPane { .. }
        );
        if let Err(e) = client.send_command(command) {
            self.error = Some(format!("{e:#}"));
        } else {
            self.command_sent = true;
            if is_spawn {
                self.pending_spawns += 1;
            }
        }
    }
}

/// 상태 → tab 제목 아이콘 (PR-12).
fn status_icon(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Running => "",
        SessionStatus::Waiting => "⏳",
        SessionStatus::NeedsApproval => "✋",
        SessionStatus::Error => "❌",
        SessionStatus::Done => "✅",
    }
}
