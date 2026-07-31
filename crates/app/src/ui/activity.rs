use super::format_bytes;
use std::sync::Arc;

pub const MAX_ACTIVITY_WORKSPACES: usize = 256;
pub const MAX_ACTIVITY_ITEMS: usize = 4_096;
pub const MAX_ACTIVITY_ITEMS_PER_WORKSPACE: usize = 256;
pub const MAX_ACTIVITY_TEXT_BYTES: usize = 32 * 1024;
pub const MAX_ACTIVITY_RETAINED_BYTES: usize = 4 * 1024 * 1024;

const REDACTED: &str = "[REDACTED]";
// Two usize words conservatively cover the strong/weak counters retained by an Arc allocation.
// Shared strings may therefore be counted more than once, which keeps the memory ceiling safe.
const ARC_ALLOCATION_OVERHEAD: usize = 2 * std::mem::size_of::<usize>();

#[derive(Clone, PartialEq)]
pub struct ActivityWorkspaceRow {
    pub workspace_id: Arc<str>,
    pub name: Arc<str>,
    pub metric_availability: ActivityMetricAvailability,
    pub state: ActivityWorkspaceState,
    pub session_count: usize,
    pub pending_events: usize,
    pub input_pressure: Option<runtime::PtyInputPressure>,
    pub backgrounded_for_secs: Option<u64>,
    pub auto_suspend_remaining_secs: Option<u64>,
    pub resource: Option<runtime::ProcessResourceSnapshot>,
    pub session_resources: Arc<[runtime::SessionResourceUsage]>,
    /// pane(세션)별 모니터링 서브행 — 워크스페이스 행 아래 들여쓰기로 렌더(2026-07-08).
    pub sessions: Arc<[ActivitySessionRow]>,
}

impl std::fmt::Debug for ActivityWorkspaceRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivityWorkspaceRow")
            .field("workspace_id", &REDACTED)
            .field("name", &REDACTED)
            .field("metric_availability", &self.metric_availability)
            .field("state", &self.state)
            .field("session_count", &self.session_count)
            .field("pending_events", &self.pending_events)
            .field("input_pressure", &self.input_pressure)
            .field("backgrounded_for_secs", &self.backgrounded_for_secs)
            .field(
                "auto_suspend_remaining_secs",
                &self.auto_suspend_remaining_secs,
            )
            .field("resource", &self.resource)
            .field("session_resources", &self.session_resources)
            .field("sessions", &self.sessions)
            .finish()
    }
}

/// pane(세션) 하나의 모니터링 행. 활성 워크스페이스는 제목·에이전트·상태까지,
/// warm은 제목·자원만 채워진다(감지 워커가 활성에서만 돈다).
#[derive(Clone, PartialEq)]
pub struct ActivitySessionRow {
    pub session: Option<runtime::SessionId>,
    pub name: Arc<str>,
    pub metric_availability: ActivityMetricAvailability,
    /// "Codex · gpt-5.5 · xhigh" — 에이전트가 아니면 None(셸).
    pub agent_line: Option<Arc<str>>,
    /// "실행 중 · ctx 69%" — warm/셸은 None.
    pub status_line: Option<Arc<str>>,
    /// 세션별 자원 샘플 (자식 프로세스 트리 합산).
    pub resource: Option<runtime::SessionResourceUsage>,
    /// 세션별 마지막 입력 backpressure 신호.
    pub pressure: Option<runtime::PtyInputPressure>,
    /// 자식 프로세스 폭주 확정 (로드맵 B1/B2) — High 뱃지와 구분되는 강조 표시.
    pub storm: bool,
}

impl std::fmt::Debug for ActivitySessionRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivitySessionRow")
            .field("session", &self.session)
            .field("name", &REDACTED)
            .field("metric_availability", &self.metric_availability)
            .field("agent_line", &self.agent_line.as_ref().map(|_| REDACTED))
            .field("status_line", &self.status_line.as_ref().map(|_| REDACTED))
            .field("resource", &self.resource)
            .field("pressure", &self.pressure)
            .field("storm", &self.storm)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivitySnapshotError {
    TooManyWorkspaces,
    TooManyItems,
    TooManyItemsInWorkspace,
    TextTooLong,
    TooManyRetainedBytes,
}

