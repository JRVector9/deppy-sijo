use super::*;

pub(super) struct QuickProjectApiOrigin {
    pub workspace_id: String,
    pub runtime_instance: u64,
    pub primary_workspace_id: String,
    unavailable_remote: bool,
}

impl App {
    /// Capture the emitting workspace before asynchronous metadata loading. A
    /// session number alone cannot identify an attached or warm runtime.
    pub(super) fn open_project_api_registration(
        &mut self,
        workspace_id: &str,
        prefill: Option<ui::environment::EnvironmentPrefill>,
    ) -> bool {
        let runtime = if self.active.id == workspace_id {
            Some(&self.active)
        } else {
            self.warm.get(workspace_id)
        };
        let Some(runtime) = runtime else { return false };
        let origin = QuickProjectApiOrigin {
            workspace_id: workspace_id.to_owned(),
            runtime_instance: runtime.runtime_instance,
            primary_workspace_id: self.active.id.clone(),
            unavailable_remote: runtime.runtime.is_remote(),
        };
        // Remote helpers cannot resolve the Mac's vault. Keep the accepted
        // gesture visible without selecting a local metadata or secret scope.
        if origin.unavailable_remote {
            self.project_keys_ui.reset();
            self.project_keys_scope = None;
            self.project_keys_snapshot = None;
            self.quick_project_api_origin = Some(origin);
            self.publish_popup_input_fence(&self.egui_ctx);
            self.egui_ctx.request_repaint();
            return true;
        }
        let environment = self
            .project_keys_scope
            .as_ref()
            .filter(|scope| scope.workspace_id == workspace_id)
            .map_or(
                deppy_core::project_keys::KeyEnvironment::Development,
                |scope| scope.environment,
            );
        let scope = crate::project_keys::contracts::ProjectKeysScope {
            workspace_id: workspace_id.to_owned(),
            environment,
        };
        let prefill = prefill.and_then(|prefill| {
            let (kind, value) = prefill.into_parts();
            crate::project_keys::contracts::SensitiveDraft::new(value.into_inner())
                .ok()
                .map(|value| (kind, value))
        });
        self.project_keys_ui.reset();
        self.project_keys_ui.begin_api_add(scope.clone(), prefill);
        self.project_keys_scope = Some(scope);
        self.project_keys_snapshot = None;
        self.quick_project_api_origin = Some(origin);
        self.publish_popup_input_fence(&self.egui_ctx);
        self.egui_ctx.request_repaint();
        true
    }

    fn project_api_registration_origin_is_current(&self) -> bool {
        self.quick_project_api_origin
            .as_ref()
            .is_some_and(|origin| {
                origin.primary_workspace_id == self.active.id
                    && (if origin.workspace_id == self.active.id {
                        Some(&self.active)
                    } else {
                        self.warm.get(&origin.workspace_id)
                    })
                    .is_some_and(|runtime| {
                        runtime.runtime.is_remote() == origin.unavailable_remote
                            && runtime.runtime_instance == origin.runtime_instance
                    })
            })
    }

    pub(super) fn cancel_project_api_registration(&mut self) {
        if self.quick_project_api_origin.take().is_some() {
            self.project_keys_ui.reset();
            self.project_keys_scope = None;
            self.project_keys_snapshot = None;
        }
    }

    pub(super) fn validate_project_api_registration(&mut self) {
        if self.quick_project_api_origin.is_some()
            && !self.project_api_registration_origin_is_current()
        {
            self.cancel_project_api_registration();
        }
    }

    /// One render owner for both Settings and direct registration. The loading
    /// state is already modal, before the worker has returned a scoped snapshot.
    pub(super) fn show_project_key_dialog(
        &mut self,
        ctx: &egui::Context,
        catalog: &i18n::Catalog,
    ) -> Vec<crate::project_keys::contracts::ProjectKeysIntent> {
        self.validate_project_api_registration();
        let unavailable_remote = self
            .quick_project_api_origin
            .as_ref()
            .is_some_and(|origin| origin.unavailable_remote);
        let snapshot = self
            .project_keys_snapshot
            .clone()
            .filter(|snapshot| self.project_keys_scope.as_ref() == Some(&snapshot.scope));
        let load_error = snapshot.as_ref().and_then(|snapshot| snapshot.error);
        // A failed initial read deliberately leaves the one-shot draft pending.
        // Keep its loading/error shell visible until it can open or is canceled.
        if let Some(snapshot) = snapshot
            .filter(|snapshot| snapshot.error.is_none() || self.project_keys_ui.has_dialog())
        {
            let intents = self.project_keys_ui.show_dialog(ctx, &snapshot, catalog);
            if self.quick_project_api_origin.is_some()
                && !self.project_keys_ui.has_dialog_or_pending_open()
            {
                self.cancel_project_api_registration();
            }
            return intents;
        }
        if self.quick_project_api_origin.is_none() {
            return Vec::new();
        }
        let mut cancel = false;
        let title = catalog.t("project_keys.api_add_title", &[]);
        let subtitle = if unavailable_remote {
            String::new()
        } else {
            catalog.t("project_keys.quick_loading", &[])
        };
        let close = ui::popup::show(
            ctx,
            ui::popup::PopupSpec {
                id: egui::Id::new("project_api_registration_loading"),
                title: &title,
                subtitle: &subtitle,
                close_label: &catalog.t("action.close", &[]),
                width: 560.0,
                close_enabled: true,
            },
            |ui| {
                ui::popup::body(ui, |ui| {
                    if unavailable_remote {
                        ui::popup::notice(
                            ui,
                            &catalog.t("project_keys.quick_ssh_unavailable", &[]),
                            ui::popup::NoticeTone::Info,
                        );
                    } else if let Some(error) = load_error {
                        ui::popup::notice(
                            ui,
                            &ui::project_keys::error_label(error, catalog),
                            ui::popup::NoticeTone::Error,
                        );
                    } else {
                        ui.add(egui::Spinner::new());
                    }
                });
                ui::popup::footer(ui, None, |ui| {
                    cancel = ui::popup::action_button(
                        ui,
                        &catalog.t("action.cancel", &[]),
                        ui::popup::ActionTone::Ghost,
                        true,
                    )
                    .clicked();
                });
            },
        );
        if close
            || cancel
            || ui::popup::take_modal_escape(ctx, egui::Id::new("project_api_registration_loading"))
        {
            self.cancel_project_api_registration();
        }
        Vec::new()
    }
}
