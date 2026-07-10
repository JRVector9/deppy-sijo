#[derive(Debug, Clone, PartialEq)]
pub struct ActivityWorkspaceRow {
    pub id: String,
    pub name: String,
    pub state: ActivityWorkspaceState,
    pub session_count: usize,
    pub pending_events: usize,
    pub input_pressure: Option<runtime::PtyInputPressure>,
    pub backgrounded_for_secs: Option<u64>,
    pub auto_suspend_remaining_secs: Option<u64>,
    pub resource: Option<runtime::ProcessResourceSnapshot>,
    pub session_resources: Vec<runtime::SessionResourceUsage>,
    /// pane(세션)별 모니터링 서브행 — 워크스페이스 행 아래 들여쓰기로 렌더(2026-07-08).
    pub sessions: Vec<ActivitySessionRow>,
}

/// pane(세션) 하나의 모니터링 행. 활성 워크스페이스는 제목·에이전트·상태까지,
/// warm은 제목·자원만 채워진다(감지 워커가 활성에서만 돈다).
#[derive(Debug, Clone, PartialEq)]
pub struct ActivitySessionRow {
    pub name: String,
    /// "Codex · gpt-5.5 · xhigh" — 에이전트가 아니면 None(셸).
    pub agent_line: Option<String>,
    /// "실행 중 · ctx 69%" — warm/셸은 None.
    pub status_line: Option<String>,
    /// 세션별 자원 샘플 (자식 프로세스 트리 합산).
    pub resource: Option<runtime::SessionResourceUsage>,
    /// 세션별 마지막 입력 backpressure 신호.
    pub pressure: Option<runtime::PtyInputPressure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityWorkspaceState {
    Active,
    Warm,
    Suspended,
}

pub enum ActivityAction {
    SwitchWorkspace(String),
    /// 모든 워크스페이스(활성+warm)의 터미널 렌더 캐시 비우기 — 작업/프로세스에 무해.
    ClearRenderCaches,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ActivityFilter {
    #[default]
    All,
    Active,
    Warm,
    Warning,
}

pub struct ActivityUi {
    filter: ActivityFilter,
    /// 직전 렌더 frame. 한 frame 이상 Activity가 렌더되지 않았으면 새 진입으로 보고
    /// `전체` 필터로 복귀한다.
    last_render_frame: Option<u64>,
}

impl ActivityUi {
    pub fn new() -> Self {
        Self {
            filter: ActivityFilter::All,
            last_render_frame: None,
        }
    }

    /// 창 프레임 없이 본문만 렌더한다 (통합 설정 창 우측 패널용, 2026-07-06).
    pub fn contents(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        rows: &[ActivityWorkspaceRow],
    ) -> Option<ActivityAction> {
        self.begin_visit_frame(ui.ctx().cumulative_frame_nr());
        let mut action = None;
        egui::Frame::NONE
            .inner_margin(egui::Margin {
                left: 26,
                right: 26,
                top: 20,
                bottom: 40,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(catalog.t("top.activity", &[]))
                            .strong()
                            .size(15.0),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // 안전한 렌더 캐시만 비운다. 실행 프로세스·스크롤 기록은 유지된다.
                        if ui
                            .button(catalog.t("activity.clear_caches", &[]))
                            .on_hover_text(catalog.t("activity.clear_caches_hint", &[]))
                            .clicked()
                        {
                            action = Some(ActivityAction::ClearRenderCaches);
                        }
                    });
                });
                ui.add_space(16.0);
                activity_hairline(ui);
                ui.add_space(16.0);