impl std::fmt::Display for ActivitySnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TooManyWorkspaces => "activity workspace limit exceeded",
            Self::TooManyItems => "activity item limit exceeded",
            Self::TooManyItemsInWorkspace => "activity per-workspace item limit exceeded",
            Self::TextTooLong => "activity text limit exceeded",
            Self::TooManyRetainedBytes => "activity retained-byte limit exceeded",
        })
    }
}

impl std::error::Error for ActivitySnapshotError {}

/// Immutable, shallow-cloneable render projection. Construction validates every nested collection
/// and retained string/allocation byte before the snapshot can reach a frame.
#[derive(Clone, PartialEq)]
pub struct ActivitySnapshot {
    rows: Arc<[ActivityWorkspaceRow]>,
    item_count: usize,
    retained_bytes: usize,
}

impl ActivitySnapshot {
    pub fn empty() -> Self {
        Self {
            rows: Arc::from([]),
            item_count: 0,
            retained_bytes: 0,
        }
    }

    pub fn try_new(
        rows: impl IntoIterator<Item = ActivityWorkspaceRow>,
    ) -> Result<Self, ActivitySnapshotError> {
        let iter = rows.into_iter();
        let mut rows = Vec::with_capacity(iter.size_hint().0.min(MAX_ACTIVITY_WORKSPACES));
        for row in iter {
            if rows.len() == MAX_ACTIVITY_WORKSPACES {
                return Err(ActivitySnapshotError::TooManyWorkspaces);
            }
            rows.push(row);
        }

        let mut item_count = rows.len();
        let rows_bytes = rows
            .len()
            .checked_mul(std::mem::size_of::<ActivityWorkspaceRow>())
            .ok_or(ActivitySnapshotError::TooManyRetainedBytes)?;
        let mut retained_bytes = checked_retained_add(ARC_ALLOCATION_OVERHEAD, rows_bytes)?;

        for row in &rows {
            let workspace_items = row
                .sessions
                .len()
                .checked_add(row.session_resources.len())
                .ok_or(ActivitySnapshotError::TooManyItemsInWorkspace)?;
            if row.sessions.len() > MAX_ACTIVITY_ITEMS_PER_WORKSPACE
                || row.session_resources.len() > MAX_ACTIVITY_ITEMS_PER_WORKSPACE
                || workspace_items > MAX_ACTIVITY_ITEMS_PER_WORKSPACE
            {
                return Err(ActivitySnapshotError::TooManyItemsInWorkspace);
            }
            item_count = item_count
                .checked_add(workspace_items)
                .ok_or(ActivitySnapshotError::TooManyItems)?;
            if item_count > MAX_ACTIVITY_ITEMS {
                return Err(ActivitySnapshotError::TooManyItems);
            }

            retained_bytes = checked_retained_add(retained_bytes, ARC_ALLOCATION_OVERHEAD)?;
            retained_bytes = checked_retained_add(retained_bytes, row.workspace_id.len())?;
            retained_bytes = checked_retained_add(retained_bytes, ARC_ALLOCATION_OVERHEAD)?;
            retained_bytes = checked_retained_add(retained_bytes, row.name.len())?;
            retained_bytes = checked_retained_add(retained_bytes, ARC_ALLOCATION_OVERHEAD)?;
            retained_bytes = checked_retained_add(
                retained_bytes,
                row.sessions
                    .len()
                    .checked_mul(std::mem::size_of::<ActivitySessionRow>())
                    .ok_or(ActivitySnapshotError::TooManyRetainedBytes)?,
            )?;
            retained_bytes = checked_retained_add(retained_bytes, ARC_ALLOCATION_OVERHEAD)?;
            retained_bytes = checked_retained_add(
                retained_bytes,
                row.session_resources
                    .len()
                    .checked_mul(std::mem::size_of::<runtime::SessionResourceUsage>())
                    .ok_or(ActivitySnapshotError::TooManyRetainedBytes)?,
            )?;

            validate_text(row.workspace_id.as_ref())?;
            validate_text(row.name.as_ref())?;
            for session in row.sessions.iter() {
                validate_text(session.name.as_ref())?;
                retained_bytes = checked_retained_add(retained_bytes, ARC_ALLOCATION_OVERHEAD)?;
                retained_bytes = checked_retained_add(retained_bytes, session.name.len())?;
                for text in [&session.agent_line, &session.status_line]
                    .into_iter()
                    .flatten()
                {
                    validate_text(text.as_ref())?;
                    retained_bytes = checked_retained_add(retained_bytes, ARC_ALLOCATION_OVERHEAD)?;
                    retained_bytes = checked_retained_add(retained_bytes, text.len())?;
                }
            }
        }

        Ok(Self {
            rows: rows.into(),
            item_count,
            retained_bytes,
        })
    }

