//! 기존 API를 같은 ID로 수정하는 초안. 비밀키를 비우면 저장된 키를 유지한다.
use super::*;

pub struct EditedCredential {
    pub credential_id: String,
    pub provider: String,
    pub label: String,
    pub credential_kind: String,
    pub env_name: Option<String>,
    pub secret: Option<SensitiveInput>,
}

pub(super) struct CredentialEditDraft {
    pub(super) credential_id: String,
    provider: String,
    label: String,
    kind: &'static str,
    env_name: String,
    secret: String,
    fields: [super::super::draft_text_edit::DraftTextEditState; 4],
    pub(super) pending: bool,
    overflowed: bool,
    /// 실패 후 빈 입력으로 재저장해 키 교체 의도가 사라지지 않게 한다.
    replacement_required: bool,
}

impl Drop for CredentialEditDraft {
    fn drop(&mut self) {
        clear_sensitive_string(&mut self.secret);
    }
}

impl CredentialsUi {
    pub(super) fn begin_edit(&mut self, meta: &CredentialListItem) {
        if !meta.editable || self.edit.as_ref().is_some_and(|edit| edit.pending) {
            return;
        }
        self.error = None;
        self.edit = Some(CredentialEditDraft {
            credential_id: meta.id().into(),
            provider: meta.provider().into(),
            label: meta.label().into(),
            kind: if meta.credential_kind() == "token" {
                "token"
            } else {
                "api_key"
            },
            env_name: meta.env_name.as_deref().unwrap_or_default().into(),
            secret: String::new(),
            fields: Default::default(),
            pending: false,
            overflowed: false,
            replacement_required: false,
        });
    }

    pub fn complete_edit(&mut self, current: bool, credential_id: &str, success: bool) {
        if !current
            || !self
                .edit
                .as_ref()
                .is_some_and(|edit| edit.pending && edit.credential_id == credential_id)
        {
            return;
        }
        if success {
            self.edit = None;
            self.remove_revealed(credential_id);
            self.error = None;
        } else {
            self.report_error(CredentialsUiErrorCode::EditFailed);
        }
    }

