use super::activity::{ActivityWorkspaceRow, ActivityWorkspaceState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentTerminalView {
    Home,
    #[default]
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum AnnouncementFilter {
    #[default]
    All,
    OpenAi,
    Anthropic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeAction {
    Inbox,
    Activity,
    Agents,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HomeMetrics {
    pub waiting: usize,
    pub unread: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct WorkspaceTotals {
    workspaces: usize,
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
}

impl AgentTerminalUi {
    pub fn new() -> Self {
        Self {
            view: AgentTerminalView::Terminal,
            announcement_filter: AnnouncementFilter::All,
        }
    }

    pub fn view(&self) -> AgentTerminalView {
        self.view
    }

    pub fn set_view(&mut self, view: AgentTerminalView) {
        self.view = view;
    }

    pub fn is_home(&self) -> bool {
        self.view == AgentTerminalView::Home
    }

    pub fn home(
        &mut self,
        ui: &mut egui::Ui,
        rows: &[ActivityWorkspaceRow],
        metrics: HomeMetrics,
    ) -> Option<HomeAction> {
        let totals = workspace_totals(rows);
        let mut action = None;
        egui::ScrollArea::vertical()
            .id_salt("agent_terminal_home")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::NONE
                    .inner_margin(egui::Margin::same(22))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        self.announcements(ui);
                        ui.add_space(14.0);
                        if let Some(next) = orchestration_insights(ui, totals, metrics) {
                            action = Some(next);
                        }
                        ui.add_space(14.0);
                        workspace_summary(ui, totals, metrics);
                        ui.add_space(14.0);
                        workspace_rows(ui, rows);
                    });
            });
        action
    }

    pub fn status_bar(&self, ui: &mut egui::Ui, rows: &[ActivityWorkspaceRow], waiting: usize) {
        let totals = workspace_totals(rows);
        let cpu = if totals.cpu_seen {
            format!("CPU {:.1}%", totals.cpu_percent)
        } else {
            "CPU —".to_owned()
        };
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 25.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add_space(10.0);
                status_dot(ui, egui::Color32::from_rgb(0x55, 0xc8, 0x79));
                ui.weak("연결됨");
                ui.separator();
                ui.weak(format!("워크스페이스 {}", totals.workspaces));
                ui.separator();
                ui.weak(format!("세션 {}", totals.sessions));
                if waiting > 0 {
                    ui.separator();
                    ui.colored_label(
                        egui::Color32::from_rgb(0xe7, 0x9a, 0x3b),
                        format!("입력 대기 {waiting}"),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(10.0);
                    // 사이드바 최대 확장 시 남는 폭이 좁아 두 값 라벨이 좌측 카운터를
                    // 덮는다(codex P2) — 좁으면 세션 합을 hover로 내리고 앱 값만 남긴다.
                    let compact = ui.available_width() < 400.0;
                    let memory = memory_label(
                        totals.app_rss_bytes,
                        totals.session_rss_bytes,
                        compact,
                    );
                    ui.weak(memory).on_hover_text(format!(
                        "앱 = 이 앱 프로세스 메모리 · 세션 {} = 모든 워크스페이스의 셸/에이전트 프로세스 합",
                        super::format_bytes(totals.session_rss_bytes),
                    ));
                    ui.separator();
                    ui.weak(cpu);
                    ui.separator();
                    ui.weak(match self.view {
                        AgentTerminalView::Home => "홈",
                        AgentTerminalView::Terminal => "터미널",
                    });
                });
            },
        );
    }

    fn announcements(&mut self, ui: &mut egui::Ui) {
        let panel = egui::Frame::NONE
            .fill(ui.visuals().panel_fill)
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .corner_radius(egui::CornerRadius::same(2))
            .inner_margin(egui::Margin::same(16));
        panel.show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                status_dot(ui, egui::Color32::from_rgb(0x55, 0xc8, 0x79));
                ui.label(egui::RichText::new("AI 공지").strong().size(17.0));
                ui.add_space(8.0);
                source_filter(
                    ui,
                    "전체",
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
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak("공식 변경 로그 채널");
                });
            });
            ui.add_space(12.0);
            let cards = [
                AnnouncementCard {
                    provider: AnnouncementFilter::OpenAi,
                    source: "OpenAI",
                    product: "API",
                    title: "API 플랫폼 업데이트",
                    description: "새 모델과 개발자 도구의 공식 변경 사항을 확인합니다.",
                    url: "https://developers.openai.com/api/docs/changelog",
                    accent: egui::Color32::from_rgb(0xa7, 0xae, 0xbc),
                },
                AnnouncementCard {
                    provider: AnnouncementFilter::Anthropic,
                    source: "Anthropic",
                    product: "Claude",
                    title: "Claude 릴리스 노트",
                    description: "Claude API와 제품의 공식 릴리스 기록으로 이동합니다.",
                    url: "https://platform.claude.com/docs/en/release-notes/overview",
                    accent: egui::Color32::from_rgb(0xd2, 0x91, 0x55),
                },
                AnnouncementCard {
                    provider: AnnouncementFilter::OpenAi,
                    source: "OpenAI",
                    product: "Codex",
                    title: "Codex 제품 업데이트",
                    description: "에이전트 워크플로와 코드 작업 도구의 변경 기록을 엽니다.",
                    url: "https://learn.chatgpt.com/docs/changelog",
                    accent: egui::Color32::from_rgb(0x76, 0x87, 0xff),
                },
            ];
            let visible: Vec<_> = cards
                .iter()
                .filter(|card| {
                    self.announcement_filter == AnnouncementFilter::All
                        || self.announcement_filter == card.provider
                })
                .collect();
            let columns = if ui.available_width() >= 760.0 {
                visible.len().clamp(1, 3)
            } else {
                1
            };
            card_columns(ui, columns, visible, announcement_card);
        });
    }
}

