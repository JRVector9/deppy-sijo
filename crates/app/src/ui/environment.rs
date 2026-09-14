//! 환경 및 API의 새 화면. 저장 작업은 기존 UI intent와 App worker가 맡는다.

use super::credentials::{CREDENTIAL_SENSITIVE_ITEM_MAX_BYTES, SensitiveInput};
use super::credentials::{CredentialsIntent, CredentialsSnapshot, CredentialsUi};
use super::env_profiles::{EnvAction, EnvProfilesSnapshot, EnvProfilesUi};

/// 선택 원문을 저장하기 전에 어느 필드에 넣을지 명시한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvironmentSelectionKind {
    ApiName,
    ApiValue,
    VariableName,
    VariableValue,
}

impl EnvironmentSelectionKind {
    pub fn accepts(self, text: &str) -> bool {
        if text.is_empty() || text.contains(['\0', '\r', '\n']) {
            return false;
        }
        match self {
            Self::ApiName => !text.trim().is_empty() && text.len() <= 4096,
            Self::VariableName => text.len() <= 256 && deppy_core::credential_env::valid_name(text),
            Self::ApiValue | Self::VariableValue => {
                text.len() <= CREDENTIAL_SENSITIVE_ITEM_MAX_BYTES
            }
        }
    }
}

/// 설정 저장과 별개인 한 번만 소비하는 초안. 값은 Debug/Clone/직렬화에 노출하지 않는다.
pub struct EnvironmentPrefill {
    kind: EnvironmentSelectionKind,
    value: SensitiveInput,
}

impl EnvironmentPrefill {
    pub fn from_selection(kind: EnvironmentSelectionKind, text: &str) -> Option<Self> {
        kind.accepts(text).then(|| Self {
            kind,
            value: SensitiveInput::try_new(text.to_owned()).expect("선택 입력 한도 확인됨"),
        })
    }
}

/// 워크스페이스는 이 요청을 소비하는 App이 원본 runtime에서 붙인다.
pub struct EnvironmentOpenRequest {
    pub session: Option<runtime::SessionId>,
    pub cwd: Option<String>,
    pub prefill: Option<EnvironmentPrefill>,
}

impl EnvironmentOpenRequest {
    /// primary 전환 때 비워지는 App 캐시와 일치할 때만 폴더 등록 안내에 쓴다.
    pub fn current_cwd(
        &self,
        current: &std::collections::HashMap<runtime::SessionId, String>,
    ) -> Option<&str> {
        let captured = self.cwd.as_deref()?;
        (current.get(&self.session?)?.as_str() == captured).then_some(captured)
    }
}

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
    pending_prefill: Option<EnvironmentPrefill>,
}

impl EnvironmentUi {
    /// 화면을 닫거나 이전 화면으로 돌아가면 공개값과 초안도 정리한다.
    pub fn reset(&mut self, env: &mut EnvProfilesUi, credentials: &mut CredentialsUi) {
        self.drawer = None;
        self.pending_prefill = None;
        self.search.clear();
        env.reset_modern_draft();
        credentials.reset_modern_draft();
        env.invalidate_cache();
        credentials.invalidate_cache();
    }

    fn cancel_editor(&mut self, env: &mut EnvProfilesUi, credentials: &mut CredentialsUi) {
        self.pending_prefill = None;
        self.drawer = None;
        env.reset_modern_draft();
        credentials.reset_modern_draft();
    }

    fn start_editor(&mut self, env: &mut EnvProfilesUi, credentials: &mut CredentialsUi) {
        self.cancel_editor(env, credentials);
        self.drawer = Some(if self.tab == Tab::Variables {
            EntryKind::Variable
        } else {
            EntryKind::Api
        });
        if self.drawer == Some(EntryKind::Api) {
            credentials.begin_modern_add();
        }
    }

    pub fn queue_prefill(&mut self, workspace_id: String, prefill: EnvironmentPrefill) {
        self.workspace = Some(workspace_id);
        self.pending_prefill = Some(prefill);
    }

    fn prepare_view(
        &mut self,
        env: &mut EnvProfilesUi,
        snapshot: &EnvProfilesSnapshot,
        credentials: &mut CredentialsUi,
        credential_snapshot: &CredentialsSnapshot,
    ) -> Option<egui::Id> {
        if self.workspace.as_deref() != Some(snapshot.workspace_id()) {
            self.reset(env, credentials);
            self.workspace = Some(snapshot.workspace_id().to_owned());
        }
        // 숨은 탭도 snapshot 정리를 거쳐 오래된 공개값이나 오류를 남기지 않는다.
        env.prepare_modern(snapshot);
        credentials.prepare_modern(credential_snapshot);
        self.apply_pending_prefill(env, snapshot, credentials, credential_snapshot)
    }

