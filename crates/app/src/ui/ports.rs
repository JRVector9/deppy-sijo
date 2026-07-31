use crate::port_inventory::{PortOwnership, PortRow, PortSnapshot, PortTerminationTarget};
use std::sync::Arc;

const PORTS_POPOVER_WIDTH: f32 = 500.0;
const PORTS_POPOVER_MAX_HEIGHT: f32 = 560.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PortsIntent {
    Refresh,
    Terminate(PortTerminationTarget),
    CopyAddress(Arc<str>),
}

#[derive(Clone)]
struct PortConfirmation {
    target: PortTerminationTarget,
    process: Arc<str>,
    socket: Arc<str>,
    workspace: Arc<str>,
}

#[derive(Default)]
pub(crate) struct PortsUi {
    open: bool,
    confirm: Option<PortConfirmation>,
    open_refresh_pending: bool,
}

impl PortsUi {
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn set_open(&mut self, open: bool) {
        if open && !self.open {
            self.open_refresh_pending = true;
        }
        self.open = open;
        if !open {
            self.confirm = None;
        }
    }

    pub(crate) fn toggle_open(&mut self) {
        self.set_open(!self.open);
    }

    pub(crate) fn take_open_refresh(&mut self) -> Option<PortsIntent> {
        if !self.open_refresh_pending {
            return None;
        }
        self.open_refresh_pending = false;
        Some(PortsIntent::Refresh)
    }

    pub(crate) fn contents(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: Option<&PortSnapshot>,
        active_workspace_id: Option<&str>,
        now_ms: u64,
        catalog: &i18n::Catalog,
    ) -> Option<PortsIntent> {
        ui.set_min_width(PORTS_POPOVER_WIDTH);
        let mut intent = None;
        ui.horizontal(|ui| {
            ui.heading(catalog.t("ports.title", &[]));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(catalog.t("ports.refresh", &[])).clicked() {
                    intent = Some(PortsIntent::Refresh);
                }
            });
        });
        ui.add_space(4.0);
        ui.weak(catalog.t("ports.description", &[]));
        ui.separator();

        let Some(snapshot) = snapshot else {
            ui.weak(catalog.t("ports.not_scanned", &[]));
            return intent;
        };
        ui.weak(port_sample_age(snapshot.sampled_at_ms, now_ms, catalog));
        ui.add_space(4.0);

        egui::ScrollArea::vertical()
            .id_salt("ports_manager_rows")
            .max_height(PORTS_POPOVER_MAX_HEIGHT)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                self.render_section(
                    ui,
                    &catalog.t("ports.active_workspace", &[]),
                    snapshot.rows.iter().filter(|row| {
                        row.ownership == PortOwnership::Workspace
                            && row.workspace_id.as_deref() == active_workspace_id
                    }),
                    &mut intent,
                    catalog,
                );
                self.render_section(
                    ui,
                    &catalog.t("ports.other_workspaces", &[]),
                    snapshot.rows.iter().filter(|row| {
                        row.ownership == PortOwnership::Workspace
                            && row.workspace_id.as_deref() != active_workspace_id
                    }),
                    &mut intent,
                    catalog,
                );
                self.render_section(
                    ui,
                    &catalog.t("ports.external", &[]),
                    snapshot
                        .rows
                        .iter()
                        .filter(|row| row.ownership != PortOwnership::Workspace),
                    &mut intent,
                    catalog,
                );
                if snapshot.rows.is_empty() {
                    ui.weak(catalog.t("ports.empty", &[]));
                }
            });
        self.render_confirmation(ui, &mut intent, catalog);
        intent
    }

    fn render_section<'a>(
        &mut self,
        ui: &mut egui::Ui,
        title: &str,
        rows: impl Iterator<Item = &'a PortRow>,
        intent: &mut Option<PortsIntent>,
        catalog: &i18n::Catalog,
    ) {
        let mut rows = rows.peekable();
        if rows.peek().is_none() {
            return;
        }
        ui.label(egui::RichText::new(title).strong());
        for row in rows {
            self.render_row(ui, row, intent, catalog);
        }
        ui.add_space(6.0);
    }

    fn render_row(
        &mut self,
        ui: &mut egui::Ui,
        row: &PortRow,
        intent: &mut Option<PortsIntent>,
        catalog: &i18n::Catalog,
    ) {
        let socket = listener_socket(row);
        let workspace = row
            .workspace_name
            .as_deref()
            .map(str::to_owned)
            .unwrap_or_else(|| catalog.t("ports.workspace_unknown", &[]));
        egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(6, 5))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(row.port.to_string())
                            .monospace()
                            .strong(),
                    );
                    ui.weak(row.process.as_ref());
                    if let Some(name) = &row.workspace_name {
                        ui.weak(name.as_ref());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if row.ownership == PortOwnership::Workspace {
                            let accessible = catalog.t(
                                "ports.terminate_socket_accessible",
                                &[
                                    ("process", row.process.as_ref()),
                                    ("socket", socket.as_str()),
                                    ("workspace", workspace.as_str()),
                                ],
                            );
                            let response = ui.button(catalog.t("ports.terminate", &[]));
                            response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    ui.is_enabled(),
                                    &accessible,
                                )
                            });
                            if response.clicked()
                                && let Some(target) = termination_target(row)
                            {
                                self.confirm = Some(PortConfirmation {
                                    target,
                                    process: Arc::clone(&row.process),
                                    socket: Arc::from(socket.as_str()),
                                    workspace: Arc::from(workspace.as_str()),
                                });
                            }
                        } else {
                            ui.weak(read_only_reason(row.ownership, catalog));
                        }
                        let accessible = catalog.t(
                            "ports.copy_socket_accessible",
                            &[("socket", socket.as_str())],
                        );
                        let response = ui.button(catalog.t("ports.copy_address", &[]));
                        response.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                ui.is_enabled(),
                                &accessible,
                            )
                        });
                        if response.clicked() {
                            *intent = Some(PortsIntent::CopyAddress(Arc::from(socket.as_str())));
                        }
                    });
                });
                ui.weak(socket.as_str());
            });
        ui.separator();
    }

    fn render_confirmation(
        &mut self,
        ui: &mut egui::Ui,
        intent: &mut Option<PortsIntent>,
        catalog: &i18n::Catalog,
    ) {
        let Some(confirm) = self.confirm.clone() else {
            return;
        };
        ui.separator();
        ui.colored_label(
            ui.visuals().warn_fg_color,
            catalog.t(
                "ports.confirm_target",
                &[
                    ("process", confirm.process.as_ref()),
                    ("socket", confirm.socket.as_ref()),
                    ("workspace", confirm.workspace.as_ref()),
                ],
            ),
        );
        ui.horizontal(|ui| {
            if ui.button(catalog.t("ports.confirm_accept", &[])).clicked() {
                *intent = Some(PortsIntent::Terminate(confirm.target));
                self.confirm = None;
            }
            if ui.button(catalog.t("ports.cancel", &[])).clicked() {
                self.confirm = None;
            }
        });
    }
}

