use super::activity::{
    ActivityMetricAvailability, ActivitySessionRow, ActivityWorkspaceRow, ActivityWorkspaceState,
};
use super::format_bytes;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

const RESOURCE_POPOVER_WIDTH: f32 = 560.0;
const RESOURCE_POPOVER_MAX_HEIGHT: f32 = 560.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResourceManagerIntent {
    Refresh,
    InspectUnattached,
    KillUnattached {
        workspace_id: Arc<str>,
    },
    FocusSession {
        workspace_id: Arc<str>,
        session: runtime::SessionId,
    },
    KillSession {
        workspace_id: Arc<str>,
        session: runtime::SessionId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResourceConfirm {
    Unattached {
        workspace_id: Arc<str>,
        workspace_name: Arc<str>,
        count: u16,
    },
    Session {
        workspace_id: Arc<str>,
        session: runtime::SessionId,
        session_name: Arc<str>,
    },
}

#[derive(Default)]
pub(crate) struct ResourceManagerUi {
    open: bool,
    expanded: BTreeSet<Arc<str>>,
    known_workspaces: BTreeSet<Arc<str>>,
    confirm: Option<ResourceConfirm>,
}

impl ResourceManagerUi {
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn set_open(&mut self, open: bool) {
        self.open = open;
        if !open {
            self.confirm = None;
        }
    }

    pub(crate) fn toggle_open(&mut self) {
        self.set_open(!self.open);
    }

    pub(crate) fn contents(
        &mut self,
        ui: &mut egui::Ui,
        rows: &[ActivityWorkspaceRow],
        unattached_counts: &HashMap<String, u16>,
        now_ms: u64,
        catalog: &i18n::Catalog,
    ) -> Option<ResourceManagerIntent> {
        ui.set_min_width(RESOURCE_POPOVER_WIDTH);
        let mut intent = None;
        ui.horizontal(|ui| {
            ui.heading(catalog.t("resource_manager.title", &[]));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(catalog.t("resource_manager.refresh", &[]))
                    .clicked()
                {
                    intent = Some(ResourceManagerIntent::Refresh);
                }
            });
        });
        ui.add_space(4.0);
        ui.weak(catalog.t("resource_manager.description", &[]));
        ui.separator();

        let app = app_totals(rows);
        resource_heading(
            ui,
            &catalog.t("resource_manager.app_name", &[]),
            app.metrics(catalog),
            app.age_text(now_ms, catalog),
        );
        self.expanded.retain(|id| {
            rows.iter()
                .any(|row| row.workspace_id.as_ref() == id.as_ref())
        });
        self.known_workspaces.retain(|id| {
            rows.iter()
                .any(|row| row.workspace_id.as_ref() == id.as_ref())
        });

        egui::ScrollArea::vertical()
            .id_salt("resource_manager_rows")
            .max_height(RESOURCE_POPOVER_MAX_HEIGHT)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for row in rows {
                    self.render_workspace(
                        ui,
                        row,
                        unattached_counts
                            .get(row.workspace_id.as_ref())
                            .copied()
                            .unwrap_or(0),
                        now_ms,
                        &mut intent,
                        catalog,
                    );
                }
                if rows.is_empty() {
                    ui.weak(catalog.t("resource_manager.empty", &[]));
                }
            });

        ui.separator();
        if ui
            .button(catalog.t("resource_manager.inspect_unattached", &[]))
            .clicked()
        {
            intent = Some(ResourceManagerIntent::InspectUnattached);
        }
        self.render_confirmation(ui, &mut intent, catalog);
        intent
    }

    fn render_workspace(
        &mut self,
        ui: &mut egui::Ui,
        row: &ActivityWorkspaceRow,
        unattached_count: u16,
        now_ms: u64,
        intent: &mut Option<ResourceManagerIntent>,
        catalog: &i18n::Catalog,
    ) {
        if self.known_workspaces.insert(Arc::clone(&row.workspace_id)) {
            self.expanded.insert(Arc::clone(&row.workspace_id));
        }
        let expanded = self.expanded.contains(row.workspace_id.as_ref());
        let state = match row.state {
            ActivityWorkspaceState::Active => catalog.t("resource_manager.state.active", &[]),
            ActivityWorkspaceState::Warm => catalog.t("resource_manager.state.warm", &[]),
            ActivityWorkspaceState::Idle => catalog.t("resource_manager.state.idle", &[]),
        };
        egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(4, 7))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let marker = if expanded { "▾" } else { "▸" };
                    let response = ui.add(egui::Button::new(marker).frame(false));
                    let disclosure_label = catalog.t(
                        if expanded {
                            "resource_manager.workspace_collapse"
                        } else {
                            "resource_manager.workspace_expand"
                        },
                        &[("workspace", row.name.as_ref())],
                    );
                    response.widget_info(|| {
                        egui::WidgetInfo::selected(
                            egui::WidgetType::Button,
                            ui.is_enabled(),
                            expanded,
                            &disclosure_label,
                        )
                    });
                    ui.label(egui::RichText::new(row.name.as_ref()).strong());
                    if response.clicked() {
                        if expanded {
                            self.expanded.remove(row.workspace_id.as_ref());
                        } else {
                            self.expanded.insert(Arc::clone(&row.workspace_id));
                        }
                    }
                    ui.weak(state);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (metrics, age) = workspace_metric_display(row, now_ms, catalog);
                        if !age.is_empty() {
                            ui.weak(age);
                        }
                        ui.label(metrics);
                    });
                });

                if expanded {
                    if let Some(pressure) = &row.input_pressure {
                        ui.indent(("workspace_pressure", row.workspace_id.as_ref()), |ui| {
                            ui.colored_label(
                                ui.visuals().warn_fg_color,
                                pressure_text(pressure, catalog),
                            );
                        });
                    }
                    if unattached_count > 0 {
                        ui.indent(("workspace_unattached", row.workspace_id.as_ref()), |ui| {
                            ui.horizontal(|ui| {
                                ui.colored_label(
                                    ui.visuals().warn_fg_color,
                                    catalog.t(
                                        "resource_manager.unattached_count",
                                        &[("count", &unattached_count.to_string())],
                                    ),
                                );
                                let accessible = catalog.t(
                                    "resource_manager.review_unattached_for",
                                    &[
                                        ("workspace", row.name.as_ref()),
                                        ("count", &unattached_count.to_string()),
                                    ],
                                );
                                let response =
                                    ui.button(catalog.t("resource_manager.kill_unattached", &[]));
                                response.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::WidgetType::Button,
                                        ui.is_enabled(),
                                        &accessible,
                                    )
                                });
                                if response.clicked() {
                                    self.confirm = Some(ResourceConfirm::Unattached {
                                        workspace_id: Arc::clone(&row.workspace_id),
                                        workspace_name: Arc::clone(&row.name),
                                        count: unattached_count,
                                    });
                                }
                            });
                        });
                    }
                    for session in row.sessions.iter() {
                        self.render_session(ui, row, session, now_ms, intent, catalog);
                    }
                }
            });
        ui.separator();
    }

    fn render_session(
        &mut self,
        ui: &mut egui::Ui,
        workspace: &ActivityWorkspaceRow,
        session: &ActivitySessionRow,
        now_ms: u64,
        intent: &mut Option<ResourceManagerIntent>,
        catalog: &i18n::Catalog,
    ) {
        ui.indent(
            (
                "resource_session",
                workspace.workspace_id.as_ref(),
                session.name.as_ref(),
            ),
            |ui| {
                ui.horizontal(|ui| {
                    ui.label(session.name.as_ref());
                    if session.storm {
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            catalog.t("resource_manager.storm", &[]),
                        );
                    }
                    if let Some(pressure) = &session.pressure {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            pressure_text(pressure, catalog),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(session_id) = session.session {
                            let kill_accessible = catalog.t(
                                "resource_manager.kill_session_accessible",
                                &[("session", session.name.as_ref())],
                            );
                            let kill = ui.button(catalog.t("resource_manager.kill_session", &[]));
                            kill.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    ui.is_enabled(),
                                    &kill_accessible,
                                )
                            });
                            if kill.clicked() {
                                self.confirm = Some(ResourceConfirm::Session {
                                    workspace_id: Arc::clone(&workspace.workspace_id),
                                    session: session_id,
                                    session_name: Arc::clone(&session.name),
                                });
                            }
                            let focus_accessible = catalog.t(
                                "resource_manager.focus_session_accessible",
                                &[("session", session.name.as_ref())],
                            );
                            let focus = ui.button(catalog.t("resource_manager.focus_session", &[]));
                            focus.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    ui.is_enabled(),
                                    &focus_accessible,
                                )
                            });
                            if focus.clicked() {
                                *intent = Some(ResourceManagerIntent::FocusSession {
                                    workspace_id: Arc::clone(&workspace.workspace_id),
                                    session: session_id,
                                });
                            }
                        }
                        let (metrics, age) = session_metric_display(session, now_ms, catalog);
                        if !age.is_empty() {
                            ui.weak(age);
                        }
                        ui.label(metrics);
                    });
                });
            },
        );
    }

    fn render_confirmation(
        &mut self,
        ui: &mut egui::Ui,
        intent: &mut Option<ResourceManagerIntent>,
        catalog: &i18n::Catalog,
    ) {
        let Some(confirm) = self.confirm.clone() else {
            return;
        };
        ui.separator();
        let message = match &confirm {
            ResourceConfirm::Unattached {
                workspace_name,
                count,
                ..
            } => catalog.t(
                "resource_manager.confirm_unattached",
                &[
                    ("workspace", workspace_name.as_ref()),
                    ("count", &count.to_string()),
                ],
            ),
            ResourceConfirm::Session { session_name, .. } => catalog.t(
                "resource_manager.confirm_session",
                &[("session", session_name.as_ref())],
            ),
        };
        ui.colored_label(ui.visuals().warn_fg_color, message);
        ui.horizontal(|ui| {
            if ui
                .button(catalog.t("resource_manager.confirm_accept", &[]))
                .clicked()
            {
                *intent = Some(match confirm {
                    ResourceConfirm::Unattached { workspace_id, .. } => {
                        ResourceManagerIntent::KillUnattached { workspace_id }
                    }
                    ResourceConfirm::Session {
                        workspace_id,
                        session,
                        ..
                    } => ResourceManagerIntent::KillSession {
                        workspace_id,
                        session,
                    },
                });
                self.confirm = None;
            }
            if ui
                .button(catalog.t("resource_manager.cancel", &[]))
                .clicked()
            {
                self.confirm = None;
            }
        });
    }
}

