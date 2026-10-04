use crate::port_inventory::{PortOwnership, PortRow, PortSnapshot, PortTerminationTarget};
use std::sync::Arc;

const PORTS_POPOVER_WIDTH: f32 = 560.0;
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
        let mut intent = None;
        let mut footer_close = false;
        let closed = super::popup::popover(
            ui,
            super::popup::PopupSpec {
                id: egui::Id::new("ports_manager"),
                width: PORTS_POPOVER_WIDTH,
                title: &catalog.t("ports.title", &[]),
                subtitle: &catalog.t("ports.description", &[]),
                close_label: &catalog.t("popup.dismiss", &[]),
                close_enabled: true,
            },
            |ui| {
                super::popup::body_with_max_height(ui, PORTS_POPOVER_MAX_HEIGHT, |ui| {
                    let Some(snapshot) = snapshot else {
                        super::popup::notice(
                            ui,
                            &catalog.t("ports.not_scanned", &[]),
                            super::popup::NoticeTone::Info,
                        );
                        return;
                    };
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(port_sample_age(
                                snapshot.sampled_at_ms,
                                now_ms,
                                catalog,
                            ))
                            .size(11.0),
                        )
                        .wrap(),
                    );
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
                        super::popup::notice(
                            ui,
                            &catalog.t("ports.empty", &[]),
                            super::popup::NoticeTone::Info,
                        );
                    }
                });
                super::popup::footer(ui, None, |ui| {
                    if super::popup::action_button(
                        ui,
                        &catalog.t("ports.refresh", &[]),
                        super::popup::ActionTone::Primary,
                        true,
                    )
                    .clicked()
                        && intent.is_none()
                    {
                        intent = Some(PortsIntent::Refresh);
                    }
                    footer_close = super::popup::action_button(
                        ui,
                        &catalog.t("action.close", &[]),
                        super::popup::ActionTone::Ghost,
                        true,
                    )
                    .clicked();
                });
            },
        );
        if closed || footer_close {
            self.set_open(false);
            ui.close();
        }
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
        let unknown = row
            .workspace_name
            .is_none()
            .then(|| catalog.t("ports.workspace_unknown", &[]));
        let workspace = row
            .workspace_name
            .as_deref()
            .or(unknown.as_deref())
            .unwrap_or_default();
        super::popup::list_row(ui, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!("{} · {}", row.port, row.process))
                        .size(13.0)
                        .strong(),
                )
                .wrap(),
            );
            if let Some(name) = &row.workspace_name {
                ui.add(
                    egui::Label::new(egui::RichText::new(name.as_ref()).size(12.0).weak()).wrap(),
                );
            }
            ui.add(
                egui::Label::new(egui::RichText::new(&socket).size(11.0).monospace().weak()).wrap(),
            );
            if row.ownership != PortOwnership::Workspace {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(read_only_reason(row.ownership, catalog))
                            .size(12.0)
                            .weak(),
                    )
                    .wrap(),
                );
            }
            super::popup::list_actions(ui, |ui| {
                let accessible = catalog.t(
                    "ports.copy_socket_accessible",
                    &[("socket", socket.as_str())],
                );
                let response = super::popup::action_button(
                    ui,
                    &catalog.t("ports.copy_address", &[]),
                    super::popup::ActionTone::Secondary,
                    true,
                );
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
                if row.ownership == PortOwnership::Workspace {
                    let accessible = catalog.t(
                        "ports.terminate_socket_accessible",
                        &[
                            ("process", row.process.as_ref()),
                            ("socket", socket.as_str()),
                            ("workspace", workspace),
                        ],
                    );
                    let response = super::popup::action_button(
                        ui,
                        &catalog.t("ports.terminate", &[]),
                        super::popup::ActionTone::Ghost,
                        true,
                    );
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
                            workspace: Arc::from(workspace),
                        });
                    }
                }
            });
        });
    }

    pub(crate) fn confirmation(
        &mut self,
        ctx: &egui::Context,
        catalog: &i18n::Catalog,
    ) -> Option<PortsIntent> {
        let confirm = self.confirm.clone()?;
        let message = catalog.t(
            "ports.confirm_target",
            &[
                ("process", confirm.process.as_ref()),
                ("socket", confirm.socket.as_ref()),
                ("workspace", confirm.workspace.as_ref()),
            ],
        );
        let choice = super::popup::confirmation(
            ctx,
            super::popup::ConfirmationSpec {
                id: egui::Id::new("ports_terminate_confirmation"),
                title: &catalog.t("ports.confirm_title", &[]),
                subtitle: confirm.workspace.as_ref(),
                target: Some(confirm.socket.as_ref()),
                message: &message,
                confirm_label: &catalog.t("ports.confirm_accept", &[]),
                cancel_label: &catalog.t("ports.cancel", &[]),
                close_label: &catalog.t("popup.dismiss", &[]),
            },
        );
        match choice {
            Some(super::popup::ConfirmationChoice::Confirm) => {
                self.confirm = None;
                Some(PortsIntent::Terminate(confirm.target))
            }
            Some(super::popup::ConfirmationChoice::Cancel) => {
                self.confirm = None;
                None
            }
            None => None,
        }
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
    #[test]
    #[ignore = "offscreen popup PNGs for manual visual review"]
    fn popup_parity_render_port_confirmation() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let row = row("serenity", "Serenity", 3000, PortOwnership::Workspace);
        let manager = PortsUi {
            confirm: Some(PortConfirmation {
                target: termination_target(&row).unwrap(),
                process: row.process.clone(),
                socket: listener_socket(&row).into(),
                workspace: "Serenity".into(),
            }),
            ..Default::default()
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 650.0))
            .build_ui_state(
                |ui, manager: &mut PortsUi| {
                    assert!(manager.confirmation(ui.ctx(), &catalog).is_none());
                },
                manager,
            );
        crate::fonts::install_cjk_fallback(&harness.ctx, None, "JetBrainsMono", "Regular");
        harness.ctx.set_visuals(egui::Visuals::dark());
        harness.run();
        let output =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/popup-parity");
        std::fs::create_dir_all(&output).unwrap();
        harness
            .render()
            .unwrap()
            .save(output.join("12-port.png"))
            .unwrap();
    }

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
                if let Some(intent) = manager
                    .contents(ui, Some(&snapshot), Some("active"), 41_000, &catalog)
                    .or_else(|| manager.confirmation(ui.ctx(), &catalog))
                {
                    intents.push(intent);
                }
            },
            (PortsUi::default(), Vec::new()),
        )
    }

    #[test]
    fn env_ports_popup_port_list_has_standard_action_sizes_and_exact_copy_intent() {
        let mut harness = harness();
        harness.set_size(egui::vec2(800.0, 900.0));
        harness.run();
        for label in [
            "Refresh",
            "Copy 127.0.0.1:3000",
            "Terminate node on 127.0.0.1:3000 in Active workspace",
        ] {
            let rect = harness.get_by_label(label).rect();
            assert!((rect.height() - 34.0).abs() < 0.5, "{label}: {rect:?}");
        }
        harness.get_by_label("Copy [::1]:4000").scroll_to_me();
        harness.run();
        harness.get_by_label("Copy [::1]:4000").click();
        harness.run();
        assert_eq!(
            harness.state().1.last(),
            Some(&PortsIntent::CopyAddress(Arc::from("[::1]:4000")))
        );
        assert!(!crate::ui::popup::modal_input_blocked(&harness.ctx));
    }

    #[test]
    fn env_ports_popup_port_list_keeps_controls_inside_a_narrow_viewport() {
        let mut harness = harness();
        harness.set_size(egui::vec2(280.0, 800.0));
        harness.run();
        for label in ["Refresh", "Copy 127.0.0.1:3000"] {
            let rect = harness.get_by_label(label).rect();
            assert!(
                rect.left() >= 0.0 && rect.right() <= 280.0,
                "{label}: {rect:?}"
            );
        }
    }

    #[test]
    fn env_ports_popup_port_list_close_and_refresh_keep_native_popover_ownership() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let snapshot = snapshot();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 800.0))
            .build_ui_state(
                move |ui, (manager, intents): &mut (PortsUi, Vec<PortsIntent>)| {
                    let anchor = ui.button("Ports menu");
                    if anchor.clicked() {
                        manager.toggle_open();
                    }
                    let mut open = manager.is_open();
                    egui::Popup::menu(&anchor)
                        .open_bool(&mut open)
                        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                        .frame(super::super::popup::popover_frame(ui.ctx()))
                        .show(|ui| {
                            if let Some(intent) = manager.contents(
                                ui,
                                Some(&snapshot),
                                Some("active"),
                                41_000,
                                &catalog,
                            ) {
                                intents.push(intent);
                            }
                        });
                    manager.set_open(open);
                },
                (PortsUi::default(), Vec::new()),
            );
        harness.get_by_label("Ports menu").click();
        harness.run();
        assert!(
            !harness
                .ctx
                .memory(|memory| memory.top_modal_layer().is_some())
        );
        harness.get_by_label("Refresh").click();
        harness.run();
        assert_eq!(harness.state().1, vec![PortsIntent::Refresh]);
        assert!(harness.state().0.is_open());
        harness.get_by_label("Close").click();
        harness.run();
        assert!(!harness.state().0.is_open());
    }

    #[test]
    #[ignore = "offscreen common port popup PNGs for visual review"]
    fn env_ports_popup_render_port_list() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let snapshot = snapshot();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 800.0))
            .build_ui_state(
                move |ui, manager: &mut PortsUi| {
                    let anchor = ui.button("Port fixture");
                    let mut open = true;
                    egui::Popup::menu(&anchor)
                        .open_bool(&mut open)
                        .frame(super::super::popup::popover_frame(ui.ctx()))
                        .show(|ui| {
                            let _ = manager.contents(
                                ui,
                                Some(&snapshot),
                                Some("active"),
                                1_000,
                                &catalog,
                            );
                        });
                },
                PortsUi::default(),
            );
        crate::fonts::install_cjk_fallback(&harness.ctx, None, "JetBrainsMono", "Regular");
        harness.ctx.set_visuals(egui::Visuals::dark());
        harness.run();
        let output = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/popup-parity/20261003");
        std::fs::create_dir_all(&output).unwrap();
        harness
            .render()
            .unwrap()
            .save(output.join("37-ports.png"))
            .unwrap();
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
        assert!(
            harness
                .ctx
                .memory(|memory| memory.area_rect(egui::Id::new("ports_terminate_confirmation")))
                .is_some(),
            "confirmation must use the shared modal"
        );
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
    fn confirmation_outlives_source_popover_and_escape_cancels() {
        let snapshot = snapshot();
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 800.0))
            .build_ui_state(
                move |ui, (manager, intents): &mut (PortsUi, Vec<PortsIntent>)| {
                    let anchor = ui.button("Ports menu");
                    if anchor.clicked() {
                        manager.toggle_open();
                    }
                    let mut open = manager.is_open();
                    egui::Popup::menu(&anchor)
                        .open_bool(&mut open)
                        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                        .show(|ui| {
                            if let Some(intent) = manager.contents(
                                ui,
                                Some(&snapshot),
                                Some("active"),
                                41_000,
                                &catalog,
                            ) {
                                intents.push(intent);
                            }
                        });
                    manager.set_open(open);
                    if let Some(intent) = manager.confirmation(ui.ctx(), &catalog) {
                        intents.push(intent);
                    }
                },
                (PortsUi::default(), Vec::new()),
            );
        harness.get_by_label("Ports menu").click();
        harness.run();
        harness
            .get_by_label("Terminate node on 127.0.0.1:3000 in Active workspace")
            .click();
        harness.run();
        harness.state_mut().0.set_open(false);
        harness.run();
        harness.get_by_label("Confirm termination");
        harness.key_press(egui::Key::Escape);
        harness.run();
        assert!(harness.state().0.confirm.is_none());
        assert!(harness.state().1.is_empty());
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