    pub fn rows(&self) -> &[ActivityWorkspaceRow] {
        &self.rows
    }
}

impl Default for ActivitySnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

impl std::fmt::Debug for ActivitySnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivitySnapshot")
            .field("workspaces", &self.rows.len())
            .field("item_count", &self.item_count)
            .field("retained_bytes", &self.retained_bytes)
            .finish_non_exhaustive()
    }
}

fn validate_text(text: &str) -> Result<(), ActivitySnapshotError> {
    if text.len() > MAX_ACTIVITY_TEXT_BYTES || text.as_bytes().contains(&0) {
        Err(ActivitySnapshotError::TextTooLong)
    } else {
        Ok(())
    }
}

fn checked_retained_add(current: usize, additional: usize) -> Result<usize, ActivitySnapshotError> {
    let total = current
        .checked_add(additional)
        .ok_or(ActivitySnapshotError::TooManyRetainedBytes)?;
    if total > MAX_ACTIVITY_RETAINED_BYTES {
        Err(ActivitySnapshotError::TooManyRetainedBytes)
    } else {
        Ok(total)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityWorkspaceState {
    Active,
    Warm,
    /// DB에는 존재하지만 현재 runtime/세션이 없는 워크스페이스.
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityMetricAvailability {
    Local,
    Pending,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "reserved for remote activity projections; local App projections must not fake it"
        )
    )]
    RemoteUnavailable,
}

pub enum ActivityAction {
    /// 모든 워크스페이스(활성+warm)의 터미널 렌더 캐시 비우기 — 작업/프로세스에 무해.
    ClearRenderCaches,
}

pub struct ActivityUi {}

impl ActivityUi {
    pub fn new() -> Self {
        Self {}
    }

    /// 창 프레임 없이 본문만 렌더한다 (통합 설정 창 우측 패널용, 2026-07-06).
    pub fn contents(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        snapshot: &ActivitySnapshot,
    ) -> Option<ActivityAction> {
        let rows = snapshot.rows();
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

                if rows.is_empty() {
                    ui.add_space(10.0);
                    ui.weak(catalog.t("activity.empty", &[]));
                    return;
                }
                for row in rows {
                    workspace_card(ui, catalog, row);
                    ui.add_space(10.0);
                }
            });
        action
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ActivitySummary {
    workspaces: usize,
    active: usize,
    warm: usize,
    idle: usize,
    sessions: usize,
    cpu_percent: Option<f32>,
    app_rss_bytes: u64,
    child_rss_bytes: u64,
    rss_bytes: u64,
    warnings: usize,
}

fn activity_summary(rows: &[ActivityWorkspaceRow]) -> ActivitySummary {
    let mut summary = ActivitySummary {
        workspaces: rows.len(),
        active: 0,
        warm: 0,
        idle: 0,
        sessions: 0,
        cpu_percent: None,
        app_rss_bytes: 0,
        child_rss_bytes: 0,
        rss_bytes: 0,
        warnings: 0,
    };
    let mut cpu = 0.0;
    let mut cpu_seen = false;
    // Runtime worker는 workspace마다 하나지만 모두 같은 in-process 앱 PID/RSS를
    // 샘플링한다. 요약에서는 PID별 한 번만 더해야 warm 수만큼 앱 메모리가 중복되지
    // 않는다. 채택은 pid별 **최신 샘플** — 워커마다 샘플 시점이 제각각이라 먼저 만난
    // 행을 쓰면 하단 상태바와 수치가 어긋난다(2026-07-18 사용자, 하단과 동일 규칙).
    let mut app_latest: std::collections::HashMap<u32, runtime::ProcessResourceSnapshot> =
        std::collections::HashMap::new();
    for row in rows {
        match row.state {
            ActivityWorkspaceState::Active => summary.active += 1,
            ActivityWorkspaceState::Warm => summary.warm += 1,
            ActivityWorkspaceState::Idle => summary.idle += 1,
        }
        summary.sessions += row.session_count;
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
            summary.child_rss_bytes = summary.child_rss_bytes.saturating_add(resource.rss_bytes);
            if let Some(value) = resource.cpu_percent {
                cpu += value;
                cpu_seen = true;
            }
        }
        if workspace_has_warning(row) {
            summary.warnings += 1;
        }
    }
    for snapshot in app_latest.values() {
        summary.app_rss_bytes = summary.app_rss_bytes.saturating_add(snapshot.rss_bytes);
        if let Some(value) = snapshot.cpu_percent {
            cpu += value;
            cpu_seen = true;
        }
    }
    summary.rss_bytes = summary
        .app_rss_bytes
        .saturating_add(summary.child_rss_bytes);
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
                    ("idle", &summary.idle.to_string()),
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
                "activity.summary.memory_detail",
                &[
                    ("app", &format_bytes(summary.app_rss_bytes)),
                    ("children", &format_bytes(summary.child_rss_bytes)),
                    ("count", &summary.warnings.to_string()),
                ],
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
            .any(|session| session.pressure.is_some() || session.storm)
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