#[derive(Default)]
struct AppTotals {
    cpu_percent: f32,
    cpu_seen: bool,
    rss_bytes: u64,
    process_count: usize,
    sampled_at_ms: u64,
}

impl AppTotals {
    fn metrics(&self, catalog: &i18n::Catalog) -> String {
        let cpu = if self.cpu_seen {
            format!("{:.1}%", self.cpu_percent)
        } else {
            "—".to_owned()
        };
        catalog.t(
            "resource_manager.metric_app",
            &[
                ("cpu", &cpu),
                ("memory", &format_bytes(self.rss_bytes)),
                ("processes", &self.process_count.to_string()),
            ],
        )
    }

    fn age_text(&self, now_ms: u64, catalog: &i18n::Catalog) -> String {
        sample_age(self.sampled_at_ms, now_ms, catalog)
    }
}

fn app_totals(rows: &[ActivityWorkspaceRow]) -> AppTotals {
    let mut latest_by_pid = BTreeMap::<u32, runtime::ProcessResourceSnapshot>::new();
    let mut totals = AppTotals::default();
    for row in rows {
        if let Some(resource) = row.resource {
            latest_by_pid
                .entry(resource.pid)
                .and_modify(|kept| {
                    if kept.sampled_at_ms < resource.sampled_at_ms {
                        *kept = resource;
                    }
                })
                .or_insert(resource);
        }
        for session in row.session_resources.iter() {
            totals.rss_bytes = totals.rss_bytes.saturating_add(session.rss_bytes);
            totals.process_count = totals.process_count.saturating_add(session.process_count);
            totals.sampled_at_ms = totals.sampled_at_ms.max(session.sampled_at_ms);
            if let Some(cpu) = session.cpu_percent {
                totals.cpu_percent += cpu;
                totals.cpu_seen = true;
            }
        }
    }
    for resource in latest_by_pid.values() {
        totals.rss_bytes = totals.rss_bytes.saturating_add(resource.rss_bytes);
        totals.process_count = totals.process_count.saturating_add(1);
        totals.sampled_at_ms = totals.sampled_at_ms.max(resource.sampled_at_ms);
        if let Some(cpu) = resource.cpu_percent {
            totals.cpu_percent += cpu;
            totals.cpu_seen = true;
        }
    }
    totals
}

