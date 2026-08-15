use super::activity::{ActivityWorkspaceRow, ActivityWorkspaceState};
use super::ports::{PortsIntent, PortsUi};
use super::resource_manager::{ResourceManagerIntent, ResourceManagerUi};
use crate::agent_surface::AgentVisualState;
use crate::port_inventory::PortSnapshot;
use crate::status_feed::StatusFeedSnapshot;
use crate::ui::agent_visuals::status_color;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatusBarIntent {
    Resource(ResourceManagerIntent),
    Ports(PortsIntent),
    /// 대기 개수 클릭 — 작업함(이미 있는 대기 카드 목록)을 연다.
    OpenWork,
    /// 상태바 팝오버에서 승인/거부 — 벨 팝오버 카드와 같은 경로로 처리한다.
    Approval(crate::ui::approvals::ApprovalDecision),
    /// 대기 중인 세션 칩 클릭 — 그 세션으로 바로 이동한다. 작업함을 거치지 않는다.
    FocusWaiting(crate::ui::notifications::AgentNotificationTarget),
}

/// 강도·모델 단축키가 실행되지 않은 이유를 하단 상태바에 잠시 보여준다.
///
/// 감지 파이프라인의 단계별 실패를 하나의 "무반응"으로 숨기지 않는다. 예약 성공은
/// 기존 `status_bar.queued_*` 라벨이 지속해서 보여주므로 여기에 중복하지 않는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentShortcutFeedback {
    NoFocusedPane,
    ProcessInfoPending,
    NoAgent,
    SurfacePending,
    CurrentValueUnknown,
    Unsupported,
    TargetUnavailable,
    DeliveryFailed,
    /// 보내긴 했는데 시한 안에 CLI가 그 값을 반영하지 않았다. 조직 한도·런치 핀 등
    /// **에이전트 쪽 거절**일 수 있고, 거절 사유는 CLI가 pane에 찍는다.
    NotConfirmed,
}

impl AgentShortcutFeedback {
    fn message_key(self) -> &'static str {
        match self {
            Self::NoFocusedPane => "status_bar.agent_shortcut.no_focused_pane",
            Self::ProcessInfoPending => "status_bar.agent_shortcut.process_info_pending",
            Self::NoAgent => "status_bar.agent_shortcut.no_agent",
            Self::SurfacePending => "status_bar.agent_shortcut.surface_pending",
            Self::CurrentValueUnknown => "status_bar.agent_shortcut.current_value_unknown",
            Self::Unsupported => "status_bar.agent_shortcut.unsupported",
            Self::TargetUnavailable => "status_bar.agent_shortcut.target_unavailable",
            Self::DeliveryFailed => "status_bar.agent_shortcut.delivery_failed",
            Self::NotConfirmed => "status_bar.agent_shortcut.not_confirmed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentTerminalView {
    Home,
    /// 「작업」 전체 페이지 — 승인·입력 대기(주의 섹션)와 에이전트 세션 그리드를 한 화면에
    /// 모은다. 2026-08-08까지 작업함(Inbox)과 플릿이 별도 페이지였는데 같은 사실을 두 번
    /// 세고 있어 합쳤다(사용자 지적).
    Fleet,
    /// git 패널 행 클릭으로 여는 파일 diff — 터미널 자리를 전면 교체한다
    /// (Home/Fleet과 같은 패턴, 2026-08-15 스펙 §1).
    Diff,
    #[default]
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum AnnouncementFilter {
    #[default]
    All,
    OpenAi,
    Anthropic,
    Grok,
    HuggingFace,
}

const ANNOUNCEMENT_VISIBLE_ROWS: usize = 5;
/// 64px 기존 행에서 정확히 30% 축소.
const ANNOUNCEMENT_ROW_HEIGHT: f32 = 44.8;
const ANNOUNCEMENT_DATE_WIDTH: f32 = 82.0;
const ANNOUNCEMENT_LINK_WIDTH: f32 = 30.0;
const ANNOUNCEMENT_LOGO_SIZE: f32 = 20.0;
const ANNOUNCEMENT_LOGO_BASE_SIZE: f32 = 22.0;
const ANNOUNCEMENT_LOGO_LEFT_GAP: f32 = 2.4;
const ANNOUNCEMENT_TITLE_GAP: f32 = 8.0;
const ANNOUNCEMENT_RIGHT_INSET: f32 = 8.0;
const ANNOUNCEMENT_REFRESH_SIZE: f32 = 24.0;
const ANNOUNCEMENT_TAB_GAP: f32 = 1.0;
const ANNOUNCEMENT_HEADER_BOTTOM_GAP: f32 = 10.0;
const ANNOUNCEMENT_ROWS_TOP_GAP: f32 = 4.8;
const ANNOUNCEMENT_DIVIDER_HEIGHT: f32 = 26.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeAction {
    Connectors,
    /// 「AI 공지」 수동 갱신(⟳) — App이 status_feed 워커를 즉시 깨운다.
    RefreshNotices,
}

pub struct NoticeTranslations<'a> {
    pub cache: &'a crate::notice_translate::TranslationCache,
    pub locale: &'a str,
}

#[derive(Debug, Clone, Copy, Default)]
struct WorkspaceTotals {
    active: usize,
    warm: usize,
    idle: usize,
    sessions: usize,
    warnings: usize,
    cpu_percent: f32,
    cpu_seen: bool,
    /// 앱 프로세스 자체(phys_footprint, pid 중복 제거).
    app_rss_bytes: u64,
    /// 전 워크스페이스 세션 프로세스 트리(셸+에이전트) 합 — 앱 메모리가 아니다.
    /// 합쳐서 "RAM"으로 표시하면 에이전트 몇 개에 수 GB로 보여 앱 메모리 급증으로
    /// 오독된다(2026-07-18 사용자 실측 보고).
    session_rss_bytes: u64,
}

pub struct AgentTerminalUi {
    view: AgentTerminalView,
    announcement_filter: AnnouncementFilter,
    resource_manager: ResourceManagerUi,
    ports: PortsUi,
    /// 상태바 승인 팝오버 열림 상태. 자원·포트 팝오버가 각자 UI 구조체에 두는 것과
    /// 같은 자리다 — 열려 있을 때만 App이 카드 데이터를 조립한다.
    approvals_open: bool,
    /// 단축키 실패 영수증. 화면을 덮는 토스트 대신 이미 항상 보이는 상태바를 쓴다.
    agent_shortcut_feedback: Option<(AgentShortcutFeedback, std::time::Instant)>,
}

/// 승인 팝오버가 그릴 데이터. App이 소유한 것을 빌려온다 — 이 모듈은 App을 모른다.
///
/// 호출측은 **대기 중인 승인이 있을 때만** 조립한다. 팝오버 열림 상태로 가르지
/// 않는 이유는 그 값을 상태바 렌더보다 **앞서** 읽어야 해서다 — 여는 프레임에는
/// 아직 닫힘이라 카드가 빈 채로 그려진다(오늘 `pty_surfaces=0`으로 단축키가 죽은
/// 것과 같은 프레임 순서 결함). 승인이 0건이면 어차피 버튼 자체가 없고, 있을 때의
/// 조립 비용은 워크스페이스 목록과 세션 제목 맵뿐이라 프레임당 감당할 수 있다.
pub(crate) struct StatusBarApprovals<'a> {
    pub pending: &'a [crate::ui::approvals::PendingApprovalItem],
    pub workspace_names: &'a HashMap<String, String>,
    pub session_titles: &'a HashMap<(String, runtime::SessionId), String>,
}

/// 한 줄에 펼 칩 개수. 넘치면 작업함이 받는다 — 상태바가 목록이 되면 안 된다.
const WAITING_CHIP_MAX: usize = 3;
/// 승인은 되돌리기 어려워 더 강한 신호를 준다.
const APPROVAL_COLOR: egui::Color32 = egui::Color32::from_rgb(0xe0, 0x71, 0x4a);
/// 입력 대기는 기다림일 뿐이라 한 단계 낮춘다.
const WAITING_COLOR: egui::Color32 = egui::Color32::from_rgb(0xe7, 0x9a, 0x3b);
/// 예약은 아직 일어나지 않은 일이라 가장 약하게 — 알리되 주장하지 않는다.
const QUEUED_COLOR: egui::Color32 = egui::Color32::from_rgb(0x8a, 0x9b, 0xb0);
const SHORTCUT_FEEDBACK_COLOR: egui::Color32 = egui::Color32::from_rgb(0xe7, 0x9a, 0x3b);
const AGENT_SHORTCUT_FEEDBACK_TTL: std::time::Duration = std::time::Duration::from_secs(4);

/// 상태를 **모양으로도** 구분한다. 색만으로 나누면 색각 차이가 있는 사용자에게는
/// 두 항목이 같은 것으로 읽히고, 26px 줄에서는 색 면적이 작아 누구에게나 약하다.
const APPROVAL_MARK: &str = "◆";
const WAITING_MARK: &str = "◐";

/// 주목 항목용 클릭 가능한 라벨. 눌리는 것은 눌리게 보여야 한다.
///
/// `outlined`는 칩(개별 이동 대상)에만 준다 — 개수는 문이고 칩은 목적지라, 테두리가
/// 그 차이를 만든다.
fn status_attention_button(
    ui: &mut egui::Ui,
    label: &str,
    color: egui::Color32,
    outlined: bool,
) -> egui::Response {
    let button = egui::Button::new(egui::RichText::new(label).color(color).size(11.5)).small();
    let button = if outlined {
        button
            .frame(true)
            .fill(egui::Color32::TRANSPARENT)
            .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.55)))
            .corner_radius(3.0)
    } else {
        button.frame(false)
    };
    let response = ui.add(button);
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

impl AgentTerminalUi {
    pub fn new() -> Self {
        Self {
            view: AgentTerminalView::Terminal,
            announcement_filter: AnnouncementFilter::All,
            resource_manager: ResourceManagerUi::default(),
            ports: PortsUi::default(),
            approvals_open: false,
            agent_shortcut_feedback: None,
        }
    }

