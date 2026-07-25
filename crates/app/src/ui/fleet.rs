//! 멀티에이전트 fleet 그리드 (기능1, leaf).
//!
//! [`FleetSession`] 스냅샷을 상태별로 정렬된 카드 그리드로 그린다. 카드를 누르면 해당
//! 세션으로 포커스하는 intent([`FleetAction::Focus`])만 돌려주고, 실제 전환은 App이 기존
//! FocusSession 경로로 수행한다(leaf+intent+host I/O 경계). 상태 색은 앱 공용 팔레트
//! (`agent_visuals::status_color`)를 재사용한다.

use std::collections::{BTreeMap, HashSet};

use crate::agent_surface::AgentVisualState;
use crate::fleet::{FleetSession, FleetSummary, FleetTarget};
use crate::prompt_library::PromptLibrary;
use crate::ui::agent_visuals::status_color;

/// fleet 그리드가 App에 돌려주는 액션.
pub enum FleetAction {
    /// PTY 세션으로 포커스(그리드 → 터미널 전환). App이 FocusSession 경로로 라우팅한다.
    Focus {
        workspace_id: String,
        tab: runtime::MuxTabId,
        pane: runtime::MuxPaneId,
    },
    /// 구조화(App Server) 세션 열기 — App이 에이전트 패널에서 해당 세션을 연다.
    OpenStructured { session_id: String },
    /// 새 에이전트 시작 — App이 에이전트 패널을 연다(fleet에 세션을 추가하는 진입점).
    LaunchAgent,
    /// 저장된 프롬프트를 여러 실행 중 에이전트에 브로드캐스트. App이 각 대상 세션에
    /// 컴포저 Send와 동일 경로로 주입한다(사용자 검토 후 전송이 아니라 즉시 전송이므로
    /// 대상·프롬프트를 사용자가 명시적으로 고른 뒤에만 발행된다).
    Broadcast {
        prompt: String,
        targets: Vec<(String, runtime::SessionId)>,
    },
}

/// 브로드캐스트 패널 상태 — 프롬프트 선택 + 파라미터 + 대상 체크.
#[derive(Default)]
struct BroadcastState {
    prompt_id: Option<String>,
    params: BTreeMap<String, String>,
    /// 체크된 대상 (workspace_id, session).
    targets: HashSet<(String, runtime::SessionId)>,
}

#[derive(Default)]
pub struct FleetUi {
    /// Some이면 브로드캐스트 패널이 열려 있다.
    broadcast: Option<BroadcastState>,
}

