//! 내부 알림 센터 (설계문서 PR-13). SessionStatusChanged를 받아
//! waiting/needs-approval/error/done을 알림으로 만들고, native 알림 intent를 반환한다.
//! 항목 클릭 시 해당 세션의 pane으로 focus 이동 (완료 기준: session focus).

use std::collections::VecDeque;
use std::fmt;

use runtime::{SessionId, SessionStatus};

use crate::agent_session::AgentSessionStatus;
use crate::agent_surface::AgentProvider;

const MAX_NOTIFICATION_HISTORY: usize = 100;
const MAX_NATIVE_INTENTS: usize = 8;
const MAX_WORKSPACE_ID_BYTES: usize = 1_024;
const MAX_STRUCTURED_SESSION_ID_BYTES: usize = 1_024;
const MAX_NATIVE_BODY_BYTES: usize = 2 * 1_024;
const MAX_NATIVE_SUMMARY_BYTES: usize = 4 * 1_024;
const MAX_NATIVE_PAYLOAD_BYTES: usize = 5 * 1_024;

/// One immutable, bounded request for the composition root to deliver through
/// the operating-system notification API. The leaf never executes that effect.
/// The payload is intentionally non-`Clone` and its `Debug` output is redacted.
pub struct NativeNotificationIntent {
    summary: Box<str>,
    body: Box<str>,
}

impl NativeNotificationIntent {
    fn new(summary: String, body: &str) -> Option<Self> {
        if !bounded_text(&summary, MAX_NATIVE_SUMMARY_BYTES)
            || !bounded_text(body, MAX_NATIVE_BODY_BYTES)
            || summary.len().saturating_add(body.len()) > MAX_NATIVE_PAYLOAD_BYTES
        {
            return None;
        }
        Some(Self {
            summary: summary.into(),
            body: body.into(),
        })
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn body(&self) -> &str {
        &self.body
    }
}

impl fmt::Debug for NativeNotificationIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeNotificationIntent")
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

/// Focus target carried by an item in the existing Settings > Notifications
/// area. PTY runtime IDs are namespaced by workspace; structured session IDs
/// are application-owned and likewise retain their workspace for pruning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentNotificationTarget {
    Pty {
        workspace_id: String,
        session: SessionId,
    },
    Structured {
        workspace_id: String,
        session_id: String,
    },
}

impl AgentNotificationTarget {
    fn workspace_id(&self) -> &str {
        match self {
            Self::Pty { workspace_id, .. } | Self::Structured { workspace_id, .. } => workspace_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentNotificationSource {
    Pty(Option<AgentProvider>),
    App(AgentProvider),
}

impl AgentNotificationSource {
    fn badge(self) -> &'static str {
        match self {
            Self::Pty(Some(AgentProvider::Codex)) => "[Codex PTY]",
            Self::Pty(Some(AgentProvider::Claude)) => "[Claude PTY]",
            Self::Pty(Some(AgentProvider::Kimi)) => "[Kimi PTY]",
            Self::Pty(None) => "[PTY]",
            Self::App(AgentProvider::Codex) => "[Codex APP]",
            // There is no Claude structured transport today, but preserving the
            // provider in the source keeps this view model transport-neutral.
            Self::App(AgentProvider::Claude) => "[Claude APP]",
            Self::App(AgentProvider::Kimi) => "[Kimi APP]",
        }
    }
}

pub struct NotificationsUi {
    items: Vec<NotificationItem>,
    native_intents: VecDeque<NativeNotificationIntent>,
}

struct NotificationItem {
    target: AgentNotificationTarget,
    source: AgentNotificationSource,
    status: SessionStatus,
    title: String,
    message_id: String,
    read: bool,
    /// 생성 시각(unix 초) — 작업함 전체 페이지의 「N분 전」 표시용 (2026-07-18).
    /// 알림이 만들어진 시각. 2026-08-08 작업함 페이지를 「작업」에 합치면서 이 값을
    /// 읽던 history_section이 사라졌다 — 벨/설정 목록은 시각을 안 쓴다. 값 자체는 알림의
    /// 실제 속성이고 relative_time_label과 함께 세션 목록의 「끝남」 표기에 쓸 예정이라 둔다.
    #[allow(dead_code)]
    created_at_secs: i64,
}

impl NotificationsUi {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            native_intents: VecDeque::with_capacity(MAX_NATIVE_INTENTS),
        }
    }

    /// Returns one pending native notification for `App::logic` to execute.
    /// The fixed-capacity queue keeps the latest burst and never creates a
    /// worker, channel, timer, or polling lifecycle.
    pub fn pop_native_intent(&mut self) -> Option<NativeNotificationIntent> {
        self.native_intents.pop_front()
    }

    /// 표시 시 모든 항목을 읽음 처리한다. 읽지 않은 게 있었으면 true (repaint 필요).
    pub fn mark_all_read(&mut self) -> bool {
        let had_unread = self.items.iter().any(|item| !item.read);
        for item in &mut self.items {
            item.read = true;
        }
        had_unread
    }

