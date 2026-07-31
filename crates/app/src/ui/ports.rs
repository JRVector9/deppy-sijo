use crate::port_inventory::{
    PortOwnership, PortProtocol, PortRow, PortSnapshot, PortTerminationTarget,
};
use std::sync::Arc;

const PORTS_POPOVER_WIDTH: f32 = 500.0;
const PORTS_POPOVER_MAX_HEIGHT: f32 = 560.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PortsIntent {
    Refresh,
    Terminate(PortTerminationTarget),
    OpenAddress(Arc<str>),
    CopyAddress(Arc<str>),
}

#[derive(Default)]
pub(crate) struct PortsUi {
    open: bool,
    confirm: Option<PortTerminationTarget>,
    initial_refresh_requested: bool,
}

impl PortsUi {
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

    pub(crate) fn take_initial_refresh(
        &mut self,
        snapshot: Option<&PortSnapshot>,
    ) -> Option<PortsIntent> {
        if snapshot.is_some() {
            self.initial_refresh_requested = true;
            return None;
        }
        if !self.open || self.initial_refresh_requested {
            return None;
        }
        self.initial_refresh_requested = true;
        Some(PortsIntent::Refresh)
    }

    pub(crate) fn contents(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: Option<&PortSnapshot>,
        active_workspace_id: Option<&str>,
    ) -> Option<PortsIntent> {
        ui.set_min_width(PORTS_POPOVER_WIDTH);
        let mut intent = None;
        ui.horizontal(|ui| {
            ui.heading("포트");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("새로고침").clicked() {
                    intent = Some(PortsIntent::Refresh);
                }
            });
        });
        ui.add_space(4.0);
        ui.weak("로컬 수신 포트입니다. 소유권이 다시 확인된 프로세스만 종료합니다.");
        ui.separator();

        let Some(snapshot) = snapshot else {
            ui.weak("아직 포트를 조회하지 않았습니다.");
            return intent;
        };

        egui::ScrollArea::vertical()
            .id_salt("ports_manager_rows")
            .max_height(PORTS_POPOVER_MAX_HEIGHT)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                self.render_section(
                    ui,
                    "현재 워크스페이스",
                    snapshot.rows.iter().filter(|row| {
                        row.ownership == PortOwnership::Workspace
                            && row.workspace_id.as_deref() == active_workspace_id
                    }),
                    &mut intent,
                );
                self.render_section(
                    ui,
                    "다른 워크스페이스",
                    snapshot.rows.iter().filter(|row| {
                        row.ownership == PortOwnership::Workspace
                            && row.workspace_id.as_deref() != active_workspace_id
                    }),
                    &mut intent,
                );
                self.render_section(
                    ui,
                    "외부 프로세스",
                    snapshot
                        .rows
                        .iter()
                        .filter(|row| row.ownership != PortOwnership::Workspace),
                    &mut intent,
                );
                if snapshot.rows.is_empty() {
                    ui.weak("열려 있는 수신 포트가 없습니다.");
                }
            });
        self.render_confirmation(ui, &mut intent);
        intent
    }

    fn render_section<'a>(
        &mut self,
        ui: &mut egui::Ui,
        title: &str,
        rows: impl Iterator<Item = &'a PortRow>,
        intent: &mut Option<PortsIntent>,
    ) {
        let mut rows = rows.peekable();
        if rows.peek().is_none() {
            return;
        }
        ui.label(egui::RichText::new(title).strong());
        for row in rows {
            self.render_row(ui, row, intent);
        }
        ui.add_space(6.0);
    }

    fn render_row(&mut self, ui: &mut egui::Ui, row: &PortRow, intent: &mut Option<PortsIntent>) {
        let address = listener_address(row);
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
                            if ui.button("종료").clicked()
                                && let Some(target) = termination_target(row)
                            {
                                self.confirm = Some(target);
                            }
                        } else {
                            ui.weak(read_only_reason(row.ownership));
                        }
                        if ui.button("복사").clicked() {
                            *intent = Some(PortsIntent::CopyAddress(Arc::from(address.as_str())));
                        }
                        if ui.button("열기").clicked() {
                            *intent = Some(PortsIntent::OpenAddress(Arc::from(address.as_str())));
                        }
                    });
                });
                ui.weak(address.as_str());
            });
        ui.separator();
    }

    fn render_confirmation(&mut self, ui: &mut egui::Ui, intent: &mut Option<PortsIntent>) {
        let Some(target) = self.confirm.clone() else {
            return;
        };
        ui.separator();
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!("{} 포트의 프로세스를 종료할까요?", target.port),
        );
        ui.horizontal(|ui| {
            if ui.button("종료 확인").clicked() {
                *intent = Some(PortsIntent::Terminate(target));
                self.confirm = None;
            }
            if ui.button("취소").clicked() {
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

fn listener_address(row: &PortRow) -> String {
    let bind = match row.bind.as_ref() {
        "*" | "0.0.0.0" | "::" => "127.0.0.1".to_owned(),
        value if value.contains(':') => format!("[{value}]"),
        value => value.to_owned(),
    };
    let scheme = match row.protocol {
        PortProtocol::Tcp => "http",
    };
    format!("{scheme}://{bind}:{}", row.port)
}

fn read_only_reason(ownership: PortOwnership) -> &'static str {
    match ownership {
        PortOwnership::Protected => "보호됨 · 읽기 전용",
        PortOwnership::Ambiguous => "소유권 불명 · 읽기 전용",
        PortOwnership::External => "외부 · 읽기 전용",
        PortOwnership::Workspace => "",
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
                row("other", "Other workspace", 4000, PortOwnership::Workspace),
                row("", "", 5000, PortOwnership::External),
            ]),
        }
    }

    fn row(id: &str, name: &str, port: u16, ownership: PortOwnership) -> PortRow {
        PortRow {
            pid: u32::from(port),
            port,
            bind: Arc::from("127.0.0.1"),
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
        egui_kittest::Harness::new_ui_state(
            move |ui, (manager, intents)| {
                if let Some(intent) = manager.contents(ui, Some(&snapshot), Some("active")) {
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
        harness.get_by_label("현재 워크스페이스");
        harness.get_by_label("다른 워크스페이스");
        harness.get_by_label("외부 프로세스");
    }

    #[test]
    fn ports_manager_exposes_terminate_only_for_owned_workspace_rows() {
        let mut harness = harness();
        harness.run();
        assert_eq!(harness.get_all_by_label("종료").count(), 2);
    }

    #[test]
    fn unopened_ports_manager_emits_no_scan_intent() {
        let mut manager = PortsUi::default();
        assert!(!manager.is_open());
        assert_eq!(manager.take_initial_refresh(None), None);
    }

    #[test]
    fn first_open_requests_one_scan_only() {
        let mut manager = PortsUi::default();
        manager.toggle_open();
        assert_eq!(
            manager.take_initial_refresh(None),
            Some(PortsIntent::Refresh)
        );
        assert_eq!(manager.take_initial_refresh(None), None);
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
