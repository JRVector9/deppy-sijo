//! Inline cloud management controls. Credential identities come from the server.
use super::CloudAgent;
use agent_mcp::{AuthKind, ConnectionInfo, ConnectionState, now};

#[derive(Clone, Default)]
struct SettingsState {
    selected: Option<String>,
    rename: String,
    manual_name: String,
    manual_input_scope: bool,
    profile: Option<usize>,
    notice: Option<(&'static str, Option<String>)>,
}

fn state_id() -> egui::Id {
    egui::Id::new("cloud-management-settings")
}
fn state(ui: &egui::Ui) -> SettingsState {
    ui.ctx()
        .data_mut(|data| data.get_temp(state_id()).unwrap_or_default())
}
fn store(ui: &egui::Ui, state: SettingsState) {
    ui.ctx()
        .data_mut(|data| data.insert_temp(state_id(), state));
}
fn result_notice(state: &mut SettingsState, result: Result<(), String>, success: &'static str) {
    state.notice = Some(match result {
        Ok(()) => (success, None),
        Err(code) => ("cloud.management_failed", Some(code)),
    });
}
fn notice(ui: &mut egui::Ui, catalog: &i18n::Catalog, state: &SettingsState) {
    if let Some((key, error)) = &state.notice {
        let response = ui.label(catalog.t(key, &[]));
        if let Some(code) = error {
            response.on_hover_text(code);
        }
    }
}

impl CloudAgent {
    pub(super) fn profile_settings(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        let mut state = state(ui);
        if state
            .profile
            .is_none_or(|index| index >= self.profiles.len())
        {
            state.profile = (!self.profiles.is_empty()).then_some(0);
        }
        egui::CollapsingHeader::new(catalog.t("cloud.profiles", &[]))
            .id_salt("cloud-profiles")
            .show(ui, |ui| {
                ui.weak(catalog.t("cloud.profile_hint", &[]));
                ui.horizontal_wrapped(|ui| {
                    let label = ui.label(catalog.t("cloud.profile_name", &[]));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.profile_name)
                            .char_limit(80)
                            .desired_width(180.0),
                    )
                    .labelled_by(label.id);
                    egui::ComboBox::from_id_salt("cloud-profile-selection")
                        .selected_text(
                            state
                                .profile
                                .and_then(|index| self.profiles.get(index))
                                .map(|profile| profile.name.clone())
                                .unwrap_or_else(|| catalog.t("cloud.profile_select", &[])),
                        )
                        .show_ui(ui, |ui| {
                            for (index, profile) in self.profiles.iter().enumerate() {
                                ui.selectable_value(&mut state.profile, Some(index), &profile.name);
                            }
                        });
                    if ui.button(catalog.t("cloud.profile_save", &[])).clicked() {
                        result_notice(&mut state, self.save_profile(), "cloud.profile_saved");
                        if state
                            .notice
                            .as_ref()
                            .is_some_and(|(_, code)| code.is_none())
                        {
                            state.profile = self
                                .profiles
                                .iter()
                                .position(|profile| profile.name == self.profile_name.trim());
                        }
                    }
                    if ui
                        .add_enabled(
                            !self.busy() && state.profile.is_some(),
                            egui::Button::new(catalog.t("cloud.profile_load", &[])),
                        )
                        .on_hover_text(catalog.t("cloud.profile_stop_first", &[]))
                        .clicked()
                        && let Some(index) = state.profile
                    {
                        result_notice(&mut state, self.load_profile(index), "cloud.profile_loaded");
                    }
                    if ui
                        .add_enabled(
                            state.profile.is_some(),
                            egui::Button::new(catalog.t("cloud.profile_delete", &[])),
                        )
                        .clicked()
                        && let Some(index) = state.profile
                    {
                        self.remove_profile(index);
                        state.profile = (!self.profiles.is_empty()).then_some(0);
                        state.notice = Some(("cloud.profile_deleted", None));
                    }
                });
                let mut reconnect = self.reconnect;
                if ui
                    .checkbox(&mut reconnect, catalog.t("cloud.reconnect", &[]))
                    .changed()
                {
                    self.set_reconnect(reconnect);
                }
                ui.weak(catalog.t("cloud.reconnect_hint", &[]));
                notice(ui, catalog, &state);
            });
        store(ui, state);
    }

    pub(super) fn connection_settings(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        let connections = self.connection_infos();
        ui.separator();
        ui.heading(catalog.t("cloud.connections", &[]));
        ui.weak(catalog.t("cloud.connections_hint", &[]));
        if connections.is_empty() {
            ui.weak(catalog.t("cloud.connections_empty", &[]));
            return;
        }
        let mut state = state(ui);
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(1));
        ui.horizontal_wrapped(|ui| {
            let label = ui.label(catalog.t("cloud.manual_name", &[]));
            ui.add(
                egui::TextEdit::singleline(&mut state.manual_name)
                    .char_limit(128)
                    .desired_width(180.0),
            )
            .labelled_by(label.id);
            ui.checkbox(
                &mut state.manual_input_scope,
                catalog.t("cloud.manual_input_scope", &[]),
            );
            if ui
                .add_enabled(
                    self.server
                        .as_ref()
                        .is_some_and(|server| server.auth.expires() > now()),
                    egui::Button::new(catalog.t("cloud.manual_issue", &[])),
                )
                .clicked()
            {
                match self.issue_manual_connection(&state.manual_name, state.manual_input_scope) {
                    Ok(token) => {
                        ui.ctx().copy_text(token);
                        state.manual_input_scope = false;
                        state.notice = Some(("cloud.manual_issued", None));
                    }
                    Err(code) => state.notice = Some(("cloud.management_failed", Some(code))),
                }
            }
        });
        ui.weak(catalog.t("cloud.manual_hint", &[]));
        if !connections
            .iter()
            .any(|info| Some(&info.principal.id) == state.selected.as_ref())
        {
            state.selected = connections.first().map(|info| info.principal.id.clone());
            state.rename = connections
                .first()
                .map_or_else(String::new, |info| info.principal.name.clone());
        }
        egui::ScrollArea::vertical()
            .id_salt("cloud-connection-list")
            .max_height(150.0)
            .show(ui, |ui| {
                for info in &connections {
                    ui.push_id(&info.principal.id, |ui| {
                        let label = format!(
                            "{} · {}",
                            info.principal.name,
                            catalog.t(activity_key(info), &[])
                        );
                        if ui
                            .selectable_label(
                                state.selected.as_deref() == Some(&info.principal.id),
                                label,
                            )
                            .on_hover_text(&info.principal.id)
                            .clicked()
                        {
                            state.selected = Some(info.principal.id.clone());
                            state.rename = info.principal.name.clone();
                            state.notice = None;
                        }
                    });
                }
            });
        if let Some(info) = connections
            .iter()
            .find(|info| Some(&info.principal.id) == state.selected.as_ref())
        {
            let active = info.state == ConnectionState::Active;
            ui.horizontal_wrapped(|ui| {
                let label = ui.label(catalog.t("cloud.connection_name", &[]));
                ui.add(
                    egui::TextEdit::singleline(&mut state.rename)
                        .char_limit(128)
                        .desired_width(180.0),
                )
                .labelled_by(label.id);
                if ui
                    .button(catalog.t("cloud.connection_rename", &[]))
                    .clicked()
                {
                    let renamed = self.server.as_ref().is_some_and(|server| {
                        server
                            .auth
                            .rename_connection(&info.principal.id, &state.rename)
                    });
                    result_notice(
                        &mut state,
                        renamed
                            .then_some(())
                            .ok_or_else(|| "invalid_connection_name".into()),
                        "cloud.connection_renamed",
                    );
                }
                if ui
                    .add_enabled(
                        active,
                        egui::Button::new(catalog.t("cloud.connection_revoke", &[])),
                    )
                    .clicked()
                {
                    self.revoke_connection(&info.principal.id);
                    state.notice = Some(("cloud.connection_revoked", None));
                }
            });
            ui.weak(format!(
                "{} · {} · {}",
                catalog.t(
                    match info.principal.auth_kind {
                        AuthKind::ManualBearer => "cloud.auth_manual",
                        AuthKind::OAuth => "cloud.auth_oauth",
                    },
                    &[]
                ),
                catalog.t(
                    if info.principal.input_scope {
                        "cloud.scope_input"
                    } else {
                        "cloud.scope_read"
                    },
                    &[]
                ),
                catalog.t(
                    "cloud.connection_expiry",
                    &[(
                        "seconds",
                        &info.principal.expires_at.saturating_sub(now()).to_string()
                    )]
                )
            ));
            ui.add(egui::Label::new(format!("ID: {}", info.principal.id)).truncate())
                .on_hover_text(&info.principal.id);
            for (key, observed) in [
                ("cloud.auth_seen", info.last_authenticated_at),
                ("cloud.initialize_seen", info.last_initialize_at),
                ("cloud.tool_seen", info.last_tool_success_at),
            ] {
                if let Some(observed) = observed {
                    let age = crate::ui::notifications::relative_time_label(
                        catalog,
                        i64::try_from(now().saturating_sub(observed)).unwrap_or(i64::MAX),
                    );
                    ui.weak(catalog.t(key, &[("age", &age)]));
                }
            }
            if !active {
                ui.label(catalog.t("cloud.reauth_hint", &[]));
                if ui.button(catalog.t("cloud.reauth", &[])).clicked() {
                    state.notice = Some(("cloud.reauth_hint", None));
                }
            }
            ui.weak(catalog.t("cloud.connection_permission_hint", &[]));
            egui::ScrollArea::vertical()
                .id_salt("cloud-connection-targets")
                .max_height(220.0)
                .show(ui, |ui| {
                    for target in self.targets.clone() {
                        ui.push_id((&info.principal.id, &target.id, &target.generation), |ui| {
                            ui.label(format!("{} · {}", target.workspace_name, target.title));
                            if let Some(local) = &target.agent_line {
                                ui.weak(catalog.t("cloud.local_status", &[("status", local)]));
                            }
                            let permission =
                                self.connection_permission(&info.principal.id, &target);
                            let mut read = active && permission.read;
                            let mut input = active && permission.input;
                            ui.horizontal_wrapped(|ui| {
                                if ui
                                    .add_enabled(
                                        active && target.live,
                                        egui::Checkbox::new(
                                            &mut read,
                                            catalog.t("cloud.connection_read", &[]),
                                        ),
                                    )
                                    .changed()
                                {
                                    input &= read;
                                    result_notice(
                                        &mut state,
                                        self.set_connection_access(
                                            &info.principal.id,
                                            &target,
                                            read,
                                            input,
                                        ),
                                        "cloud.permission_saved",
                                    );
                                }
                                if ui
                                    .add_enabled(
                                        active && target.live && read && info.principal.input_scope,
                                        egui::Checkbox::new(
                                            &mut input,
                                            catalog.t("cloud.connection_input", &[]),
                                        ),
                                    )
                                    .changed()
                                {
                                    result_notice(
                                        &mut state,
                                        self.set_connection_access(
                                            &info.principal.id,
                                            &target,
                                            read,
                                            input,
                                        ),
                                        "cloud.permission_saved",
                                    );
                                }
                            });
                            let lease = self.connection_lease(&target);
                            let owner = lease
                                .as_ref()
                                .map(|lease| {
                                    connections
                                        .iter()
                                        .find(|info| info.principal.id == lease.connection_id)
                                        .map_or_else(
                                            || lease.connection_id.clone(),
                                            |info| info.principal.name.clone(),
                                        )
                                })
                                .unwrap_or_else(|| catalog.t("cloud.lease_you", &[]));
                            ui.label(catalog.t("cloud.lease_owner", &[("client", &owner)]));
                            if let Some(lease) = &lease {
                                ui.weak(catalog.t(
                                    "cloud.lease_expiry",
                                    &[(
                                        "seconds",
                                        &lease.expires_at.saturating_sub(now()).to_string(),
                                    )],
                                ));
                                if let Some(run) = &lease.run_id {
                                    ui.weak(catalog.t("cloud.lease_run", &[("run", run)]));
                                }
                                ui.ctx()
                                    .request_repaint_after(std::time::Duration::from_secs(1));
                            }
                            if ui
                                .add_enabled(
                                    input || lease.is_some(),
                                    egui::Button::new(catalog.t("cloud.session_take_control", &[])),
                                )
                                .clicked()
                            {
                                self.take_session_control(&target);
                                state.notice = Some(("cloud.control_taken", None));
                            }
                            if !target.live {
                                ui.weak(catalog.t("cloud.exited", &[]));
                            }
                        });
                    }
                });
            notice(ui, catalog, &state);
        }
        store(ui, state);
    }
}

fn activity_key(info: &ConnectionInfo) -> &'static str {
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