    /// 안 읽은 항목 수 — 항목별 read 플래그에서 파생 (pruning에 자동 정합).
    pub fn unread(&self) -> usize {
        self.items.iter().filter(|item| !item.read).count()
    }

    /// 상태 변경을 알림으로 만든다. Running 복귀는 알리지 않는다.
    /// `title`은 tab/세션 제목 (mux 스냅샷에서 조회).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn on_status(
        &mut self,
        workspace_id: &str,
        session: SessionId,
        status: SessionStatus,
        title: &str,
        catalog: &i18n::Catalog,
    ) {
        self.on_pty_status(workspace_id, session, status, title, None, catalog);
    }

    /// PTY status with an optional detected provider. Existing runtime-only
    /// callers can keep using `on_status`; agent-aware callers get a precise
    /// `[Codex PTY]`/`[Claude PTY]` source badge.
    pub fn on_pty_status(
        &mut self,
        workspace_id: &str,
        session: SessionId,
        status: SessionStatus,
        title: &str,
        provider: Option<AgentProvider>,
        catalog: &i18n::Catalog,
    ) {
        if !bounded_text(workspace_id, MAX_WORKSPACE_ID_BYTES) {
            return;
        }
        let target = AgentNotificationTarget::Pty {
            workspace_id: workspace_id.to_owned(),
            session,
        };
        self.push_status(
            target,
            AgentNotificationSource::Pty(provider),
            status,
            title,
            catalog,
        );
    }

    /// Structured App Server lifecycle projected into the same notification
    /// semantics as PTY sessions. Active/idle/off transitions do not notify.
    pub fn on_structured_status(
        &mut self,
        workspace_id: &str,
        session_id: &str,
        status: AgentSessionStatus,
        title: &str,
        catalog: &i18n::Catalog,
    ) {
        if !bounded_text(workspace_id, MAX_WORKSPACE_ID_BYTES)
            || !bounded_text(session_id, MAX_STRUCTURED_SESSION_ID_BYTES)
        {
            return;
        }
        let status = match status {
            AgentSessionStatus::AwaitingApproval => SessionStatus::NeedsApproval,
            AgentSessionStatus::Completed => SessionStatus::Done,
            AgentSessionStatus::Failed => SessionStatus::Error,
            AgentSessionStatus::Starting
            | AgentSessionStatus::Ready
            | AgentSessionStatus::Running
            | AgentSessionStatus::Interrupted
            | AgentSessionStatus::Stopped => return,
        };
        self.push_status(
            AgentNotificationTarget::Structured {
                workspace_id: workspace_id.to_owned(),
                session_id: session_id.to_owned(),
            },
            AgentNotificationSource::App(AgentProvider::Codex),
            status,
            title,
            catalog,
        );
    }

    /// MCP-proxy 승인 대기 알림 (v3.9 리뷰 P2). 승인 행은 도구 호출이 proxy에서 블록되는
    /// 방식이라 PTY 화면에 아무 프롬프트도 출력되지 않는다 — 상태 감지기(BUILTIN.approval
    /// regex)가 볼 게 없어, pending 행 발견 시점에 직접 알린다(OS 알림 + 목록 항목·뱃지).
    /// `title`은 도구 이름 — 세션 제목보다 "무엇을 승인하나"가 판단에 유용하다.
    ///
    /// 같은 세션의 연속 승인도 **매번** 알린다(dedupe=false) — 상태 전이 알림과 달리
    /// 승인 행은 각각이 개별 사건이고, 에이전트는 도구 승인을 연달아 요청한다. 상태
    /// 기반 중복 억제를 그대로 걸면 첫 건만 알리고 나머지는 조용해진다(2026-07-17 실측:
    /// 같은 세션 2번째 승인에서 native 알림 미발화 확인). 스팸 방지는 호출측
    /// (`App::approval_notified`)이 승인 id 단위로 이미 한다.
    pub fn on_mcp_approval(
        &mut self,
        workspace_id: &str,
        session: SessionId,
        tool_name: &str,
        catalog: &i18n::Catalog,
    ) {
        if !bounded_text(workspace_id, MAX_WORKSPACE_ID_BYTES) {
            return;
        }
        self.push_status_with_dedupe(
            AgentNotificationTarget::Pty {
                workspace_id: workspace_id.to_owned(),
                session,
            },
            AgentNotificationSource::Pty(None),
            SessionStatus::NeedsApproval,
            tool_name,
            catalog,
            false,
        );
    }

    /// 완료/실패 알림의 최후 그물(겹 ④, `agent_detect::agent_vanished_sessions`가 고른
    /// 대상). exit sentinel도 화면 regex도 결과를 못 낸 상태에서 에이전트 프로세스
    /// 자체가 사라졌을 때만 호출된다 — 완료인지 실패인지 모르므로 **Done/Error로
    /// 만들지 않는다**. `SessionStatus`를 쓰지 않아 history 목록·아이콘·색·`retain_*`
    /// 정리 로직과 무관하다(그 전부가 "결과가 확정됐다"는 전제로 짜여 있다) — OS 알림
    /// 한 번만 낸다. 뒤이어 진짜 결과(exit sentinel이 늦게 쓰였거나 사용자가 폴백
    /// 셸에서 직접 exit)가 오면 그건 각자 경로(`on_pty_status`/`on_pty_exit`)가 정상
    /// 발화한다.
    pub fn on_agent_vanished(&mut self, title: &str, catalog: &i18n::Catalog) {
        if !bounded_text(title, MAX_NATIVE_BODY_BYTES) {
            return;
        }
        let rendered = catalog.t("notification.session.agent_vanished", &[("title", title)]);
        let Some(intent) = NativeNotificationIntent::new(rendered, title) else {
            return;
        };
        if self.native_intents.len() == MAX_NATIVE_INTENTS {
            self.native_intents.pop_front();
        }
        self.native_intents.push_back(intent);
    }

    /// 상태 전이 알림 — 같은 대상의 같은 상태가 반복되면 억제한다(재연결/resume 시
    /// 같은 이벤트가 다시 오는 게 흔하다).
    fn push_status(
        &mut self,
        target: AgentNotificationTarget,
        source: AgentNotificationSource,
        status: SessionStatus,
        title: &str,
        catalog: &i18n::Catalog,
    ) {
        self.push_status_with_dedupe(target, source, status, title, catalog, true);
    }

    fn push_status_with_dedupe(
        &mut self,
        target: AgentNotificationTarget,
        source: AgentNotificationSource,
        status: SessionStatus,
        title: &str,
        catalog: &i18n::Catalog,
        dedupe: bool,
    ) {
        let Some(message_id) = notification_message_id(status) else {
            // 진행 재개는 알림 아님
            return;
        };
        if !bounded_text(title, MAX_NATIVE_BODY_BYTES) || !bounded_target(&target) {
            return;
        }
        let rendered = catalog.t(message_id, &[("title", title)]);
        match status {
            SessionStatus::Running => return, // 진행 재개는 알림 아님
            SessionStatus::Idle => return,    // 쉬는 중 — 알림 아님
            SessionStatus::Waiting
            | SessionStatus::NeedsApproval
            | SessionStatus::Error
            | SessionStatus::Done => {}
        };
        // Duplicate provider events are common around reconnect/resume. Only a
        // state transition for the same logical target creates a new item.
        let duplicate = dedupe
            && self
                .items
                .iter()
                .rev()
                .find(|item| item.target == target)
                .is_some_and(|item| item.status == status);
        if duplicate {
            return;
        }
        let Some(native_intent) = NativeNotificationIntent::new(rendered, title) else {
            return;
        };
        if self.native_intents.len() == MAX_NATIVE_INTENTS {
            self.native_intents.pop_front();
        }
        self.native_intents.push_back(native_intent);
        self.items.push(NotificationItem {
            target,
            source,
            status,
            title: title.to_owned(),
            message_id: message_id.to_owned(),
            // 생성 시엔 항상 안 읽음. on_status는 창이 숨겨져(minimized/occluded) ui()가
            // 스킵돼도 logic()에서 호출되므로, 여기서 self.open으로 읽음 처리하면 사용자가
            // 보지 못한 background 알림이 읽음이 돼 unread 신호를 잃는다. 실제 읽음은
            // show()(=가시일 때만 호출)가 처리한다.
            read: false,
            created_at_secs: deppy_core::time::unix_secs_i64(),
        });
        // 최근 100개만 유지
        if self.items.len() > MAX_NOTIFICATION_HISTORY {
            let cut = self.items.len() - MAX_NOTIFICATION_HISTORY;
            self.items.drain(..cut);
        }
    }

    /// 세션 종료 알림 (완료 기준: done/error). exit code로 Done/Error 결정.
    /// status detector가 방금(직전 항목으로) 같은 결과를 냈으면 중복 발화하지 않되,
    /// 그 사이에 다른 항목(Waiting/재개 등)이 끼었으면 exit은 새 알림으로 낸다.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn on_exit(
        &mut self,
        workspace_id: &str,
        session: SessionId,
        exit_code: Option<u32>,
        title: &str,
        catalog: &i18n::Catalog,
    ) {
        self.on_pty_exit(workspace_id, session, exit_code, title, None, catalog);
    }

    pub fn on_pty_exit(
        &mut self,
        workspace_id: &str,
        session: SessionId,
        exit_code: Option<u32>,
        title: &str,
        provider: Option<AgentProvider>,
        catalog: &i18n::Catalog,
    ) {
        if !bounded_text(workspace_id, MAX_WORKSPACE_ID_BYTES)
            || !bounded_text(title, MAX_NATIVE_BODY_BYTES)
        {
            return;
        }
        let status = if exit_code == Some(0) {
            SessionStatus::Done
        } else {
            SessionStatus::Error
        };
        // 이 세션의 "마지막 항목"이 같은 결과 상태일 때만 중복으로 본다 —
        // 다른 세션의 알림이 사이에 끼어도 판정이 흔들리지 않게 세션 기준으로 찾는다.
        // (Running은 알림 항목이 아니므로 애초에 items에 없다 — 중간 재개는 다른
        //  상태 항목으로 남고, 그 뒤 exit은 정상적으로 새 알림이 된다)
        let dup = self
            .items
            .iter()
            .rev()
            .find(|item| {
                item.target
                    == (AgentNotificationTarget::Pty {
                        workspace_id: workspace_id.to_owned(),
                        session,
                    })
            })
            .is_some_and(|item| item.status == status);
        if !dup {
            self.on_pty_status(workspace_id, session, status, title, provider, catalog);
        }
    }

    /// workspace가 삭제되면 그 workspace의 모든 알림을 제거한다.
    pub fn prune_workspace(&mut self, workspace_id: &str) {
        self.items
            .retain(|item| item.target.workspace_id() != workspace_id);
    }

    /// workspace가 Suspended(축출)되면 그 workspace의 진행형(Waiting/승인) 알림을 제거한다 —
    /// 워커가 죽어 더는 조치 불가하므로. 결과(Done/Error)는 기록이라 유지한다.
    pub fn prune_transient(&mut self, workspace_id: &str) {
        self.items.retain(|item| {
            item.target.workspace_id() != workspace_id
                || matches!(item.status, SessionStatus::Done | SessionStatus::Error)
        });
    }

    /// 활성 workspace의 세션이 사라지면 그 workspace의 진행형 알림을 정리한다.
    /// 결과 상태(Done/Error)는 기록이라 유지하고, 다른 workspace(warm 등)의 진행형은
    /// alive를 알 수 없어 유지한다(총량은 on_status의 100개 cap으로 유계).
    pub fn retain_sessions(&mut self, active_workspace_id: &str, alive: &[SessionId]) {
        self.items.retain(|item| {
            matches!(item.status, SessionStatus::Done | SessionStatus::Error)
                || item.target.workspace_id() != active_workspace_id
                || match &item.target {
                    AgentNotificationTarget::Pty { session, .. } => alive.contains(session),
                    // Structured liveness is owned by AgentSessionsUi rather
                    // than the active PTY mux and is pruned separately.
                    AgentNotificationTarget::Structured { .. } => true,
                }
        });
    }

    pub fn retain_structured_sessions(&mut self, alive: &[String]) {
        self.items.retain(|item| {
            matches!(item.status, SessionStatus::Done | SessionStatus::Error)
                || match &item.target {
                    AgentNotificationTarget::Structured { session_id, .. } => {
                        alive.contains(session_id)
                    }
                    AgentNotificationTarget::Pty { .. } => true,
                }
        });
    }

    /// 벨 팝오버의 「최근 알림」 섹션 (v3.9 N1) — 최신 N개만. 전체 목록/비우기는
    /// 설정→알림(contents)이 계속 담당한다. 팝오버는 "빠른 확인"만 한다.
    ///
    /// 팝오버 본문은 이 함수 + 대기 섹션(N2 승인 카드 / N3 PTY 카드)으로 구성된다 —
    /// 세 섹션이 서로 다른 PR에서 채워지므로 렌더 함수를 분리해 둔다.
    pub fn recent_section(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        max_items: usize,
    ) -> Option<AgentNotificationTarget> {
        section_label(ui, &catalog.t("inbox.recent", &[]));
        if self.items.is_empty() {
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(catalog.t("notification.empty", &[]))
                    .size(11.0)
                    .weak(),
            );
            return None;
        }
        let mut clicked = None;
        // 최신 항목이 위로 — 팝오버는 최근 max_items개만 보여준다.
        for item in self.items.iter().rev().take(max_items) {
            let icon = status_icon(item.status);
            let label = catalog.t(&item.message_id, &[("title", &item.title)]);
            ui.horizontal(|ui| {
                ui.colored_label(notification_status_color(item.status), "●");
                if ui
                    .button(format!("{} {icon} {label}", item.source.badge()))
                    .on_hover_text(catalog.t("notification.goto_session", &[]))
                    .clicked()
                {
                    clicked = Some(item.target.clone());
                }
            });
        }
        clicked
    }

    /// 창 프레임 없이 본문만 렌더 (통합 설정 창 우측 패널용). 읽음 처리는 show()가 한다.
    pub fn contents(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
    ) -> Option<AgentNotificationTarget> {
        let mut clicked = None;
        if self.items.is_empty() {
            ui.label(catalog.t("notification.empty", &[]));
        }
        if !self.items.is_empty()
            && ui
                .button(catalog.t("notification.clear_all", &[]))
                .clicked()
        {
            self.items.clear();
        }
        // 최신 항목이 위로
        for item in self.items.iter().rev() {
            let icon = status_icon(item.status);
            let label = catalog.t(&item.message_id, &[("title", &item.title)]);
            ui.horizontal(|ui| {
                ui.colored_label(notification_status_color(item.status), "●");
                if ui
                    .button(format!("{} {icon} {label}", item.source.badge()))
                    .on_hover_text(catalog.t("notification.goto_session", &[]))
                    .clicked()
                {
                    clicked = Some(item.target.clone());
                }
            });
        }
        clicked
    }
}

