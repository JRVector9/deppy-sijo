//! Connection list, inline setup and explicit authentication consent.
use super::{
    Action, CloudAgent, layout,
    settings::{SettingsState, Tab, result_notice},
};
use agent_mcp::{AuthKind, ConnectionInfo, ConnectionState, now};

pub(super) fn activity_key(info: &ConnectionInfo) -> &'static str {
    match info.state {
        ConnectionState::Revoked => "cloud.activity_revoked",
        ConnectionState::Expired => "cloud.activity_expired",
        ConnectionState::Active if info.last_tool_success_at.is_some() => "cloud.activity_tool",
        ConnectionState::Active if info.last_initialize_at.is_some() => {
            "cloud.activity_initialized"
        }
        ConnectionState::Active if info.last_authenticated_at.is_some() => {
            "cloud.activity_authenticated"
        }
        ConnectionState::Active => "cloud.activity_waiting",
    }
}
fn short_key(info: &ConnectionInfo) -> &'static str {
    match activity_key(info) {
        "cloud.activity_tool" => "cloud.ui.tool_ok",
        "cloud.activity_expired" => "cloud.ui.expired",
        "cloud.activity_revoked" => "cloud.ui.revoked",
        "cloud.activity_authenticated" => "cloud.ui.auth_ok",
        "cloud.activity_initialized" => "cloud.ui.initialized",
        _ => "cloud.ui.auth_pending",
    }
}
fn color(ui: &egui::Ui, info: &ConnectionInfo) -> egui::Color32 {
    if info.state != ConnectionState::Active {
        layout::warn(ui)
    } else if info.last_tool_success_at.is_some() {
        layout::good(ui)
    } else {
        layout::secondary(ui)
    }
}
fn auth_key(info: &ConnectionInfo) -> &'static str {
    match info.principal.auth_kind {
        AuthKind::ManualBearer => "cloud.auth_manual",
        AuthKind::OAuth => "cloud.auth_oauth",
    }
}