    /// 기존 보기에서도 같은 로딩/초안 수명 경계를 사용한다.
    pub(crate) fn prepare_classic(
        &mut self,
        env: &mut EnvProfilesUi,
        snapshot: &EnvProfilesSnapshot,
        credentials: &mut CredentialsUi,
        credential_snapshot: &CredentialsSnapshot,
    ) -> Option<egui::Id> {
        let focus = self.prepare_view(env, snapshot, credentials, credential_snapshot)?;
        match self.drawer.take() {
            Some(EntryKind::Api) => credentials.open_prefilled_form(),
            Some(EntryKind::Variable) => env.open_prefilled_form(),
            None => {}
        }
        Some(focus)
    }

    /// 두 snapshot이 준비된 다음에만 채워 첫 로딩의 초안 초기화에 지워지지 않는다.
    fn apply_pending_prefill(
        &mut self,
        env: &mut EnvProfilesUi,
        snapshot: &EnvProfilesSnapshot,
        credentials: &mut CredentialsUi,
        credential_snapshot: &CredentialsSnapshot,
    ) -> Option<egui::Id> {
        if !snapshot.is_available() || !credential_snapshot.is_available() {
            return None;
        }
        let prefill = self.pending_prefill.take()?;
        self.search.clear();
        match prefill.kind {
            EnvironmentSelectionKind::ApiName | EnvironmentSelectionKind::ApiValue => {
                self.tab = Tab::Api;
                self.drawer = Some(EntryKind::Api);
                Some(credentials.prefill_modern(prefill.kind, prefill.value))
            }
            EnvironmentSelectionKind::VariableName | EnvironmentSelectionKind::VariableValue => {
                self.tab = Tab::Variables;
                self.drawer = Some(EntryKind::Variable);
                Some(env.prefill_modern(prefill.kind, prefill.value))
            }
        }
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
        let prefill_focus = self.prepare_view(env, snapshot, credentials, credential_snapshot);
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
                self.start_editor(env, credentials);
            }
        });
        ui.separator();
        let available = ui.available_width();
        if self.drawer.is_some() && available >= 700.0 {
            let editor_width = 320.0;
            // 목록의 스크롤 영역이 최소 높이로 축소되지 않도록 높이도 전달한다.
            let list_height = ui.available_height();
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2((available - editor_width - 20.0).max(0.0), list_height),
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
        if let Some(id) = prefill_focus {
            ui.memory_mut(|memory| memory.request_focus(id));
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
            self.cancel_editor(env, credentials);
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
                // 변수 추가에서 API로 전환한 경우도 서비스 기본 선택을 준비한다.
                credentials.begin_modern_add();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_classic_prefill_waits_for_snapshots_and_is_consumed_once() {
        for kind in [
            EnvironmentSelectionKind::ApiName,
            EnvironmentSelectionKind::ApiValue,
            EnvironmentSelectionKind::VariableName,
            EnvironmentSelectionKind::VariableValue,
        ] {
            let mut view = EnvironmentUi::default();
            let mut env = EnvProfilesUi::new();
            let mut credentials = CredentialsUi::new();
            view.queue_prefill(
                "origin".into(),
                EnvironmentPrefill::from_selection(kind, "FAKE_VALUE").unwrap(),
            );
            assert!(
                view.prepare_classic(
                    &mut env,
                    &EnvProfilesSnapshot::loading(1, "origin", true),
                    &mut credentials,
                    &CredentialsSnapshot::loading(1)
                )
                .is_none()
            );
            assert!(view.pending_prefill.is_some());
            let loaded = CredentialsSnapshot::try_new(2, vec![]).unwrap();
            assert!(
                view.prepare_classic(&mut env, &ready("origin"), &mut credentials, &loaded)
                    .is_some()
            );
            assert!(view.pending_prefill.is_none());
            assert!(view.drawer.is_none());
            assert!(
                view.prepare_classic(&mut env, &ready("origin"), &mut credentials, &loaded)
                    .is_none()
            );
        }
    }

    #[test]
    fn environment_context_reactivated_cwd_requires_current_detection() {
        let session = runtime::SessionId(7);
        let request = EnvironmentOpenRequest {
            session: Some(session),
            cwd: Some("/old-project".into()),
            prefill: None,
        };
        let mut detected = std::collections::HashMap::new();
        assert_eq!(request.current_cwd(&detected), None);
        detected.insert(session, "/new-project".into());
        assert_eq!(request.current_cwd(&detected), None);
        detected.insert(session, "/old-project".into());
        assert_eq!(request.current_cwd(&detected), Some("/old-project"));
        detected.clear();
        assert_eq!(request.current_cwd(&detected), None);
    }

    #[test]
    fn environment_context_selection_limits_preserve_values_without_truncating() {
        use EnvironmentSelectionKind as K;
        assert!(EnvironmentPrefill::from_selection(K::VariableName, "MY_API_KEY").is_some());
        assert!(EnvironmentPrefill::from_selection(K::VariableName, "내 API").is_none());
        assert!(EnvironmentPrefill::from_selection(K::ApiName, "내 API").is_some());
        for invalid in ["", "abc\nvalue", "abc\0value", "abc\rvalue"] {
            assert!(EnvironmentPrefill::from_selection(K::ApiValue, invalid).is_none());
        }
        let value = "가".repeat(CREDENTIAL_SENSITIVE_ITEM_MAX_BYTES / 3);
        let prefill = EnvironmentPrefill::from_selection(K::ApiValue, &value).unwrap();
        assert_eq!(prefill.value.into_inner(), value);
        assert!(EnvironmentPrefill::from_selection(K::ApiValue, &(value + "가")).is_none());
        let prefill = EnvironmentPrefill::from_selection(K::VariableValue, " value ").unwrap();
        assert_eq!(prefill.value.into_inner(), " value ");
    }

    fn ready(workspace: &str) -> EnvProfilesSnapshot {
        EnvProfilesSnapshot::try_new(2, workspace, true, Some("profile"), 0, vec![], vec![])
            .unwrap()
    }

    #[test]
    fn environment_context_cancel_or_replace_discards_delayed_prefill() {
        let mut view = EnvironmentUi::default();
        let mut env = EnvProfilesUi::new();
        let mut credentials = CredentialsUi::new();
        let prefill = || {
            EnvironmentPrefill::from_selection(
                EnvironmentSelectionKind::ApiValue,
                "cancelled-fake-value",
            )
            .unwrap()
        };
        let loaded = CredentialsSnapshot::try_new(2, vec![]).unwrap();
        view.queue_prefill("origin".into(), prefill());
        view.start_editor(&mut env, &mut credentials);
        assert!(view.pending_prefill.is_none());
        assert!(
            view.prepare_view(&mut env, &ready("origin"), &mut credentials, &loaded)
                .is_none()
        );
        view.queue_prefill("origin".into(), prefill());
        view.cancel_editor(&mut env, &mut credentials);
        assert!(view.pending_prefill.is_none());
        assert!(view.drawer.is_none());
        assert!(
            view.prepare_view(&mut env, &ready("origin"), &mut credentials, &loaded)
                .is_none()
        );
    }

    #[test]
    fn environment_context_pending_prefill_survives_loading_but_not_navigation() {
        let mut view = EnvironmentUi::default();
        let mut env = EnvProfilesUi::new();
        let mut credentials = CredentialsUi::new();
        let loading = EnvProfilesSnapshot::loading(1, "origin", true);
        let credential_loading = CredentialsSnapshot::loading(1);
        let credential_ready = CredentialsSnapshot::try_new(2, vec![]).unwrap();
        let prefill = || {
            EnvironmentPrefill::from_selection(EnvironmentSelectionKind::ApiValue, "fake-value")
                .unwrap()
        };
        view.queue_prefill("origin".into(), prefill());
        assert!(
            view.prepare_view(&mut env, &loading, &mut credentials, &credential_loading)
                .is_none()
        );
        assert!(view.pending_prefill.is_some());
        assert!(
            view.prepare_view(
                &mut env,
                &ready("origin"),
                &mut credentials,
                &credential_ready
            )
            .is_some()
        );
        assert!(view.pending_prefill.is_none());
        assert!(
            view.prepare_view(
                &mut env,
                &ready("origin"),
                &mut credentials,
                &credential_ready
            )
            .is_none()
        );
        view.queue_prefill("origin".into(), prefill());
        assert!(
            view.prepare_view(
                &mut env,
                &ready("other"),
                &mut credentials,
                &credential_ready
            )
            .is_none()
        );
        assert!(view.pending_prefill.is_none());
        assert!(view.drawer.is_none());
        view.queue_prefill("origin".into(), prefill());
        view.reset(&mut env, &mut credentials);
        assert!(view.pending_prefill.is_none());
    }
}