    pub(super) fn render_edit_form(
        &mut self,
        ctx: &egui::Context,
        snapshot: &CredentialsSnapshot,
        catalog: &i18n::Catalog,
        intent: &mut Option<CredentialsIntent>,
    ) {
        let Some(edit) = self.edit.as_mut() else {
            return;
        };
        let mut cancel = false;
        let mut save = false;
        crate::ui::popup::set_pending_modal(ctx, true);
        let response = egui::Modal::new(egui::Id::new("credential_edit_window")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.heading(catalog.t("credentials.edit.title", &[]));
            ui.add_enabled_ui(!edit.pending, |ui| {
                for (index, key, value) in [
                    (0, "credentials.provider", &mut edit.provider),
                    (1, "credentials.label", &mut edit.label),
                    (2, "credentials.env_name", &mut edit.env_name),
                ] {
                    ui.label(catalog.t(key, &[]));
                    let limit = if index == 2 {
                        256
                    } else {
                        CREDENTIAL_TEXT_INPUT_MAX_BYTES
                    };
                    let response = ui.add(
                        egui::TextEdit::singleline(value)
                            .id(ui.id().with(("api_edit", index)))
                            .char_limit(limit)
                            .desired_width(320.0),
                    );
                    edit.fields[index].track(ui.ctx(), response.id);
                    truncate_utf8(value, limit);
                }
                ui.horizontal(|ui| {
                    ui.label(catalog.t("credentials.kind", &[]));
                    for kind in ["api_key", "token"] {
                        ui.selectable_value(&mut edit.kind, kind, kind);
                    }
                });
                ui.label(catalog.t("credentials.secret", &[]));
                let response = ui.add(
                    egui::TextEdit::singleline(&mut edit.secret)
                        .id(ui.id().with("api_edit_secret"))
                        .password(true)
                        .desired_width(320.0)
                        .hint_text(if edit.replacement_required {
                            catalog.t("credentials.edit.retry_secret", &[])
                        } else {
                            catalog.t("credentials.edit.keep_secret", &[])
                        }),
                );
                edit.fields[3].track(ui.ctx(), response.id);
                if edit.secret.len() > CREDENTIAL_SENSITIVE_ITEM_MAX_BYTES {
                    clear_sensitive_string(&mut edit.secret);
                    edit.fields[3].clear();
                    edit.overflowed = true;
                } else if response.changed() {
                    edit.overflowed = false;
                }
            });
            let valid = !edit.provider.trim().is_empty()
                && !edit.label.trim().is_empty()
                && !edit.overflowed
                && !edit.pending
                && !self.operations_pending
                && (!edit.replacement_required || !edit.secret.is_empty())
                && snapshot.is_available()
                && (edit.env_name.trim().is_empty()
                    || deppy_core::credential_env::valid_name(edit.env_name.trim()));
            if edit.overflowed {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    CredentialsUiErrorCode::DraftLimitExceeded.message(),
                );
            }
            if self.error == Some(CredentialsUiErrorCode::EditFailed) {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    CredentialsUiErrorCode::EditFailed.message(),
                );
            }
            ui.horizontal(|ui| {
                save = ui
                    .add_enabled(
                        valid && intent.is_none(),
                        egui::Button::new(catalog.t("action.save", &[])),
                    )
                    .clicked();
                cancel = ui
                    .add_enabled(
                        !edit.pending,
                        egui::Button::new(catalog.t("action.cancel", &[])),
                    )
                    .clicked();
                if edit.pending {
                    ui.spinner();
                }
            });
        });
        cancel |= response.should_close() && !edit.pending;
        if save {
            edit.fields[3].clear();
            let secret = if edit.secret.is_empty() {
                None
            } else {
                match SensitiveInput::try_new(std::mem::take(&mut edit.secret)) {
                    Ok(secret) => Some(secret),
                    Err(_) => {
                        edit.overflowed = true;
                        return;
                    }
                }
            };
            edit.pending = true;
            edit.replacement_required = secret.is_some();
            self.error = None;
            *intent = Some(CredentialsIntent::Edit {
                revision: snapshot.revision(),
                credential: EditedCredential {
                    credential_id: edit.credential_id.clone(),
                    provider: edit.provider.trim().into(),
                    label: edit.label.trim().into(),
                    credential_kind: edit.kind.into(),
                    env_name: (!edit.env_name.trim().is_empty())
                        .then(|| edit.env_name.trim().into()),
                    secret,
                },
            });
        } else if cancel {
            self.edit = None;
            if self.error == Some(CredentialsUiErrorCode::EditFailed) {
                self.error = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_edit_닫으면_비밀초안과_undo를_비우고_늦은_응답을_무시한다() {
        let item = CredentialListItem::new("api", "service", "name", "api_key", None::<String>)
            .with_env_binding(Some("API_KEY".into()), false)
            .with_editable(true);
        let mut ui = CredentialsUi::new();
        ui.begin_edit(&item);
        let draft = ui.edit.as_mut().unwrap();
        assert!(draft.secret.is_empty());
        assert_eq!(draft.env_name, "API_KEY");
        draft.secret = "fake-edit-secret".into();
        draft.pending = true;
        let (history, ctx, id, shared) = crate::ui::draft_text_edit::recorded_fake_input();
        draft.fields[3] = history;
        ui.reset_modern_draft();
        assert!(ui.edit.is_none());
        let cursor = egui::text::CCursorRange::one(egui::text::CCursor::new(0));
        assert!(shared.undoer().undo(&(cursor, String::new())).is_none());
        assert!(egui::text_edit::TextEditState::load(&ctx, id).is_none());
        ui.begin_edit(&item);
        ui.edit.as_mut().unwrap().label = "replacement draft".into();
        ui.complete_edit(true, "api", true);
        assert_eq!(ui.edit.as_ref().unwrap().label, "replacement draft");
        assert!(ui.edit.as_ref().unwrap().secret.is_empty());
    }
}