    pub(crate) fn show_agent_shortcut_feedback(&mut self, feedback: AgentShortcutFeedback) {
        self.agent_shortcut_feedback = Some((
            feedback,
            std::time::Instant::now() + AGENT_SHORTCUT_FEEDBACK_TTL,
        ));
    }

    pub(crate) fn clear_agent_shortcut_feedback(&mut self) {
        self.agent_shortcut_feedback = None;
    }

    pub(crate) fn agent_shortcut_feedback_ttl() -> std::time::Duration {
        AGENT_SHORTCUT_FEEDBACK_TTL
    }

    fn active_agent_shortcut_feedback(
        &mut self,
        now: std::time::Instant,
    ) -> Option<AgentShortcutFeedback> {
        let active = self
            .agent_shortcut_feedback
            .filter(|(_, expires_at)| *expires_at > now)
            .map(|(feedback, _)| feedback);
        if active.is_none() {
            self.agent_shortcut_feedback = None;
        }
        active
    }

    pub fn view(&self) -> AgentTerminalView {
        self.view
    }

    pub fn set_view(&mut self, view: AgentTerminalView) {
        self.view = view;
    }

    pub fn home(
        &mut self,
        ui: &mut egui::Ui,
        feed: &StatusFeedSnapshot,
        translations: NoticeTranslations<'_>,
        slack: &connector_contract::SlackProjection,
        catalog: &i18n::Catalog,
    ) -> Option<HomeAction> {
        let mut action = None;
        egui::ScrollArea::vertical()
            .id_salt("agent_terminal_home")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::NONE
                    .inner_margin(egui::Margin::same(22))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        // Home V1은 별도 "Home" title/subtitle 없이 실제로 동작하는 두
                        // surface만 표시한다. 목업의 나머지 개념은 roadmap으로 분리한다.
                        if ui.available_width() >= 760.0 {
                            ui.columns(2, |columns| {
                                if self.announcements(
                                    &mut columns[0],
                                    feed,
                                    translations.cache,
                                    translations.locale,
                                    catalog,
                                ) {
                                    action = Some(HomeAction::RefreshNotices);
                                }
                                if connections_panel(&mut columns[1], slack, catalog) {
                                    action = Some(HomeAction::Connectors);
                                }
                            });
                        } else {
                            if self.announcements(
                                ui,
                                feed,
                                translations.cache,
                                translations.locale,
                                catalog,
                            ) {
                                action = Some(HomeAction::RefreshNotices);
                            }
                            ui.add_space(12.0);
                            if connections_panel(ui, slack, catalog) {
                                action = Some(HomeAction::Connectors);
                            }
                        }
                    });
            });
        action
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn status_bar_with_managers(
        &mut self,
        ui: &mut egui::Ui,
        claude_usage: Option<crate::app::ProviderUsage>,
        codex_usage: Option<crate::app::ProviderUsage>,
        codex_meta: Option<crate::ui::agent_sessions::CodexUsageMeta>,
        // `kimi_usage`: 값이 없으면(설치 안 했거나 안 씀) 켜져 있어도 칸 자체를 안
        // 그린다 — Kimi 전용 규칙(2026-08-10)이라 `disabled_agents`와는 별개다.
        kimi_usage: Option<crate::app::ProviderUsage>,
        // `disabled_agents`: 런처 카드 스위치로 끈 에이전트 id 목록. usage 값과는
        // 분리된 신호다 — "켜짐인데 값 없음"(Claude·Codex는 「—」로 자리를 지킨다)과
        // "꺼짐"(셋 다 칸 자체가 없다)을 값 하나로는 구분할 수 없기 때문이다
        // (`crate::app::top_provider_usage`가 이 둘을 여기서 갈라 그린다).
        disabled_agents: &[String],
        rows: &[ActivityWorkspaceRow],
        approvals: usize,
        // `waiting_sessions`: 입력 대기 세션 — (표시 라벨, 이동 대상). 칩으로 직접 노출한다.
        waiting_sessions: &[(String, crate::ui::notifications::AgentNotificationTarget)],
        // `queued`: 턴이 끝나면 보낼 예약. 눌렀는데 화면이 그대로면 "안 먹었다"로 읽힌다.
        queued: &[String],
        // `approval_cards`: 승인 팝오버 내용. 팝오버가 닫혀 있으면 빈 슬라이스가 와서
        // idle 비용이 0이다(벨 팝오버와 같은 규칙).
        approval_cards: StatusBarApprovals<'_>,
        mcp_count: usize,
        ports: Option<&PortSnapshot>,
        unattached_counts: &HashMap<String, u16>,
        active_workspace_id: Option<&str>,
        now_ms: u64,
        catalog: &i18n::Catalog,
    ) -> Option<StatusBarIntent> {
        let shortcut_feedback = self.active_agent_shortcut_feedback(std::time::Instant::now());
        let totals = workspace_totals(rows);
        let cpu_value = if totals.cpu_seen {
            format!("{:.1}%", totals.cpu_percent)
        } else {
            "—".to_owned()
        };
        let cpu = catalog.t("status_bar.cpu", &[("value", &cpu_value)]);
        let mut intent = None;
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 25.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add_space(10.0);
                // 칸이 하나도 없으면(claude·codex·kimi 셋 다 꺼짐) 상자도 이 뒤의
                // 구분선도 그리지 않는다 — 반환값이 그 신호다(app.rs 주석 참고).
                let usage_shown = crate::app::top_provider_usage(
                    ui,
                    claude_usage,
                    codex_usage,
                    codex_meta.as_ref(),
                    kimi_usage,
                    disabled_agents,
                );
                if usage_shown {
                    crate::ui::designall::vertical_separator(ui, 14.0);
                }
                ui.weak(catalog.t(
                    "status_bar.sessions",
                    &[("count", &totals.sessions.to_string())],
                ));
                crate::ui::designall::vertical_separator(ui, 14.0);
                // 등록·활성화된 MCP 서버 수 (2026-07-18 사용자 요청).
                ui.weak(catalog.t("status_bar.mcp", &[("count", &mcp_count.to_string())]))
                    .on_hover_text(catalog.t("status_bar.mcp_hover", &[]));
                // 서비스 상태 점등은 레일 하단 세로 스택으로 이동했다
                // (file_tree::rail_service_status, 2026-08-07 사용자 지시).
                // 주목이 필요한 것만 자리를 차지한다 — 조용할 땐 아무것도 그리지 않는다.
                // 승인과 입력 대기는 성격이 달라 나눈다(승인은 되돌리기 어렵고, 입력은
                // 그냥 기다림이다). 화면에서 보고 합칠지 정한다.
                // 예약은 사용자가 방금 누른 것의 영수증이다 — 대기/승인보다 먼저 보인다.
                if let Some(feedback) = shortcut_feedback {
                    crate::ui::designall::vertical_separator(ui, 14.0);
                    ui.colored_label(
                        SHORTCUT_FEEDBACK_COLOR,
                        egui::RichText::new(catalog.t(feedback.message_key(), &[])).size(11.5),
                    );
                }
                if !queued.is_empty() {
                    crate::ui::designall::vertical_separator(ui, 14.0);
                    for label in queued.iter().take(WAITING_CHIP_MAX) {
                        ui.colored_label(
                            QUEUED_COLOR,
                            egui::RichText::new(label).size(11.5).italics(),
                        );
                    }
                }
                if approvals == 0 {
                    // 마지막 승인을 처리하면 이 블록이 통째로 건너뛰어져 열림 상태가
                    // 그대로 남는다. 그러면 다음 승인이 도착하는 순간 사용자가 누르지도
                    // 않은 팝오버가 저절로 뜬다.
                    self.approvals_open = false;
                }
                if approvals > 0 {
                    crate::ui::designall::vertical_separator(ui, 14.0);
                    let label = format!(
                        "{APPROVAL_MARK} {}",
                        catalog.t("status_bar.approvals", &[("count", &approvals.to_string())])
                    );
                    // 뷰를 바꾸지 않고 **그 자리에서** 처리한다 — 승인하려고 터미널을
                    // 떠나면 "가지 않고 판단"이라는 목적이 사라진다. 자원·포트 팝오버와
                    // 같은 패턴이라 이 줄의 상호작용이 한 가지로 유지된다.
                    let approval_response =
                        status_attention_button(ui, &label, APPROVAL_COLOR, false);
                    // 버튼은 **열기만** 한다. 닫기는 팝오버의 CloseOnClickOutside가
                    // 맡는다 — 둘 다 토글하면 열린 상태에서 버튼을 눌렀을 때 닫힘과
                    // 토글이 같은 프레임에 맞물려 서로 상쇄된다.
                    if approval_response.clicked() {
                        self.approvals_open = true;
                    }
                    let mut open = self.approvals_open;
                    egui::Popup::menu(&approval_response)
                        .open_bool(&mut open)
                        .align(egui::RectAlign::TOP_START)
                        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                        .width(420.0)
                        .show(|ui| {
                            let action = crate::ui::inbox_approvals::render(
                                ui,
                                catalog,
                                approval_cards.pending,
                                approval_cards.workspace_names,
                                approval_cards.session_titles,
                                crate::ui::inbox_approvals::POPUP_MAX_CARDS,
                                deppy_core::time::unix_secs_i64(),
                            );
                            if let Some(decision) = action.decision {
                                intent = Some(StatusBarIntent::Approval(decision));
                            }
                            if let Some(target) = action.goto {
                                intent = Some(StatusBarIntent::FocusWaiting(target));
                            }
                            // 상한을 넘으면 카드가 잘린다 — 나머지로 갈 길이 없으면
                            // 팝오버가 막다른 길이 된다. 링크는 잘렸을 때만 낸다.
                            if approvals > crate::ui::inbox_approvals::POPUP_MAX_CARDS {
                                ui.separator();
                                if ui
                                    .button(catalog.t("status_bar.approvals_open_inbox", &[]))
                                    .on_hover_text(
                                        catalog.t("status_bar.approvals_open_inbox.hint", &[]),
                                    )
                                    .clicked()
                                {
                                    intent = Some(StatusBarIntent::OpenWork);
                                }
                            }
                        });
                    self.approvals_open = open;
                }
                if !waiting_sessions.is_empty() {
                    crate::ui::designall::vertical_separator(ui, 14.0);
                    let label = format!(
                        "{WAITING_MARK} {}",
                        catalog.t(
                            "status_bar.waiting",
                            &[("count", &waiting_sessions.len().to_string())],
                        )
                    );
                    if status_attention_button(ui, &label, WAITING_COLOR, false).clicked() {
                        intent = Some(StatusBarIntent::OpenWork);
                    }
                    // 칩은 "일일이 찾아가지 않기" 위한 직접 이동 대상이다. 한 줄이
                    // 목록을 다 담을 수는 없으므로 몇 개만 펴고 나머지는 작업함이 받는다.
                    for (label, target) in waiting_sessions.iter().take(WAITING_CHIP_MAX) {
                        if status_attention_button(ui, label, WAITING_COLOR, true).clicked() {
                            intent = Some(StatusBarIntent::FocusWaiting(target.clone()));
                        }
                    }
                    let overflow = waiting_sessions.len().saturating_sub(WAITING_CHIP_MAX);
                    if overflow > 0
                        && status_attention_button(ui, &format!("+{overflow}"), WAITING_COLOR, true)
                            .clicked()
                    {
                        intent = Some(StatusBarIntent::OpenWork);
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(10.0);
                    // 사이드바 최대 확장 시 남는 폭이 좁아 두 값 라벨이 좌측 카운터를
                    // 덮는다(codex P2) — 좁으면 세션 합을 hover로 내리고 앱 값만 남긴다.
                    let compact = ui.available_width() < 400.0;
                    let memory = memory_label(
                        catalog,
                        totals.app_rss_bytes,
                        totals.session_rss_bytes,
                        compact,
                    );
                    let resource_label = format!("{cpu} · {memory}");
                    let resource_response = status_action_button(ui, &resource_label)
                        .on_hover_text(catalog.t(
                            "status_bar.memory_hover",
                            &[("sessions", &super::format_bytes(totals.session_rss_bytes))],
                        ));
                    if resource_response.clicked() {
                        self.resource_manager.toggle_open();
                    }
                    let mut resource_open = self.resource_manager.is_open();
                    egui::Popup::menu(&resource_response)
                        .open_bool(&mut resource_open)
                        .align(egui::RectAlign::TOP_END)
                        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                        .width(560.0)
                        .show(|ui| {
                            if let Some(action) = self.resource_manager.contents(
                                ui,
                                rows,
                                unattached_counts,
                                now_ms,
                                catalog,
                            ) {
                                intent = Some(StatusBarIntent::Resource(action));
                            }
                        });
                    self.resource_manager.set_open(resource_open);
                    crate::ui::designall::vertical_separator(ui, 14.0);
                    let port_label = ports
                        .map(|snapshot| {
                            catalog.t(
                                "status_bar.ports",
                                &[("count", &snapshot.rows.len().to_string())],
                            )
                        })
                        .unwrap_or_else(|| catalog.t("status_bar.ports_unknown", &[]));
                    let port_response = status_action_button(ui, &port_label)
                        .on_hover_text(catalog.t("status_bar.ports_hint", &[]));
                    if port_response.clicked() {
                        self.ports.toggle_open();
                        if let Some(action) = self.ports.take_open_refresh() {
                            intent = Some(StatusBarIntent::Ports(action));
                        }
                    }
                    let mut ports_open = self.ports.is_open();
                    egui::Popup::menu(&port_response)
                        .open_bool(&mut ports_open)
                        .align(egui::RectAlign::TOP_END)
                        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                        .width(500.0)
                        .show(|ui| {
                            if let Some(action) =
                                self.ports
                                    .contents(ui, ports, active_workspace_id, now_ms, catalog)
                            {
                                intent = Some(StatusBarIntent::Ports(action));
                            }
                        });
                    self.ports.set_open(ports_open);
                });
            },
        );
        intent
    }

    /// 반환: 수동 갱신(⟳) 클릭 여부.
    fn announcements(
        &mut self,
        ui: &mut egui::Ui,
        feed: &StatusFeedSnapshot,
        translations: &crate::notice_translate::TranslationCache,
        locale: &str,
        catalog: &i18n::Catalog,
    ) -> bool {
        let mut refresh_clicked = false;
        let panel = egui::Frame::NONE
            .fill(ui.visuals().panel_fill)
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .corner_radius(egui::CornerRadius::same(2))
            // 기존 16px 좌우 여백을 절반으로 줄이고 양쪽을 정확히 맞춘다.
            .inner_margin(egui::Margin::symmetric(8, 16));
        panel.show(ui, |ui| {
            ui.set_min_height(250.0);
            ui.horizontal(|ui| {
                // 시안처럼 필터는 하나의 segmented control로 붙이고, 새로고침은
                // 같은 행의 맨 오른쪽에 독립된 정사각 버튼으로 둔다.
                ui.spacing_mut().item_spacing.x = ANNOUNCEMENT_TAB_GAP;
                source_filter(
                    ui,
                    &catalog.t("home.notices.filter_all", &[]),
                    &mut self.announcement_filter,
                    AnnouncementFilter::All,
                );
                source_filter(
                    ui,
                    "OpenAI",
                    &mut self.announcement_filter,
                    AnnouncementFilter::OpenAi,
                );
                source_filter(
                    ui,
                    "Anthropic",
                    &mut self.announcement_filter,
                    AnnouncementFilter::Anthropic,
                );
                source_filter(
                    ui,
                    "Grok",
                    &mut self.announcement_filter,
                    AnnouncementFilter::Grok,
                );
                source_filter(
                    ui,
                    "Hugging Face",
                    &mut self.announcement_filter,
                    AnnouncementFilter::HuggingFace,
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if announcement_refresh_button(
                        ui,
                        &catalog.t("home.notices.refresh_hover", &[]),
                    )
                    .clicked()
                    {
                        refresh_clicked = true;
                    }
                });
            });
            ui.add_space(ANNOUNCEMENT_HEADER_BOTTOM_GAP);
            crate::ui::hairline(ui);
            ui.add_space(ANNOUNCEMENT_ROWS_TOP_GAP);
            // 실제 상태 페이지의 최신 인시던트 5건씩 (2026-07-20 사용자 — 정적 링크
            // 카드에서 교체). 아직 첫 조회 전이면 안내 문구.
            let mut cards: Vec<AnnouncementCard> = [
                (AnnouncementFilter::OpenAi, "OpenAI", feed.openai.as_ref()),
                (
                    AnnouncementFilter::Anthropic,
                    "Claude",
                    feed.claude.as_ref(),
                ),
                (AnnouncementFilter::Grok, "Grok", feed.grok.as_ref()),
                (
                    AnnouncementFilter::HuggingFace,
                    "Hugging Face",
                    feed.hugging_face.as_ref(),
                ),
            ]
            .into_iter()
            .filter(|(provider, ..)| {
                self.announcement_filter == AnnouncementFilter::All
                    || self.announcement_filter == *provider
            })
            .filter_map(|(provider, source, status)| {
                status.map(|status| (provider, source, status))
            })
            .flat_map(|(_, source, status)| {
                status
                    .incidents
                    .iter()
                    .map(move |incident| AnnouncementCard { source, incident })
            })
            .collect();
            if cards.is_empty() {
                ui.weak(
                    if feed.claude.is_none()
                        && feed.openai.is_none()
                        && feed.hugging_face.is_none()
                        && feed.grok.is_none()
                    {
                        catalog.t("home.notices.loading", &[])
                    } else {
                        catalog.t("home.notices.empty", &[])
                    },
                );
            } else {
                // 공급자를 가로질러 최신 날짜순으로 정렬한다. 화면에는 정확히 5행만
                // 보이고 나머지는 이 패널 안에서만 스크롤한다(2026-07-20 사용자).
                cards.sort_by(|left, right| right.incident.date.cmp(&left.incident.date));
                let viewport_height = ANNOUNCEMENT_ROW_HEIGHT * ANNOUNCEMENT_VISIBLE_ROWS as f32;
                ui.scope(|ui| {
                    // show_rows는 전역 item spacing을 행 높이에 더한다. 여기서는 0으로
                    // 고정해 44.8px × 5행 viewport가 정확히 다섯 행과 일치하게 한다.
                    ui.spacing_mut().item_spacing.y = 0.0;
                    egui::ScrollArea::vertical()
                        .id_salt("home-announcement-rows")
                        // scrollbar가 우측 폭을 예약하면 divider의 우측 여백만 커진다.
                        // wheel/drag 스크롤은 유지하면서 숨겨 좌우 8px을 정확히 맞춘다.
                        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                        .max_height(viewport_height)
                        .min_scrolled_height(viewport_height)
                        .auto_shrink([false, false])
                        .show_rows(ui, ANNOUNCEMENT_ROW_HEIGHT, cards.len(), |ui, range| {
                            for card in &cards[range] {
                                announcement_row(ui, card, translations, locale, catalog);
                            }
                        });
                });
            }
        });
        refresh_clicked
    }
}

