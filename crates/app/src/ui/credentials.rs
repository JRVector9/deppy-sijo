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
        // 좌측 라벨 컬럼 + 넓은 입력 폼 — 라벨 far-left/입력 far-right로 큰 빈 공간이 생기던
        // 우측정렬을 폼 스타일로 교체(#5 레이아웃 수정). 모든 입력이 같은 x에서 시작해 정렬.
        const LABEL_W: f32 = 88.0;
        const FIELD_W: f32 = 320.0;
        let text_color = ui.visuals().text_color();
        let field_row = |ui: &mut egui::Ui, label: String, add: &mut dyn FnMut(&mut egui::Ui)| {
            ui.horizontal(|ui| {
                let (r, _) =
                    ui.allocate_exact_size(egui::vec2(LABEL_W, 30.0), egui::Sense::hover());
                ui.painter().text(
                    egui::pos2(r.left(), r.center().y),
                    egui::Align2::LEFT_CENTER,
                    label,
                    egui::FontId::proportional(13.5),
                    text_color,
                );
                add(ui);
            });
        };
        field_row(ui, catalog.t("credentials.provider", &[]), &mut |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.provider).desired_width(FIELD_W));
        });
        field_row(ui, catalog.t("credentials.label", &[]), &mut |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.label).desired_width(FIELD_W));
        });
        field_row(ui, catalog.t("credentials.kind", &[]), &mut |ui| {
            for kind in ["api_key", "token"] {
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
