//! 환경 및 API의 새 화면. 저장 작업은 기존 UI intent와 App worker가 맡는다.

use super::credentials::{CredentialsIntent, CredentialsSnapshot, CredentialsUi};
use super::env_profiles::{EnvAction, EnvProfilesSnapshot, EnvProfilesUi};

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Tab {
    #[default]
    All,
    Api,
    Variables,
    Files,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Api,
    Variable,
}

#[derive(Default)]
pub struct EnvironmentUi {
    tab: Tab,
    search: String,
    drawer: Option<EntryKind>,
    workspace: Option<String>,
}

impl EnvironmentUi {
    /// 화면을 닫거나 이전 화면으로 돌아가면 공개값과 초안도 정리한다.
    pub fn reset(&mut self, env: &mut EnvProfilesUi, credentials: &mut CredentialsUi) {
        self.drawer = None;
        self.search.clear();
        env.reset_modern_draft();
        credentials.reset_modern_draft();
        env.invalidate_cache();
        credentials.clear_revealed_secrets();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn contents(
        &mut self,
        ui: &mut egui::Ui,
        env: &mut EnvProfilesUi,
        snapshot: &EnvProfilesSnapshot,
        credentials: &mut CredentialsUi,
        credential_snapshot: &CredentialsSnapshot,
        application: &crate::environment_application::ApplicationView,
        catalog: &i18n::Catalog,
        live_reload: &mut bool,
    ) -> (Option<EnvAction>, Option<CredentialsIntent>) {
        if self.workspace.as_deref() != Some(snapshot.workspace_id()) {
            self.reset(env, credentials);
            self.workspace = Some(snapshot.workspace_id().to_owned());
        }
        // 숨은 탭도 snapshot 정리를 거쳐 오래된 공개값이나 오류를 남기지 않는다.
        env.prepare_modern(snapshot);
        credentials.prepare_modern(credential_snapshot);
        let mut env_intent = None;
        let mut credential_intent = None;
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);
        ui.label(egui::RichText::new(catalog.t("env.modern.subtitle", &[])).weak());
        ui.horizontal_wrapped(|ui| {
            for (tab, key) in [
                (Tab::All, "env.modern.all"),
                (Tab::Api, "credentials.api_keys"),
                (Tab::Variables, "env.env_vars"),
                (Tab::Files, "env.modern.files"),
            ] {
                ui.selectable_value(&mut self.tab, tab, catalog.t(key, &[]));
            }
            if ui
                .button(format!("+ {}", catalog.t("action.add", &[])))
                .clicked()
            {
                self.drawer = Some(if self.tab == Tab::Variables {
                    EntryKind::Variable
                } else {
                    EntryKind::Api
                });
                credentials.begin_modern_add();
            }
        });
        ui.separator();
        let available = ui.available_width();
        if self.drawer.is_some() && available >= 700.0 {
            let editor_width = 320.0;
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2((available - editor_width - 20.0).max(0.0), 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        self.list(
                            ui,
                            env,
                            snapshot,
                            credentials,
                            credential_snapshot,
                            catalog,
                            &mut env_intent,
                            &mut credential_intent,
                        );
                    },
                );
                egui::Frame::group(ui.style())
                    .inner_margin(14)
                    .show(ui, |ui| {
                        ui.set_width(editor_width - 28.0);
                        self.editor(
                            ui,
                            env,
                            snapshot,
                            credentials,
                            credential_snapshot,
                            catalog,
                            &mut env_intent,
                            &mut credential_intent,
                        );
                    });
            });
        } else if self.drawer.is_some() {
            // 좁은 창에서는 목록 위에 입력만 표시하며 가로로 밀어내지 않는다.
            self.editor(
                ui,
                env,
                snapshot,
                credentials,
                credential_snapshot,
                catalog,
                &mut env_intent,
                &mut credential_intent,
            );
        } else {
            self.list(
                ui,
                env,
                snapshot,
                credentials,
                credential_snapshot,
                catalog,
                &mut env_intent,
                &mut credential_intent,
            );
        }
        env.finish_modern(ui, snapshot, catalog, &mut env_intent);
        credentials.finish_modern(ui, credential_snapshot, catalog, &mut credential_intent);
        ui.add_space(6.0);
        let (status, color) = if application.failed {
            ("env.apply_failed", ui.visuals().error_fg_color)
        } else if application.pending {
            ("env.apply_pending", ui.visuals().weak_text_color())
        } else {
            ("env.modern.next_session", ui.visuals().weak_text_color())
        };
        ui.label(
            egui::RichText::new(catalog.t(status, &[]))
                .small()
                .color(color),
        );
        ui.collapsing(catalog.t("env.modern.advanced", &[]), |ui| {
            super::env_profiles::render_application_status(ui, application, catalog);
            ui.checkbox(live_reload, catalog.t("env.live_reload", &[]))
                .on_hover_text(catalog.t("env.live_reload_hint", &[]));
            credentials.modern_maintenance(
                ui,
                credential_snapshot,
                catalog,
                &mut credential_intent,
            );
        });
        (env_intent, credential_intent)
    }

    #[allow(clippy::too_many_arguments)]
    fn list(
        &mut self,
        ui: &mut egui::Ui,
        env: &mut EnvProfilesUi,
        snapshot: &EnvProfilesSnapshot,
        credentials: &mut CredentialsUi,
        credential_snapshot: &CredentialsSnapshot,
        catalog: &i18n::Catalog,
        env_intent: &mut Option<EnvAction>,
        credential_intent: &mut Option<CredentialsIntent>,
    ) {
        ui.set_width(ui.available_width());
        if self.tab == Tab::Files {
            env.modern_files(ui, snapshot, catalog, env_intent);
            return;
        }
        ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .hint_text(catalog.t("env.modern.search", &[]))
                .char_limit(256)
                .desired_width(f32::INFINITY),
        );
        let query = self.search.to_lowercase();
        if matches!(self.tab, Tab::All | Tab::Api) {
            credentials.modern_list(ui, credential_snapshot, catalog, &query, credential_intent);
        }
        if matches!(self.tab, Tab::All | Tab::Variables) {
            env.modern_list(ui, snapshot, catalog, &query, env_intent);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn editor(
        &mut self,
        ui: &mut egui::Ui,
        env: &mut EnvProfilesUi,
        snapshot: &EnvProfilesSnapshot,
        credentials: &mut CredentialsUi,
        credential_snapshot: &CredentialsSnapshot,
        catalog: &i18n::Catalog,
        env_intent: &mut Option<EnvAction>,
        credential_intent: &mut Option<CredentialsIntent>,
    ) {
        let Some(mut kind) = self.drawer else { return };
        ui.horizontal_wrapped(|ui| {
            ui.strong(catalog.t("env.modern.add_title", &[]));
            if ui.small_button(catalog.t("action.cancel", &[])).clicked() {
                self.drawer = None;
            }
        });
        if self.drawer.is_none() {
            env.reset_modern_draft();
            credentials.reset_modern_draft();
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(
                &mut kind,
                EntryKind::Api,
                catalog.t("credentials.api_keys", &[]),
            );
            ui.selectable_value(
                &mut kind,
                EntryKind::Variable,
                catalog.t("env.env_vars", &[]),
            );
        });
        self.drawer = Some(kind);
        ui.separator();
        match kind {
            EntryKind::Api => {
                credentials.modern_form(ui, credential_snapshot, catalog, credential_intent)
            }
            EntryKind::Variable => env.modern_form(ui, snapshot, catalog, env_intent),
        }
        if matches!(credential_intent, Some(CredentialsIntent::Add { .. }))
            || matches!(env_intent, Some(EnvAction::DotenvWrite { .. }))
        {
            // 완료 알림은 worker ACK가 만든 상태만 표시한다.
            self.drawer = None;
        }
    }
}

pub(super) fn field_label(ui: &mut egui::Ui, catalog: &i18n::Catalog, key: &str) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(catalog.t(key, &[])).strong());
}