#[derive(Clone, Copy)]
struct AnnouncementCard {
    provider: AnnouncementFilter,
    source: &'static str,
    product: &'static str,
    title: &'static str,
    description: &'static str,
    url: &'static str,
    accent: egui::Color32,
}

fn source_filter(
    ui: &mut egui::Ui,
    label: &str,
    selected: &mut AnnouncementFilter,
    value: AnnouncementFilter,
) {
    let button = egui::Button::new(label)
        .selected(*selected == value)
        .corner_radius(egui::CornerRadius::same(1));
    if ui.add(button).clicked() {
        *selected = value;
    }
}

fn announcement_card(ui: &mut egui::Ui, card: &&AnnouncementCard) {
    let response = egui::Frame::NONE
        .fill(ui.visuals().faint_bg_color)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(egui::CornerRadius::same(2))
        .inner_margin(egui::Margin::same(13))
        .show(ui, |ui| {
            ui.set_min_height(126.0);
            ui.horizontal(|ui| {
                provider_mark(ui, card.source, card.accent);
                ui.weak(card.source);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(card.product);
                });
            });
            ui.add_space(10.0);
            ui.label(egui::RichText::new(card.title).strong().size(15.0));
            ui.add_space(5.0);
            ui.weak(card.description);
            ui.add_space(10.0);
            ui.hyperlink_to("원문 보기 →", card.url);
        })
        .response;
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
}

fn orchestration_insights(
    ui: &mut egui::Ui,
    totals: WorkspaceTotals,
    metrics: HomeMetrics,
) -> Option<HomeAction> {
    let mut action = None;
    let panel = egui::Frame::NONE
        .fill(ui.visuals().panel_fill)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(egui::CornerRadius::same(2))
        .inner_margin(egui::Margin::same(16));
    panel.show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("오케스트레이션 인사이트")
                    .strong()
                    .size(16.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak("전체 워크스페이스 신호");
            });
        });
        ui.add_space(10.0);
        let mut insights = Vec::new();
        if metrics.waiting > 0 {
            insights.push((
                egui::Color32::from_rgb(0xe7, 0x9a, 0x3b),
                "입력 대기를 한 번에 검토하세요",
                format!(
                    "{0}건의 권한 확인 또는 응답이 작업 진행을 막고 있습니다.",
                    metrics.waiting
                ),
                "작업함 열기",
                HomeAction::Inbox,
            ));
        }
        if totals.warnings > 0 {
            insights.push((
                egui::Color32::from_rgb(0xed, 0x5b, 0x61),
                "리소스 또는 입력 압력을 확인하세요",
                format!(
                    "{}개 워크스페이스에서 주의 신호가 감지됐습니다.",
                    totals.warnings
                ),
                "활동 보기",
                HomeAction::Activity,
            ));
        }
        if totals.active + totals.warm > 1 || totals.idle > 0 {
            insights.push((
                egui::Color32::from_rgb(0x43, 0xb8, 0xcd),
                "가용 워크스페이스에 작업을 분산할 수 있습니다",
                format!(
                    "실행 가능 {} · 유휴 {} 워크스페이스",
                    totals.active + totals.warm,
                    totals.idle
                ),
                "에이전트 열기",
                HomeAction::Agents,
            ));
        }
        if insights.is_empty() {
            insights.push((
                egui::Color32::from_rgb(0x55, 0xc8, 0x79),
                "현재 막힌 작업이 없습니다",
                "모든 워크스페이스가 정상 범위에서 동작하고 있습니다.".to_owned(),
                "활동 보기",
                HomeAction::Activity,
            ));
        }
        let columns = if ui.available_width() >= 820.0 {
            insights.len().clamp(1, 3)
        } else {
            1
        };
        let rows: Vec<_> = insights.iter().collect();
        card_columns(ui, columns, rows, |ui, insight| {
            let (color, title, detail, button, next) = insight;
            egui::Frame::NONE
                .fill(ui.visuals().faint_bg_color)
                .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                .corner_radius(egui::CornerRadius::same(2))
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.set_min_height(102.0);
                    ui.horizontal(|ui| {
                        status_dot(ui, *color);
                        ui.label(egui::RichText::new(*title).strong());
                    });
                    ui.add_space(6.0);
                    ui.weak(detail);
                    ui.add_space(8.0);
                    if ui.small_button(*button).clicked() {
                        action = Some(*next);
                    }
                });
        });
    });
    action
}

