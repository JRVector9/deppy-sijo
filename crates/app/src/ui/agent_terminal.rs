use super::activity::{ActivityWorkspaceRow, ActivityWorkspaceState};
use crate::status_feed::{ProviderStatus, ServiceIndicator, StatusFeedSnapshot};

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
    /// 「AI 공지」 수동 갱신(⟳) — App이 status_feed 워커를 즉시 깨운다.
    RefreshNotices,
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
        feed: &StatusFeedSnapshot,
        translations: &std::collections::HashMap<String, String>,
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
                        if self.announcements(ui, feed, translations) {
                            action = Some(HomeAction::RefreshNotices);
                        }
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

    pub fn status_bar(
        &self,
        ui: &mut egui::Ui,
        rows: &[ActivityWorkspaceRow],
        waiting: usize,
        mcp_count: usize,
        feed: &StatusFeedSnapshot,
    ) {
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
                ui.separator();
                // 등록·활성화된 MCP 서버 수 (2026-07-18 사용자 요청).
                ui.weak(format!("MCP {mcp_count}"))
                    .on_hover_text("활성화된 MCP 서버 수 (커넥터 센터에서 관리)");
                // AI 서비스 상태 점등 (2026-07-18 사용자) — status.claude.com /
                // status.openai.com 5분 폴링. 클릭 시 상태 페이지를 연다.
                ui.separator();
                service_status_light(
                    ui,
                    "Claude",
                    feed.claude.as_ref(),
                    crate::status_feed::CLAUDE_STATUS_URL,
                );
                service_status_light(
                    ui,
                    "OpenAI",
                    feed.openai.as_ref(),
                    crate::status_feed::OPENAI_STATUS_URL,
                );
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

    /// 반환: 수동 갱신(⟳) 클릭 여부.
    fn announcements(
        &mut self,
        ui: &mut egui::Ui,
        feed: &StatusFeedSnapshot,
        translations: &std::collections::HashMap<String, String>,
    ) -> bool {
        let mut refresh_clicked = false;
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
                    if ui
                        .small_button("⟳")
                        .on_hover_text("지금 갱신 (공지는 60분마다 자동 갱신)")
                        .clicked()
                    {
                        refresh_clicked = true;
                    }
                    ui.weak("status.claude.com · status.openai.com — 60분마다 갱신");
                });
            });
            ui.add_space(12.0);
            // 실제 상태 페이지의 최신 인시던트 3건씩 (2026-07-18 사용자 — 정적 링크
            // 카드에서 교체). 아직 첫 조회 전이면 안내 문구.
            let cards: Vec<AnnouncementCard> = [
                (
                    AnnouncementFilter::Anthropic,
                    "Claude",
                    feed.claude.as_ref(),
                    egui::Color32::from_rgb(0xd2, 0x91, 0x55),
                ),
                (
                    AnnouncementFilter::OpenAi,
                    "OpenAI",
                    feed.openai.as_ref(),
                    egui::Color32::from_rgb(0xa7, 0xae, 0xbc),
                ),
            ]
            .into_iter()
            .filter(|(provider, ..)| {
                self.announcement_filter == AnnouncementFilter::All
                    || self.announcement_filter == *provider
            })
            .filter_map(|(provider, source, status, accent)| {
                status.map(|status| (provider, source, status, accent))
            })
            .flat_map(|(_, source, status, accent)| {
                status
                    .incidents
                    .iter()
                    .map(move |incident| AnnouncementCard {
                        source,
                        incident,
                        accent,
                    })
            })
            .collect();
            if cards.is_empty() {
                ui.weak(if feed.claude.is_none() && feed.openai.is_none() {
                    "공지를 불러오는 중… (상태 페이지 조회)"
                } else {
                    "최근 공지 없음"
                });
            } else {
                // 리스트 형태(2026-07-18 사용자) — 카드 그리드 대신 전체 폭 행.
                for (index, card) in cards.iter().enumerate() {
                    if index > 0 {
                        crate::ui::hairline(ui);
                    }
                    announcement_row(ui, card, translations);
                }
            }
        });
        refresh_clicked
    }
}

/// 홈 공지 카드 1장 — 상태 페이지 인시던트 1건.
#[derive(Clone, Copy)]
struct AnnouncementCard<'a> {
    source: &'static str,
    incident: &'a crate::status_feed::IncidentNotice,
    accent: egui::Color32,
}

/// Statuspage 인시던트 상태 → 한국어 라벨 (미지 값은 원문 그대로).
fn incident_status_label(status: &str) -> &str {
    match status {
        "resolved" => "해결됨",
        "investigating" => "조사 중",
        "identified" => "원인 파악됨",
        "monitoring" => "관찰 중",
        "postmortem" => "사후 분석",
        other => other,
    }
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

/// 공지 리스트 행 1개 — 제공자 마크 · 제목(번역 있으면 번역, hover에 원문) ·
/// 우측에 상태/날짜/원문 링크.
fn announcement_row(
    ui: &mut egui::Ui,
    card: &AnnouncementCard<'_>,
    translations: &std::collections::HashMap<String, String>,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        provider_mark(ui, card.source, card.accent);
        // 우측 메타(상태·날짜·링크) 폭을 예약하고 제목은 남는 폭에서 truncate —
        // 긴 제목이 우측 메타를 밀어내지 않게 한다.
        let reserved = 250.0;
        let title_width = (ui.available_width() - reserved).max(120.0);
        let translated = translations.get(&card.incident.title);
        let title_text = translated.unwrap_or(&card.incident.title);
        let title = ui.add_sized(
            [title_width, 20.0],
            egui::Label::new(egui::RichText::new(title_text).strong())
                .truncate()
                .halign(egui::Align::LEFT),
        );
        if let Some(_translated) = translated {
            // 번역 표시 중 — 원문은 hover로 보존.
            title.on_hover_text(&card.incident.title);
        } else {
            title.on_hover_text(title_text);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.hyperlink_to("원문 →", &card.incident.url);
            ui.weak(&card.incident.date);
            ui.weak(incident_status_label(&card.incident.status));
        });
    });
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
        for resource in &row.session_resources {
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

/// indicator → 점등 색. 미조회(None)/미지 값은 회색.
fn indicator_color(ui: &egui::Ui, provider: Option<&ProviderStatus>) -> egui::Color32 {
    match provider.map(|p| p.indicator) {
        Some(ServiceIndicator::Operational) => egui::Color32::from_rgb(0x55, 0xc8, 0x79),
        Some(ServiceIndicator::Minor) => egui::Color32::from_rgb(0xe7, 0x9a, 0x3b),
        Some(ServiceIndicator::Major) | Some(ServiceIndicator::Critical) => {
            egui::Color32::from_rgb(0xed, 0x5b, 0x61)
        }
        Some(ServiceIndicator::Unknown) | None => ui.visuals().weak_text_color(),
    }
}

/// 상태바의 서비스 점등 1개 — 점 + 이름, hover에 상태 문구, 클릭 시 상태 페이지.
fn service_status_light(
    ui: &mut egui::Ui,
    name: &str,
    provider: Option<&ProviderStatus>,
    page_url: &str,
) {
    status_dot(ui, indicator_color(ui, provider));
    let hover = match provider {
        Some(p) => p.description.clone(),
        None => "상태 확인 중…".to_owned(),
    };
    let label = ui
        .add(egui::Label::new(egui::RichText::new(name).weak()).sense(egui::Sense::click()))
        .on_hover_text(format!("{hover}\n클릭: {page_url}"));
    if label.clicked() {
        ui.ctx().open_url(egui::OpenUrl::new_tab(page_url));
    }
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