fn connections_panel(
    ui: &mut egui::Ui,
    slack: &connector_contract::SlackProjection,
    catalog: &i18n::Catalog,
) -> bool {
    let mut manage_clicked = false;
    egui::Frame::NONE
        .fill(ui.visuals().panel_fill)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(egui::CornerRadius::same(2))
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.set_min_height(250.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(catalog.t("home.connections.title", &[]))
                        .strong()
                        .size(17.0),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    manage_clicked = ui
                        .small_button(catalog.t("home.connections.manage", &[]))
                        .clicked();
                });
            });
            ui.add_space(12.0);
            crate::ui::hairline(ui);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                slack_mark(ui);
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("Slack").strong());
                    ui.weak(catalog.t("home.connections.slack_detail", &[]));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (label, color) = match slack.status {
                        connector_contract::SlackStatus::NotConfigured => (
                            catalog.t("home.connections.not_connected", &[]),
                            ui.visuals().weak_text_color(),
                        ),
                        connector_contract::SlackStatus::Ready => (
                            catalog.t("connectors.slack.ready", &[]),
                            status_color(AgentVisualState::Active),
                        ),
                        connector_contract::SlackStatus::Checking => (
                            catalog.t("connectors.checking", &[]),
                            status_color(AgentVisualState::Active),
                        ),
                        connector_contract::SlackStatus::NeedsAuthorization => (
                            catalog.t("connectors.needs_auth", &[]),
                            status_color(AgentVisualState::Waiting),
                        ),
                        connector_contract::SlackStatus::Connected => (
                            catalog.t(
                                "connectors.connected_tools",
                                &[("count", &slack.tool_count.to_string())],
                            ),
                            status_color(AgentVisualState::Complete),
                        ),
                        connector_contract::SlackStatus::Failed => (
                            catalog.t("home.connections.failed", &[]),
                            status_color(AgentVisualState::Error),
                        ),
                    };
                    ui.colored_label(color, label);
                    status_dot(ui, color);
                });
            });
        });
    manage_clicked
}