fn workspace_summary(ui: &mut egui::Ui, totals: WorkspaceTotals, metrics: HomeMetrics) {
    let values = [
        ("전체 워크스페이스", totals.workspaces, "등록된 프로젝트"),
        ("실행 중", totals.active + totals.warm, "active + warm"),
        ("전체 세션", totals.sessions, "모든 워크스페이스"),
        ("입력 대기", metrics.waiting, "권한 확인과 응답"),
        ("확인 필요", totals.warnings, "리소스·입력 압력"),
        ("유휴", totals.idle, "작업 위임 가능"),
    ];
    ui.label(egui::RichText::new("전체 작업 상태").strong().size(16.0));
    ui.add_space(8.0);
    let columns = if ui.available_width() >= 850.0 { 3 } else { 2 };
    for chunk in values.chunks(columns) {
        ui.columns(columns, |uis| {
            for (column, (label, value, detail)) in uis.iter_mut().zip(chunk) {
                egui::Frame::NONE
                    .fill(column.visuals().panel_fill)
                    .stroke(column.visuals().widgets.noninteractive.bg_stroke)
                    .corner_radius(egui::CornerRadius::same(2))
                    .inner_margin(egui::Margin::same(13))
                    .show(column, |ui| {
                        ui.set_min_height(78.0);
                        ui.weak(*label);
                        ui.label(egui::RichText::new(value.to_string()).strong().size(22.0));
                        ui.weak(egui::RichText::new(*detail).size(11.0));
                    });
            }
        });
        ui.add_space(6.0);
    }
    if metrics.unread > 0 {
        ui.weak(format!("읽지 않은 최근 알림 {}건", metrics.unread));
    }
}

fn workspace_rows(ui: &mut egui::Ui, rows: &[ActivityWorkspaceRow]) {
    ui.label(egui::RichText::new("워크스페이스").strong().size(16.0));
    ui.add_space(8.0);
    for row in rows {
        let (state, color) = match row.state {
            ActivityWorkspaceState::Active => ("활성", egui::Color32::from_rgb(0x55, 0xc8, 0x79)),
            ActivityWorkspaceState::Warm => {
                ("백그라운드", egui::Color32::from_rgb(0x4c, 0xa8, 0xdf))
            }
            ActivityWorkspaceState::Idle => ("유휴", ui.visuals().weak_text_color()),
        };
        egui::Frame::NONE
            .fill(ui.visuals().panel_fill)
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .corner_radius(egui::CornerRadius::same(2))
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    status_dot(ui, color);
                    ui.label(egui::RichText::new(&row.name).strong());
                    ui.weak(format!("세션 {}", row.session_count));
                    if workspace_has_warning(row) {
                        ui.colored_label(egui::Color32::from_rgb(0xed, 0x5b, 0x61), "확인 필요");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(state);
                    });
                });
            });
        ui.add_space(4.0);
    }
}