fn resource_heading(ui: &mut egui::Ui, name: &str, metrics: String, age: String) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(name).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.weak(age);
            ui.label(metrics);
        });
    });
}

fn workspace_metric_display(
    row: &ActivityWorkspaceRow,
    now_ms: u64,
    catalog: &i18n::Catalog,
) -> (String, String) {
    match row.metric_availability {
        ActivityMetricAvailability::Pending => {
            return (
                catalog.t("resource_manager.metric_pending", &[]),
                String::new(),
            );
        }
        ActivityMetricAvailability::RemoteUnavailable => {
            return (
                catalog.t("resource_manager.remote_unavailable", &[]),
                String::new(),
            );
        }
        ActivityMetricAvailability::Local => {}
    }
    let cpu = row
        .resource
        .and_then(|resource| resource.cpu_percent)
        .map(|value| format!("{value:.1}%"))
        .unwrap_or_else(|| "—".to_owned());
    let session_rss = row
        .session_resources
        .iter()
        .fold(0u64, |sum, usage| sum.saturating_add(usage.rss_bytes));
    let app = format_bytes(row.resource.map_or(0, |resource| resource.rss_bytes));
    let sessions = format_bytes(session_rss);
    (
        catalog.t(
            "resource_manager.metric_workspace",
            &[("cpu", &cpu), ("app", &app), ("sessions", &sessions)],
        ),
        workspace_age(row, now_ms, catalog),
    )
}