                let summary = activity_summary(rows);
                summary_cards(ui, catalog, summary);
                ui.add_space(18.0);

                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(catalog.t("activity.workspaces", &[]))
                            .strong()
                            .size(14.0),
                    );
                    ui.add_space(10.0);
                    for (filter, key) in [
                        (ActivityFilter::All, "activity.filter.all"),
                        (ActivityFilter::Active, "activity.filter.active"),
                        (ActivityFilter::Warm, "activity.filter.warm"),
                        (ActivityFilter::Warning, "activity.filter.warning"),
                    ] {
                        if ui
                            .selectable_label(self.filter == filter, catalog.t(key, &[]))
                            .clicked()
                        {
                            self.filter = filter;
                        }
                    }
                });
                ui.add_space(8.0);
                activity_hairline(ui);
                ui.add_space(10.0);

                let filtered: Vec<_> = rows
                    .iter()
                    .filter(|row| activity_filter_matches(self.filter, row))
                    .collect();
                if filtered.is_empty() {
                    ui.add_space(10.0);
                    ui.weak(catalog.t("activity.empty_filtered", &[]));
                    return;
                }
                for row in filtered {
                    workspace_card(ui, catalog, row, &mut action);
                    ui.add_space(10.0);
                }
            });
        action
    }

    fn begin_visit_frame(&mut self, frame: u64) {
        let entering = self
            .last_render_frame
            .is_none_or(|last| frame > last.saturating_add(1));
        if entering {
            self.filter = ActivityFilter::All;
        }
        self.last_render_frame = Some(frame);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ActivitySummary {
    workspaces: usize,
    active: usize,
    warm: usize,
    sessions: usize,
    cpu_percent: Option<f32>,
    rss_bytes: u64,
    warnings: usize,
}

fn activity_summary(rows: &[ActivityWorkspaceRow]) -> ActivitySummary {
    let mut summary = ActivitySummary {
        workspaces: rows.len(),
        active: 0,
        warm: 0,
        sessions: 0,
        cpu_percent: None,
        rss_bytes: 0,
        warnings: 0,
    };
    let mut cpu = 0.0;
    let mut cpu_seen = false;
    for row in rows {
        match row.state {
            ActivityWorkspaceState::Active => summary.active += 1,
            ActivityWorkspaceState::Warm | ActivityWorkspaceState::Suspended => summary.warm += 1,
        }
        summary.sessions += row.session_count;
        if let Some(resource) = &row.resource {
            summary.rss_bytes = summary.rss_bytes.saturating_add(resource.rss_bytes);
            if let Some(value) = resource.cpu_percent {
                cpu += value;
                cpu_seen = true;
            }
        }
        for resource in &row.session_resources {
            summary.rss_bytes = summary.rss_bytes.saturating_add(resource.rss_bytes);
            if let Some(value) = resource.cpu_percent {
                cpu += value;
                cpu_seen = true;
            }
        }
        if workspace_has_warning(row) {
            summary.warnings += 1;
        }
    }
    summary.cpu_percent = cpu_seen.then_some(cpu);
    summary
}

fn summary_cards(ui: &mut egui::Ui, catalog: &i18n::Catalog, summary: ActivitySummary) {
    let cpu = summary
        .cpu_percent
        .map(|value| format!("{value:.1}%"))
        .unwrap_or_else(|| catalog.t("activity.cpu_pending", &[]));
    let values = [
        (
            catalog.t("activity.summary.workspaces", &[]),
            summary.workspaces.to_string(),
            catalog.t(
                "activity.summary.workspace_detail",
                &[
                    ("active", &summary.active.to_string()),
                    ("warm", &summary.warm.to_string()),
                ],
            ),
        ),
        (
            catalog.t("activity.summary.sessions", &[]),
            summary.sessions.to_string(),
            catalog.t("activity.summary.sessions_detail", &[]),
        ),
        (
            catalog.t("activity.summary.cpu", &[]),
            cpu,
            catalog.t("activity.summary.cpu_detail", &[]),
        ),
        (
            catalog.t("activity.summary.memory", &[]),
            format_bytes(summary.rss_bytes),
            catalog.t(
                "activity.summary.warning_detail",
                &[("count", &summary.warnings.to_string())],
            ),
        ),
    ];
    ui.columns(4, |columns| {
        for (column, (label, value, detail)) in columns.iter_mut().zip(values) {
            let fill = column.visuals().panel_fill;
            let border = column.visuals().widgets.noninteractive.bg_stroke;
            egui::Frame::NONE
                .fill(fill)
                .stroke(border)
                .inner_margin(egui::Margin::symmetric(12, 10))
                .show(column, |ui| {
                    ui.set_min_height(72.0);
                    ui.weak(egui::RichText::new(label).size(12.0));
                    ui.add_space(5.0);
                    ui.label(egui::RichText::new(value).strong().size(20.0));
                    ui.add_space(3.0);
                    ui.weak(egui::RichText::new(detail).size(11.0));
                });
        }
    });
}

fn activity_filter_matches(filter: ActivityFilter, row: &ActivityWorkspaceRow) -> bool {
    match filter {
        ActivityFilter::All => true,
        ActivityFilter::Active => row.state == ActivityWorkspaceState::Active,
        ActivityFilter::Warm => row.state != ActivityWorkspaceState::Active,
        ActivityFilter::Warning => workspace_has_warning(row),
    }
}

fn workspace_has_warning(row: &ActivityWorkspaceRow) -> bool {
    row.input_pressure.is_some()
        || row
            .resource
            .as_ref()
            .is_some_and(|r| r.high_cpu || r.high_rss)
        || row
            .session_resources
            .iter()
            .any(|resource| resource.high_cpu || resource.high_rss)
        || row
            .sessions
            .iter()
            .any(|session| session.pressure.is_some())
}

fn activity_hairline(ui: &mut egui::Ui) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        ui.painter().round_to_pixel_center(rect.center().y),
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
}