/// 상태바 메모리 라벨. 앱/세션 분리 표시 — 합산 단일 "RAM"은 세션 에이전트 몇 개에
/// 수 GB로 보여 앱 메모리 급증으로 오독된다(2026-07-18 사용자 보고). compact(좁은 폭)
/// 에서는 세션 합을 hover로 내리고 앱 값만 남긴다 — 오독 방지가 우선이라 앱 값을 남긴다.
fn memory_label(app_rss: u64, session_rss: u64, compact: bool) -> String {
    if compact {
        format!("앱 {}", super::format_bytes(app_rss))
    } else {
        format!(
            "앱 {} · 세션 {}",
            super::format_bytes(app_rss),
            super::format_bytes(session_rss),
        )
    }
}

fn workspace_totals(rows: &[ActivityWorkspaceRow]) -> WorkspaceTotals {
    let mut totals = WorkspaceTotals {
        workspaces: rows.len(),
        ..WorkspaceTotals::default()
    };
    let mut app_pids = std::collections::HashSet::new();
    for row in rows {
        match row.state {
            ActivityWorkspaceState::Active => totals.active += 1,
            ActivityWorkspaceState::Warm => totals.warm += 1,
            ActivityWorkspaceState::Idle => totals.idle += 1,
        }
        totals.sessions += row.session_count;
        totals.warnings += usize::from(workspace_has_warning(row));
        if let Some(resource) = row.resource
            && app_pids.insert(resource.pid)
        {
            totals.app_rss_bytes = totals.app_rss_bytes.saturating_add(resource.rss_bytes);
            if let Some(cpu) = resource.cpu_percent {
                totals.cpu_percent += cpu;
                totals.cpu_seen = true;
            }
        }
        for resource in &row.session_resources {
            totals.session_rss_bytes = totals.session_rss_bytes.saturating_add(resource.rss_bytes);
            if let Some(cpu) = resource.cpu_percent {
                totals.cpu_percent += cpu;
                totals.cpu_seen = true;
            }
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

fn card_columns<T>(
    ui: &mut egui::Ui,
    columns: usize,
    values: Vec<T>,
    mut render: impl FnMut(&mut egui::Ui, &T),
) {
    for chunk in values.chunks(columns) {
        ui.columns(columns, |uis| {
            for (column, value) in uis.iter_mut().zip(chunk) {
                render(column, value);
            }
        });
        ui.add_space(6.0);
    }
}

fn provider_mark(ui: &mut egui::Ui, source: &str, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 1.0, color.gamma_multiply(0.22));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        source.chars().next().unwrap_or('A'),
        egui::FontId::monospace(12.0),
        color,
    );
}

fn status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_defaults_to_terminal() {
        assert_eq!(AgentTerminalUi::new().view(), AgentTerminalView::Terminal);
    }

    #[test]
    fn empty_totals_are_stable() {
        let totals = workspace_totals(&[]);
        assert_eq!(totals.workspaces, 0);
        assert_eq!(totals.sessions, 0);
        assert!(!totals.cpu_seen);
    }

    #[test]
    fn totals_split_app_and_session_rss() {
        // 앱(중복 pid 1회)과 세션 프로세스 합을 분리 집계해야 한다 — 합쳐 "RAM"으로
        // 표시하면 에이전트 메모리가 앱 급증으로 오독된다(2026-07-18 사용자 보고).
        let row = |child_session: u64, child_rss: u64| ActivityWorkspaceRow {
            name: "ws".to_owned(),
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
            }],
            sessions: Vec::new(),
        };
        let totals = workspace_totals(&[row(1, 700), row(2, 300)]);
        assert_eq!(totals.app_rss_bytes, 100, "같은 앱 pid는 한 번만");
        assert_eq!(totals.session_rss_bytes, 1000, "세션 프로세스는 각각 합산");
    }

    #[test]
    fn memory_label_은_좁은_폭에서_앱_값만_남긴다() {
        let mib = 1024 * 1024;
        let full = memory_label(150 * mib, 3 * 1024 * mib, false);
        assert!(full.contains("앱") && full.contains("세션"), "{full}");
        // 좁은 폭에서는 좌측 카운터를 덮지 않게 세션 합을 hover로 내린다(codex P2).
        let compact = memory_label(150 * mib, 3 * 1024 * mib, true);
        assert!(
            compact.contains("앱") && !compact.contains("세션"),
            "{compact}"
        );
    }
}
