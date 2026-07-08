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
        rows: &[ActivityWorkspaceRow],
    ) -> Option<ActivityAction> {
        let mut action = None;
        if rows.is_empty() {
            ui.label(catalog.t("activity.empty", &[]));
            return action;
        }
        // 전체 렌더 캐시 비우기 — 안전한 것(렌더 캐시)만 담는다. 절전/스크롤백은
        // 작업·기록에 영향이 있어 이 버튼에 포함하지 않는다(2026-07-08 검토).
        if ui
            .button(catalog.t("activity.clear_caches", &[]))
            .on_hover_text(catalog.t("activity.clear_caches_hint", &[]))
            .clicked()
        {
            action = Some(ActivityAction::ClearRenderCaches);
        }
        ui.add_space(6.0);
        egui::Grid::new("activity_workspace_grid")
            .num_columns(6)
            .striped(true)
            .show(ui, |ui| {
                ui.strong(catalog.t("activity.workspace", &[]));
                ui.strong(catalog.t("activity.state", &[]));
                ui.strong(catalog.t("activity.sessions", &[]));
                ui.strong(catalog.t("activity.queue", &[]));
                ui.strong(catalog.t("activity.resources", &[]));
                ui.strong(catalog.t("activity.action", &[]));
                ui.end_row();

                for row in rows {
                    ui.label(&row.name);
                    state_label(ui, catalog, row);
                    ui.label(row.session_count.to_string());
                    ui.horizontal(|ui| {
                        ui.label(row.pending_events.to_string());
                        if let Some(pressure) = &row.input_pressure {
                            input_pressure_badge(ui, catalog, pressure);
                        }
                    });
                    ui.label(resource_label(
                        catalog,
                        row.resource.as_ref(),
                        &row.session_resources,
                    ));
                    if row.state != ActivityWorkspaceState::Active {
                        if ui.button(catalog.t("activity.switch", &[])).clicked() {
                            action = Some(ActivityAction::SwitchWorkspace(row.id.clone()));
                        }
                    } else {
                        ui.weak(catalog.t("activity.current", &[]));
                    }
                    ui.end_row();

                    // pane(세션)별 서브행 — 이름 들여쓰기, 상태/에이전트/압력/자원을 각 열에.
                    for s in &row.sessions {
                        ui.weak(format!("└ {}", s.name));
                        ui.label(s.status_line.as_deref().unwrap_or(""));
                        ui.label(s.agent_line.as_deref().unwrap_or(""));
                        ui.horizontal(|ui| {
                            if let Some(pressure) = &s.pressure {
                                input_pressure_badge(ui, catalog, pressure);
                            }
                        });
                        ui.label(session_resource_text(catalog, s.resource.as_ref()));
                        ui.label("");
                        ui.end_row();
                    }
                }
            });
        action
    }
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