fn workspace_card(ui: &mut egui::Ui, catalog: &i18n::Catalog, row: &ActivityWorkspaceRow) {
    let fill = ui.visuals().panel_fill;
    let border = ui.visuals().widgets.noninteractive.bg_stroke;
    let idle = row.state == ActivityWorkspaceState::Idle;
    egui::Frame::NONE
        .fill(fill)
        .stroke(border)
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(row.name.as_ref()).strong().size(14.0));
                    ui.add_space(4.0);
                    state_label(ui, catalog, row);
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if row.state == ActivityWorkspaceState::Active {
                        ui.weak(catalog.t("activity.current", &[]));
                    }
                    ui.add_space(10.0);
                    let resources = if idle {
                        zero_resource_label(catalog)
                    } else {
                        resource_label(catalog, row.resource.as_ref(), &row.session_resources)
                    };
                    ui.label(egui::RichText::new(resources).monospace().size(12.0));
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
                for session in row.sessions.iter() {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.set_min_height(38.0);
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(session.name.as_ref()).size(13.0));
                            if let Some(agent) = &session.agent_line {
                                ui.weak(egui::RichText::new(agent.as_ref()).size(11.0));
                            }
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let resources = match session.resource.as_ref() {
                                Some(resource) => session_resource_text(catalog, Some(resource)),
                                None if idle => zero_resource_label(catalog),
                                None => String::new(),
                            };
                            ui.label(egui::RichText::new(resources).monospace().size(11.0));
                            if session.storm {
                                ui.label(
                                    egui::RichText::new(catalog.t("activity.resource_storm", &[]))
                                        .strong()
                                        .color(egui::Color32::from_rgb(240, 150, 150))
                                        .size(11.0),
                                );
                            }
                            if let Some(pressure) = &session.pressure {
                                input_pressure_badge(ui, catalog, pressure);
                            }
                            if let Some(status) = &session.status_line {
                                ui.weak(egui::RichText::new(status.as_ref()).size(11.0));
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

/// 런타임 자체가 없으면 측정 실패가 아니라 실제 프로세스 사용량이 0이다.
/// 워크스페이스 헤더와 영속 세션 행에 같은 형식을 사용한다.
fn zero_resource_label(catalog: &i18n::Catalog) -> String {
    catalog.t(
        "activity.resource_label",
        &[("cpu", "0.0%"), ("rss", "0 B")],
    )
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
        ActivityWorkspaceState::Idle => "activity.state.idle",
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
        ActivityWorkspaceState::Idle => {
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
        .corner_radius(egui::CornerRadius::same(2))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: impl Into<Arc<str>>) -> ActivitySessionRow {
        ActivitySessionRow {
            session: None,
            name: name.into(),
            metric_availability: ActivityMetricAvailability::Pending,
            agent_line: None,
            status_line: None,
            resource: None,
            pressure: None,
            storm: false,
        }
    }

    fn workspace(
        name: impl Into<Arc<str>>,
        sessions: Vec<ActivitySessionRow>,
    ) -> ActivityWorkspaceRow {
        ActivityWorkspaceRow {
            workspace_id: Arc::from(""),
            name: name.into(),
            metric_availability: ActivityMetricAvailability::Local,
            state: ActivityWorkspaceState::Idle,
            session_count: sessions.len(),
            pending_events: 0,
            input_pressure: None,
            backgrounded_for_secs: None,
            auto_suspend_remaining_secs: None,
            resource: None,
            session_resources: Arc::from([]),
            sessions: sessions.into(),
        }
    }

    fn snapshot_with_retained_bytes(target: usize) -> ActivitySnapshot {
        const WORKSPACES: usize = 16;
        const SESSIONS_PER_WORKSPACE: usize = 255;
        let fixed = retained_fixture_fixed_bytes(WORKSPACES, SESSIONS_PER_WORKSPACE);
        assert!(target >= fixed);
        let mut remaining = target - fixed;
        let mut rows = Vec::with_capacity(WORKSPACES);
        for _ in 0..WORKSPACES {
            let mut sessions = Vec::with_capacity(SESSIONS_PER_WORKSPACE);
            for _ in 0..SESSIONS_PER_WORKSPACE {
                let bytes = remaining.min(MAX_ACTIVITY_TEXT_BYTES);
                remaining -= bytes;
                sessions.push(session(Arc::<str>::from("x".repeat(bytes))));
            }
            rows.push(workspace(Arc::<str>::from(""), sessions));
        }
        assert_eq!(remaining, 0);
        ActivitySnapshot::try_new(rows).unwrap()
    }

    fn retained_fixture_fixed_bytes(workspaces: usize, sessions_per_workspace: usize) -> usize {
        let sessions = workspaces * sessions_per_workspace;
        ARC_ALLOCATION_OVERHEAD
            + workspaces * std::mem::size_of::<ActivityWorkspaceRow>()
            + workspaces * 4 * ARC_ALLOCATION_OVERHEAD
            + sessions * std::mem::size_of::<ActivitySessionRow>()
            + sessions * ARC_ALLOCATION_OVERHEAD
    }

    #[test]
    fn bytes_format_uses_mib_and_gib() {
        assert_eq!(format_bytes(512 * 1024 * 1024), "512.0 MiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }

    #[test]
    fn empty_activity_summary_is_zeroed() {
        let summary = activity_summary(ActivitySnapshot::empty().rows());
        assert_eq!(summary.workspaces, 0);
        assert_eq!(summary.idle, 0);
        assert_eq!(summary.sessions, 0);
        assert_eq!(summary.cpu_percent, None);
        assert_eq!(summary.app_rss_bytes, 0);
        assert_eq!(summary.child_rss_bytes, 0);
        assert_eq!(summary.rss_bytes, 0);
        assert_eq!(summary.warnings, 0);
    }

    #[test]
    fn summary_deduplicates_app_pid_but_keeps_distinct_session_children() {
        let row =
            |name: &str, app_cpu: f32, child_session: u64, child_rss: u64| ActivityWorkspaceRow {
                workspace_id: Arc::from(name),
                name: Arc::from(name),
                metric_availability: ActivityMetricAvailability::Local,
                state: ActivityWorkspaceState::Warm,
                session_count: 1,
                pending_events: 0,
                input_pressure: None,
                backgrounded_for_secs: Some(1),
                auto_suspend_remaining_secs: Some(1),
                resource: Some(runtime::ProcessResourceSnapshot {
                    pid: 42,
                    sampled_at_ms: 0,
                    rss_bytes: 100,
                    cpu_percent: Some(app_cpu),
                    high_cpu: false,
                    high_rss: false,
                }),
                session_resources: Arc::from([runtime::SessionResourceUsage {
                    session: runtime::SessionId(child_session),
                    pid: Some(child_session as u32),
                    process_group: Some(child_session as u32),
                    identity_source: runtime::ProcessIdentitySource::PortablePty,
                    sampled_at_ms: 0,
                    process_count: 1,
                    rss_bytes: child_rss,
                    cpu_percent: Some(child_rss as f32),
                    high_cpu: false,
                    high_rss: false,
                }]),
                sessions: Arc::from([]),
            };

        let summary = activity_summary(&[row("one", 10.0, 7, 20), row("two", 99.0, 8, 30)]);
        assert_eq!(summary.app_rss_bytes, 100, "동일 앱 PID는 한 번만 합산");
        assert_eq!(summary.child_rss_bytes, 50, "세션 자식은 각각 합산");
        assert_eq!(summary.rss_bytes, 150);
        assert_eq!(
            summary.cpu_percent,
            Some(60.0),
            "중복 앱 CPU도 한 번만 합산"
        );
    }

    #[test]
    fn idle_workspace_is_kept_in_the_full_summary() {
        let rows = [ActivityWorkspaceRow {
            workspace_id: Arc::from("idle-project"),
            name: Arc::from("idle-project"),
            metric_availability: ActivityMetricAvailability::Local,
            state: ActivityWorkspaceState::Idle,
            session_count: 0,
            pending_events: 0,
            input_pressure: None,
            backgrounded_for_secs: None,
            auto_suspend_remaining_secs: None,
            resource: None,
            session_resources: Arc::from([]),
            sessions: Arc::from([]),
        }];
        let summary = activity_summary(&rows);
        assert_eq!(summary.workspaces, 1);
        assert_eq!(summary.idle, 1);
        assert_eq!(summary.active, 0);
        assert_eq!(summary.warm, 0);
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
    fn idle_resource_label_reports_zero_instead_of_missing_sample() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let label = zero_resource_label(&catalog);
        assert_eq!(label, "CPU 0.0% / RSS 0 B");
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

    #[test]
    fn snapshot_accepts_exact_workspace_cap_and_rejects_plus_one() {
        let rows = (0..MAX_ACTIVITY_WORKSPACES)
            .map(|_| workspace(Arc::<str>::from("w"), Vec::new()))
            .collect::<Vec<_>>();
        assert_eq!(
            ActivitySnapshot::try_new(rows.clone())
                .unwrap()
                .rows()
                .len(),
            MAX_ACTIVITY_WORKSPACES
        );
        let mut plus_one = rows;
        plus_one.push(workspace(Arc::<str>::from("w"), Vec::new()));
        assert_eq!(
            ActivitySnapshot::try_new(plus_one),
            Err(ActivitySnapshotError::TooManyWorkspaces)
        );

        let consumed = std::cell::Cell::new(0);
        let unbounded_source = (0..).map(|_| {
            consumed.set(consumed.get() + 1);
            workspace(Arc::<str>::from("w"), Vec::new())
        });
        assert_eq!(
            ActivitySnapshot::try_new(unbounded_source),
            Err(ActivitySnapshotError::TooManyWorkspaces)
        );
        assert_eq!(consumed.get(), MAX_ACTIVITY_WORKSPACES + 1);
    }

    #[test]
    fn snapshot_accepts_exact_item_cap_and_rejects_plus_one() {
        const WORKSPACES: usize = 16;
        const SESSIONS_PER_WORKSPACE: usize = 255;
        let rows = (0..WORKSPACES)
            .map(|_| {
                workspace(
                    Arc::<str>::from("w"),
                    (0..SESSIONS_PER_WORKSPACE)
                        .map(|_| session(Arc::<str>::from("s")))
                        .collect(),
                )
            })
            .collect::<Vec<_>>();
        let snapshot = ActivitySnapshot::try_new(rows.clone()).unwrap();
        assert_eq!(snapshot.item_count, MAX_ACTIVITY_ITEMS);

        let mut too_many = rows;
        too_many.push(workspace(Arc::<str>::from("w"), Vec::new()));
        assert_eq!(
            ActivitySnapshot::try_new(too_many),
            Err(ActivitySnapshotError::TooManyItems)
        );
    }

    #[test]
    fn snapshot_rejects_per_workspace_item_plus_one() {
        let exact = workspace(
            Arc::<str>::from("w"),
            (0..MAX_ACTIVITY_ITEMS_PER_WORKSPACE)
                .map(|_| session(Arc::<str>::from("s")))
                .collect(),
        );
        assert!(ActivitySnapshot::try_new(vec![exact]).is_ok());
        let plus_one = workspace(
            Arc::<str>::from("w"),
            (0..=MAX_ACTIVITY_ITEMS_PER_WORKSPACE)
                .map(|_| session(Arc::<str>::from("s")))
                .collect(),
        );
        assert_eq!(
            ActivitySnapshot::try_new(vec![plus_one]),
            Err(ActivitySnapshotError::TooManyItemsInWorkspace)
        );
    }

    #[test]
    fn snapshot_accepts_exact_text_cap_and_rejects_plus_one_or_nul() {
        let exact = workspace(
            Arc::<str>::from("w"),
            vec![session(Arc::<str>::from(
                "x".repeat(MAX_ACTIVITY_TEXT_BYTES),
            ))],
        );
        assert!(ActivitySnapshot::try_new(vec![exact]).is_ok());

        for rejected in [
            Arc::<str>::from("x".repeat(MAX_ACTIVITY_TEXT_BYTES + 1)),
            Arc::<str>::from("not\0safe"),
        ] {
            assert_eq!(
                ActivitySnapshot::try_new(vec![workspace("w", vec![session(rejected)])]),
                Err(ActivitySnapshotError::TextTooLong)
            );
        }
    }

    #[test]
    fn snapshot_counts_and_validates_workspace_identity_text() {
        let exact = ActivityWorkspaceRow {
            workspace_id: Arc::from("x".repeat(MAX_ACTIVITY_TEXT_BYTES)),
            ..workspace("visible", Vec::new())
        };
        assert!(ActivitySnapshot::try_new([exact]).is_ok());

        let plus_one = ActivityWorkspaceRow {
            workspace_id: Arc::from("x".repeat(MAX_ACTIVITY_TEXT_BYTES + 1)),
            ..workspace("visible", Vec::new())
        };
        assert_eq!(
            ActivitySnapshot::try_new([plus_one]),
            Err(ActivitySnapshotError::TextTooLong)
        );
    }

    #[test]
    fn snapshot_accepts_exact_retained_byte_cap_and_rejects_plus_one() {
        let exact = snapshot_with_retained_bytes(MAX_ACTIVITY_RETAINED_BYTES);
        assert_eq!(exact.retained_bytes, MAX_ACTIVITY_RETAINED_BYTES);

        let fixed = retained_fixture_fixed_bytes(16, 255);
        let mut remaining = MAX_ACTIVITY_RETAINED_BYTES + 1 - fixed;
        let mut rows = Vec::with_capacity(16);
        for _ in 0..16 {
            let mut sessions = Vec::with_capacity(255);
            for _ in 0..255 {
                let bytes = remaining.min(MAX_ACTIVITY_TEXT_BYTES);
                remaining -= bytes;
                sessions.push(session(Arc::<str>::from("x".repeat(bytes))));
            }
            rows.push(workspace("", sessions));
        }
        assert_eq!(remaining, 0);
        assert_eq!(
            ActivitySnapshot::try_new(rows),
            Err(ActivitySnapshotError::TooManyRetainedBytes)
        );
    }

    #[test]
    fn snapshot_clone_and_render_reuse_the_same_arc_rows() {
        let snapshot = ActivitySnapshot::try_new(vec![workspace(
            Arc::<str>::from("workspace"),
            vec![session(Arc::<str>::from("session"))],
        )])
        .unwrap();
        let cloned = snapshot.clone();
        assert!(Arc::ptr_eq(&snapshot.rows, &cloned.rows));

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let ctx = egui::Context::default();
        let mut activity = ActivityUi::new();
        let before = Arc::strong_count(&snapshot.rows);
        for _ in 0..300 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                assert!(activity.contents(ui, &catalog, &snapshot).is_none());
            });
        }
        assert_eq!(Arc::strong_count(&snapshot.rows), before);
    }

    #[test]
    fn debug_redacts_all_title_like_content() {
        let row = workspace(
            Arc::<str>::from("/private/workspace"),
            vec![ActivitySessionRow {
                session: Some(runtime::SessionId(1)),
                name: Arc::from("secret pane title"),
                metric_availability: ActivityMetricAvailability::RemoteUnavailable,
                agent_line: Some(Arc::from("provider and model")),
                status_line: Some(Arc::from("private status")),
                resource: None,
                pressure: None,
                storm: false,
            }],
        );
        let row_debug = format!("{row:?}");
        for secret in [
            "/private/workspace",
            "secret pane title",
            "provider and model",
            "private status",
        ] {
            assert!(!row_debug.contains(secret));
        }
        let snapshot = ActivitySnapshot::try_new(vec![row]).unwrap();
        let snapshot_debug = format!("{snapshot:?}");
        assert!(!snapshot_debug.contains("private"));
        assert!(!snapshot_debug.contains("title"));
    }

    #[test]
    fn production_source_has_no_render_host_or_polling_edges() {
        let source = include_str!("activity.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "std::fs",
            "Db::",
            "Keyring",
            "std::process",
            "Command::",
            "TcpStream",
            "reqwest",
            "ureq",
            "std::thread",
            "thread::spawn",
            "mpsc",
            "channel(",
            "recv(",
            "try_recv(",
            ".poll(",
            "sleep(",
            "request_repaint",
            "request_repaint_after",
            "Instant::now",
            "SystemTime",
        ] {
            assert!(
                !production.contains(forbidden),
                "activity leaf must not contain {forbidden}"
            );
        }
        let render = production.split("pub fn contents").nth(1).unwrap();
        assert!(!render.contains(".clone("));
    }
}
