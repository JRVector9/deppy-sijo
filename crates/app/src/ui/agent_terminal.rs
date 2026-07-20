use super::activity::{ActivityWorkspaceRow, ActivityWorkspaceState};
use crate::status_feed::{ProviderStatus, ServiceIndicator, StatusFeedSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentTerminalView {
    Home,
    /// 「작업함」 전체 페이지 — 벨 팝오버와 같은 대기 카드 + 전체 알림 목록
    /// (2026-07-18 사용자 확정 디자인, 사이드바 하단 nav로 진입).
    Inbox,
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
const ANNOUNCEMENT_ROW_HEIGHT: f32 = 48.0;

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

    pub fn home(
        &mut self,
        ui: &mut egui::Ui,
        feed: &StatusFeedSnapshot,
        translations: NoticeTranslations<'_>,
        slack_status: super::connectors::SlackMcpStatus,
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
                                if connections_panel(&mut columns[1], slack_status, catalog) {
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
                            if connections_panel(ui, slack_status, catalog) {
                                action = Some(HomeAction::Connectors);
                            }
                        }
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
        catalog: &i18n::Catalog,
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
                ui.weak(catalog.t("status_bar.connected", &[]));
                ui.separator();
                ui.weak(catalog.t(
                    "status_bar.workspaces",
                    &[("count", &totals.workspaces.to_string())],
                ));
                ui.separator();
                ui.weak(catalog.t(
                    "status_bar.sessions",
                    &[("count", &totals.sessions.to_string())],
                ));
                ui.separator();
                // 등록·활성화된 MCP 서버 수 (2026-07-18 사용자 요청).
                ui.weak(catalog.t("status_bar.mcp", &[("count", &mcp_count.to_string())]))
                    .on_hover_text(catalog.t("status_bar.mcp_hover", &[]));
                // 핵심 서비스 상태 점등 — Claude/OpenAI/GitHub를 5분마다 폴링하고
                // 클릭하면 각 공식 상태 페이지를 연다.
                ui.separator();
                service_status_light(
                    ui,
                    "Claude",
                    feed.claude.as_ref(),
                    crate::status_feed::CLAUDE_STATUS_URL,
                    catalog,
                );
                service_status_light(
                    ui,
                    "OpenAI",
                    feed.openai.as_ref(),
                    crate::status_feed::OPENAI_STATUS_URL,
                    catalog,
                );
                service_status_light(
                    ui,
                    "GitHub",
                    feed.github.as_ref(),
                    crate::status_feed::GITHUB_STATUS_URL,
                    catalog,
                );
                if waiting > 0 {
                    ui.separator();
                    ui.colored_label(
                        egui::Color32::from_rgb(0xe7, 0x9a, 0x3b),
                        catalog.t("status_bar.waiting", &[("count", &waiting.to_string())]),
                    );
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
                    ui.weak(memory).on_hover_text(catalog.t(
                        "status_bar.memory_hover",
                        &[("sessions", &super::format_bytes(totals.session_rss_bytes))],
                    ));
                    ui.separator();
                    ui.weak(cpu);
                    ui.separator();
                    ui.weak(match self.view {
                        AgentTerminalView::Home => catalog.t("status_bar.view.home", &[]),
                        AgentTerminalView::Inbox => catalog.t("status_bar.view.inbox", &[]),
                        AgentTerminalView::Terminal => catalog.t("status_bar.view.terminal", &[]),
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
            ui.horizontal_wrapped(|ui| {
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
                    if ui
                        .small_button("⟳")
                        .on_hover_text(catalog.t("home.notices.refresh_hover", &[]))
                        .clicked()
                    {
                        refresh_clicked = true;
                    }
                });
            });
            ui.add_space(12.0);
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
                    // 고정해 48px × 5행 viewport가 정확히 다섯 행과 일치하게 한다.
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
    slack_status: super::connectors::SlackMcpStatus,
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
                super::connectors::slack_mark(ui);
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("Slack").strong());
                    ui.weak(catalog.t("home.connections.slack_detail", &[]));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (label, color) = match slack_status {
                        super::connectors::SlackMcpStatus::NotConfigured => (
                            catalog.t("home.connections.not_connected", &[]),
                            ui.visuals().weak_text_color(),
                        ),
                        super::connectors::SlackMcpStatus::Ready => (
                            catalog.t("connectors.slack.ready", &[]),
                            egui::Color32::from_rgb(0x4c, 0xa8, 0xdf),
                        ),
                        super::connectors::SlackMcpStatus::Checking => (
                            catalog.t("connectors.checking", &[]),
                            egui::Color32::from_rgb(0x4c, 0xa8, 0xdf),
                        ),
                        super::connectors::SlackMcpStatus::NeedsAuth => (
                            catalog.t("connectors.needs_auth", &[]),
                            egui::Color32::from_rgb(0xe7, 0x9a, 0x3b),
                        ),
                        super::connectors::SlackMcpStatus::Connected { tools } => (
                            catalog.t(
                                "connectors.connected_tools",
                                &[("count", &tools.to_string())],
                            ),
                            egui::Color32::from_rgb(0x55, 0xc8, 0x79),
                        ),
                        super::connectors::SlackMcpStatus::Failed => (
                            catalog.t("home.connections.failed", &[]),
                            egui::Color32::from_rgb(0xed, 0x5b, 0x61),
                        ),
                    };
                    ui.colored_label(color, label);
                    status_dot(ui, color);
                });
            });
        });
    manage_clicked
}

