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
        // 입력 컴포넌트를 설정 컨트롤과 동일하게 우측 정렬 + 고정폭으로 통일한다(#7).
        // FIELD_W는 언어 드롭다운/스텝퍼와 같은 폭.
        const FIELD_W: f32 = 130.0;
        let field_row = |ui: &mut egui::Ui, label: String, add: &mut dyn FnMut(&mut egui::Ui)| {
            ui.horizontal(|ui| {
                ui.label(label);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    add(ui)
                });
            });
        };
        field_row(ui, catalog.t("credentials.provider", &[]), &mut |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.provider).desired_width(FIELD_W));
        });
        field_row(ui, catalog.t("credentials.label", &[]), &mut |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.label).desired_width(FIELD_W));
        });
        field_row(ui, catalog.t("credentials.kind", &[]), &mut |ui| {
            // 우측 정렬이라 오른쪽부터 배치 — token, api_key 순으로 넣어야 화면상 api_key가 왼쪽.
            for kind in ["token", "api_key"] {
                ui.selectable_value(&mut self.kind, kind, kind);
            }
        });
        field_row(ui, catalog.t("credentials.secret", &[]), &mut |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.secret_input)
                    .password(true)
                    .desired_width(FIELD_W),
            );
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