fn workspace_card(
    ui: &mut egui::Ui,
    catalog: &i18n::Catalog,
    row: &ActivityWorkspaceRow,
    action: &mut Option<ActivityAction>,
) {
    let fill = ui.visuals().panel_fill;
    let border = ui.visuals().widgets.noninteractive.bg_stroke;
    egui::Frame::NONE
        .fill(fill)
        .stroke(border)
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(&row.name).strong().size(14.0));
                    ui.add_space(4.0);
                    state_label(ui, catalog, row);
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if row.state == ActivityWorkspaceState::Active {
                        ui.weak(catalog.t("activity.current", &[]));
                    } else if ui.button(catalog.t("activity.switch", &[])).clicked() {
                        *action = Some(ActivityAction::SwitchWorkspace(row.id.clone()));
                    }
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new(resource_label(
                            catalog,
                            row.resource.as_ref(),
                            &row.session_resources,
                        ))
                        .monospace()
                        .size(12.0),
                    );
                    if row.pending_events > 0 {
                        ui.weak(format!(
                            "{} {}",
                            catalog.t("activity.queue", &[]),
                            row.pending_events
                        ));
                    }
                    if let Some(pressure) = &row.input_pressure {
                        input_pressure_badge(ui, catalog, pressure);
                    }
                });
            });

            if !row.sessions.is_empty() {
                ui.add_space(10.0);
                activity_hairline(ui);
                for session in &row.sessions {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.set_min_height(38.0);
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(&session.name).size(13.0));
                            if let Some(agent) = &session.agent_line {
                                ui.weak(egui::RichText::new(agent).size(11.0));
                            }
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(session_resource_text(
                                    catalog,
                                    session.resource.as_ref(),
                                ))
                                .monospace()
                                .size(11.0),
                            );
                            if let Some(pressure) = &session.pressure {
                                input_pressure_badge(ui, catalog, pressure);
                            }
                            if let Some(status) = &session.status_line {
                                ui.weak(egui::RichText::new(status).size(11.0));
                            }
                        });
                    });
                }
            } else {
                ui.add_space(8.0);
                ui.weak(catalog.t(
                    "activity.session_count",
                    &[("count", &row.session_count.to_string())],
                ));
            }
        });
}

/// 세션 서브행의 자원 셀 — 세션 트리(셸+자손) 합산 CPU/RSS(+프로세스 수, 높음 표시).
fn session_resource_text(
    catalog: &i18n::Catalog,
    usage: Option<&runtime::SessionResourceUsage>,
) -> String {
    let Some(u) = usage else {
        return String::new();
    };
    let cpu = u
        .cpu_percent
        .map(|v| format!("{v:.1}%"))
        .unwrap_or_else(|| catalog.t("activity.cpu_pending", &[]));
    let mut label = catalog.t(
        "activity.resource_label",
        &[("cpu", &cpu), ("rss", &format_bytes(u.rss_bytes))],
    );
    if u.process_count > 1 {
        label.push_str(&format!(" · {}p", u.process_count));
    }
    if u.high_cpu || u.high_rss {
        label.push_str(" · ");
        label.push_str(&catalog.t("activity.resource_high", &[]));
    }
    label
}