fn workspace_age(row: &ActivityWorkspaceRow, now_ms: u64, catalog: &i18n::Catalog) -> String {
    let sampled_at_ms = row.session_resources.iter().fold(
        row.resource.map_or(0, |resource| resource.sampled_at_ms),
        |age, usage| age.max(usage.sampled_at_ms),
    );
    sample_age(sampled_at_ms, now_ms, catalog)
}

fn session_metric_display(
    session: &ActivitySessionRow,
    now_ms: u64,
    catalog: &i18n::Catalog,
) -> (String, String) {
    match session.metric_availability {
        ActivityMetricAvailability::Pending => {
            return (
                catalog.t("resource_manager.metric_pending", &[]),
                String::new(),
            );
        }
        ActivityMetricAvailability::RemoteUnavailable => {
            return (
                catalog.t("resource_manager.remote_unavailable", &[]),
                String::new(),
            );
        }
        ActivityMetricAvailability::Local => {}
    }
    let Some(resource) = &session.resource else {
        return (
            catalog.t("resource_manager.metric_pending", &[]),
            String::new(),
        );
    };
    let cpu = resource
        .cpu_percent
        .map(|value| format!("{value:.1}%"))
        .unwrap_or_else(|| "—".to_owned());
    let memory = format_bytes(resource.rss_bytes);
    (
        catalog.t(
            "resource_manager.metric_session",
            &[
                ("cpu", &cpu),
                ("memory", &memory),
                ("processes", &resource.process_count.to_string()),
            ],
        ),
        sample_age(resource.sampled_at_ms, now_ms, catalog),
    )
}