fn termination_target(row: &PortRow) -> Option<PortTerminationTarget> {
    Some(PortTerminationTarget {
        workspace_id: Arc::clone(row.workspace_id.as_ref()?),
        pid: row.pid,
        port: row.port,
        bind: Arc::clone(&row.bind),
        protocol: row.protocol,
        process_started_at: Arc::clone(&row.process_started_at),
    })
}

fn listener_socket(row: &PortRow) -> String {
    let bind = match row.bind.as_ref() {
        value if value.contains(':') => format!("[{value}]"),
        value => value.to_owned(),
    };
    format!("{bind}:{}", row.port)
}

fn read_only_reason(ownership: PortOwnership, catalog: &i18n::Catalog) -> String {
    match ownership {
        PortOwnership::Protected => catalog.t("ports.protected_read_only", &[]),
        PortOwnership::Ambiguous => catalog.t("ports.ambiguous_read_only", &[]),
        PortOwnership::External => catalog.t("ports.external_read_only", &[]),
        PortOwnership::Workspace => String::new(),
    }
}

fn port_sample_age(sampled_at_ms: u64, now_ms: u64, catalog: &i18n::Catalog) -> String {
    if sampled_at_ms == 0 || now_ms == 0 || sampled_at_ms > now_ms {
        return catalog.t("ports.sample_pending", &[]);
    }
    let seconds = now_ms.saturating_sub(sampled_at_ms) / 1_000;
    if seconds == 0 {
        catalog.t("ports.sample_now", &[])
    } else if seconds >= 30 {
        catalog.t("ports.sample_stale", &[("seconds", &seconds.to_string())])
    } else {
        catalog.t("ports.sample_age", &[("seconds", &seconds.to_string())])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port_inventory::{PortOwnership, PortProtocol, PortRow, PortSnapshot};
    use egui_kittest::kittest::Queryable;
    use std::sync::Arc;

    fn snapshot() -> PortSnapshot {
        PortSnapshot {
            generation: 1,
            sampled_at_ms: 1_000,
            rows: Arc::from([
                row("active", "Active workspace", 3000, PortOwnership::Workspace),
                row_with_bind(
                    "other",
                    "Other workspace",
                    4000,
                    "::1",
                    PortOwnership::Workspace,
                ),
                row("", "", 5000, PortOwnership::External),
            ]),
        }
    }

    fn row(id: &str, name: &str, port: u16, ownership: PortOwnership) -> PortRow {
        row_with_bind(id, name, port, "127.0.0.1", ownership)
    }

    fn row_with_bind(
        id: &str,
        name: &str,
        port: u16,
        bind: &str,
        ownership: PortOwnership,
    ) -> PortRow {
        PortRow {
            pid: u32::from(port),
            port,
            bind: Arc::from(bind),
            protocol: PortProtocol::Tcp,
            process: Arc::from("node"),
            process_started_at: Arc::from("fixture-start"),
            workspace_id: (!id.is_empty()).then(|| Arc::from(id)),
            workspace_name: (!name.is_empty()).then(|| Arc::from(name)),
            ownership,
        }
    }

    fn harness() -> egui_kittest::Harness<'static, (PortsUi, Vec<PortsIntent>)> {
        let snapshot = snapshot();
        let catalog = i18n::Catalog::load("en-US").unwrap();
        egui_kittest::Harness::new_ui_state(
            move |ui, (manager, intents)| {
                if let Some(intent) =
                    manager.contents(ui, Some(&snapshot), Some("active"), 41_000, &catalog)
                {
                    intents.push(intent);
                }
            },
            (PortsUi::default(), Vec::new()),
        )
    }

    #[test]
    fn ports_manager_groups_active_other_and_external_rows() {
        let mut harness = harness();
        harness.run();
        assert!(harness.get_all_by_label("Active workspace").count() >= 1);
        harness.get_by_label("Other workspaces");
        harness.get_by_label("External");
        harness.get_by_label("Stale · 40 seconds ago");
    }

    #[test]
    fn ports_manager_exposes_terminate_only_for_owned_workspace_rows() {
        let mut harness = harness();
        harness.run();
        harness.get_by_label("Terminate node on 127.0.0.1:3000 in Active workspace");
        harness.get_by_label("Terminate node on [::1]:4000 in Other workspace");
        assert!(harness.query_by_label("Terminate").is_none());
        assert!(harness.query_by_label("Open address").is_none());
        harness.get_by_label("Copy 127.0.0.1:3000");
        harness.get_by_label("Copy [::1]:4000");
    }

    #[test]
    fn unopened_ports_manager_emits_no_scan_intent() {
        let mut manager = PortsUi::default();
        assert!(!manager.is_open());
        assert_eq!(manager.take_open_refresh(), None);
    }

    #[test]
    fn every_open_transition_requests_one_refresh_even_with_cached_snapshot() {
        let mut manager = PortsUi::default();
        manager.set_open(true);
        assert_eq!(manager.take_open_refresh(), Some(PortsIntent::Refresh));
        assert_eq!(manager.take_open_refresh(), None);
        manager.set_open(false);
        manager.set_open(true);
        assert_eq!(manager.take_open_refresh(), Some(PortsIntent::Refresh));
    }

    #[test]
    fn port_confirmation_identifies_exact_target_and_can_cancel_then_confirm() {
        let mut harness = harness();
        harness.run();
        let terminate = "Terminate node on 127.0.0.1:3000 in Active workspace";
        harness.get_by_label(terminate).click();
        harness.run();
        harness.get_by_label("Terminate node at 127.0.0.1:3000 in Active workspace?");
        harness.get_by_label("Cancel").click();
        harness.run();
        assert!(
            harness
                .query_by_label("Terminate node at 127.0.0.1:3000 in Active workspace?")
                .is_none()
        );

        harness.get_by_label(terminate).click();
        harness.run();
        harness.get_by_label("Confirm termination").click();
        harness.run();
        assert!(matches!(
            harness.state().1.last(),
            Some(PortsIntent::Terminate(target))
                if target.port == 3000 && target.bind.as_ref() == "127.0.0.1"
        ));
    }

    #[test]
    fn socket_address_preserves_ipv4_ipv6_and_wildcards_without_http_guessing() {
        assert_eq!(
            listener_socket(&row("a", "A", 3000, PortOwnership::Workspace)),
            "127.0.0.1:3000"
        );
        assert_eq!(
            listener_socket(&row_with_bind(
                "a",
                "A",
                4000,
                "::1",
                PortOwnership::Workspace
            )),
            "[::1]:4000"
        );
        assert_eq!(
            listener_socket(&row_with_bind(
                "a",
                "A",
                5000,
                "::",
                PortOwnership::Workspace
            )),
            "[::]:5000"
        );
        assert_eq!(
            listener_socket(&row_with_bind(
                "a",
                "A",
                6000,
                "*",
                PortOwnership::Workspace
            )),
            "*:6000"
        );
    }

    #[test]
    fn production_source_has_no_host_process_filesystem_or_polling_edges() {
        let source = include_str!("ports.rs")
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