fn bounded_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.as_bytes().contains(&0)
}

fn bounded_target(target: &AgentNotificationTarget) -> bool {
    match target {
        AgentNotificationTarget::Pty { workspace_id, .. } => {
            bounded_text(workspace_id, MAX_WORKSPACE_ID_BYTES)
        }
        AgentNotificationTarget::Structured {
            workspace_id,
            session_id,
        } => {
            bounded_text(workspace_id, MAX_WORKSPACE_ID_BYTES)
                && bounded_text(session_id, MAX_STRUCTURED_SESSION_ID_BYTES)
        }
    }
}

/// 팝오버 섹션 제목 — 좌측 세로 막대 + 작은 라벨 (대기/최근 공용).
pub fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(3.0, 12.0), egui::Sense::hover());
        ui.painter().rect_filled(
            rect,
            1.0,
            ui.visuals().widgets.noninteractive.bg_stroke.color,
        );
        // 보조 라벨 의도라 weak만 — strong과 함께 걸면 egui가 strong을 우선해
        // weak가 조용히 무시된다(리뷰 P3).
        ui.label(egui::RichText::new(text).size(11.0).weak());
    });
}

/// 경과 초 → 「방금/N분 전/N시간 전/N일 전」 로케일 문구 (작업함 페이지 시간 표시).
/// diff_panel의 relative_time_label과 같은 구간 규칙이되, 새 사용자 문자열 규칙에 따라
/// i18n 카탈로그를 쓴다.
#[allow(dead_code)]
fn relative_time_label(catalog: &i18n::Catalog, elapsed_secs: i64) -> String {
    let secs = elapsed_secs.max(0);
    if secs < 60 {
        catalog.t("inbox.time.just_now", &[])
    } else if secs < 3600 {
        catalog.t(
            "inbox.time.minutes_ago",
            &[("count", &(secs / 60).to_string())],
        )
    } else if secs < 86_400 {
        catalog.t(
            "inbox.time.hours_ago",
            &[("count", &(secs / 3600).to_string())],
        )
    } else {
        catalog.t(
            "inbox.time.days_ago",
            &[("count", &(secs / 86_400).to_string())],
        )
    }
}

