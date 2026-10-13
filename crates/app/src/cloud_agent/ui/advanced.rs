//! Existing endpoint/profile/recovery controls in the standard settings row layout.
use super::{
    Action, CloudAgent, layout,
    settings::{SettingsState, result_notice},
};

impl CloudAgent {
    pub(super) fn advanced_settings(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        state: &mut SettingsState,
    ) {
        let running = self.busy();
        layout::title(ui, catalog.t("cloud.ui.endpoint_settings", &[]));
        layout::setting_row(
            ui,
            &catalog.t("cloud.ui.address_mode", &[]),
            &catalog.t("cloud.ui.address_hint", &[]),
            |ui| {
                ui.add_enabled_ui(!running, |ui| {
                    egui::ComboBox::from_id_salt("cloud-address-mode")
                        .selected_text(catalog.t(
                            if self.automatic {
                                "cloud.mode_auto"
                            } else {
                                "cloud.mode_manual"
                            },
                            &[],
                        ))
                        .width(ui.available_width().min(230.0))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.automatic,
                                true,
                                catalog.t("cloud.mode_auto", &[]),
                            );
                            ui.selectable_value(
                                &mut self.automatic,
                                false,
                                catalog.t("cloud.mode_manual", &[]),
                            );
                        });
                });
            },
        );
        if !self.automatic {
            layout::setting_row(
                ui,
                &catalog.t("cloud.hostname", &[]),
                &catalog.t("cloud.tunnel_hint", &[]),
                |ui| {
                    ui.add_enabled_ui(!running, |ui| {
                        layout::text_input(
                            ui,
                            ui.available_width().min(230.0),
                            egui::TextEdit::singleline(&mut self.hostname).char_limit(259),
                        )
                    })
                    .inner
                    .on_hover_text(catalog.t("cloud.hostname", &[]));
                },
            );
        }
        layout::setting_row(
            ui,
            &catalog.t("cloud.port", &[]),
            &catalog.t("cloud.ui.port_hint", &[]),
            |ui| {
                ui.add_enabled_ui(!running, |ui| {
                    ui.add_sized(
                        [230.0_f32.min(ui.available_width()), 34.0],
                        egui::DragValue::new(&mut self.port).range(1024..=65535),
                    );
                });
            },
        );
        layout::setting_row(
            ui,
            &catalog.t("cloud.ui.reconnect", &[]),
            &catalog.t("cloud.ui.reconnect_hint", &[]),
            |ui| {
                let mut reconnect = self.reconnect;
                if layout::toggle(ui, &mut reconnect, &catalog.t("cloud.reconnect", &[])) {
                    self.set_reconnect(reconnect);
                }
            },
        );
        layout::hint(ui, catalog.t("cloud.ui.address_locked", &[]));
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(!running, |ui| {
                    let response = layout::primary(ui, &catalog.t("cloud.ui.apply", &[]));
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            ui.is_enabled(),
                            catalog.t("cloud.start", &[]),
                        )
                    });
                    if response.clicked() {
                        self.action = Some(Action::Start);
                        ui.ctx().request_repaint();
                    }
                });
                ui.add_enabled_ui(
                    running && self.connection != super::super::Connection::Stopping,
                    |ui| {
                        if layout::button_named(
                            ui,
                            &catalog.t("cloud.ui.disconnect", &[]),
                            &catalog.t("cloud.stop", &[]),
                        )
                        .clicked()
                        {
                            self.action = Some(Action::Stop);
                            ui.ctx().request_repaint();
                        }
                    },
                );
            });
        });
        ui.add_space(18.0);
        layout::title(ui, catalog.t("cloud.profiles", &[]));
        if state
            .profile
            .is_none_or(|index| index >= self.profiles.len())
        {
            state.profile = (!self.profiles.is_empty()).then_some(0);
        }
        layout::setting_row(
            ui,
            &catalog.t("cloud.profiles", &[]),
            &catalog.t("cloud.ui.profile_coordinates", &[]),
            |ui| {
                ui.menu_button("⋯", |ui| {
                    if layout::button(ui, &catalog.t("cloud.ui.profile_edit", &[])).clicked() {
                        state.profile_editing = true;
                        ui.close();
                    }
                    ui.add_enabled_ui(!running && state.profile.is_some(), |ui| {
                        if layout::button(ui, &catalog.t("cloud.profile_load", &[])).clicked()
                            && let Some(index) = state.profile
                        {
                            result_notice(state, self.load_profile(index), "cloud.profile_loaded");
                            ui.close();
                        }
                    });
                    ui.add_enabled_ui(state.profile.is_some(), |ui| {
                        if layout::button(ui, &catalog.t("cloud.profile_delete", &[])).clicked()
                            && let Some(index) = state.profile
                        {
                            self.remove_profile(index);
                            state.profile = (!self.profiles.is_empty()).then_some(0);
                            state.notice = Some(("cloud.profile_deleted", None));
                            ui.close();
                        }
                    });
                });
                if layout::button_named(
                    ui,
                    &catalog.t("action.save", &[]),
                    &catalog.t("cloud.profile_save", &[]),
                )
                .clicked()
                {
                    if self.profile_name.trim().is_empty() {
                        state.profile_editing = true;
                    } else {
                        result_notice(state, self.save_profile(), "cloud.profile_saved");
                        if state
                            .notice
                            .as_ref()
                            .is_some_and(|(_, error)| error.is_none())
                        {
                            state.profile = self
                                .profiles
                                .iter()
                                .position(|profile| profile.name == self.profile_name.trim());
                            state.profile_editing = false;
                        }
                    }
                }
                egui::ComboBox::from_id_salt("cloud-profile-selection")
                    .selected_text(
                        state
                            .profile
                            .and_then(|index| self.profiles.get(index))
                            .map_or_else(
                                || catalog.t("cloud.profile_select", &[]),
                                |profile| profile.name.clone(),
                            ),
                    )
                    .width(ui.available_width().min(230.0))
                    .show_ui(ui, |ui| {
                        for (index, profile) in self.profiles.iter().enumerate() {
                            if ui
                                .selectable_value(&mut state.profile, Some(index), &profile.name)
                                .changed()
                            {
                                self.profile_name = profile.name.clone();
                            }
                        }
                    });
            },
        );
        if state.profile_editing {
            layout::hint(ui, catalog.t("cloud.profile_hint", &[]));
            let label = ui.label(catalog.t("cloud.profile_name", &[]));
            layout::text_input(
                ui,
                ui.available_width().min(230.0),
                egui::TextEdit::singleline(&mut self.profile_name)
                    .char_limit(80)
                    .desired_width(230.0_f32.min(ui.available_width())),
            )
            .labelled_by(label.id);
            if layout::button(ui, &catalog.t("action.cancel", &[])).clicked() {
                state.profile_editing = false;
            }
        }
        ui.add_space(18.0);
        egui::CollapsingHeader::new(catalog.t("cloud.ui.legacy_details", &[]))
            .id_salt("cloud-legacy-details")
            .show(ui, |ui| {
                self.endpoint_info(ui, catalog);
                if let Some(server) = &self.server {
                    ui.horizontal_wrapped(|ui| {
                        ui.checkbox(&mut self.reveal, catalog.t("cloud.reveal_token", &[]));
                        let token = server.auth.token_for_user();
                        ui.monospace(if self.reveal {
                            token.as_str()
                        } else {
                            "••••••••••••••••"
                        });
                        if layout::button(ui, &catalog.t("cloud.copy_token", &[])).clicked() {
                            ui.ctx().copy_text(token.to_string());
                        }
                    });
                    layout::hint(
                        ui,
                        catalog.t(
                            "cloud.expiry",
                            &[(
                                "seconds",
                                &server
                                    .auth
                                    .expires()
                                    .saturating_sub(agent_mcp::now())
                                    .to_string(),
                            )],
                        ),
                    );
                    ui.add_enabled_ui(self.ready(), |ui| {
                        if layout::button(ui, &catalog.t("cloud.rotate", &[])).clicked() {
                            self.action = Some(Action::Rotate);
                            ui.ctx().request_repaint();
                        }
                    });
                }
                self.ended_settings(ui, catalog);
                if layout::button(ui, &catalog.t("cloud.copy_instruction", &[])).clicked() {
                    ui.ctx().copy_text(super::CLOUD_AGENT_INSTRUCTIONS.into());
                }
                layout::hint(ui, catalog.t("cloud.answer_hint", &[]));
            });
    }
    fn ended_settings(&self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        ui.add_space(12.0);
        layout::title(ui, catalog.t("cloud.ended_history", &[]));
        layout::hint(ui, catalog.t("cloud.ended_history_hint", &[]));
        if self.ended_load_failed {
            ui.colored_label(
                layout::warn(ui),
                catalog.t("cloud.ended_history_error", &[]),
            );
        }
        let historical = self.historical_rows();
        if historical.is_empty() && self.ended_rx.is_some() {
            ui.spinner();
            layout::hint(ui, catalog.t("cloud.ended_history_loading", &[]));
        } else if historical.is_empty() && !self.ended_load_failed {
            layout::hint(ui, catalog.t("cloud.ended_history_empty", &[]));
        }
        for row in historical.iter().take(256) {
            ui.add(
                egui::Label::new(format!(
                    "{} · {} · {}",
                    row.workspace_name,
                    row.title,
                    catalog.t("cloud.ended_history_status", &[])
                ))
                .truncate(),
            )
            .on_hover_text(&row.id);
        }
    }
}
