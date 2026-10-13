//! Session grants and actual single-writer leases; consent never acquires a lease.
use super::{
    CloudAgent, layout,
    settings::{SettingsState, result_notice},
};
use agent_mcp::{ConnectionState, now};

impl CloudAgent {
    pub(super) fn permission_settings(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        state: &mut SettingsState,
    ) {
        let connections = self.connection_infos();
        if !connections
            .iter()
            .any(|info| state.selected.as_deref() == Some(&info.principal.id))
        {
            state.selected = connections.first().map(|info| info.principal.id.clone());
        }
        ui.horizontal_wrapped(|ui| {
            layout::hint(ui, catalog.t("cloud.ui.target", &[]));
            egui::ComboBox::from_id_salt("cloud-permission-actor")
                .selected_text(
                    connections
                        .iter()
                        .find(|info| Some(&info.principal.id) == state.selected.as_ref())
                        .map_or_else(
                            || catalog.t("cloud.ui.select_connection", &[]),
                            |info| self.redact(&info.principal.name),
                        ),
                )
                .width(230.0_f32.min(ui.available_width()))
                .show_ui(ui, |ui| {
                    for info in &connections {
                        ui.selectable_value(
                            &mut state.selected,
                            Some(info.principal.id.clone()),
                            self.redact(&info.principal.name),
                        );
                    }
                });
            let active = connections
                .iter()
                .find(|info| Some(&info.principal.id) == state.selected.as_ref())
                .is_some_and(|info| info.state == ConnectionState::Active);
            ui.add_enabled_ui(active, |ui| {
                if layout::button(ui, &catalog.t("cloud.ui.reclaim_actor", &[])).clicked()
                    && let Some(id) = state.selected.clone()
                {
                    for target in self.targets.clone() {
                        let permission = self.connection_permission(&id, &target);
                        if self
                            .control
                            .permission(&id, &target.id, &target.generation)
                            .input
                        {
                            result_notice(
                                state,
                                self.set_connection_access(&id, &target, permission.read, false),
                                "cloud.control_taken",
                            );
                        }
                    }
                }
            });
        });
        layout::hint(ui, catalog.t("cloud.ui.permission_intro", &[]));
        ui.add_space(12.0);
        let Some(info) = connections
            .iter()
            .find(|info| Some(&info.principal.id) == state.selected.as_ref())
        else {
            layout::hint(ui, catalog.t("cloud.ui.permission_no_connection", &[]));
            return;
        };
        let active = info.state == ConnectionState::Active;
        if !active {
            layout::hint(ui, catalog.t("cloud.reauth_hint", &[]));
            ui.add_space(12.0);
        }
        egui::ScrollArea::horizontal()
            .id_salt("cloud-permission-table-horizontal")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.set_min_width(710.0);
                let width = ui.available_width().max(710.0);
                let columns = [
                    width * 0.38,
                    width * 0.16,
                    80.0,
                    80.0,
                    width * (1.0 - 0.38 - 0.16) - 160.0,
                ];
                let header = ui
                    .allocate_exact_size(egui::vec2(width, 34.0), egui::Sense::hover())
                    .0;
                let mut x = header.left();
                for (index, key) in [
                    "cloud.ui.workspace_session",
                    "cloud.ui.local_state",
                    "cloud.ui.read",
                    "cloud.ui.input",
                    "cloud.ui.control",
                ]
                .iter()
                .enumerate()
                {
                    let mut cell = layout::child(
                        ui,
                        egui::Rect::from_min_size(
                            egui::pos2(x, header.top() + 8.0),
                            egui::vec2(
                                columns[index] - if matches!(index, 2 | 3) { 0.0 } else { 8.0 },
                                24.0,
                            ),
                        ),
                        ("cloud-table-header", index),
                    );
                    if matches!(index, 2 | 3) {
                        cell.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                            layout::hint(ui, catalog.t(key, &[]));
                        });
                    } else {
                        layout::hint(&mut cell, catalog.t(key, &[]));
                    }
                    x += columns[index];
                }
                layout::line(ui);
                for target in self.targets.clone() {
                    ui.push_id((&info.principal.id, &target.id, &target.generation), |ui| {
                        let rect = ui
                            .allocate_exact_size(egui::vec2(width, 68.0), egui::Sense::hover())
                            .0;
                        let mut x = rect.left();
                        let mut cells = Vec::new();
                        for (index, w) in columns.iter().enumerate() {
                            cells.push(layout::child(
                                ui,
                                egui::Rect::from_min_size(
                                    egui::pos2(
                                        x + if matches!(index, 2 | 3) {
                                            (*w - layout::CONTROL_HEIGHT) / 2.0
                                        } else {
                                            0.0
                                        },
                                        rect.top()
                                            + if matches!(index, 2 | 3) {
                                                (68.0 - layout::CONTROL_HEIGHT) / 2.0
                                            } else {
                                                6.0
                                            },
                                    ),
                                    egui::vec2(
                                        if matches!(index, 2 | 3) {
                                            layout::CONTROL_HEIGHT
                                        } else {
                                            *w - 8.0
                                        },
                                        if matches!(index, 2 | 3) {
                                            layout::CONTROL_HEIGHT
                                        } else {
                                            56.0
                                        },
                                    ),
                                ),
                                ("cloud-permission-cell", index),
                            ));
                            x += w;
                        }
                        layout::title(
                            &mut cells[0],
                            self.redact(&format!("{} · {}", target.workspace_name, target.title)),
                        );
                        layout::hint(
                            &mut cells[0],
                            self.redact(
                                target
                                    .agent_line
                                    .as_deref()
                                    .unwrap_or(&catalog.t("workspace.spawn.shell", &[])),
                            ),
                        );
                        let local = target.local_status_label(catalog);
                        let observation = target.local_observation_label(catalog);
                        cells[1]
                            .label(egui::RichText::new(&local).size(13.0))
                            .on_hover_text(format!(
                                "{}\n{}",
                                catalog.t("cloud.local_status", &[("status", &local)]),
                                observation
                            ));
                        let permission = self.connection_permission(&info.principal.id, &target);
                        let mut read = active && permission.read;
                        let mut input = active && permission.input;
                        let read_response = cells[2]
                            .add_enabled_ui(active && target.live, |ui| {
                                ui.add_sized(
                                    egui::vec2(layout::CONTROL_HEIGHT, layout::CONTROL_HEIGHT),
                                    egui::Checkbox::without_text(&mut read),
                                )
                            })
                            .inner;
                        read_response.widget_info(|| {
                            egui::WidgetInfo::selected(
                                egui::WidgetType::Checkbox,
                                active && target.live,
                                read,
                                catalog.t("cloud.connection_read", &[]),
                            )
                        });
                        if read_response.changed() {
                            input &= read;
                            result_notice(
                                state,
                                self.set_connection_access(
                                    &info.principal.id,
                                    &target,
                                    read,
                                    input,
                                ),
                                "cloud.permission_saved",
                            );
                        }
                        let input_enabled =
                            active && target.live && read && info.principal.input_scope;
                        let input_response = cells[3]
                            .add_enabled_ui(input_enabled, |ui| {
                                ui.add_sized(
                                    egui::vec2(layout::CONTROL_HEIGHT, layout::CONTROL_HEIGHT),
                                    egui::Checkbox::without_text(&mut input),
                                )
                            })
                            .inner;
                        input_response.widget_info(|| {
                            egui::WidgetInfo::selected(
                                egui::WidgetType::Checkbox,
                                input_enabled,
                                input,
                                catalog.t("cloud.connection_input", &[]),
                            )
                        });
                        if input_response.changed() {
                            result_notice(
                                state,
                                self.set_connection_access(
                                    &info.principal.id,
                                    &target,
                                    read,
                                    input,
                                ),
                                "cloud.permission_saved",
                            );
                        }
                        let lease = self.connection_lease(&target);
                        let owner = lease
                            .as_ref()
                            .map(|lease| {
                                connections
                                    .iter()
                                    .find(|info| info.principal.id == lease.connection_id)
                                    .map_or_else(
                                        || self.redact(&lease.connection_id),
                                        |info| self.redact(&info.principal.name),
                                    )
                            })
                            .unwrap_or_else(|| catalog.t("cloud.lease_you", &[]));
                        layout::hint(
                            &mut cells[4],
                            catalog.t("cloud.lease_owner", &[("client", &owner)]),
                        );
                        if let Some(lease) = &lease {
                            cells[4].response().on_hover_text(catalog.t(
                                "cloud.lease_expiry",
                                &[(
                                    "seconds",
                                    &lease.expires_at.saturating_sub(now()).to_string(),
                                )],
                            ));
                        }
                        if target.live
                            && (input || lease.is_some())
                            && layout::button_named(
                                &mut cells[4],
                                &catalog.t("cloud.ui.reclaim", &[]),
                                &catalog.t("cloud.session_take_control", &[]),
                            )
                            .clicked()
                        {
                            self.take_session_control(&target);
                            state.notice = Some(("cloud.control_taken", None));
                        }
                        if !target.live {
                            layout::hint(&mut cells[4], catalog.t("cloud.exited", &[]));
                        }
                        ui.painter().hline(
                            rect.x_range(),
                            rect.bottom(),
                            egui::Stroke::new(
                                1.0,
                                ui.visuals().widgets.noninteractive.bg_stroke.color,
                            ),
                        );
                    });
                }
            });
        egui::CollapsingHeader::new(catalog.t("cloud.ui.permission_difference", &[]))
            .id_salt("cloud-permission-help")
            .show(ui, |ui| {
                layout::hint(ui, catalog.t("cloud.connection_permission_hint", &[]));
                if layout::button(ui, &catalog.t("cloud.take_control", &[])).clicked() {
                    self.take_control();
                    state.notice = Some(("cloud.control_taken", None));
                }
            });
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(1));
    }
}