fn notification_status_color(status: SessionStatus) -> egui::Color32 {
    crate::ui::agent_visuals::status_color(crate::agent_surface::AgentVisualState::from_pty(Some(
        status,
    )))
}

fn status_icon(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Waiting => "⏳",
        SessionStatus::NeedsApproval => "✋",
        SessionStatus::Error => "❌",
        SessionStatus::Done => "✅",
        SessionStatus::Idle => "",
        SessionStatus::Running => "",
    }
}

fn notification_message_id(status: SessionStatus) -> Option<&'static str> {
    match status {
        SessionStatus::Waiting => Some("notification.session.waiting"),
        SessionStatus::NeedsApproval => Some("notification.session.needs_approval"),
        SessionStatus::Error => Some("notification.session.error"),
        SessionStatus::Done => Some("notification.session.done"),
        SessionStatus::Idle => None, // 쉬는 중은 알림 아님
        SessionStatus::Running => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = "ws-1";

    fn catalog() -> i18n::Catalog {
        i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap()
    }

    fn is_pty(item: &NotificationItem, workspace_id: &str, session: SessionId) -> bool {
        matches!(
            &item.target,
            AgentNotificationTarget::Pty {
                workspace_id: item_workspace,
                session: item_session,
            } if item_workspace == workspace_id && *item_session == session
        )
    }

    #[test]
    fn running_복귀는_알림_아님() {
        let mut n = NotificationsUi::new();
        let catalog = catalog();
        n.on_status(WS, SessionId(1), SessionStatus::Running, "t", &catalog);
        assert_eq!(n.unread(), 0);
        n.on_status(WS, SessionId(1), SessionStatus::Error, "t", &catalog);
        assert_eq!(n.unread(), 1);
    }

    #[test]
    fn status_알림은_message_id를_저장한다() {
        let mut n = NotificationsUi::new();
        let catalog = catalog();
        n.on_status(
            WS,
            SessionId(1),
            SessionStatus::NeedsApproval,
            "review",
            &catalog,
        );
        let item = n.items.first().unwrap();
        assert_eq!(item.message_id, "notification.session.needs_approval");
        assert_eq!(
            catalog.t(&item.message_id, &[("title", &item.title)]),
            "Approval needed: review"
        );
    }

    /// 2026-07-17 실측 회귀: 실행 중인 앱에 같은 세션의 승인 행을 연달아 넣었을 때
    /// 두 번째부터 osascript(OS 알림)가 발화되지 않았다 — 상태 전이용 중복 억제가
    /// 개별 사건인 승인에까지 걸렸다. 에이전트는 도구 승인을 연달아 요청하므로
    /// 첫 건만 알리면 나머지를 놓친다.
    #[test]
    fn 같은_세션의_연속_mcp승인은_매번_알린다() {
        let mut n = NotificationsUi::new();
        let catalog = catalog();
        n.on_mcp_approval(WS, SessionId(1), "write_file", &catalog);
        n.on_mcp_approval(WS, SessionId(1), "run_command", &catalog);
        assert_eq!(n.items.len(), 2, "두 승인 모두 알림 항목이 되어야 한다");
        assert_eq!(n.unread(), 2);
        assert_eq!(n.items[0].title, "write_file");
        assert_eq!(n.items[1].title, "run_command");
    }

    /// 겹④(최후의 그물) — Done/Error를 판정하지 않고 native intent 하나만 낸다.
    /// history 항목(`items`)에는 안 올린다 — SessionStatus가 없어 아이콘/색/dedup을
    /// 정할 근거가 없고, 뒤이어 오는 진짜 결과 알림과 섞이면 안 된다.
    #[test]
    fn agent_vanished은_native_intent만_내고_history에는_안_올린다() {
        let mut n = NotificationsUi::new();
        let catalog = catalog();
        n.on_agent_vanished("작업", &catalog);
        assert!(
            n.items.is_empty(),
            "결과 미확정 상태라 history에 남기지 않는다"
        );
        let intent = n.pop_native_intent().expect("native intent");
        assert_eq!(intent.body(), "작업");
        assert!(n.pop_native_intent().is_none());
    }

    #[test]
    fn agent_vanished도_oversized_title은_거부한다() {
        let mut n = NotificationsUi::new();
        let catalog = catalog();
        n.on_agent_vanished(&"x".repeat(MAX_NATIVE_BODY_BYTES + 1), &catalog);
        assert!(n.pop_native_intent().is_none());
    }

    #[test]
    fn 상태_알림은_native_intent로만_발행된다() {
        let mut notifications = NotificationsUi::new();
        let catalog = catalog();

        notifications.on_status(
            WS,
            SessionId(1),
            SessionStatus::NeedsApproval,
            "review",
            &catalog,
        );

        let intent = notifications.pop_native_intent().expect("native intent");
        assert_eq!(intent.summary(), "Approval needed: review");
        assert_eq!(intent.body(), "review");
        let debug = format!("{intent:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("review"));
        assert!(notifications.pop_native_intent().is_none());
    }

    #[test]
    fn 상태_중복은_history와_native_intent_모두_억제한다() {
        let mut notifications = NotificationsUi::new();
        let catalog = catalog();

        for _ in 0..2 {
            notifications.on_status(WS, SessionId(1), SessionStatus::Waiting, "same", &catalog);
        }

        assert_eq!(notifications.items.len(), 1);
        assert!(notifications.pop_native_intent().is_some());
        assert!(notifications.pop_native_intent().is_none());
    }

    #[test]
    fn native_intent_queue는_최근_고정용량만_유지한다() {
        let mut notifications = NotificationsUi::new();
        let catalog = catalog();

        for index in 0..(MAX_NATIVE_INTENTS + 2) {
            notifications.on_mcp_approval(WS, SessionId(1), &format!("tool-{index}"), &catalog);
        }

        assert_eq!(notifications.items.len(), MAX_NATIVE_INTENTS + 2);
        let titles: Vec<_> = std::iter::from_fn(|| notifications.pop_native_intent())
            .map(|intent| intent.body().to_owned())
            .collect();
        assert_eq!(titles.len(), MAX_NATIVE_INTENTS);
        assert_eq!(titles.first().map(String::as_str), Some("tool-2"));
        assert_eq!(titles.last().map(String::as_str), Some("tool-9"));
    }

    #[test]
    fn invalid_or_oversized_payload는_history와_intent를_만들지_않는다() {
        let mut notifications = NotificationsUi::new();
        let catalog = catalog();
        let oversized_title = "x".repeat(MAX_NATIVE_BODY_BYTES + 1);
        let oversized_workspace = "w".repeat(MAX_WORKSPACE_ID_BYTES + 1);
        let oversized_session = "s".repeat(MAX_STRUCTURED_SESSION_ID_BYTES + 1);

        notifications.on_status(
            WS,
            SessionId(1),
            SessionStatus::Done,
            &oversized_title,
            &catalog,
        );
        notifications.on_status(
            &oversized_workspace,
            SessionId(2),
            SessionStatus::Done,
            "title",
            &catalog,
        );
        notifications.on_structured_status(
            WS,
            &oversized_session,
            AgentSessionStatus::Completed,
            "title",
            &catalog,
        );
        notifications.on_mcp_approval(WS, SessionId(3), "contains\0nul", &catalog);

        assert!(notifications.items.is_empty());
        assert!(notifications.pop_native_intent().is_none());
    }

    #[test]
    fn native_intent의_각_field와_total_byte_budget은_fail_closed다() {
        assert!(NativeNotificationIntent::new("summary".to_owned(), "body").is_some());
        assert!(
            NativeNotificationIntent::new("s".repeat(MAX_NATIVE_SUMMARY_BYTES + 1), "body")
                .is_none()
        );
        assert!(
            NativeNotificationIntent::new(
                "summary".to_owned(),
                &"b".repeat(MAX_NATIVE_BODY_BYTES + 1),
            )
            .is_none()
        );
        assert!(NativeNotificationIntent::new("s".repeat(3_500), &"b".repeat(1_800)).is_none());
        assert!(NativeNotificationIntent::new("contains\0nul".to_owned(), "body").is_none());
        assert!(NativeNotificationIntent::new("summary".to_owned(), "contains\0nul").is_none());
    }

    #[test]
    fn history는_100개로_유계다() {
        let mut notifications = NotificationsUi::new();
        let catalog = catalog();

        for index in 0..(MAX_NOTIFICATION_HISTORY + 20) {
            notifications.on_mcp_approval(WS, SessionId(1), &format!("tool-{index}"), &catalog);
        }

        assert_eq!(notifications.items.len(), MAX_NOTIFICATION_HISTORY);
        assert_eq!(notifications.items[0].title, "tool-20");
        assert_eq!(notifications.items[99].title, "tool-119");
    }

    #[test]
    fn production_leaf에는_platform_notification_effect가_없다() {
        let source = include_str!("notifications.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");
        for forbidden in [
            "platform::",
            "std::process::Command",
            "Command::new(",
            "osascript",
            "notify-rust",
        ] {
            assert!(
                !production.contains(forbidden),
                "production notification leaf contains forbidden native effect: {forbidden}"
            );
        }
    }

    /// 반면 상태 전이(PTY 감지)는 같은 대상·같은 상태가 반복되면 억제한다 —
    /// 위 수정이 이 규약을 깨지 않았는지 함께 고정한다.
    #[test]
    fn 상태전이_알림의_중복_억제는_유지된다() {
        let mut n = NotificationsUi::new();
        let catalog = catalog();
        n.on_status(WS, SessionId(1), SessionStatus::Waiting, "t", &catalog);
        n.on_status(WS, SessionId(1), SessionStatus::Waiting, "t", &catalog);
        assert_eq!(n.items.len(), 1, "같은 상태 반복은 한 번만 알린다");
    }

    #[test]
    fn 다른_workspace의_같은_세션id는_별개_알림() {
        let mut n = NotificationsUi::new();
        let catalog = catalog();
        // 워커마다 SessionId가 리셋되므로 (ws, session)로 구분돼야 한다
        n.on_status("ws-a", SessionId(1), SessionStatus::Done, "A작업", &catalog);
        n.on_status("ws-b", SessionId(1), SessionStatus::Done, "B작업", &catalog);
        assert_eq!(n.items.len(), 2);
        // 활성(ws-a) 기준 retain: ws-a의 진행형만 정리, ws-b(다른 workspace)는 유지
        n.on_status(
            "ws-a",
            SessionId(2),
            SessionStatus::Waiting,
            "A대기",
            &catalog,
        );
        n.on_status(
            "ws-b",
            SessionId(2),
            SessionStatus::Waiting,
            "B대기",
            &catalog,
        );
        n.retain_sessions("ws-a", &[]); // ws-a에 alive 세션 없음
        // ws-a의 Waiting(진행형)은 정리, Done(결과)은 유지, ws-b는 전부 유지
        let has = |ws: &str, sess: u64, st: SessionStatus| {
            n.items
                .iter()
                .any(|i| is_pty(i, ws, SessionId(sess)) && i.status == st)
        };
        assert!(has("ws-a", 1, SessionStatus::Done));
        assert!(!has("ws-a", 2, SessionStatus::Waiting));
        assert!(has("ws-b", 1, SessionStatus::Done));
        assert!(has("ws-b", 2, SessionStatus::Waiting));
    }

    #[test]
    fn 센터가_열려있어도_생성시엔_안읽음() {
        // 숨김 중 logic()에서 on_status가 호출될 수 있으므로 self.open으로 읽음 처리하면
        // 안 된다 — 사용자가 본 시점(show=가시)에만 읽음. 생성 시엔 항상 unread.
        let mut n = NotificationsUi::new();
        let catalog = catalog();
        n.mark_all_read();
        n.on_status(WS, SessionId(1), SessionStatus::Done, "완료", &catalog);
        assert_eq!(n.unread(), 1);
    }

    #[test]
    fn retain은_결과상태_유지하고_진행형만_정리() {
        let mut n = NotificationsUi::new();
        let catalog = catalog();
        n.on_status(WS, SessionId(1), SessionStatus::Done, "a", &catalog); // 결과 → 유지
        n.on_status(
            WS,
            SessionId(2),
            SessionStatus::NeedsApproval,
            "b",
            &catalog,
        ); // 진행형 → 정리
        n.on_status(WS, SessionId(3), SessionStatus::Error, "c", &catalog); // 결과 → 유지
        // 1·2 사라짐(닫힘/archive). Done(1)·Error(3)는 기록이라 유지, 승인(2)만 정리
        n.retain_sessions(WS, &[SessionId(3)]);
        let sessions: Vec<_> = n
            .items
            .iter()
            .filter_map(|item| match item.target {
                AgentNotificationTarget::Pty { session, .. } => Some(session),
                AgentNotificationTarget::Structured { .. } => None,
            })
            .collect();
        assert!(sessions.contains(&SessionId(1)));
        assert!(sessions.contains(&SessionId(3)));
        assert!(!sessions.contains(&SessionId(2)));
    }

    #[test]
    fn exit_알림과_status_중복_방지() {
        let mut n = NotificationsUi::new();
        let catalog = catalog();
        // status detector가 먼저 Done 감지 → 이후 exit(0)는 중복 발화 안 함
        n.on_status(WS, SessionId(1), SessionStatus::Done, "a", &catalog);
        n.on_exit(WS, SessionId(1), Some(0), "a", &catalog);
        assert_eq!(n.items.len(), 1);
        // regex 없는 agent: status 없이 exit만 → Done 알림 생성
        n.on_exit(WS, SessionId(2), Some(0), "b", &catalog);
        assert_eq!(
            n.items
                .iter()
                .filter(|item| is_pty(item, WS, SessionId(2)))
                .count(),
            1
        );
        // 비정상 종료 → Error
        n.on_exit(WS, SessionId(3), Some(1), "c", &catalog);
        assert!(matches!(
            n.items
                .iter()
                .find(|item| is_pty(item, WS, SessionId(3)))
                .unwrap()
                .status,
            SessionStatus::Error
        ));
    }

    #[test]
    fn unread는_읽은_옛항목과_무관하게_pruning에_정합() {
        let mut n = NotificationsUi::new();
        let catalog = catalog();
        n.on_status(WS, SessionId(1), SessionStatus::Done, "old", &catalog); // 읽을 항목
        n.mark_all_read(); // 보기 → 모두 읽음
        assert_eq!(n.unread(), 0);
        n.on_status(WS, SessionId(2), SessionStatus::Waiting, "new", &catalog); // 진행형, 안 읽음 1
        assert_eq!(n.unread(), 1);
        // session 2(진행형)가 사라짐 → 정리되어 unread 0, 옛 읽은 Done(1)은 남아도 unread 0
        n.retain_sessions(WS, &[SessionId(1)]);
        assert_eq!(n.unread(), 0);
    }

    #[test]
    fn structured_status_알림은_target과_source를_보존한다() {
        let mut n = NotificationsUi::new();
        let catalog = catalog();
        n.on_structured_status(
            WS,
            "structured-1",
            AgentSessionStatus::AwaitingApproval,
            "review",
            &catalog,
        );
        assert_eq!(n.items.len(), 1);
        let item = &n.items[0];
        assert_eq!(item.source.badge(), "[Codex APP]");
        assert_eq!(item.status, SessionStatus::NeedsApproval);
        assert_eq!(
            item.target,
            AgentNotificationTarget::Structured {
                workspace_id: WS.to_owned(),
                session_id: "structured-1".to_owned(),
            }
        );
    }

    #[test]
    fn pty_provider_badge는_status와_exit_경로에서_보존된다() {
        let mut n = NotificationsUi::new();
        let catalog = catalog();
        n.on_pty_status(
            WS,
            SessionId(11),
            SessionStatus::NeedsApproval,
            "codex",
            Some(AgentProvider::Codex),
            &catalog,
        );
        n.on_pty_exit(
            WS,
            SessionId(12),
            Some(0),
            "claude",
            Some(AgentProvider::Claude),
            &catalog,
        );

        assert_eq!(n.items[0].source.badge(), "[Codex PTY]");
        assert_eq!(n.items[1].source.badge(), "[Claude PTY]");
        assert!(is_pty(&n.items[0], WS, SessionId(11)));
        assert!(is_pty(&n.items[1], WS, SessionId(12)));
    }

    #[test]
    fn structured_active와_idle은_알림이_아니다() {
        let mut n = NotificationsUi::new();
        let catalog = catalog();
        for status in [
            AgentSessionStatus::Starting,
            AgentSessionStatus::Ready,
            AgentSessionStatus::Running,
            AgentSessionStatus::Stopped,
        ] {
            n.on_structured_status(WS, "structured-1", status, "review", &catalog);
        }
        assert!(n.items.is_empty());
    }

    #[test]
    fn structured_duplicate_status는_한번만_알린다() {
        let mut n = NotificationsUi::new();
        let catalog = catalog();
        n.on_structured_status(
            WS,
            "structured-1",
            AgentSessionStatus::Completed,
            "review",
            &catalog,
        );
        n.on_structured_status(
            WS,
            "structured-1",
            AgentSessionStatus::Completed,
            "review",
            &catalog,
        );
        assert_eq!(n.items.len(), 1);
    }

    /// 작업함 페이지의 시간 표시 — 구간별 로케일 문구 (2026-07-18).
    #[test]
    fn relative_time_label은_구간별_로케일_문구를_낸다() {
        let catalog = i18n::Catalog::load("ko-KR").expect("ko-KR catalog");
        assert_eq!(relative_time_label(&catalog, 5), "방금");
        assert_eq!(relative_time_label(&catalog, 90), "1분 전");
        assert_eq!(relative_time_label(&catalog, 2 * 3600), "2시간 전");
        assert_eq!(relative_time_label(&catalog, 3 * 86_400), "3일 전");
        // 음수(시계 역행)는 방금으로 고정 — 미래 표기를 만들지 않는다.
        assert_eq!(relative_time_label(&catalog, -10), "방금");
    }
}