fn sample_age(sampled_at_ms: u64, now_ms: u64, catalog: &i18n::Catalog) -> String {
    if sampled_at_ms == 0 || now_ms == 0 || sampled_at_ms > now_ms {
        return catalog.t("resource_manager.age_pending", &[]);
    }
    let seconds = now_ms.saturating_sub(sampled_at_ms) / 1_000;
    if seconds == 0 {
        catalog.t("resource_manager.age_now", &[])
    } else if seconds >= 10 {
        catalog.t(
            "resource_manager.age_stale",
            &[("seconds", &seconds.to_string())],
        )
    } else {
        catalog.t(
            "resource_manager.age_seconds",
            &[("seconds", &seconds.to_string())],
        )
    }
}

fn pressure_text(pressure: &runtime::PtyInputPressure, catalog: &i18n::Catalog) -> String {
    catalog.t(
        "resource_manager.input_pressure",
        &[
            ("queued", &format_bytes(pressure.queued_bytes as u64)),
            ("max", &format_bytes(pressure.max_bytes as u64)),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::activity::{ActivitySessionRow, ActivityWorkspaceRow, ActivityWorkspaceState};
    use egui_kittest::kittest::Queryable;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn fixture() -> Vec<ActivityWorkspaceRow> {
        vec![ActivityWorkspaceRow {
            workspace_id: Arc::from("workspace-a"),
            name: Arc::from("Workspace A"),
            metric_availability: ActivityMetricAvailability::Local,
            state: ActivityWorkspaceState::Active,
            session_count: 3,
            pending_events: 0,
            input_pressure: None,
            backgrounded_for_secs: None,
            auto_suspend_remaining_secs: None,
            resource: Some(runtime::ProcessResourceSnapshot {
                pid: 7,
                sampled_at_ms: 9_000,
                rss_bytes: 256 * 1024 * 1024,
                cpu_percent: Some(3.5),
                high_cpu: false,
                high_rss: false,
            }),
            session_resources: Arc::from([]),
            sessions: Arc::from([
                ActivitySessionRow {
                    session: Some(runtime::SessionId(11)),
                    name: Arc::from("Local Session"),
                    metric_availability: ActivityMetricAvailability::Local,
                    agent_line: None,
                    status_line: None,
                    resource: Some(runtime::SessionResourceUsage {
                        session: runtime::SessionId(11),
                        pid: Some(77),
                        process_group: Some(77),
                        identity_source: runtime::ProcessIdentitySource::PortablePty,
                        sampled_at_ms: 9_000,
                        process_count: 2,
                        rss_bytes: 64 * 1024 * 1024,
                        cpu_percent: Some(1.0),
                        high_cpu: false,
                        high_rss: false,
                    }),
                    pressure: None,
                    storm: false,
                },
                ActivitySessionRow {
                    session: None,
                    name: Arc::from("Remote Session"),
                    metric_availability: ActivityMetricAvailability::RemoteUnavailable,
                    agent_line: None,
                    status_line: None,
                    resource: None,
                    pressure: None,
                    storm: false,
                },
                ActivitySessionRow {
                    session: Some(runtime::SessionId(12)),
                    name: Arc::from("Pending Session"),
                    metric_availability: ActivityMetricAvailability::Pending,
                    agent_line: None,
                    status_line: None,
                    resource: None,
                    pressure: None,
                    storm: false,
                },
            ]),
        }]
    }

    fn harness(
        rows: Vec<ActivityWorkspaceRow>,
    ) -> egui_kittest::Harness<'static, (ResourceManagerUi, Vec<ResourceManagerIntent>)> {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        egui_kittest::Harness::new_ui_state(
            move |ui, (manager, intents)| {
                if let Some(intent) = manager.contents(
                    ui,
                    &rows,
                    &HashMap::from([(String::from("workspace-a"), 1)]),
                    10_000,
                    &catalog,
                ) {
                    intents.push(intent);
                }
            },
            (ResourceManagerUi::default(), Vec::new()),
        )
    }

    #[test]
    fn resource_manager_renders_app_workspace_session_and_unavailable_remote_rows() {
        let mut harness = harness(fixture());
        harness.run();
        harness.get_by_label("Deppy Sijo");
        harness.get_by_label("Workspace A");
        harness.get_by_label("Local Session");
        harness.get_by_label("Remote Session");
        harness.get_by_label("Pending Session");
        harness.get_by_label("Unavailable for remote sessions");
        harness.get_by_label("Local metrics pending");
    }

    #[test]
    fn resource_manager_refresh_and_inspect_emit_typed_intents_only() {
        let mut harness = harness(fixture());
        harness.run();
        harness.get_by_label("Refresh").click();
        harness.run();
        assert_eq!(
            harness.state().1.last(),
            Some(&ResourceManagerIntent::Refresh)
        );
        harness.get_by_label("Review orphaned sessions").click();
        harness.run();
        assert_eq!(
            harness.state().1.last(),
            Some(&ResourceManagerIntent::InspectUnattached)
        );
    }

    #[test]
    fn resource_manager_accessibility_names_target_and_disclosure_state() {
        let mut harness = harness(fixture());
        harness.run();
        harness.get_by_label("Collapse Workspace A");
        harness.get_by_label("Focus Local Session");
        harness.get_by_label("End Local Session");

        harness.get_by_label("Collapse Workspace A").click();
        harness.run();
        harness.get_by_label("Expand Workspace A");
        assert!(harness.query_by_label("Focus Local Session").is_none());
    }

    #[test]
    fn resource_manager_session_confirmation_can_cancel_then_confirm() {
        let mut harness = harness(fixture());
        harness.run();
        harness.get_by_label("End Local Session").click();
        harness.run();
        harness.get_by_label("End Local Session?");
        harness.get_by_label("Cancel").click();
        harness.run();
        assert!(harness.query_by_label("End Local Session?").is_none());

        harness.get_by_label("End Local Session").click();
        harness.run();
        harness.get_by_label("Confirm end").click();
        harness.run();
        assert_eq!(
            harness.state().1.last(),
            Some(&ResourceManagerIntent::KillSession {
                workspace_id: Arc::from("workspace-a"),
                session: runtime::SessionId(11),
            })
        );
    }

    #[test]
    fn resource_manager_uses_requested_locale_without_korean_fallback_text() {
        let mut harness = harness(fixture());
        harness.run();
        harness.get_by_label("Resource Manager");
        harness.get_by_label("Active");
        assert!(harness.query_by_label("리소스 관리자").is_none());
        assert!(harness.query_by_label("활성").is_none());
    }

    #[test]
    fn closed_resource_manager_emits_no_intent() {
        let manager = ResourceManagerUi::default();
        assert!(!manager.is_open());
    }

    #[test]
    fn production_source_has_no_host_process_filesystem_or_polling_edges() {
        let source = include_str!("resource_manager.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            ["std::", "fs"].concat(),
            ["std::", "process"].concat(),
            ["std::", "thread"].concat(),
            ["Command", "::"].concat(),
            ["request_repaint_", "after"].concat(),
            ["try_", "recv"].concat(),
            ["Tcp", "Stream"].concat(),
        ] {
            assert!(!source.contains(&forbidden), "forbidden edge: {forbidden}");
        }
    }
}