fn state_label(ui: &mut egui::Ui, catalog: &i18n::Catalog, row: &ActivityWorkspaceRow) {
    let key = match row.state {
        ActivityWorkspaceState::Active => "activity.state.active",
        ActivityWorkspaceState::Warm => "activity.state.warm",
        ActivityWorkspaceState::Suspended => "activity.state.suspended",
    };
    let mut text = catalog.t(key, &[]);
    if let Some(secs) = row.backgrounded_for_secs {
        text.push_str(" · ");
        text.push_str(&catalog.t(
            "activity.backgrounded_for",
            &[("seconds", &secs.to_string())],
        ));
    }
    if let Some(secs) = row.auto_suspend_remaining_secs {
        text.push_str(" · ");
        text.push_str(&catalog.t(
            "activity.auto_suspend_in",
            &[("seconds", &secs.to_string())],
        ));
    }
    match row.state {
        ActivityWorkspaceState::Active => {
            ui.colored_label(ui.visuals().selection.stroke.color, text);
        }
        ActivityWorkspaceState::Warm => {
            ui.label(text);
        }
        ActivityWorkspaceState::Suspended => {
            ui.weak(text);
        }
    };
}

fn resource_label(
    catalog: &i18n::Catalog,
    snapshot: Option<&runtime::ProcessResourceSnapshot>,
    session_resources: &[runtime::SessionResourceUsage],
) -> String {
    let Some(snapshot) = snapshot else {
        return catalog.t("activity.resource_unavailable", &[]);
    };
    let cpu = snapshot
        .cpu_percent
        .map(|value| format!("{value:.1}%"))
        .unwrap_or_else(|| catalog.t("activity.cpu_pending", &[]));
    let rss = format_bytes(snapshot.rss_bytes);
    let mut label = catalog.t("activity.resource_label", &[("cpu", &cpu), ("rss", &rss)]);
    if snapshot.high_cpu || snapshot.high_rss {
        label.push_str(" · ");
        label.push_str(&catalog.t("activity.resource_high", &[]));
    }
    if !session_resources.is_empty() {
        let child_rss = session_resources
            .iter()
            .fold(0u64, |acc, usage| acc.saturating_add(usage.rss_bytes));
        let child_cpu_seen = session_resources
            .iter()
            .any(|usage| usage.cpu_percent.is_some());
        let child_cpu = child_cpu_seen
            .then(|| {
                session_resources
                    .iter()
                    .filter_map(|usage| usage.cpu_percent)
                    .sum::<f32>()
            })
            .map(|value| format!("{value:.1}%"))
            .unwrap_or_else(|| catalog.t("activity.cpu_pending", &[]));
        label.push_str(" · ");
        label.push_str(&catalog.t(
            "activity.child_resource_label",
            &[("cpu", &child_cpu), ("rss", &format_bytes(child_rss))],
        ));
        if session_resources
            .iter()
            .any(|usage| usage.high_cpu || usage.high_rss)
        {
            label.push_str(" · ");
            label.push_str(&catalog.t("activity.resource_high", &[]));
        }
    }
    label
}

/// PTY 입력 backpressure 뱃지 — 마지막 pressure 신호를 색으로 구분해 표시한다.
/// QueueFull은 큐가 빠지면 회복되는 일시 상태(경고색), 나머지 사유는 에러색.
fn input_pressure_badge(
    ui: &mut egui::Ui,
    catalog: &i18n::Catalog,
    pressure: &runtime::PtyInputPressure,
) {
    let color = if input_pressure_is_transient(pressure.reason) {
        ui.visuals().warn_fg_color
    } else {
        ui.visuals().error_fg_color
    };
    let text = input_pressure_badge_text(catalog, pressure);
    egui::Frame::new()
        .fill(color.gamma_multiply(0.15))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(6, 1))
        .show(ui, |ui| {
            ui.colored_label(color, egui::RichText::new(text).small());
        })
        .response
        .on_hover_text(catalog.t(
            "workspace.input_pressure",
            &[
                ("queued", &format_bytes(pressure.queued_bytes as u64)),
                ("max", &format_bytes(pressure.max_bytes as u64)),
            ],
        ));
}

/// QueueFull만 일시적(재시도 가능) — SessionClosed/WriterUnavailable/PayloadTooLarge는
/// 재시도로 회복되지 않는다 (input_queue.rs 경계 정의와 동일).
fn input_pressure_is_transient(reason: runtime::PtyInputRejectReason) -> bool {
    matches!(reason, runtime::PtyInputRejectReason::QueueFull)
}

