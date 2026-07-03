use crate::secret::{self, SecretStore, SecretString};
use crate::storage::{CredentialMeta, Db};

/// 자격증명 관리 창 상태. secret 입력값은 추가 즉시 비운다.
pub struct CredentialsUi {
    open: bool,
    provider: String,
    label: String,
    kind: &'static str,
    secret_input: String,
    error: Option<String>,
    cached: Option<Vec<CredentialMeta>>,
}

impl CredentialsUi {
    pub fn new() -> Self {
        Self {
            open: false,
            provider: String::new(),
            label: String::new(),
            kind: "api_key",
            secret_input: String::new(),
            error: None,
            cached: None,
        }
    }

    pub fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open = true;
        }
    }

    /// 모든 닫힘 경로는 여기를 지난다 — 입력 중이던 secret 평문을 즉시 버린다
    fn close(&mut self) {
        self.open = false;
        self.secret_input.clear();
        self.error = None;
    }

    pub fn show(&mut self, ctx: &egui::Context, db: &Db, store: &dyn SecretStore) {
        if !self.open {
            return;
        }
        let mut open = true;
        egui::Window::new("자격증명")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| self.contents(ui, db, store));
        if !open {
            self.close();
        }
    }

    fn contents(&mut self, ui: &mut egui::Ui, db: &Db, store: &dyn SecretStore) {
        let list = match &self.cached {
            Some(list) => list.clone(),
            None => match db.list_credentials() {
                Ok(list) => {
                    self.cached = Some(list.clone());
                    list
                }
                Err(e) => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("목록 조회 실패: {e:#}"),
                    );
                    return;
                }
            },
        };

        if list.is_empty() {
            ui.label("저장된 자격증명이 없습니다.");
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
                if ui.button("삭제").clicked() {
                    delete_id = Some(meta.id.clone());
                }
            });
        }
        if let Some(id) = delete_id {
            self.error = self.delete(db, store, &id).err().map(|e| format!("{e:#}"));
        }

        ui.separator();
        ui.heading("추가");
        ui.horizontal(|ui| {
            ui.label("provider");
            ui.text_edit_singleline(&mut self.provider);
        });
        ui.horizontal(|ui| {
            ui.label("라벨");
            ui.text_edit_singleline(&mut self.label);
        });
        ui.horizontal(|ui| {
            ui.label("종류");
            for kind in ["api_key", "token"] {
                ui.selectable_value(&mut self.kind, kind, kind);
            }
        });
        ui.horizontal(|ui| {
            ui.label("secret");
            ui.add(egui::TextEdit::singleline(&mut self.secret_input).password(true));
        });
        let filled = !self.provider.trim().is_empty()
            && !self.label.trim().is_empty()
            && !self.secret_input.is_empty();
        if ui.add_enabled(filled, egui::Button::new("추가")).clicked() {
            self.error = self.add(db, store).err().map(|e| format!("{e:#}"));
        }

        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    }

    fn add(&mut self, db: &Db, store: &dyn SecretStore) -> anyhow::Result<()> {
        // 입력 평문은 SecretString으로 옮기고 입력창은 즉시 비운다
        let secret = SecretString::new(std::mem::take(&mut self.secret_input));
        let id = uuid::Uuid::new_v4().to_string();
        store.set_secret(&id, &secret)?;
        let meta = CredentialMeta {
            id: id.clone(),
            provider: self.provider.trim().to_owned(),
            label: self.label.trim().to_owned(),
            credential_kind: self.kind.to_owned(),
            masked_hint: Some(secret::masked_hint(secret.expose())),
        };
        if let Err(e) = db.insert_credential(&meta) {
            // metadata 실패 시 keyring 고아 entry 방지 (rollback)
            let _ = store.delete_secret(&id);
            return Err(e);
        }
        tracing::info!(credential_id = %id, "credential 추가"); // secret 값은 로그 금지
        self.provider.clear();
        self.label.clear();
        self.cached = None;
        Ok(())
    }

    fn delete(&mut self, db: &Db, store: &dyn SecretStore, id: &str) -> anyhow::Result<()> {
        // 참조 검사 먼저 — 어느 단계에서 실패해도 재시도 가능한 순서:
        // keyring 실패 시 metadata 보존, DB 실패 시 delete_secret이 NoEntry 허용이라 재시도 수렴
        if db.credential_in_use(id)? {
            anyhow::bail!("env var가 참조 중인 credential입니다 — 해당 변수를 먼저 삭제하세요");
        }
        store.delete_secret(id)?;
        db.delete_credential(id)?; // env_vars FK는 backstop
        tracing::info!(credential_id = %id, "credential 삭제");
        self.cached = None;
        Ok(())
    }
}