fn status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn slack_mark(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 3.0, egui::Color32::from_rgb(0xf2, 0xf2, 0xf4));
    let center = rect.center();
    for (offset, color) in [
        (
            egui::vec2(-4.0, -4.0),
            egui::Color32::from_rgb(0x36, 0xc5, 0xf0),
        ),
        (
            egui::vec2(4.0, -4.0),
            egui::Color32::from_rgb(0x2e, 0xb6, 0x7d),
        ),
        (
            egui::vec2(-4.0, 4.0),
            egui::Color32::from_rgb(0xec, 0xb2, 0x2e),
        ),
        (
            egui::vec2(4.0, 4.0),
            egui::Color32::from_rgb(0xe0, 0x1e, 0x5a),
        ),
    ] {
        ui.painter().circle_filled(center + offset, 3.2, color);
    }
}

/// 홈 공지 카드 1장 — 상태 페이지 인시던트 1건.
#[derive(Clone, Copy)]
struct AnnouncementCard<'a> {
    source: &'static str,
    incident: &'a crate::status_feed::IncidentNotice,
}

fn source_filter(
    ui: &mut egui::Ui,
    label: &str,
    selected: &mut AnnouncementFilter,
    value: AnnouncementFilter,
) {
    let is_selected = *selected == value;
    let foreground = if is_selected {
        ui.visuals().hyperlink_color
    } else {
        ui.visuals().text_color()
    };
    let stroke = if is_selected {
        egui::Stroke::new(1.0, ui.visuals().hyperlink_color)
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke
    };
    let button = egui::Button::new(egui::RichText::new(label).color(foreground).size(14.0))
        .min_size(egui::vec2(0.0, 32.0))
        .fill(ui.visuals().extreme_bg_color)
        .stroke(stroke)
        .corner_radius(egui::CornerRadius::same(1));
    if ui.add(button).clicked() {
        *selected = value;
    }
}

fn announcement_refresh_button(ui: &mut egui::Ui, hover_text: &str) -> egui::Response {
    // egui Button은 glyph intrinsic size+padding이 24pt를 넘으면 add_sized에서도 overflow
    // 한다. 정확한 hitbox를 먼저 할당하고 painter로 글리프를 그려 24×24를 보장한다.
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ANNOUNCEMENT_REFRESH_SIZE, ANNOUNCEMENT_REFRESH_SIZE),
        egui::Sense::click(),
    );
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), "⟳"));
    if ui.is_rect_visible(rect) {
        let fill = if response.hovered() {
            ui.visuals().widgets.hovered.weak_bg_fill
        } else {
            ui.visuals().extreme_bg_color
        };
        ui.painter().rect(
            rect,
            3.0,
            fill,
            ui.visuals().widgets.noninteractive.bg_stroke,
            egui::StrokeKind::Inside,
        );
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "⟳",
            egui::FontId::proportional(18.0),
            ui.style().interact(&response).text_color(),
        );
    }
    response.on_hover_text(hover_text)
}

fn announcement_source_label(source: &str) -> &str {
    match source {
        "Claude" => "Anthropic",
        other => other,
    }
}

#[derive(Debug, Clone, Copy)]
struct AnnouncementColumns {
    date: egui::Rect,
    divider_x: f32,
    logo: egui::Rect,
    title: egui::Rect,
    link: egui::Rect,
}

/// 모든 가상화 행이 같은 row 폭에서 정확히 같은 x anchor를 쓰게 열 geometry를 한 곳에서
/// 계산한다. 날짜는 상단 `전체` 탭의 바깥 시작선과 맞추고, 로고는 divider 뒤 기존 18px
/// 여백 2.4pt에서 시작한다.
fn announcement_columns(row_rect: egui::Rect) -> AnnouncementColumns {
    let content_left = row_rect.left();
    let content_right = (row_rect.right() - ANNOUNCEMENT_RIGHT_INSET).max(content_left);
    let content_width = content_right - content_left;
    let date_right = content_left + ANNOUNCEMENT_DATE_WIDTH.min(content_width * 0.24);
    let divider_x = date_right + 4.0;
    let logo_left = divider_x + ANNOUNCEMENT_LOGO_LEFT_GAP;
    let logo = egui::Rect::from_min_size(
        egui::pos2(
            logo_left,
            row_rect.center().y - ANNOUNCEMENT_LOGO_SIZE / 2.0,
        ),
        egui::vec2(ANNOUNCEMENT_LOGO_SIZE, ANNOUNCEMENT_LOGO_SIZE),
    );
    let link = egui::Rect::from_min_max(
        egui::pos2(
            (content_right - ANNOUNCEMENT_LINK_WIDTH).max(logo.right()),
            row_rect.top(),
        ),
        egui::pos2(content_right, row_rect.bottom()),
    );
    let title_left = logo.right() + ANNOUNCEMENT_TITLE_GAP;
    let title = egui::Rect::from_min_max(
        egui::pos2(title_left, row_rect.top()),
        egui::pos2((link.left() - 8.0).max(title_left), row_rect.bottom()),
    );
    let date = egui::Rect::from_min_max(
        row_rect.left_top(),
        egui::pos2(date_right, row_rect.bottom()),
    );
    AnnouncementColumns {
        date,
        divider_x,
        logo,
        title,
        link,
    }
}

/// 외부 이미지 없이 작은 크기에 맞춰 그리는 provider mark. 공급자명은 화면에서 제거하되
/// hover/accessibility에는 남겨 로고만으로 구분하기 어려운 사용자도 확인할 수 있게 한다.
pub(crate) fn paint_announcement_provider_logo(ui: &mut egui::Ui, rect: egui::Rect, source: &str) {
    let label = format!("{} logo", announcement_source_label(source));
    let response = ui
        .interact(
            rect,
            ui.id().with((
                "home-announcement-provider-logo",
                source,
                rect.min.y.to_bits(),
            )),
            egui::Sense::hover(),
        )
        .on_hover_text(announcement_source_label(source));
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Image, ui.is_enabled(), &label)
    });
    let painter = ui.painter();
    let center = rect.center();
    let scale = rect.width().min(rect.height()) / ANNOUNCEMENT_LOGO_BASE_SIZE;
    match source {
        "Claude" => {
            let color = egui::Color32::from_rgb(0xe7, 0x9a, 0x3b);
            let stroke = egui::Stroke::new(1.8 * scale, color);
            for index in 0..8 {
                let angle = index as f32 * std::f32::consts::TAU / 8.0;
                let direction = egui::vec2(angle.cos(), angle.sin());
                painter.line_segment(
                    [
                        center + direction * (2.8 * scale),
                        center + direction * (8.0 * scale),
                    ],
                    stroke,
                );
            }
            painter.circle_filled(center, 2.2 * scale, color);
        }
        // Kimi(Moonshot) — 초승달. 로고 분기가 없으면 기본 아이콘으로 떨어져
        // 다른 provider와 구분되지 않는다.
        "Kimi" => {
            let color = egui::Color32::from_rgb(0x6b, 0x8a, 0xff);
            painter.circle_filled(center, 8.0 * scale, color);
            painter.circle_filled(
                center + egui::vec2(3.4 * scale, -2.2 * scale),
                6.4 * scale,
                ui.visuals().panel_fill,
            );
        }
        "Grok" => {
            let color = egui::Color32::from_rgb(0xa5, 0x70, 0xff);
            let stroke = egui::Stroke::new(2.0, color);
            painter.line_segment(
                [
                    center + egui::vec2(-7.0, 7.0),
                    center + egui::vec2(7.0, -7.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    center + egui::vec2(-5.0, -6.0),
                    center + egui::vec2(5.0, 6.0),
                ],
                stroke,
            );
            painter.circle_stroke(center + egui::vec2(4.0, -4.0), 3.0, stroke);
        }
        "Hugging Face" => {
            let yellow = egui::Color32::from_rgb(0xf4, 0xc4, 0x30);
            let ink = egui::Color32::from_rgb(0x4a, 0x3b, 0x16);
            painter.circle_filled(center, 8.0, yellow);
            painter.circle_filled(center + egui::vec2(-2.8, -1.5), 1.0, ink);
            painter.circle_filled(center + egui::vec2(2.8, -1.5), 1.0, ink);
            let smile = egui::Stroke::new(1.2, ink);
            painter.line_segment(
                [
                    center + egui::vec2(-3.0, 2.5),
                    center + egui::vec2(0.0, 4.0),
                ],
                smile,
            );
            painter.line_segment(
                [center + egui::vec2(0.0, 4.0), center + egui::vec2(3.0, 2.5)],
                smile,
            );
        }
        _ => {
            // OpenAI knot를 작은 크기에서 읽히는 여섯 개의 연결 루프로 단순화한다.
            let color = ui.visuals().hyperlink_color;
            let stroke = egui::Stroke::new(1.35 * scale, color);
            for index in 0..6 {
                let angle = index as f32 * std::f32::consts::TAU / 6.0;
                let loop_center = center + egui::vec2(angle.cos(), angle.sin()) * (4.2 * scale);
                painter.circle_stroke(loop_center, 3.4 * scale, stroke);
            }
            painter.circle_stroke(center, 2.2 * scale, stroke);
        }
    }
}