impl FleetUi {
    /// fleet 페이지를 그린다. 세션이 없으면 안내 문구만 보인다.
    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        sessions: &[FleetSession],
        summary: FleetSummary,
        catalog: &i18n::Catalog,
        library: &PromptLibrary,
    ) -> Option<FleetAction> {
        let mut action = None;
        egui::Frame::central_panel(ui.style())
            .inner_margin(egui::Margin::symmetric(16, 14))
            .show(ui, |ui| {
                // 브로드캐스트 버튼은 브로드캐스트 가능한 세션(PTY)이 있을 때만 — 구조화만
                // 있는 fleet에서 눌러도 대상이 비는 막다른 버튼이 되지 않게(리뷰 Medium).
                let has_broadcast_target = sessions.iter().any(|s| s.broadcast_key().is_some());
                match header(ui, summary, catalog, has_broadcast_target) {
                    Some(HeaderClick::Launch) => action = Some(FleetAction::LaunchAgent),
                    Some(HeaderClick::Broadcast) => {
                        // 대상 기본값 = 작업 중이 아닌 세션(진행 중 에이전트는 방해하지 않음).
                        self.broadcast = Some(BroadcastState {
                            prompt_id: library.prompts.first().map(|p| p.id.clone()),
                            params: BTreeMap::new(),
                            targets: sessions
                                .iter()
                                .filter(|s| s.state != AgentVisualState::Active)
                                .filter_map(|s| s.broadcast_key())
                                .collect(),
                        });
                    }
                    None => {}
                }
                ui.add_space(12.0);
                if sessions.is_empty() {
                    ui.add_space(48.0);
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new(catalog.t("fleet.empty", &[])).weak());
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(catalog.t("fleet.empty.hint", &[]))
                                .weak()
                                .small(),
                        );
                        ui.add_space(12.0);
                        if ui.button(catalog.t("fleet.new_agent", &[])).clicked() {
                            action = Some(FleetAction::LaunchAgent);
                        }
                    });
                    return;
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            for session in sessions {
                                if card(ui, session, catalog) {
                                    action = Some(match &session.target {
                                        FleetTarget::Pty { tab, pane, .. } => FleetAction::Focus {
                                            workspace_id: session.workspace_id.clone(),
                                            tab: tab.clone(),
                                            pane: pane.clone(),
                                        },
                                        FleetTarget::Structured { session_id } => {
                                            FleetAction::OpenStructured {
                                                session_id: session_id.clone(),
                                            }
                                        }
                                    });
                                }
                            }
                        });
                    });
            });
        // 브로드캐스트 창은 떠 있는 Window라 중앙 패널과 독립적으로 그린다.
        if let Some(sent) = self.broadcast_window(ui.ctx(), sessions, catalog, library) {
            action = Some(sent);
        }
        action
    }

    /// 브로드캐스트 창 — 프롬프트 선택 + 파라미터 + 대상 체크 + 전송. 닫혀 있으면 아무것도
    /// 그리지 않는다. 전송/닫기 시 상태를 제거한다.
    fn broadcast_window(
        &mut self,
        ctx: &egui::Context,
        sessions: &[FleetSession],
        catalog: &i18n::Catalog,
        library: &PromptLibrary,
    ) -> Option<FleetAction> {
        // 닫혀 있으면(Some 아님) 창을 그리지 않는다.
        self.broadcast.as_ref()?;
        let mut action = None;
        let mut open = true;
        egui::Window::new(catalog.t("fleet.broadcast.title", &[]))
            .id(egui::Id::new("fleet_broadcast"))
            .collapsible(false)
            .resizable(true)
            .default_width(520.0)
            .open(&mut open)
            .show(ctx, |ui| {
                action = self.broadcast_body(ui, sessions, catalog, library);
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            open = false;
        }
        // 전송했거나(action Some) 닫으면 패널 상태를 버린다.
        if !open || action.is_some() {
            self.broadcast = None;
        }
        action
    }

    fn broadcast_body(
        &mut self,
        ui: &mut egui::Ui,
        sessions: &[FleetSession],
        catalog: &i18n::Catalog,
        library: &PromptLibrary,
    ) -> Option<FleetAction> {
        let state = self.broadcast.as_mut()?;
        if library.prompts.is_empty() {
            ui.weak(catalog.t("fleet.broadcast.no_prompts", &[]));
            return None;
        }
        // ① 프롬프트 선택.
        let selected_title = state
            .prompt_id
            .as_ref()
            .and_then(|id| library.get(id))
            .map(|p| p.title.clone())
            .unwrap_or_else(|| catalog.t("fleet.broadcast.pick_prompt", &[]));
        egui::ComboBox::from_id_salt("fleet_bc_prompt")
            .selected_text(selected_title)
            .width(300.0)
            .show_ui(ui, |ui| {
                for prompt in &library.prompts {
                    let picked = state.prompt_id.as_deref() == Some(prompt.id.as_str());
                    if ui.selectable_label(picked, &prompt.title).clicked() {
                        state.prompt_id = Some(prompt.id.clone());
                        state.params.clear();
                    }
                }
            });
        // ② 파라미터 + 미리보기.
        let prompt = state.prompt_id.as_ref().and_then(|id| library.get(id));
        let ready_prompt = if let Some(prompt) = prompt {
            let names = prompt.params();
            if !names.is_empty() {
                egui::Grid::new("fleet_bc_params").num_columns(2).show(ui, |ui| {
                    for name in &names {
                        ui.monospace(format!("{{{{{name}}}}}"));
                        ui.text_edit_singleline(state.params.entry(name.clone()).or_default());
                        ui.end_row();
                    }
                });
            }
            let rendered = crate::prompt_library::render(&prompt.body, &state.params);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.add(egui::Label::new(egui::RichText::new(&rendered).monospace()).wrap());
            });
            let filled = names
                .iter()
                .all(|n| state.params.get(n).is_some_and(|v| !v.trim().is_empty()));
            filled.then_some(rendered)
        } else {
            None
        };
        ui.add_space(6.0);
        ui.separator();
        // ③ 대상 체크박스 — PTY 세션만(구조화는 steer 경로라 브로드캐스트 대상 아님).
        ui.label(catalog.t("fleet.broadcast.targets", &[]));
        egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
            for session in sessions {
                let Some(key) = session.broadcast_key() else {
                    continue;
                };
                let mut checked = state.targets.contains(&key);
                let label = format!(
                    "{} · {}  ({})",
                    session.title,
                    session.workspace_name,
                    state_label(session.state, catalog)
                );
                if ui.checkbox(&mut checked, label).changed() {
                    if checked {
                        state.targets.insert(key);
                    } else {
                        state.targets.remove(&key);
                    }
                }
            }
        });
        // ④ 전송 — 현재 PTY 세션과 교집합만 보낸다. 패널 연 뒤 종료된 stale 대상을 제외해
        // 카운트가 실제 전송 수와 일치하게 한다(세션 순회 순서라 결정적).
        ui.add_space(6.0);
        let effective_targets: Vec<(String, runtime::SessionId)> = sessions
            .iter()
            .filter_map(|s| s.broadcast_key())
            .filter(|key| state.targets.contains(key))
            .collect();
        let count = effective_targets.len();
        let can_send = ready_prompt.is_some() && count > 0;
        let mut out = None;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    can_send,
                    egui::Button::new(
                        catalog.t("fleet.broadcast.send", &[("count", &count.to_string())]),
                    ),
                )
                .clicked()
                && let Some(prompt_text) = ready_prompt
            {
                out = Some(FleetAction::Broadcast {
                    prompt: prompt_text,
                    targets: effective_targets,
                });
            }
            if !can_send {
                ui.weak(catalog.t("fleet.broadcast.fill", &[]));
            }
        });
        out
    }
}