fn input_pressure_badge_text(
    catalog: &i18n::Catalog,
    pressure: &runtime::PtyInputPressure,
) -> String {
    catalog.t(
        "activity.input_pressure",
        &[
            ("queued", &format_bytes(pressure.queued_bytes as u64)),
            ("max", &format_bytes(pressure.max_bytes as u64)),
        ],
    )
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_format_uses_mib_and_gib() {
        assert_eq!(format_bytes(512 * 1024 * 1024), "512.0 MiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }

    #[test]
    fn empty_activity_summary_is_zeroed() {
        let summary = activity_summary(&[]);
        assert_eq!(summary.workspaces, 0);
        assert_eq!(summary.sessions, 0);
        assert_eq!(summary.cpu_percent, None);
        assert_eq!(summary.rss_bytes, 0);
        assert_eq!(summary.warnings, 0);
    }

    #[test]
    fn activity_reentry_resets_filter_to_all_immediately() {
        let mut ui = ActivityUi::new();
        ui.filter = ActivityFilter::Warning;
        ui.begin_visit_frame(10);
        assert_eq!(ui.filter, ActivityFilter::All);

        ui.filter = ActivityFilter::Active;
        ui.begin_visit_frame(11);
        assert_eq!(ui.filter, ActivityFilter::Active);

        ui.begin_visit_frame(13);
        assert_eq!(ui.filter, ActivityFilter::All);
    }

    #[test]
    fn resource_label_marks_high_usage() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let snapshot = runtime::ProcessResourceSnapshot {
            pid: 1,
            sampled_at_ms: 0,
            rss_bytes: 2 * 1024 * 1024 * 1024,
            cpu_percent: Some(250.0),
            high_cpu: true,
            high_rss: true,
        };
        let label = resource_label(&catalog, Some(&snapshot), &[]);
        assert!(label.contains("250.0%"));
        assert!(label.contains("2.0 GiB"));
        assert!(label.contains("High"));
    }

    #[test]
    fn resource_label_includes_child_usage() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let snapshot = runtime::ProcessResourceSnapshot {
            pid: 1,
            sampled_at_ms: 0,
            rss_bytes: 512 * 1024 * 1024,
            cpu_percent: Some(10.0),
            high_cpu: false,
            high_rss: false,
        };
        let child = runtime::SessionResourceUsage {
            session: runtime::SessionId(7),
            pid: Some(100),
            process_group: Some(100),
            identity_source: runtime::ProcessIdentitySource::PortablePty,
            sampled_at_ms: 0,
            process_count: 2,
            rss_bytes: 128 * 1024 * 1024,
            cpu_percent: Some(25.0),
            high_cpu: true,
            high_rss: false,
        };
        let label = resource_label(&catalog, Some(&snapshot), &[child]);
        assert!(label.contains("Child CPU 25.0%"));
        assert!(label.contains("128.0 MiB"));
        assert!(label.contains("High"));
    }

    #[test]
    fn input_pressure_badge_text_formats_queued_and_max_bytes() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let pressure = runtime::PtyInputPressure {
            attempted_bytes: 10,
            queued_bytes: 1024,
            queued_messages: 1,
            max_bytes: 4 * 1024,
            max_messages: 16,
            reason: runtime::PtyInputRejectReason::QueueFull,
        };
        let text = input_pressure_badge_text(&catalog, &pressure);
        assert!(text.contains("input"));
        assert!(text.contains("1.0 KiB"));
        assert!(text.contains("4.0 KiB"));
    }

    /// 뱃지 색 경계: QueueFull만 경고(일시적), 나머지는 에러(재시도 불가).
    #[test]
    fn input_pressure_severity_boundary() {
        assert!(input_pressure_is_transient(
            runtime::PtyInputRejectReason::QueueFull
        ));
        for reason in [
            runtime::PtyInputRejectReason::SessionClosed,
            runtime::PtyInputRejectReason::WriterUnavailable,
            runtime::PtyInputRejectReason::PayloadTooLarge,
        ] {
            assert!(!input_pressure_is_transient(reason));
        }
    }
}