fn announcement_cell(ui: &mut egui::Ui, rect: egui::Rect, text: egui::RichText) -> egui::Response {
    let mut cell = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    cell.set_clip_rect(rect.intersect(ui.clip_rect()));
    // add_sized는 내부에 centered_and_justified layout을 만들어 실제 글리프를 셀 중앙에
    // 놓는다. 고정된 column 시작선에서 Label 자체를 바로 추가해 날짜·제목의 첫 글자가
    // 모든 행에서 정확히 같은 x에 오게 한다.
    cell.add(egui::Label::new(text).truncate().halign(egui::Align::LEFT))
}

fn paint_external_link_icon(painter: &egui::Painter, center: egui::Pos2, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.5, color);
    let body = egui::Rect::from_center_size(center + egui::vec2(-1.5, 1.5), egui::vec2(11.0, 11.0));
    painter.line_segment([body.left_top(), body.left_bottom()], stroke);
    painter.line_segment([body.left_bottom(), body.right_bottom()], stroke);
    painter.line_segment([body.right_bottom(), body.right_top()], stroke);
    painter.line_segment(
        [
            center + egui::vec2(-0.5, 0.5),
            center + egui::vec2(5.0, -5.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            center + egui::vec2(1.0, -5.0),
            center + egui::vec2(5.0, -5.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            center + egui::vec2(5.0, -5.0),
            center + egui::vec2(5.0, -1.0),
        ],
        stroke,
    );
}

/// 공지 리스트 행 1개 — `날짜 | 공급자 로고 | 제목 | 외부 링크` 구조.
fn announcement_row(
    ui: &mut egui::Ui,
    card: &AnnouncementCard<'_>,
    translations: &crate::notice_translate::TranslationCache,
    locale: &str,
    catalog: &i18n::Catalog,
) {
    // 먼저 viewport 전체 폭을 확정하고 같은 rect에서 셀·구분선·hover를 그린다.
    let (row_rect, row_response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ANNOUNCEMENT_ROW_HEIGHT),
        egui::Sense::hover(),
    );
    if row_response.hovered() {
        ui.painter()
            .rect_filled(row_rect, 0.0, ui.visuals().faint_bg_color);
    }

    let columns = announcement_columns(row_rect);

    announcement_cell(
        ui,
        columns.date,
        egui::RichText::new(&card.incident.date)
            .color(ui.visuals().weak_text_color())
            .size(12.5),
    );
    ui.painter().vline(
        columns.divider_x,
        egui::Rangef::new(
            row_rect.center().y - ANNOUNCEMENT_DIVIDER_HEIGHT / 2.0,
            row_rect.center().y + ANNOUNCEMENT_DIVIDER_HEIGHT / 2.0,
        ),
        ui.visuals().widgets.noninteractive.bg_stroke,
    );
    paint_announcement_provider_logo(ui, columns.logo, card.source);

    let translated = translations.get(card.source, locale, &card.incident.title);
    let title_text = translated.unwrap_or(&card.incident.title);
    let title = announcement_cell(
        ui,
        columns.title,
        egui::RichText::new(title_text).strong().size(14.0),
    );
    if translated.is_some() {
        title.on_hover_text(&card.incident.title);
    } else {
        title.on_hover_text(title_text);
    }
    let link_label = catalog.t("home.notices.original_link", &[]);
    let link = ui
        .interact(
            columns.link,
            ui.id().with(("home-announcement-link", &card.incident.url)),
            egui::Sense::click(),
        )
        .on_hover_text(&link_label);
    link.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Link, ui.is_enabled(), &link_label)
    });
    let link_color = if link.hovered() {
        ui.visuals().text_color()
    } else {
        ui.visuals().hyperlink_color
    };
    paint_external_link_icon(ui.painter(), columns.link.center(), link_color);
    if link.clicked() {
        ui.ctx()
            .open_url(egui::OpenUrl::new_tab(&card.incident.url));
    }

    crate::ui::hairline_at(
        ui.painter(),
        row_rect.x_range(),
        row_rect.bottom(),
        ui.visuals().widgets.noninteractive.bg_stroke.color,
    );
}

/// 상태바 메모리 라벨. 앱/세션 분리 표시 — 합산 단일 "RAM"은 세션 에이전트 몇 개에
/// 수 GB로 보여 앱 메모리 급증으로 오독된다(2026-07-18 사용자 보고). compact(좁은 폭)
/// 에서는 세션 합을 hover로 내리고 앱 값만 남긴다 — 오독 방지가 우선이라 앱 값을 남긴다.
fn memory_label(catalog: &i18n::Catalog, app_rss: u64, session_rss: u64, compact: bool) -> String {
    if compact {
        catalog.t(
            "status_bar.memory_compact",
            &[("app", &super::format_bytes(app_rss))],
        )
    } else {
        catalog.t(
            "status_bar.memory_full",
            &[
                ("app", &super::format_bytes(app_rss)),
                ("sessions", &super::format_bytes(session_rss)),
            ],
        )
    }
}

fn workspace_totals(rows: &[ActivityWorkspaceRow]) -> WorkspaceTotals {
    let mut totals = WorkspaceTotals::default();
    // 앱 스냅샷은 pid별 **최신 샘플**을 고른다 — 워크스페이스 워커마다 2초 주기
    // 샘플 시점이 제각각이라, 먼저 만난 행을 쓰면 다른 표시(구 상단 표시·설정)와
    // 수 MB 어긋났다(2026-07-18 사용자 보고 — 표시 수치 불일치의 원인).
    let mut app_latest: std::collections::HashMap<u32, runtime::ProcessResourceSnapshot> =
        std::collections::HashMap::new();
    for row in rows {
        match row.state {
            ActivityWorkspaceState::Active => totals.active += 1,
            ActivityWorkspaceState::Warm => totals.warm += 1,
            ActivityWorkspaceState::Idle => totals.idle += 1,
        }
        totals.sessions += row.session_count;
        totals.warnings += usize::from(workspace_has_warning(row));
        if let Some(resource) = row.resource {
            app_latest
                .entry(resource.pid)
                .and_modify(|kept| {
                    if resource.sampled_at_ms > kept.sampled_at_ms {
                        *kept = resource;
                    }
                })
                .or_insert(resource);
        }
        for resource in row.session_resources.iter() {
            totals.session_rss_bytes = totals.session_rss_bytes.saturating_add(resource.rss_bytes);
            if let Some(cpu) = resource.cpu_percent {
                totals.cpu_percent += cpu;
                totals.cpu_seen = true;
            }
        }
    }
    for snapshot in app_latest.values() {
        totals.app_rss_bytes = totals.app_rss_bytes.saturating_add(snapshot.rss_bytes);
        if let Some(cpu) = snapshot.cpu_percent {
            totals.cpu_percent += cpu;
            totals.cpu_seen = true;
        }
    }
    totals
}

fn workspace_has_warning(row: &ActivityWorkspaceRow) -> bool {
    row.input_pressure.is_some()
        || row
            .resource
            .is_some_and(|resource| resource.high_cpu || resource.high_rss)
        || row
            .session_resources
            .iter()
            .any(|resource| resource.high_cpu || resource.high_rss)
        || row
            .sessions
            .iter()
            .any(|session| session.pressure.is_some())
}