impl CloudAgent {
    pub(super) fn connector_endpoint(&self) -> Option<String> {
        self.endpoint().filter(|endpoint| {
            !self.automatic
                || (endpoint.starts_with("https://")
                    && self.connection == super::super::Connection::Ready
                    && self.generated_hostname.is_some())
        })
    }
    pub(super) fn connection_settings(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        state: &mut SettingsState,
    ) {
        let ready_key = match self.connection {
            super::super::Connection::Preparing => "cloud.preparing",
            super::super::Connection::Verifying => "cloud.verifying",
            super::super::Connection::Stopping => "cloud.stopping",
            super::super::Connection::Ready
                if self.automatic && self.generated_hostname.is_some() =>
            {
                "cloud.ready"
            }
            super::super::Connection::Ready
                if !self.automatic && !self.hostname.trim().is_empty() =>
            {
                "cloud.manual_unverified"
            }
            super::super::Connection::Ready => "cloud.local_only",
            _ => "cloud.ui.service_stopped",
        };
        let service = ui
            .allocate_exact_size(egui::vec2(ui.available_width(), 61.0), egui::Sense::hover())
            .0;
        let mut copy = layout::child(
            ui,
            egui::Rect::from_min_max(
                service.min,
                egui::pos2(service.right() - 220.0, service.bottom()),
            ),
            "cloud-service-copy",
        );
        layout::title(&mut copy, catalog.t(ready_key, &[]));
        copy.add_space(5.0);
        layout::hint(&mut copy, catalog.t("cloud.ui.service_hint", &[]));
        let mut actions = layout::child(
            ui,
            egui::Rect::from_min_max(
                egui::pos2(service.right() - 210.0, service.top() + 12.0),
                service.max,
            ),
            "cloud-service-actions",
        );
        actions.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            if layout::button(ui, &catalog.t("cloud.ui.connection_info", &[])).clicked() {
                state.server_info = !state.server_info;
            }
            let (key, color) = match self.connection {
                super::super::Connection::Ready
                    if self.automatic && self.generated_hostname.is_some() =>
                {
                    ("cloud.ui.address_verified", layout::good(ui))
                }
                super::super::Connection::Ready => {
                    ("cloud.ui.address_unverified", layout::warn(ui))
                }
                super::super::Connection::Verifying | super::super::Connection::Preparing => {
                    ("cloud.ui.address_checking", layout::warn(ui))
                }
                _ => ("cloud.ui.address_stopped", layout::secondary(ui)),
            };
            ui.label(
                egui::RichText::new(catalog.t(key, &[]))
                    .size(12.0)
                    .color(color),
            );
        });
        if state.server_info {
            ui.add_space(12.0);
            self.endpoint_info(ui, catalog);
            if layout::button(ui, &catalog.t("cloud.ui.address_settings", &[])).clicked() {
                state.tab = Tab::Advanced;
            }
        }
        if let Some(error) = &self.error {
            ui.add_space(10.0);
            let message = if error == "tunnel_companion_missing" {
                catalog.t("cloud.helper_missing", &[])
            } else if error.starts_with("tunnel_") {
                catalog.t("cloud.auto_failed", &[])
            } else {
                catalog.t("cloud.error", &[("code", error)])
            };
            layout::notice(ui, layout::warn(ui), |ui| {
                ui.label(self.redact(&message));
                if layout::button(ui, &catalog.t("cloud.ui.retry", &[])).clicked() {
                    self.action = Some(Action::Start);
                }
            });
        }
        ui.add_space(17.0);
        layout::line(ui);
        ui.add_space(12.0);
        let connections = self.connection_infos();
        if !connections
            .iter()
            .any(|info| state.selected.as_deref() == Some(&info.principal.id))
        {
            state.selected = connections.first().map(|info| info.principal.id.clone());
            state.rename = connections
                .first()
                .map_or_else(String::new, |info| info.principal.name.clone());
            state.editing = false;
        }
        ui.horizontal(|ui| {
            layout::title(ui, catalog.t("cloud.ui.registered", &[]));
            layout::hint(ui, connections.len().to_string());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if layout::primary(ui, &catalog.t("cloud.ui.add", &[])).clicked() {
                    state.adding = true;
                    state.manual = false;
                }
            });
        });
        self.approval_settings(ui, catalog, state);
        ui.add_space(8.0);
        layout::line(ui);
        if connections.is_empty() && !state.adding {
            ui.add_space(32.0);
            ui.vertical_centered(|ui| {
                layout::title(ui, catalog.t("cloud.ui.empty", &[]));
                layout::hint(ui, catalog.t("cloud.connections_empty", &[]));
                ui.add_space(12.0);
                if layout::primary(ui, &catalog.t("cloud.ui.first_add", &[])).clicked() {
                    state.adding = true;
                }
            });
        } else {
            let current = state.selected.clone();
            let selected = connections
                .iter()
                .find(|info| Some(&info.principal.id) == current.as_ref())
                .cloned();
            let mut clicked = None;
            let redaction = self.redaction.clone();
            layout::list_detail(
                ui,
                "cloud-connections",
                1.0 / 2.22,
                |ui| {
                    if connections.is_empty() {
                        return;
                    }
                    layout::hint(ui, catalog.t("cloud.connection_name", &[]));
                    ui.add_space(8.0);
                    for info in &connections {
                        ui.push_id(&info.principal.id, |ui| {
                            let name = redaction.redact_wrapped_text(&info.principal.name);
                            let activity = catalog.t(activity_key(info), &[]);
                            let accessible = format!("{name} · {activity}");
                            let recent = info.last_tool_success_at.map(|time| {
                                crate::ui::notifications::relative_time_label(
                                    catalog,
                                    now().saturating_sub(time) as i64,
                                )
                            });
                            let subtitle = format!(
                                "{} · {}",
                                catalog.t(auth_key(info), &[]),
                                recent.unwrap_or_else(|| catalog.t("cloud.ui.no_tools", &[]))
                            );
                            if layout::list_row(
                                ui,
                                current.as_deref() == Some(&info.principal.id),
                                &accessible,
                                layout::RowContent {
                                    title: &name,
                                    subtitle: &subtitle,
                                    status: &catalog.t(short_key(info), &[]),
                                    icon: name.chars().next(),
                                },
                                color(ui, info),
                            )
                            .clicked()
                            {
                                clicked = Some(info.clone());
                            }
                        });
                    }
                },
                |ui| {
                    if state.adding {
                        self.connection_add(ui, catalog, state);
                    } else if let Some(info) = &selected {
                        self.connection_detail(ui, catalog, state, info);
                    }
                },
            );
            if let Some(info) = clicked {
                state.selected = Some(info.principal.id);
                state.rename = info.principal.name;
                state.editing = false;
                state.adding = false;
                state.notice = None;
                ui.ctx().request_repaint();
            }
        }
        ui.add_space(18.0);
        layout::line(ui);
        ui.add_space(18.0);
        layout::hint(ui, catalog.t("cloud.ui.workflow", &[]));
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(1));
    }
    pub(super) fn endpoint_info(&self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        if let Some(server) = &self.server {
            layout::hint(
                ui,
                catalog.t(
                    "cloud.local_ready",
                    &[("port", &server.addr.port().to_string())],
                ),
            );
            if let Some(endpoint) = self.connector_endpoint() {
                layout::title(
                    ui,
                    catalog.t(
                        if endpoint.starts_with("https://") {
                            "cloud.public_url"
                        } else {
                            "cloud.local_url"
                        },
                        &[],
                    ),
                );
                ui.horizontal_wrapped(|ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(self.redact(&endpoint)).monospace())
                            .wrap(),
                    );
                    if layout::button(ui, &catalog.t("cloud.copy_url", &[])).clicked() {
                        ui.ctx().copy_text(endpoint);
                    }
                });
            }
        }
    }
    fn approval_settings(
        &self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        state: &mut SettingsState,
    ) {
        let Some(server) = &self.server else {
            return;
        };
        for approval in server.auth.approvals() {
            ui.add_space(12.0);
            layout::notice(ui, layout::warn(ui), |ui| {
                layout::title(
                    ui,
                    catalog.t(
                        "cloud.oauth_request",
                        &[("client", &self.redact(&approval.client_name))],
                    ),
                );
                layout::hint(ui, catalog.t("cloud.ui.approval_hint", &[]));
                if state.approval.as_deref() != Some(&approval.id) {
                    if layout::button(ui, &catalog.t("cloud.ui.inspect", &[])).clicked() {
                        state.approval = Some(approval.id.clone());
                    }
                } else {
                    layout::hint(ui, self.redact(&approval.redirect_uri));
                    layout::hint(
                        ui,
                        catalog.t(
                            if approval.input {
                                "cloud.oauth_input"
                            } else {
                                "cloud.oauth_read"
                            },
                            &[],
                        ),
                    );
                    ui.horizontal_wrapped(|ui| {
                        for (allow, key) in
                            [(false, "cloud.oauth_deny"), (true, "cloud.oauth_approve")]
                        {
                            if layout::button(ui, &catalog.t(key, &[])).clicked() {
                                server.auth.approve(&approval.id, allow);
                                state.approval = None;
                                state.notice = Some(("cloud.ui.approval_done", None));
                            }
                        }
                    });
                }
            });
        }
    }
    fn connection_add(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        state: &mut SettingsState,
    ) {
        ui.horizontal_wrapped(|ui| {
            layout::title(ui, catalog.t("cloud.ui.add", &[]));
            if layout::button(ui, &catalog.t("action.close", &[])).clicked() {
                state.adding = false;
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(
                &mut state.manual,
                false,
                catalog.t("cloud.ui.connector_auth", &[]),
            );
            ui.selectable_value(
                &mut state.manual,
                true,
                catalog.t("cloud.ui.independent_token", &[]),
            );
        });
        ui.add_space(12.0);
        if !self.ready() {
            layout::hint(ui, catalog.t("cloud.ui.start_first", &[]));
            if layout::primary(ui, &catalog.t("cloud.ui.address_settings", &[])).clicked() {
                state.tab = Tab::Advanced;
            }
        }
        if state.manual {
            let label = ui.label(catalog.t("cloud.manual_name", &[]));
            layout::text_input(
                ui,
                ui.available_width(),
                egui::TextEdit::singleline(&mut state.manual_name)
                    .char_limit(128)
                    .desired_width(ui.available_width()),
            )
            .labelled_by(label.id);
            ui.checkbox(
                &mut state.manual_input_scope,
                catalog.t("cloud.manual_input_scope", &[]),
            );
            layout::hint(ui, catalog.t("cloud.manual_hint", &[]));
            ui.add_enabled_ui(
                self.server
                    .as_ref()
                    .is_some_and(|server| server.auth.expires() > now()),
                |ui| {
                    if layout::primary(ui, &catalog.t("cloud.manual_issue", &[])).clicked() {
                        let existing = self
                            .connection_infos()
                            .into_iter()
                            .map(|info| info.principal.id)
                            .collect::<std::collections::HashSet<_>>();
                        match self
                            .issue_manual_connection(&state.manual_name, state.manual_input_scope)
                        {
                            Ok(token) => {
                                ui.ctx().copy_text(token);
                                state.manual_input_scope = false;
                                state.adding = false;
                                state.selected = self
                                    .connection_infos()
                                    .into_iter()
                                    .find(|info| !existing.contains(&info.principal.id))
                                    .map(|info| info.principal.id);
                                state.notice = Some(("cloud.manual_issued", None));
                            }
                            Err(code) => {
                                state.notice = Some(("cloud.management_failed", Some(code)))
                            }
                        }
                    }
                },
            );
        } else {
            self.endpoint_info(ui, catalog);
            layout::hint(ui, catalog.t("cloud.ui.setup_steps", &[]));
            layout::hint(ui, catalog.t("cloud.client_setup_hint", &[]));
        }
        egui::CollapsingHeader::new(catalog.t("cloud.ui.instructions", &[]))
            .id_salt("cloud-inline-instructions")
            .show(ui, |ui| {
                layout::hint(ui, catalog.t("cloud.answer_hint", &[]));
                if layout::button(ui, &catalog.t("cloud.copy_instruction", &[])).clicked() {
                    ui.ctx().copy_text(super::CLOUD_AGENT_INSTRUCTIONS.into());
                }
            });
    }
    fn connection_detail(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        state: &mut SettingsState,
        info: &ConnectionInfo,
    ) {
        ui.horizontal_wrapped(|ui| {
            layout::detail_title(ui, self.redact(&info.principal.name));
            ui.colored_label(color(ui, info), catalog.t(short_key(info), &[]));
            if layout::button(ui, &catalog.t("cloud.ui.rename", &[])).clicked() {
                state.editing = !state.editing;
                state.rename = info.principal.name.clone();
            }
        });
        if state.editing {
            ui.horizontal_wrapped(|ui| {
                let label = ui.label(catalog.t("cloud.connection_name", &[]));
                layout::text_input(
                    ui,
                    180.0,
                    egui::TextEdit::singleline(&mut state.rename)
                        .char_limit(128)
                        .desired_width(180.0),
                )
                .labelled_by(label.id);
                if layout::button(ui, &catalog.t("cloud.connection_rename", &[])).clicked() {
                    let renamed = self.server.as_ref().is_some_and(|server| {
                        server
                            .auth
                            .rename_connection(&info.principal.id, &state.rename)
                    });
                    result_notice(
                        state,
                        renamed
                            .then_some(())
                            .ok_or_else(|| "invalid_connection_name".into()),
                        "cloud.connection_renamed",
                    );
                    if renamed {
                        state.editing = false;
                    }
                }
            });
        }
        ui.add_space(12.0);
        layout::fact(
            ui,
            &catalog.t("cloud.ui.auth_method", &[]),
            &catalog.t(auth_key(info), &[]),
        );
        layout::fact(
            ui,
            &catalog.t("cloud.ui.observed", &[]),
            &catalog.t(activity_key(info), &[]),
        );
        let recent = info
            .last_tool_success_at
            .map(|time| {
                crate::ui::notifications::relative_time_label(
                    catalog,
                    now().saturating_sub(time) as i64,
                )
            })
            .unwrap_or_else(|| catalog.t("cloud.ui.no_tools", &[]));
        layout::fact(ui, &catalog.t("cloud.ui.recent", &[]), &recent);
        layout::fact(
            ui,
            &catalog.t("cloud.ui.expiry", &[]),
            &catalog.t(
                "cloud.connection_expiry",
                &[(
                    "seconds",
                    &info.principal.expires_at.saturating_sub(now()).to_string(),
                )],
            ),
        );
        let (read, input) = self
            .targets
            .iter()
            .map(|target| self.connection_permission(&info.principal.id, target))
            .fold((0, 0), |(r, i), p| {
                (r + usize::from(p.read), i + usize::from(p.input))
            });
        layout::fact(
            ui,
            &catalog.t("cloud.ui.session_access", &[]),
            &catalog.t(
                "cloud.ui.access_counts",
                &[("read", &read.to_string()), ("input", &input.to_string())],
            ),
        );
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(info.state == ConnectionState::Active, |ui| {
                if layout::primary(ui, &catalog.t("cloud.ui.permissions_edit", &[])).clicked() {
                    state.tab = Tab::Permissions;
                }
            });
            if info.state != ConnectionState::Active {
                if layout::button(ui, &catalog.t("cloud.reauth", &[])).clicked() {
                    state.adding = true;
                }
            } else if layout::button(ui, &catalog.t("cloud.ui.work_open", &[])).clicked() {
                state.tab = Tab::Work;
            }
        });
        ui.add_space(12.0);
        layout::hint(
            ui,
            catalog.t(
                if info.last_tool_success_at.is_some() {
                    "cloud.ui.observation_hint"
                } else if info.state == ConnectionState::Active {
                    "cloud.connections_hint"
                } else {
                    "cloud.reauth_hint"
                },
                &[],
            ),
        );
        egui::CollapsingHeader::new(catalog.t("cloud.ui.manage_details", &[]))
            .id_salt(("cloud-details", &info.principal.id))
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(self.redact(&info.principal.id)).monospace(),
                    )
                    .wrap(),
                );
                for (key, observed) in [
                    ("cloud.auth_seen", info.last_authenticated_at),
                    ("cloud.initialize_seen", info.last_initialize_at),
                    ("cloud.tool_seen", info.last_tool_success_at),
                ] {
                    if let Some(observed) = observed {
                        layout::hint(
                            ui,
                            catalog.t(
                                key,
                                &[(
                                    "age",
                                    &crate::ui::notifications::relative_time_label(
                                        catalog,
                                        now().saturating_sub(observed) as i64,
                                    ),
                                )],
                            ),
                        );
                    }
                }
                self.endpoint_info(ui, catalog);
                ui.add_enabled_ui(info.state == ConnectionState::Active, |ui| {
                    if layout::button(ui, &catalog.t("cloud.connection_revoke", &[])).clicked() {
                        self.revoke_connection(&info.principal.id);
                        state.notice = Some(("cloud.connection_revoked", None));
                    }
                });
            });
    }
}
