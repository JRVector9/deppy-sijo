//! Connection list, inline setup and explicit authentication consent.
use super::{
    CloudAgent, layout,
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
pub(super) fn short_key(info: &ConnectionInfo) -> &'static str {
    match activity_key(info) {
        "cloud.activity_tool" => "cloud.ui.tool_ok",
        "cloud.activity_expired" => "cloud.ui.expired",
        "cloud.activity_revoked" => "cloud.ui.revoked",
        "cloud.activity_authenticated" => "cloud.ui.auth_ok",
        "cloud.activity_initialized" => "cloud.ui.initialized",
        _ => "cloud.ui.auth_pending",
    }
}
pub(super) fn color(ui: &egui::Ui, info: &ConnectionInfo) -> egui::Color32 {
    if info.state != ConnectionState::Active {
        layout::warn(ui)
    } else if info.last_tool_success_at.is_some() {
        layout::good(ui)
    } else {
        layout::warn(ui)
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
            endpoint.starts_with("https://")
                && (!self.automatic
                    || (self.connection == super::super::Connection::Ready
                        && self.generated_hostname.is_some()))
        })
    }
    pub(super) fn endpoint_info(&self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        if let Some(endpoint) = self.connector_endpoint() {
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
    pub(super) fn approval_settings(
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
                    for (allow, key) in [(false, "cloud.oauth_deny"), (true, "cloud.oauth_approve")]
                    {
                        if layout::button(ui, &catalog.t(key, &[])).clicked() {
                            server.auth.approve(&approval.id, allow);
                            state.notice = Some(("cloud.ui.approval_done", None));
                        }
                    }
                });
            });
        }
    }
    pub(super) fn connection_detail(
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