/// 헤더 우측 버튼 클릭.
enum HeaderClick {
    Launch,
    Broadcast,
}

/// 상단 헤더: 제목 + 총계 + (우측) 브로드캐스트·새 에이전트 버튼 + 상태별 칩.
fn header(
    ui: &mut egui::Ui,
    summary: FleetSummary,
    catalog: &i18n::Catalog,
    has_broadcast_target: bool,
) -> Option<HeaderClick> {
    let mut click = None;
    ui.horizontal(|ui| {
        ui.heading(catalog.t("fleet.title", &[]));
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                catalog.t("fleet.session_count", &[("count", &summary.total.to_string())]),
            )
            .weak(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(catalog.t("fleet.new_agent", &[])).clicked() {
                click = Some(HeaderClick::Launch);
            }
            // 브로드캐스트는 실행 중 에이전트가 있을 때만.
            if has_broadcast_target && ui.button(catalog.t("fleet.broadcast", &[])).clicked() {
                click = Some(HeaderClick::Broadcast);
            }
        });
    });
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        chip(ui, AgentVisualState::Waiting, summary.waiting, catalog);
        chip(ui, AgentVisualState::Error, summary.error, catalog);
        chip(ui, AgentVisualState::Complete, summary.done, catalog);
        chip(ui, AgentVisualState::Active, summary.working, catalog);
        chip(ui, AgentVisualState::Idle, summary.idle, catalog);
    });
    click
}

