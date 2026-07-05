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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityWorkspaceState {
    Active,
    Warm,
    Suspended,
}

pub enum ActivityAction {
    SwitchWorkspace(String),
}

pub struct ActivityUi {
    open: bool,
}

impl ActivityUi {
    pub fn new() -> Self {
        Self { open: false }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        catalog: &i18n::Catalog,
        rows: &[ActivityWorkspaceRow],
    ) -> Option<ActivityAction> {
        if !self.open {
            return None;
        }
        if rows.iter().any(|row| row.backgrounded_for_secs.is_some()) {
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
        let mut open = true;
        let mut action = None;
        egui::Window::new(catalog.t("activity.title", &[]))
            .open(&mut open)
            .resizable(true)
            .default_width(560.0)
            .show(ctx, |ui| {
                if rows.is_empty() {
                    ui.label(catalog.t("activity.empty", &[]));
                    return;
                }
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
                            ui.label(queue_label(catalog, row));
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
                        }
                    });
            });
        self.open = open;
        action
    }
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

fn queue_label(catalog: &i18n::Catalog, row: &ActivityWorkspaceRow) -> String {
    let mut label = row.pending_events.to_string();
    if let Some(pressure) = &row.input_pressure {
        label.push_str(" · ");
        label.push_str(&catalog.t(
            "activity.input_pressure",
            &[
                ("queued", &format_bytes(pressure.queued_bytes as u64)),
                ("max", &format_bytes(pressure.max_bytes as u64)),
            ],
        ));
    }
    label
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
    fn queue_label_includes_input_pressure() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let row = ActivityWorkspaceRow {
            id: "ws".into(),
            name: "workspace".into(),
            state: ActivityWorkspaceState::Active,
            session_count: 1,
            pending_events: 3,
            input_pressure: Some(runtime::PtyInputPressure {
                attempted_bytes: 10,
                queued_bytes: 1024,
                queued_messages: 1,
                max_bytes: 4 * 1024,
                max_messages: 16,
                reason: runtime::PtyInputRejectReason::QueueFull,
            }),
            backgrounded_for_secs: None,
            auto_suspend_remaining_secs: None,
            resource: None,
            session_resources: Vec::new(),
        };
        let label = queue_label(&catalog, &row);
        assert!(label.contains('3'));
        assert!(label.contains("input"));
        assert!(label.contains("1.0 KiB"));
    }
}
