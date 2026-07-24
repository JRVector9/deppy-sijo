//! 멀티에이전트 fleet 그리드 (기능1, leaf).
//!
//! [`FleetSession`] 스냅샷을 상태별로 정렬된 카드 그리드로 그린다. 카드를 누르면 해당
//! 세션으로 포커스하는 intent([`FleetAction::Focus`])만 돌려주고, 실제 전환은 App이 기존
//! FocusSession 경로로 수행한다(leaf+intent+host I/O 경계). 상태 색은 앱 공용 팔레트
//! (`agent_visuals::status_color`)를 재사용한다.

use crate::agent_surface::AgentVisualState;
use crate::fleet::{FleetSession, FleetSummary};
use crate::ui::agent_visuals::status_color;

/// fleet 그리드가 App에 돌려주는 액션.
pub enum FleetAction {
    /// 이 세션으로 포커스(그리드 → 터미널 전환). App이 FocusSession 경로로 라우팅한다.
    Focus {
        workspace_id: String,
        tab: runtime::MuxTabId,
        pane: runtime::MuxPaneId,
    },
}

#[derive(Default)]
pub struct FleetUi {}

impl FleetUi {
    /// fleet 페이지를 그린다. 세션이 없으면 안내 문구만 보인다.
    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        sessions: &[FleetSession],
        summary: FleetSummary,
    ) -> Option<FleetAction> {
        let mut action = None;
        egui::Frame::central_panel(ui.style())
            .inner_margin(egui::Margin::symmetric(16, 14))
            .show(ui, |ui| {
                header(ui, summary);
                ui.add_space(12.0);
                if sessions.is_empty() {
                    ui.add_space(48.0);
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new("실행 중인 에이전트 세션이 없습니다.").weak());
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "워크스페이스에서 에이전트를 시작하면 여기에 모입니다.",
                            )
                            .weak()
                            .small(),
                        );
                    });
                    return;
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            for session in sessions {
                                if card(ui, session) {
                                    action = Some(FleetAction::Focus {
                                        workspace_id: session.workspace_id.clone(),
                                        tab: session.tab.clone(),
                                        pane: session.pane.clone(),
                                    });
                                }
                            }
                        });
                    });
            });
        action
    }
}

/// 상단 헤더: 제목 + 총계 + 상태별 칩.
fn header(ui: &mut egui::Ui, summary: FleetSummary) {
    ui.horizontal(|ui| {
        ui.heading("에이전트 Fleet");
        ui.add_space(8.0);
        ui.label(egui::RichText::new(format!("{}개 세션", summary.total)).weak());
    });
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        chip(ui, AgentVisualState::Waiting, "대기", summary.waiting);
        chip(ui, AgentVisualState::Error, "오류", summary.error);
        chip(ui, AgentVisualState::Complete, "완료", summary.done);
        chip(ui, AgentVisualState::Active, "작업 중", summary.working);
        chip(ui, AgentVisualState::Idle, "유휴", summary.idle);
    });
}

/// 상태별 칩 — 색 점 + "라벨 n". 0이면 흐리게(회색) 표시.
fn chip(ui: &mut egui::Ui, state: AgentVisualState, label: &str, count: usize) {
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
fn card(ui: &mut egui::Ui, session: &FleetSession) -> bool {
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
            egui::RichText::new(state_label(session.state))
                .small()
                .color(state_color),
        );
        ui.label(egui::RichText::new("·").small().weak());
        ui.add(
            egui::Label::new(egui::RichText::new(&session.workspace_name).small().weak()).truncate(),
        );
        if !session.active_workspace {
            ui.label(egui::RichText::new("warm").small().weak());
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

/// 상태별 한국어 라벨.
fn state_label(state: AgentVisualState) -> &'static str {
    match state {
        AgentVisualState::Waiting => "대기",
        AgentVisualState::Error => "오류",
        AgentVisualState::Complete => "완료",
        AgentVisualState::Active => "작업 중",
        AgentVisualState::Idle => "유휴",
        AgentVisualState::Off => "off",
    }
}
