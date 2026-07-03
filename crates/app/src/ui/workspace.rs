//! 워크스페이스 뷰 (설계문서 PR-10): tab bar + split pane 렌더.
//! Runtime Boundary(2장) 준수 — 명령 전송/이벤트 수신/스냅샷 렌더만.
//! mux 배치는 MuxUpdated 스냅샷이 유일한 근거, active pane만 live render (14.4).

use std::collections::HashMap;
use std::sync::Arc;

use runtime::{
    LayoutNode, MuxSnapshot, RuntimeClient, RuntimeCommand, RuntimeEvent, SessionId, SpawnKind,
    SplitDirection,
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
                        self.sessions.entry(*session).or_default().exit_code = Some(*exit_code);
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
        let Some(active_tab) = mux
            .active_tab
            .as_ref()
            .and_then(|id| mux.tabs.iter().find(|tab| &tab.id == id))
        else {
            ui.centered_and_justified(|ui| ui.label("+ 새 셸 로 시작하세요"));
            self.flush_command_repaint(ui.ctx());
            return;
        };

        // 실행 중인 세션이 있으면 출력 폴링 유지.
        // 기준은 mux의 pane 목록 — 아직 Viewport 캐시가 없는 신규 세션도 running이다
        let any_running = mux
            .tabs
            .iter()
            .flat_map(|tab| &tab.panes)
            .filter_map(|pane| pane.session_id)
            .any(|session| {
                self.sessions
                    .get(&session)
                    .is_none_or(|view| view.exit_code.is_none())
            });
        if any_running {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }

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
                    if ui.selectable_label(active, &tab.title).clicked() && !active {
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

        // pane 크기 → cols/rows (변화 시에만 Resize — active pane만; 비활성은
        // 마지막 스냅샷 그대로 두고 포커스 시 맞춘다)
        let cell = renderer_egui::cell_size(ui.ctx(), config.font_size);
        let avail = ui.available_size();
        let cols = ((avail.x / cell.x) as u16).clamp(10, 500);
        let rows = (((avail.y - cell.y) / cell.y) as u16).clamp(3, 200);
        if focused && self.sent_sizes.get(&session) != Some(&(cols, rows)) {
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

        // 포커스 표시. egui 포커스는 클릭 시에만 요청한다 —
        // 매 프레임 request_focus는 다른 창(자격증명 등)의 입력 포커스를 뺏는다.
        if focused {
            ui.painter().rect_stroke(
                output.response.rect,
                0.0,
                egui::Stroke::new(1.0, egui::Color32::from_rgb(0x69, 0x9d, 0xe6)),
                egui::StrokeKind::Outside,
            );
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