fn status_action_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let response = ui.add(
        egui::Button::new(egui::RichText::new(label).weak())
            .frame(false)
            .min_size(egui::vec2(0.0, 22.0)),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status_feed::{ProviderStatus, ServiceIndicator};
    use std::sync::Arc;
    use std::sync::Mutex;

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

    #[test]
    fn view_defaults_to_terminal() {
        assert_eq!(AgentTerminalUi::new().view(), AgentTerminalView::Terminal);
    }

    #[test]
    fn empty_totals_are_stable() {
        let totals = workspace_totals(&[]);
        assert_eq!(totals.sessions, 0);
        assert!(!totals.cpu_seen);
    }

    #[test]
    fn totals_split_app_and_session_rss() {
        // 앱(중복 pid 1회)과 세션 프로세스 합을 분리 집계해야 한다 — 합쳐 "RAM"으로
        // 표시하면 에이전트 메모리가 앱 급증으로 오독된다(2026-07-18 사용자 보고).
        let row = |child_session: u64, child_rss: u64| ActivityWorkspaceRow {
            workspace_id: "ws".into(),
            runtime_instance: Some(child_session),
            name: "ws".into(),
            metric_availability: super::super::activity::ActivityMetricAvailability::Local,
            state: ActivityWorkspaceState::Warm,
            session_count: 1,
            pending_events: 0,
            input_pressure: None,
            backgrounded_for_secs: None,
            auto_suspend_remaining_secs: None,
            resource: Some(runtime::ProcessResourceSnapshot {
                pid: 42,
                sampled_at_ms: 0,
                rss_bytes: 100,
                cpu_percent: None,
                high_cpu: false,
                high_rss: false,
            }),
            session_resources: vec![runtime::SessionResourceUsage {
                session: runtime::SessionId(child_session),
                pid: Some(child_session as u32),
                process_group: None,
                identity_source: runtime::ProcessIdentitySource::PortablePty,
                sampled_at_ms: 0,
                process_count: 1,
                rss_bytes: child_rss,
                cpu_percent: None,
                high_cpu: false,
                high_rss: false,
            }]
            .into(),
            sessions: std::sync::Arc::from([]),
        };
        let totals = workspace_totals(&[row(1, 700), row(2, 300)]);
        assert_eq!(totals.app_rss_bytes, 100, "같은 앱 pid는 한 번만");
        assert_eq!(totals.session_rss_bytes, 1000, "세션 프로세스는 각각 합산");
    }

    #[test]
    fn memory_label_은_좁은_폭에서_앱_값만_남긴다() {
        let catalog = i18n::Catalog::load("ko-KR").expect("ko-KR catalog");
        let mib = 1024 * 1024;
        let full = memory_label(&catalog, 150 * mib, 3 * 1024 * mib, false);
        assert!(full.contains("앱") && full.contains("세션"), "{full}");
        // 좁은 폭에서는 좌측 카운터를 덮지 않게 세션 합을 hover로 내린다(codex P2).
        let compact = memory_label(&catalog, 150 * mib, 3 * 1024 * mib, true);
        assert!(
            compact.contains("앱") && !compact.contains("세션"),
            "{compact}"
        );
    }

    #[test]
    fn kittest_하단상태바에는_서비스_점등이_없다_레일로_이동() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, fonts_ready| {
                if !*fonts_ready {
                    return;
                }
                AgentTerminalUi::new().status_bar_with_managers(
                    ui,
                    None,
                    None,
                    None,
                    None,
                    &[],
                    &[],
                    2,
                    &[],
                    &[],
                    StatusBarApprovals {
                        pending: &[],
                        workspace_names: &HashMap::new(),
                        session_titles: &HashMap::new(),
                    },
                    5,
                    None,
                    &HashMap::new(),
                    None,
                    0,
                    &catalog,
                );
            },
            false,
        );
        harness.set_size(egui::vec2(1400.0, 100.0));
        install_sidebar_test_fonts(&harness.ctx);
        *harness.state_mut() = true;
        harness.run();

        harness.get_by_label("Sessions 0");
        harness.get_by_label("MCP 5");
        // 서비스 상태 점등은 레일 하단으로 이동했다 — 상태바에 남아 있으면 회귀다.
        assert!(harness.query_by_label("Claude").is_none());
        assert!(harness.query_by_label("OpenAI").is_none());
        assert!(harness.query_by_label("GitHub").is_none());
        // 승인과 입력 대기는 성격이 달라 따로 센다 — 여기서는 승인 2건만 있고
        // 입력 대기 세션은 없으므로 승인 라벨만 나와야 한다. 표식(◆)이 붙어야
        // 색각 차이와 무관하게 승인/입력이 구분된다.
        harness.get_by_label(format!("{APPROVAL_MARK} Approval 2").as_str());
        assert!(harness.query_by_label("Waiting for input 0").is_none());
        assert!(harness.query_by_label("Terminal").is_none());
        harness.get_by_label("CPU — · App 0 B · Sessions 0 B");
        harness.get_by_label("Ports —");
    }

    /// 표: 켜짐+값 있음 → 값, 켜짐+값 없음 → Claude·Codex는 "—"로 자리 유지(1급
    /// provider 규칙)/Kimi는 칸 없음(기존 규칙, 2026-08-10), 꺼짐 → 셋 다 칸 없음.
    /// 로고는 `paint_announcement_provider_logo`가 "{provider} logo"로 라벨을 다는
    /// `Image` 위젯이라, 칸이 그려졌는지를 클릭 없이도 값으로 확인할 수 있다.
    #[test]
    fn kittest_사용량_바_칸은_켜짐_값없음과_꺼짐을_구분해_그린다() {
        use egui_kittest::kittest::Queryable;

        let some_usage: Option<crate::app::ProviderUsage> = Some((Some(10), Some(20)));

        fn run(
            claude: Option<crate::app::ProviderUsage>,
            codex: Option<crate::app::ProviderUsage>,
            kimi: Option<crate::app::ProviderUsage>,
            disabled: Vec<String>,
        ) -> egui_kittest::Harness<'static, bool> {
            let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
            let mut harness = egui_kittest::Harness::new_ui_state(
                move |ui, fonts_ready| {
                    if !*fonts_ready {
                        return;
                    }
                    AgentTerminalUi::new().status_bar_with_managers(
                        ui,
                        claude,
                        codex,
                        None,
                        kimi,
                        &disabled,
                        &[],
                        0,
                        &[],
                        &[],
                        StatusBarApprovals {
                            pending: &[],
                            workspace_names: &HashMap::new(),
                            session_titles: &HashMap::new(),
                        },
                        0,
                        None,
                        &HashMap::new(),
                        None,
                        0,
                        &catalog,
                    );
                },
                false,
            );
            harness.set_size(egui::vec2(1400.0, 100.0));
            install_sidebar_test_fonts(&harness.ctx);
            *harness.state_mut() = true;
            harness.run();
            harness
        }

        // 켜짐+값 없음(Claude) / 켜짐+값 있음(Codex) / 값 없어서 칸 없음(Kimi, 기존 규칙).
        let harness = run(None, some_usage, None, Vec::new());
        assert!(
            harness.query_by_label("Anthropic logo").is_some(),
            "값이 없어도 켜져 있으면 Claude 칸은 남아야 한다"
        );
        assert!(
            harness.query_by_label("Codex logo").is_some(),
            "값이 있으면 Codex 칸이 그려져야 한다"
        );
        assert!(
            harness.query_by_label("Kimi logo").is_none(),
            "Kimi는 값이 없으면 켜져 있어도 칸을 안 그린다(기존 규칙)"
        );

        // 꺼짐이 "값 있음"보다 우선한다 — Claude를 꺼도 값은 여전히 있다.
        let harness = run(some_usage, some_usage, None, vec!["claude".to_owned()]);
        assert!(
            harness.query_by_label("Anthropic logo").is_none(),
            "꺼진 Claude는 값이 있어도 칸이 사라져야 한다"
        );
        assert!(harness.query_by_label("Codex logo").is_some());

        // 꺼짐이 Kimi의 "값 있으면 보인다" 규칙보다도 우선한다.
        let harness = run(None, some_usage, some_usage, vec!["kimi".to_owned()]);
        assert!(
            harness.query_by_label("Kimi logo").is_none(),
            "꺼진 Kimi는 값이 있어도 칸이 사라져야 한다"
        );

        // 셋 다 꺼지면(재현 시나리오) 로고가 하나도 안 남는다 — 칸이 0개일 때
        // 빈 상자·구분선이 남지 않는지는 app.rs의
        // `칸이_없으면_top_provider_usage는_아무것도_그리지_않았다고_보고한다`가
        // 반환값으로 고정한다.
        let harness = run(
            some_usage,
            some_usage,
            some_usage,
            vec!["claude".to_owned(), "codex".to_owned(), "kimi".to_owned()],
        );
        assert!(harness.query_by_label("Anthropic logo").is_none());
        assert!(harness.query_by_label("Codex logo").is_none());
        assert!(harness.query_by_label("Kimi logo").is_none());
    }

    #[test]
    fn kittest_에이전트_단축키_실패가_상태바에_보인다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let mut terminal = AgentTerminalUi::new();
        terminal.show_agent_shortcut_feedback(AgentShortcutFeedback::NoAgent);
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, fonts_ready| {
                if !*fonts_ready {
                    return;
                }
                terminal.status_bar_with_managers(
                    ui,
                    None,
                    None,
                    None,
                    None,
                    &[],
                    &[],
                    0,
                    &[],
                    &[],
                    StatusBarApprovals {
                        pending: &[],
                        workspace_names: &HashMap::new(),
                        session_titles: &HashMap::new(),
                    },
                    0,
                    None,
                    &HashMap::new(),
                    None,
                    0,
                    &catalog,
                );
            },
            false,
        );
        harness.set_size(egui::vec2(1400.0, 100.0));
        install_sidebar_test_fonts(&harness.ctx);
        *harness.state_mut() = true;
        harness.run();

        harness.get_by_label("이 pane에서 에이전트를 찾지 못했습니다");
    }

    /// 승인은 **그 자리에서** 끝나야 한다. 뷰를 바꾸면 터미널을 떠나게 되어
    /// "가지 않고 판단"이라는 목적이 사라진다 — 개수 클릭이 작업함으로 점프하던
    /// 동작을 팝오버로 바꾼 이유다.
    ///
    /// 실제로 **클릭해서** 확인한다. 클릭 없이 라벨만 보면, 누군가 동작을 다시
    /// `OpenWork`(= 뷰 전환)로 되돌려도 이 테스트가 통과해 버린다.
    #[test]
    fn kittest_승인_개수를_눌러도_뷰가_바뀌지_않는다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let seen: Arc<Mutex<Vec<(bool, AgentTerminalView)>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let terminal = Arc::new(Mutex::new(AgentTerminalUi::new()));
        let shared = Arc::clone(&terminal);

        let mut harness = egui_kittest::Harness::builder().build_ui_state(
            move |ui, fonts_ready| {
                if !*fonts_ready {
                    return;
                }
                let mut terminal = shared.lock().unwrap();
                let intent = terminal.status_bar_with_managers(
                    ui,
                    None,
                    None,
                    None,
                    None,
                    &[],
                    &[],
                    2,
                    &[],
                    &[],
                    StatusBarApprovals {
                        pending: &[],
                        workspace_names: &HashMap::new(),
                        session_titles: &HashMap::new(),
                    },
                    0,
                    None,
                    &HashMap::new(),
                    None,
                    0,
                    &catalog,
                );
                // 뷰 전환 의도(OpenWork)가 나오면 즉시 잡힌다.
                let switched = matches!(intent, Some(StatusBarIntent::OpenWork));
                recorder.lock().unwrap().push((switched, terminal.view()));
            },
            false,
        );
        harness.set_size(egui::vec2(1400.0, 100.0));
        install_sidebar_test_fonts(&harness.ctx);
        *harness.state_mut() = true;
        harness.run();

        let label = format!("{APPROVAL_MARK} 승인 2");
        harness.get_by_label(label.as_str()).click();
        harness.run();
        harness.run();

        let frames = seen.lock().unwrap();
        assert!(
            !frames.iter().any(|(switched, _)| *switched),
            "승인 개수 클릭이 뷰 전환(OpenWork) 의도를 냈다 — 터미널을 떠나면 안 된다"
        );
        assert!(
            frames
                .iter()
                .all(|(_, view)| *view == AgentTerminalView::Terminal),
            "뷰가 터미널에서 벗어났다"
        );
        // 클릭이 팝오버를 실제로 열었는지 — 안 열리면 이 기능이 통째로 죽은 것이다.
        assert!(
            terminal.lock().unwrap().approvals_open,
            "클릭했는데 승인 팝오버가 열리지 않았다"
        );
    }

    /// 상한(5건)을 넘으면 카드가 잘리는데 나머지로 갈 길이 없으면 팝오버가 막다른
    /// 길이 된다. 링크는 **잘렸을 때만** 나와야 한다 — 항상 띄우면 5건 이하에서
    /// 쓸모없는 버튼이 자리를 차지한다.
    #[test]
    fn 승인이_상한을_넘을_때만_작업함_링크가_나온다() {
        use crate::ui::inbox_approvals::POPUP_MAX_CARDS;
        use egui_kittest::kittest::Queryable;

        let label = i18n::Catalog::load("ko-KR")
            .unwrap()
            .t("status_bar.approvals_open_inbox", &[]);

        // (승인 건수, 링크가 보여야 하는가)
        for (approvals, expected) in [(POPUP_MAX_CARDS, false), (POPUP_MAX_CARDS + 1, true)] {
            let catalog = i18n::Catalog::load("ko-KR").unwrap();
            let terminal = Arc::new(Mutex::new(AgentTerminalUi::new()));
            terminal.lock().unwrap().approvals_open = true;
            let shared = Arc::clone(&terminal);

            let mut harness = egui_kittest::Harness::builder().build_ui_state(
                move |ui, fonts_ready| {
                    if !*fonts_ready {
                        return;
                    }
                    shared.lock().unwrap().status_bar_with_managers(
                        ui,
                        None,
                        None,
                        None,
                        None,
                        &[],
                        &[],
                        approvals,
                        &[],
                        &[],
                        StatusBarApprovals {
                            pending: &[],
                            workspace_names: &HashMap::new(),
                            session_titles: &HashMap::new(),
                        },
                        0,
                        None,
                        &HashMap::new(),
                        None,
                        0,
                        &catalog,
                    );
                },
                false,
            );
            harness.set_size(egui::vec2(1400.0, 300.0));
            install_sidebar_test_fonts(&harness.ctx);
            *harness.state_mut() = true;
            harness.run();
            harness.run();

            assert_eq!(
                harness.query_by_label(label.as_str()).is_some(),
                expected,
                "승인 {approvals}건에서 작업함 링크 노출이 기대와 다르다"
            );
        }
    }

    /// 마지막 승인을 처리하면 버튼과 함께 팝오버 블록이 사라진다. 열림 상태를
    /// 그때 정리하지 않으면 **다음 승인이 도착하는 순간 저절로 열린다** — 사용자가
    /// 누르지 않았는데 화면이 튀어나오는 것은 명백한 오작동이다.
    #[test]
    fn 승인이_0이_되면_팝오버_열림_상태가_정리된다() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let terminal = Arc::new(Mutex::new(AgentTerminalUi::new()));
        // 열린 상태를 만들어 둔다(사용자가 눌러서 연 상황).
        terminal.lock().unwrap().approvals_open = true;
        let shared = Arc::clone(&terminal);

        let mut harness = egui_kittest::Harness::builder().build_ui_state(
            move |ui, fonts_ready| {
                if !*fonts_ready {
                    return;
                }
                shared.lock().unwrap().status_bar_with_managers(
                    ui,
                    None,
                    None,
                    None,
                    None,
                    &[],
                    &[],
                    // 승인이 0건 — 마지막 건을 방금 처리한 상황이다.
                    0,
                    &[],
                    &[],
                    StatusBarApprovals {
                        pending: &[],
                        workspace_names: &HashMap::new(),
                        session_titles: &HashMap::new(),
                    },
                    0,
                    None,
                    &HashMap::new(),
                    None,
                    0,
                    &catalog,
                );
            },
            false,
        );
        harness.set_size(egui::vec2(1400.0, 100.0));
        install_sidebar_test_fonts(&harness.ctx);
        *harness.state_mut() = true;
        harness.run();

        assert!(
            !terminal.lock().unwrap().approvals_open,
            "승인이 0건인데 팝오버 열림 상태가 남았다 — 다음 승인에서 저절로 열린다"
        );
    }

    /// 칩은 "일일이 찾아가지 않고 클릭해서 이동"의 실체다. 한 줄이 목록이 되면 안 되므로
    /// `WAITING_CHIP_MAX`까지만 펴고 나머지는 `+N`으로 접어 작업함이 받는다.
    #[test]
    fn kittest_입력대기_칩은_상한까지만_펴고_나머지는_접는다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let waiting: Vec<(String, crate::ui::notifications::AgentNotificationTarget)> = (1..=5)
            .map(|n| {
                (
                    format!("agent-{n}"),
                    crate::ui::notifications::AgentNotificationTarget::Pty {
                        workspace_id: "ws".to_owned(),
                        session: runtime::SessionId(n),
                    },
                )
            })
            .collect();
        let mut harness = egui_kittest::Harness::builder().build_ui_state(
            move |ui, fonts_ready| {
                if !*fonts_ready {
                    return;
                }
                AgentTerminalUi::new().status_bar_with_managers(
                    ui,
                    None,
                    None,
                    None,
                    None,
                    &[],
                    &[],
                    0,
                    &waiting,
                    &[],
                    StatusBarApprovals {
                        pending: &[],
                        workspace_names: &HashMap::new(),
                        session_titles: &HashMap::new(),
                    },
                    0,
                    None,
                    &HashMap::new(),
                    None,
                    0,
                    &catalog,
                );
            },
            false,
        );
        harness.set_size(egui::vec2(1400.0, 100.0));
        install_sidebar_test_fonts(&harness.ctx);
        *harness.state_mut() = true;
        harness.run();

        // 개수는 전부를 센다 — 접힌 것도 대기 중이다. 표식(◐)은 승인(◆)과 모양이
        // 달라야 한다 — 색만으로 나누면 색각 차이가 있으면 같게 읽힌다.
        assert_ne!(WAITING_MARK, APPROVAL_MARK);
        harness.get_by_label(format!("{WAITING_MARK} 입력 대기 5").as_str());
        for n in 1..=WAITING_CHIP_MAX {
            harness.get_by_label(format!("agent-{n}").as_str());
        }
        assert!(
            harness.query_by_label("agent-4").is_none(),
            "상한을 넘은 칩이 펴졌다 — 상태바가 목록이 되면 한 줄 원칙이 깨진다"
        );
        harness.get_by_label("+2");
    }

    #[test]
    fn status_bar_has_resource_and_port_actions_without_terminal_label() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui,
                  (terminal, intents, fonts_ready): &mut (
                AgentTerminalUi,
                Vec<StatusBarIntent>,
                bool,
            )| {
                if !*fonts_ready {
                    return;
                }
                if let Some(intent) = terminal.status_bar_with_managers(
                    ui,
                    None,
                    None,
                    None,
                    None,
                    &[],
                    &[],
                    0,
                    &[],
                    &[],
                    StatusBarApprovals {
                        pending: &[],
                        workspace_names: &HashMap::new(),
                        session_titles: &HashMap::new(),
                    },
                    0,
                    None,
                    &HashMap::new(),
                    None,
                    0,
                    &catalog,
                ) {
                    intents.push(intent);
                }
            },
            (AgentTerminalUi::new(), Vec::new(), false),
        );
        harness.set_size(egui::vec2(1400.0, 100.0));
        install_sidebar_test_fonts(&harness.ctx);
        harness.state_mut().2 = true;
        harness.run();

        assert!(harness.query_by_label("터미널").is_none());
        harness.get_by_label("CPU — · 앱 0 B · 세션 0 B");
        harness.get_by_label("포트 —");
    }

    #[test]
    fn status_bar_port_action_is_the_same_for_pointer_and_keyboard() {
        use egui_kittest::kittest::Queryable;

        fn harness() -> egui_kittest::Harness<'static, (AgentTerminalUi, Vec<StatusBarIntent>, bool)>
        {
            let catalog = i18n::Catalog::load("ko-KR").unwrap();
            let mut harness = egui_kittest::Harness::new_ui_state(
                move |ui,
                      (terminal, intents, fonts_ready): &mut (
                    AgentTerminalUi,
                    Vec<StatusBarIntent>,
                    bool,
                )| {
                    if !*fonts_ready {
                        return;
                    }
                    if let Some(intent) = terminal.status_bar_with_managers(
                        ui,
                        None,
                        None,
                        None,
                        None,
                        &[],
                        &[],
                        0,
                        &[],
                        &[],
                        StatusBarApprovals {
                            pending: &[],
                            workspace_names: &HashMap::new(),
                            session_titles: &HashMap::new(),
                        },
                        0,
                        None,
                        &HashMap::new(),
                        None,
                        0,
                        &catalog,
                    ) {
                        intents.push(intent);
                    }
                },
                (AgentTerminalUi::new(), Vec::new(), false),
            );
            harness.set_size(egui::vec2(1400.0, 100.0));
            install_sidebar_test_fonts(&harness.ctx);
            harness.state_mut().2 = true;
            harness.run();
            harness
        }

        let mut pointer = harness();
        pointer.get_by_label("포트 —").click();
        pointer.run();
        assert_eq!(
            pointer.state().1,
            vec![StatusBarIntent::Ports(PortsIntent::Refresh)]
        );

        let mut keyboard = harness();
        keyboard.get_by_label("포트 —").focus();
        keyboard.key_press(egui::Key::Enter);
        keyboard.run();
        assert_eq!(keyboard.state().1, pointer.state().1);
    }

    #[test]
    fn status_bar_cached_ports_popup_refreshes_on_every_reopen() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load("en-US").unwrap();
        let snapshot = crate::port_inventory::PortSnapshot {
            generation: 7,
            sampled_at_ms: 1_000,
            rows: Arc::from([crate::port_inventory::PortRow {
                pid: 41,
                port: 3000,
                bind: Arc::from("127.0.0.1"),
                protocol: crate::port_inventory::PortProtocol::Tcp,
                process: Arc::from("node"),
                process_started_at: Arc::from("birth"),
                workspace_id: Some(Arc::from("active")),
                workspace_name: Some(Arc::from("Active")),
                ownership: crate::port_inventory::PortOwnership::Workspace,
            }]),
        };
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui,
                  (terminal, intents, fonts_ready): &mut (
                AgentTerminalUi,
                Vec<StatusBarIntent>,
                bool,
            )| {
                if !*fonts_ready {
                    return;
                }
                if let Some(intent) = terminal.status_bar_with_managers(
                    ui,
                    None,
                    None,
                    None,
                    None,
                    &[],
                    &[],
                    0,
                    &[],
                    &[],
                    StatusBarApprovals {
                        pending: &[],
                        workspace_names: &HashMap::new(),
                        session_titles: &HashMap::new(),
                    },
                    0,
                    Some(&snapshot),
                    &HashMap::new(),
                    Some("active"),
                    41_000,
                    &catalog,
                ) {
                    intents.push(intent);
                }
            },
            (AgentTerminalUi::new(), Vec::new(), false),
        );
        harness.set_size(egui::vec2(1400.0, 140.0));
        install_sidebar_test_fonts(&harness.ctx);
        harness.state_mut().2 = true;
        harness.run();

        harness.get_by_label("Ports 1").click();
        harness.run();
        assert_eq!(
            harness.state().1,
            vec![StatusBarIntent::Ports(PortsIntent::Refresh)]
        );
        harness.key_press(egui::Key::Escape);
        harness.run();
        harness.get_by_label("Ports 1").click();
        harness.run();
        assert_eq!(
            harness.state().1,
            vec![
                StatusBarIntent::Ports(PortsIntent::Refresh),
                StatusBarIntent::Ports(PortsIntent::Refresh),
            ]
        );
    }

    #[test]
    fn production_has_only_context_aware_status_bar_entry_point() {
        let source = include_str!("agent_terminal.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!source.contains("pub(crate) fn status_bar("));
        assert!(source.contains("pub(crate) fn status_bar_with_managers("));
    }

    #[test]
    fn unchanged_home_snapshot_renders_300_frames_without_host_commands() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let feed = StatusFeedSnapshot::default();
        let translations = crate::notice_translate::TranslationCache::default();
        let slack = connector_contract::SlackProjection::default();
        let context = egui::Context::default();
        let mut home = AgentTerminalUi::new();

        for _ in 0..300 {
            let output = context.run_ui(egui::RawInput::default(), |ui| {
                assert_eq!(
                    home.home(
                        ui,
                        &feed,
                        NoticeTranslations {
                            cache: &translations,
                            locale: i18n::FALLBACK_LOCALE,
                        },
                        &slack,
                        &catalog,
                    ),
                    None
                );
            });
            assert!(output.platform_output.commands.is_empty());
        }
    }

    #[test]
    fn production_source_has_no_host_io_or_periodic_repaint_edge() {
        let source = include_str!("agent_terminal.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            ["std::", "fs"].concat(),
            ["std::", "process"].concat(),
            ["request_repaint_", "after"].concat(),
            ["req", "west"].concat(),
            ["Tcp", "Stream"].concat(),
            ["Udp", "Socket"].concat(),
            ["clip", "board"].concat(),
            ["r", "fd::"].concat(),
            ["Keyring", "SecretStore"].concat(),
        ] {
            assert!(!source.contains(&forbidden), "forbidden edge: {forbidden}");
        }
    }

    #[test]
    fn kittest_home_v1은_제목없이_외부업데이트와_slack만_렌더한다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let feed = StatusFeedSnapshot::default();
        let translations = crate::notice_translate::TranslationCache::default();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, (home, actions): &mut (AgentTerminalUi, Vec<HomeAction>)| {
                if let Some(action) = home.home(
                    ui,
                    &feed,
                    NoticeTranslations {
                        cache: &translations,
                        locale: i18n::FALLBACK_LOCALE,
                    },
                    &connector_contract::SlackProjection::default(),
                    &catalog,
                ) {
                    actions.push(action);
                }
            },
            (AgentTerminalUi::new(), Vec::new()),
        );
        harness.run();

        assert!(harness.query_by_label("External Updates").is_none());
        harness.get_by_label("My Connections");
        harness.get_by_label("Slack");
        harness.get_by_label("Not connected");
        let all_rect = harness.get_by_label("All").rect();
        let openai_rect = harness.get_by_label("OpenAI").rect();
        let anthropic_rect = harness.get_by_label("Anthropic").rect();
        let grok_rect = harness.get_by_label("Grok").rect();
        let hugging_face = harness.get_by_label("Hugging Face").rect();
        let refresh = harness.get_by_label("⟳").rect();
        assert!(
            all_rect.left() < openai_rect.left()
                && openai_rect.left() < anthropic_rect.left()
                && anthropic_rect.left() < grok_rect.left()
                && grok_rect.left() < hugging_face.left()
        );
        for (left, right) in [
            (all_rect, openai_rect),
            (openai_rect, anthropic_rect),
            (anthropic_rect, grok_rect),
            (grok_rect, hugging_face),
        ] {
            assert!((right.left() - left.right() - ANNOUNCEMENT_TAB_GAP).abs() <= 0.1);
        }
        assert!(
            (refresh.width() - ANNOUNCEMENT_REFRESH_SIZE).abs() <= 0.1,
            "새로고침 폭은 24pt여야 함: {refresh:?}"
        );
        assert!(
            (refresh.height() - ANNOUNCEMENT_REFRESH_SIZE).abs() <= 0.1,
            "새로고침 높이는 24pt여야 함: {refresh:?}"
        );
        assert!(
            hugging_face.right() <= refresh.left(),
            "필터와 우측 새로고침 버튼이 겹치면 안 됨"
        );
        assert!(harness.query_by_label("MLX").is_none());
        assert!(harness.query_by_label("Home").is_none());

        harness.get_by_label("Manage").click();
        harness.run();
        assert_eq!(harness.state().1, vec![HomeAction::Connectors]);
    }

    #[test]
    fn kittest_home_공지는_날짜_공급자_제목_링크를_정확히_5행_보인다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let feed = StatusFeedSnapshot {
            claude: Some(ProviderStatus {
                indicator: ServiceIndicator::Operational,
                description: "Operational".to_owned(),
                incidents: (1..=6)
                    .map(|index| crate::status_feed::IncidentNotice {
                        title: format!("Announcement {index}"),
                        status: "resolved".to_owned(),
                        date: format!("2026-07-{:02}", 21 - index),
                        url: format!("https://status.claude.com/incidents/{index}"),
                    })
                    .collect(),
            }),
            ..StatusFeedSnapshot::default()
        };
        let translations = crate::notice_translate::TranslationCache::default();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, home: &mut AgentTerminalUi| {
                home.home(
                    ui,
                    &feed,
                    NoticeTranslations {
                        cache: &translations,
                        locale: i18n::FALLBACK_LOCALE,
                    },
                    &connector_contract::SlackProjection::default(),
                    &catalog,
                );
            },
            AgentTerminalUi::new(),
        );
        harness.run();

        assert_eq!(ANNOUNCEMENT_VISIBLE_ROWS, 5);
        assert!(harness.query_by_label("Claude").is_none());
        assert_eq!(
            harness.get_all_by_label("Anthropic").count(),
            1,
            "공급자명은 필터에만 남고 행에서는 로고로 대체돼야 함"
        );
        assert!(
            harness.get_all_by_label("Anthropic logo").count() >= ANNOUNCEMENT_VISIBLE_ROWS,
            "보이는 각 행에는 공급자 로고가 있어야 함"
        );
        let all_left = harness.get_by_label("All").rect().left();
        let date_left = harness.get_by_label("2026-07-20").rect().left();
        assert!(
            (date_left - all_left).abs() <= 0.5,
            "날짜 시작선({date_left})은 전체 탭 시작선({all_left})과 같아야 함"
        );
        assert!(
            harness.get_all_by_label("Source →").count() >= ANNOUNCEMENT_VISIBLE_ROWS,
            "각 행에 접근 가능한 외부 링크가 있어야 함"
        );
        let first = harness.get_by_label("Announcement 1").rect();
        let first_top = first.top();
        let title_left = first.left();
        for index in 2..=5 {
            let row = harness
                .get_by_label(&format!("Announcement {index}"))
                .rect();
            assert!(
                (row.left() - title_left).abs() <= 0.5,
                "{index}번째 제목도 첫 행과 같은 x anchor를 써야 함"
            );
        }
        // show_rows는 경계의 다음 행을 접근성 트리에 준비할 수 있다. 실제 위치를 재서
        // 여섯째 행이 정확히 5행 viewport 밖에서 시작하는지 확인한다.
        let sixth = harness.get_by_label("Announcement 6").rect();
        let sixth_top = sixth.top();
        assert!(
            (sixth.left() - title_left).abs() <= 0.5,
            "스크롤 뒤 후속 행도 첫 행과 같은 제목 x anchor를 써야 함"
        );
        let viewport_height = ANNOUNCEMENT_ROW_HEIGHT * ANNOUNCEMENT_VISIBLE_ROWS as f32;
        assert!(sixth_top - first_top >= viewport_height - 0.5);
    }

    #[test]
    fn 공지_열_geometry는_행이_달라도_같고_요청한_여백을_쓴다() {
        let first_row = egui::Rect::from_min_size(
            egui::pos2(20.0, 10.0),
            egui::vec2(900.0, ANNOUNCEMENT_ROW_HEIGHT),
        );
        let first = announcement_columns(first_row);
        let later = announcement_columns(egui::Rect::from_min_size(
            egui::pos2(20.0, 310.0),
            egui::vec2(900.0, ANNOUNCEMENT_ROW_HEIGHT),
        ));

        assert!((ANNOUNCEMENT_ROW_HEIGHT - 64.0 * 0.7).abs() < f32::EPSILON);
        assert_eq!(first.date.left(), 20.0, "날짜 앞쪽 별도 inset 없음");
        assert!((first.logo.left() - first.divider_x - 2.4).abs() < 0.01);
        assert!((first.title.left() - first.logo.right() - 8.0).abs() < 0.01);
        assert!((first_row.right() - first.link.right() - 8.0).abs() < 0.01);
        assert_eq!(ANNOUNCEMENT_DIVIDER_HEIGHT, 26.0);
        assert_eq!(ANNOUNCEMENT_HEADER_BOTTOM_GAP, 10.0);
        assert!((ANNOUNCEMENT_ROWS_TOP_GAP - 8.0 * 0.6).abs() < f32::EPSILON);
        assert_eq!(first.date.left(), later.date.left());
        assert_eq!(first.logo.left(), later.logo.left());
        assert_eq!(first.title.left(), later.title.left());
        assert_eq!(first.link.left(), later.link.left());
    }
}