/// 홈 공지 카드 1장 — 상태 페이지 인시던트 1건.
#[derive(Clone, Copy)]
struct AnnouncementCard<'a> {
    source: &'static str,
    incident: &'a crate::status_feed::IncidentNotice,
}

/// Statuspage 인시던트 상태 → 로케일 라벨 (미지 값은 원문 그대로).
fn incident_status_label(catalog: &i18n::Catalog, status: &str) -> String {
    match status {
        "resolved" => catalog.t("home.notices.status.resolved", &[]),
        "investigating" => catalog.t("home.notices.status.investigating", &[]),
        "identified" => catalog.t("home.notices.status.identified", &[]),
        "monitoring" => catalog.t("home.notices.status.monitoring", &[]),
        "postmortem" => catalog.t("home.notices.status.postmortem", &[]),
        "trending" => catalog.t("home.notices.status.trending", &[]),
        "release" => catalog.t("home.notices.status.release", &[]),
        "update" => catalog.t("home.notices.status.update", &[]),
        other => other.to_owned(),
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

/// 공지 리스트 행 1개 — 중복되는 제공자 장식 없이 제목·상태·날짜·원문만 표시한다.
fn announcement_row(
    ui: &mut egui::Ui,
    card: &AnnouncementCard<'_>,
    translations: &crate::notice_translate::TranslationCache,
    locale: &str,
    catalog: &i18n::Catalog,
) {
    // allocate_ui의 response rect는 자식 content 폭으로 줄 수 있어 divider 길이가 행마다
    // 달라졌다. 먼저 viewport 전체 폭을 확정하고 같은 rect로 content와 divider를 그린다.
    let (row_rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ANNOUNCEMENT_ROW_HEIGHT),
        egui::Sense::hover(),
    );
    let content_rect = row_rect.shrink2(egui::vec2(0.0, 2.0));
    let mut row_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(content_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    row_ui.set_clip_rect(content_rect.intersect(ui.clip_rect()));
    row_ui.spacing_mut().item_spacing.y = 1.0;

    let translated = translations.get(card.source, locale, &card.incident.title);
    let title_text = translated.unwrap_or(&card.incident.title);
    let title = row_ui.add_sized(
        [row_ui.available_width(), 20.0],
        egui::Label::new(egui::RichText::new(title_text).strong())
            .truncate()
            .halign(egui::Align::LEFT),
    );
    if translated.is_some() {
        title.on_hover_text(&card.incident.title);
    } else {
        title.on_hover_text(title_text);
    }
    row_ui.horizontal(|ui| {
        ui.weak(incident_status_label(catalog, &card.incident.status));
        if !card.incident.date.is_empty() {
            ui.weak(&card.incident.date);
        }
        ui.hyperlink_to(
            catalog.t("home.notices.original_link", &[]),
            &card.incident.url,
        );
    });

    ui.painter().hline(
        row_rect.x_range(),
        row_rect.bottom() - 0.5,
        ui.visuals().widgets.noninteractive.bg_stroke,
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
    catalog: &i18n::Catalog,
) {
    status_dot(ui, indicator_color(ui, provider));
    let hover = match provider {
        Some(p) => p.description.clone(),
        None => catalog.t("status_bar.service_checking", &[]),
    };
    let click = catalog.t("status_bar.service_click", &[("url", page_url)]);
    let label = ui
        .add(egui::Label::new(egui::RichText::new(name).weak()).sense(egui::Sense::click()))
        .on_hover_text(format!("{hover}\n{click}"));
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
    fn kittest_하단상태바에_claude_openai_github가_함께_표시된다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let feed = StatusFeedSnapshot::default();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            AgentTerminalUi::new().status_bar(ui, &[], 0, 0, &feed, &catalog);
        });
        harness.run();

        harness.get_by_label("Claude");
        harness.get_by_label("OpenAI");
        harness.get_by_label("GitHub");
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
                    super::super::connectors::SlackMcpStatus::NotConfigured,
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
        let all = harness.get_by_label("All").rect().left();
        let openai = harness.get_by_label("OpenAI").rect().left();
        let anthropic = harness.get_by_label("Anthropic").rect().left();
        let grok = harness.get_by_label("Grok").rect().left();
        let hugging_face = harness.get_by_label("Hugging Face").rect().left();
        assert!(all < openai && openai < anthropic && anthropic < grok && grok < hugging_face);
        assert!(harness.query_by_label("MLX").is_none());
        assert!(harness.query_by_label("Home").is_none());

        harness.get_by_label("Manage").click();
        harness.run();
        assert_eq!(harness.state().1, vec![HomeAction::Connectors]);
    }

    #[test]
    fn kittest_home_공지는_공급자장식없이_정확히_5행을_보인다() {
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
                    super::super::connectors::SlackMcpStatus::NotConfigured,
                    &catalog,
                );
            },
            AgentTerminalUi::new(),
        );
        harness.run();

        assert_eq!(ANNOUNCEMENT_VISIBLE_ROWS, 5);
        assert!(harness.query_by_label("Claude").is_none());
        let first_top = harness.get_by_label("Announcement 1").rect().top();
        for index in 2..=5 {
            harness.get_by_label(&format!("Announcement {index}"));
        }
        // show_rows는 경계의 다음 행을 접근성 트리에 준비할 수 있다. 실제 위치를 재서
        // 여섯째 행이 정확히 5행 viewport 밖에서 시작하는지 확인한다.
        let sixth_top = harness.get_by_label("Announcement 6").rect().top();
        let viewport_height = ANNOUNCEMENT_ROW_HEIGHT * ANNOUNCEMENT_VISIBLE_ROWS as f32;
        assert!(sixth_top - first_top >= viewport_height - 0.5);
    }
}