/// 상태별 칩 — 색 점 + "라벨 n". 0이면 흐리게(회색) 표시.
fn chip(ui: &mut egui::Ui, state: AgentVisualState, count: usize, catalog: &i18n::Catalog) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(88.0, 22.0), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let dim = count == 0;
    let dot_color = if dim {
        ui.visuals().weak_text_color()
    } else {
        status_color(state)
    };
    let text_color = if dim {
        ui.visuals().weak_text_color()
    } else {
        ui.visuals().text_color()
    };
    let label = state_label(state, catalog);
    let p = ui.painter();
    p.circle_filled(egui::pos2(rect.left() + 6.0, rect.center().y), 4.0, dot_color);
    p.text(
        egui::pos2(rect.left() + 16.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        format!("{label} {count}"),
        egui::FontId::proportional(12.5),
        text_color,
    );
}

/// 세션 카드 하나 — 좌측 상태 바 + 제목/상태/워크스페이스/보조 줄. 클릭 시 true.
fn card(ui: &mut egui::Ui, session: &FleetSession, catalog: &i18n::Catalog) -> bool {
    let size = egui::vec2(252.0, 96.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return response.clicked();
    }
    let visuals = ui.visuals();
    let bg = if response.hovered() {
        visuals.widgets.hovered.bg_fill
    } else {
        visuals.faint_bg_color
    };
    let border = visuals.widgets.noninteractive.bg_stroke.color;
    let state_color = status_color(session.state);
    {
        let p = ui.painter();
        p.rect_filled(rect, 6.0, bg);
        p.rect_stroke(
            rect,
            6.0,
            egui::Stroke::new(1.0, border),
            egui::StrokeKind::Inside,
        );
        // 좌측 상태 바.
        let bar = egui::Rect::from_min_size(rect.left_top(), egui::vec2(4.0, rect.height()));
        p.rect_filled(bar, 6.0, state_color);
    }

    // 내용은 child UI(top-down)로 — 라벨 truncate가 카드 폭을 넘지 않게 클립한다.
    let inner = rect.shrink2(egui::vec2(14.0, 10.0));
    let mut content = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    content.set_clip_rect(inner.intersect(ui.clip_rect()));
    content.spacing_mut().item_spacing.y = 3.0;
    // 1행: 제목.
    content.add(egui::Label::new(egui::RichText::new(&session.title).strong()).truncate());
    // 2행: 상태 라벨(색) + 워크스페이스 + active/warm.
    content.horizontal(|ui| {
        ui.label(
            egui::RichText::new(state_label(session.state, catalog))
                .small()
                .color(state_color),
        );
        ui.label(egui::RichText::new("·").small().weak());
        ui.add(
            egui::Label::new(egui::RichText::new(&session.workspace_name).small().weak()).truncate(),
        );
        if !session.active_workspace {
            ui.label(egui::RichText::new(catalog.t("fleet.warm", &[])).small().weak());
        }
    });
    // 3행: 대기 사유 우선, 없으면 에이전트 라인("Codex · gpt-5.5 · xhigh").
    if let Some(message) = &session.waiting_message {
        content.add(
            egui::Label::new(egui::RichText::new(message).small().color(state_color)).truncate(),
        );
    } else if let Some(line) = &session.agent_line {
        content.add(
            egui::Label::new(egui::RichText::new(line).small().weak().monospace()).truncate(),
        );
    }

    response.clicked()
}

/// 상태별 라벨(i18n).
fn state_label(state: AgentVisualState, catalog: &i18n::Catalog) -> String {
    let key = match state {
        AgentVisualState::Waiting => "fleet.state.waiting",
        AgentVisualState::Error => "fleet.state.error",
        AgentVisualState::Complete => "fleet.state.done",
        AgentVisualState::Active => "fleet.state.working",
        AgentVisualState::Idle => "fleet.state.idle",
        AgentVisualState::Off => "fleet.state.off",
    };
    catalog.t(key, &[])
}
