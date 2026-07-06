#[derive(Debug, Clone, PartialEq)]
pub struct CredentialListItem {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub credential_kind: String,
    pub masked_hint: Option<String>,
}

pub struct NewCredential {
    pub provider: String,
    pub label: String,
    pub credential_kind: String,
    pub secret: String,
}

pub trait CredentialService {
    fn list_credentials(&self) -> anyhow::Result<Vec<CredentialListItem>>;
    fn add_credential(&self, credential: NewCredential) -> anyhow::Result<()>;
    fn delete_credential(&self, id: &str) -> anyhow::Result<()>;
}

/// 자격증명 관리 창 상태. secret 입력값은 추가 즉시 비운다.
pub struct CredentialsUi {
    provider: String,
    label: String,
    kind: &'static str,
    secret_input: String,
    error: Option<String>,
    cached: Option<Vec<CredentialListItem>>,
}

impl CredentialsUi {
    pub fn new() -> Self {
        Self {
            provider: String::new(),
            label: String::new(),
            kind: "api_key",
            secret_input: String::new(),
            error: None,
            cached: None,
        }
    }

    /// 다른 창(커넥터)이 credential을 추가했을 때 목록 캐시를 버린다.
    pub fn invalidate_cache(&mut self) {
        self.cached = None;
    }

    pub fn contents(
        &mut self,
        ui: &mut egui::Ui,
        credentials: &dyn CredentialService,
        catalog: &i18n::Catalog,
    ) {
        let list = match &self.cached {
            Some(list) => list.clone(),
            None => match credentials.list_credentials() {
                Ok(list) => {
                    self.cached = Some(list.clone());
                    list
                }
                Err(e) => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        catalog.t("common.list_failed", &[("message", &format!("{e:#}"))]),
                    );
                    return;
                }
            },
        };

        if list.is_empty() {
            ui.label(catalog.t("credentials.empty", &[]));
        }
        let mut delete_id = None;
        for meta in &list {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{} · {} ({}) {}",
                    meta.label,
                    meta.provider,
                    meta.credential_kind,
                    meta.masked_hint.as_deref().unwrap_or(""),
                ));
                if ui.button(catalog.t("action.delete", &[])).clicked() {
                    delete_id = Some(meta.id.clone());
                }
            });
        }
        if let Some(id) = delete_id {
            self.error = self
                .delete(credentials, &id)
                .err()
                .map(|e| format!("{e:#}"));
        }

        ui.separator();
        ui.heading(catalog.t("credentials.add", &[]));
        ui.horizontal(|ui| {
            ui.label(catalog.t("credentials.provider", &[]));
            ui.text_edit_singleline(&mut self.provider);
        });
        ui.horizontal(|ui| {
            ui.label(catalog.t("credentials.label", &[]));
            ui.text_edit_singleline(&mut self.label);
        });
        ui.horizontal(|ui| {
            ui.label(catalog.t("credentials.kind", &[]));
            for kind in ["api_key", "token"] {
                ui.selectable_value(&mut self.kind, kind, kind);
            }
        });
        ui.horizontal(|ui| {
            ui.label(catalog.t("credentials.secret", &[]));
            ui.add(egui::TextEdit::singleline(&mut self.secret_input).password(true));
        });
        let filled = !self.provider.trim().is_empty()
            && !self.label.trim().is_empty()
            && !self.secret_input.is_empty();
        if ui
            .add_enabled(filled, egui::Button::new(catalog.t("action.add", &[])))
            .clicked()
        {
            self.error = self.add(credentials).err().map(|e| format!("{e:#}"));
        }

        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    }

    fn add(&mut self, credentials: &dyn CredentialService) -> anyhow::Result<()> {
        let request = NewCredential {
            provider: self.provider.trim().to_owned(),
            label: self.label.trim().to_owned(),
            credential_kind: self.kind.to_owned(),
            // 입력 평문은 service boundary로 옮기고 입력창은 즉시 비운다.
            secret: std::mem::take(&mut self.secret_input),
        };
        credentials.add_credential(request)?;
        self.provider.clear();
        self.label.clear();
        self.cached = None;
        Ok(())
    }

    fn delete(&mut self, credentials: &dyn CredentialService, id: &str) -> anyhow::Result<()> {
        credentials.delete_credential(id)?;
        self.cached = None;
        Ok(())
    }
}
