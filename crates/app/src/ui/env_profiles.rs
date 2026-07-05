use crate::env::{self, EnvLayer, EnvValue};
use crate::storage::{CredentialMeta, Db, EnvProfileRow, EnvVarRow};

/// 프로젝트 환경(env profile) 관리 창.
pub struct EnvProfilesUi {
    open: bool,
    selected: Option<String>,
    new_name: String,
    new_kind: &'static str,
    var_key: String,
    var_is_secret: bool,
    var_plain_value: String,
    var_credential_id: Option<String>,
    error: Option<String>,
    profiles: Option<Vec<EnvProfileRow>>,
    vars: Option<Vec<EnvVarRow>>,
    /// 캐시가 속한 workspace — 다른 workspace로 바뀌면 캐시/선택을 통째로 버린다
    /// (§6.1 "프로젝트 A 키가 B에 들어감" 방지 — codex 리뷰)
    cached_workspace: Option<String>,
}

impl EnvProfilesUi {
    pub fn new() -> Self {
        Self {
            open: false,
            selected: None,
            new_name: String::new(),
            new_kind: "local",
            var_key: String::new(),
            var_is_secret: false,
            var_plain_value: String::new(),
            var_credential_id: None,
            error: None,
            profiles: None,
            vars: None,
            cached_workspace: None,
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        db: &mut Db,
        workspace_id: &str,
        catalog: &i18n::Catalog,
    ) {
        if !self.open {
            return;
        }
        let mut open = true;
        egui::Window::new(catalog.t("env.title", &[]))
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                if let Err(e) = self.contents(ui, db, workspace_id, catalog) {
                    self.error = Some(format!("{e:#}"));
                }
                if let Some(error) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
            });
        self.open = open;
    }

    fn contents(
        &mut self,
        ui: &mut egui::Ui,
        db: &mut Db,
        workspace_id: &str,
        catalog: &i18n::Catalog,
    ) -> anyhow::Result<()> {
        if self.cached_workspace.as_deref() != Some(workspace_id) {
            // workspace가 바뀌었다 — 이전 workspace의 profile/var/선택으로
            // 조회·삭제·upsert하면 안 된다
            self.profiles = None;
            self.vars = None;
            self.selected = None;
            self.cached_workspace = Some(workspace_id.to_owned());
        }
        let profiles = match &self.profiles {
            Some(p) => p.clone(),
            None => {
                let p = db.list_env_profiles(workspace_id)?;
                self.profiles = Some(p.clone());
                p
            }
        };

        // ---- profile 목록 ----
        if profiles.is_empty() {
            ui.label(catalog.t("env.empty_profiles", &[]));
        }
        let mut delete_profile = None;
        for profile in &profiles {
            ui.horizontal(|ui| {
                let selected = self.selected.as_deref() == Some(profile.id.as_str());
                let title = if profile.is_production {
                    format!("⚠ {} ({})", profile.name, profile.kind)
                } else {
                    format!("{} ({})", profile.name, profile.kind)
                };
                if ui.selectable_label(selected, title).clicked() {
                    self.selected = Some(profile.id.clone());
                    self.vars = None;
                }
                if ui.button(catalog.t("action.delete", &[])).clicked() {
                    delete_profile = Some(profile.id.clone());
                }
            });
        }
        if let Some(id) = delete_profile {
            db.delete_env_profile(&id)?;
            if self.selected.as_deref() == Some(id.as_str()) {
                self.selected = None;
            }
            self.profiles = None;
            self.vars = None;
            self.error = None;
        }

        // ---- profile 생성 ----
        ui.horizontal(|ui| {
            ui.label(catalog.t("common.name", &[]));
            ui.text_edit_singleline(&mut self.new_name);
            for kind in ["local", "staging", "production", "custom"] {
                ui.selectable_value(&mut self.new_kind, kind, kind);
            }
        });
        let name_filled = !self.new_name.trim().is_empty();
        if ui
            .add_enabled(
                name_filled,
                egui::Button::new(catalog.t("env.create_profile", &[])),
            )
            .clicked()
        {
            db.insert_env_profile(workspace_id, self.new_name.trim(), self.new_kind)?;
            self.new_name.clear();
            self.profiles = None;
            self.error = None;
        }

        // ---- 선택된 profile의 env vars ----
        let Some(profile_id) = self.selected.clone() else {
            return Ok(());
        };
        let Some(profile) = profiles.iter().find(|p| p.id == profile_id) else {
            return Ok(());
        };
        ui.separator();
        ui.heading(catalog.t("env.vars_heading", &[("name", &profile.name)]));
        if profile.is_production {
            // production guard (설계문서 6.4) — spawn 직전 경고는 PR-09에서 이 플래그를 소비
            ui.colored_label(
                ui.visuals().warn_fg_color,
                catalog.t("env.production_warning", &[]),
            );
        }

        let credentials = db.list_credentials()?;
        let vars = match &self.vars {
            Some(v) => v.clone(),
            None => {
                let v = db.list_env_vars(&profile_id)?;
                self.vars = Some(v.clone());
                v
            }
        };

        let mut delete_key = None;
        for var in &vars {
            ui.horizontal(|ui| {
                ui.label(describe_var(
                    var,
                    &credentials,
                    &catalog.t("env.deleted_credential", &[]),
                ));
                // OS env와 충돌하면 precedence 결과를 표시 (설계문서 6.2)
                if std::env::var_os(&var.key).is_some() {
                    ui.weak(catalog.t("env.os_override", &[]));
                }
                if ui.button(catalog.t("action.delete", &[])).clicked() {
                    delete_key = Some(var.key.clone());
                }
            });
        }
        if let Some(key) = delete_key {
            db.delete_env_var(&profile_id, &key)?;
            self.vars = None;
            self.error = None;
        }

        // ---- env var 추가 ----
        ui.horizontal(|ui| {
            ui.label(catalog.t("common.key", &[]));
            ui.text_edit_singleline(&mut self.var_key);
            ui.selectable_value(&mut self.var_is_secret, false, "plain");
            ui.selectable_value(&mut self.var_is_secret, true, "secret");
        });
        if self.var_is_secret {
            ui.horizontal(|ui| {
                ui.label(catalog.t("env.credential", &[]));
                let current = self
                    .var_credential_id
                    .as_ref()
                    .and_then(|id| credentials.iter().find(|c| &c.id == id))
                    .map(|c| c.label.clone())
                    .unwrap_or_else(|| catalog.t("common.select", &[]));
                egui::ComboBox::from_id_salt("var_credential")
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        for cred in &credentials {
                            ui.selectable_value(
                                &mut self.var_credential_id,
                                Some(cred.id.clone()),
                                format!("{} ({})", cred.label, cred.provider),
                            );
                        }
                    });
            });
        } else {
            ui.horizontal(|ui| {
                ui.label(catalog.t("common.value", &[]));
                ui.text_edit_singleline(&mut self.var_plain_value);
            });
        }
        // key에 '='/NUL은 env로 전달 불가 (spawn 계층에서 깨진다 — codex 리뷰)
        let key_valid = {
            let key = self.var_key.trim();
            !key.is_empty() && !key.contains('=') && !key.contains('\0')
        };
        let filled = key_valid
            && if self.var_is_secret {
                self.var_credential_id.is_some()
            } else {
                true
            };
        if ui
            .add_enabled(filled, egui::Button::new(catalog.t("env.add_var", &[])))
            .clicked()
        {
            let value = if self.var_is_secret {
                EnvValue::Secret {
                    credential_id: self.var_credential_id.clone().unwrap(),
                }
            } else {
                EnvValue::Plain(self.var_plain_value.clone())
            };
            Db::validate_env_var_for_persistence(self.var_key.trim(), &value)?;
            db.upsert_env_var(&profile_id, self.var_key.trim(), &value)?;
            self.var_key.clear();
            self.var_plain_value.clear();
            self.vars = None;
            self.error = None;
        }

        // ---- 적용 결과 미리보기 (EnvDiffPreview 최소형) ----
        if !vars.is_empty() {
            ui.separator();
            ui.heading(catalog.t("env.preview_heading", &[]));
            let os_layer = EnvLayer {
                name: "OS".into(),
                vars: vars
                    .iter()
                    .filter_map(|v| {
                        // 충돌 감지(var_os)와 같은 기준 — 비-UTF8 값도 누락하지
                        // 않는다 (lossy 표기. codex 리뷰)
                        std::env::var_os(&v.key).map(|val| {
                            (
                                v.key.clone(),
                                EnvValue::Plain(val.to_string_lossy().into_owned()),
                            )
                        })
                    })
                    .collect(),
            };
            let profile_layer = EnvLayer {
                name: profile.name.clone(),
                vars: vars
                    .iter()
                    .map(|v| (v.key.clone(), v.value.clone()))
                    .collect(),
            };
            for resolved in env::resolve(&[os_layer, profile_layer]) {
                let conflict = if resolved.overridden.is_empty() {
                    String::new()
                } else {
                    let overridden = resolved.overridden.join(", ");
                    catalog.t("env.preview_overridden", &[("names", &overridden)])
                };
                ui.label(format!(
                    "{} ← {}{}",
                    display_value(
                        &resolved.key,
                        &resolved.value,
                        &credentials,
                        &catalog.t("env.deleted_credential", &[]),
                    ),
                    resolved.source,
                    conflict
                ));
            }
        }
        Ok(())
    }
}

/// 목록 표시용. secret은 credential 라벨/힌트만 노출한다.
fn describe_var(var: &EnvVarRow, credentials: &[CredentialMeta], deleted_label: &str) -> String {
    display_value(&var.key, &var.value, credentials, deleted_label)
}

fn display_value(
    key: &str,
    value: &EnvValue,
    credentials: &[CredentialMeta],
    deleted_label: &str,
) -> String {
    match value {
        EnvValue::Plain(v) => format!("{key} = {v}"),
        EnvValue::Secret { credential_id } => {
            let cred = credentials.iter().find(|c| &c.id == credential_id);
            match cred {
                Some(c) => format!(
                    "{key} = [secret: {} {}]",
                    c.label,
                    c.masked_hint.as_deref().unwrap_or("")
                ),
                None => format!("{key} = [secret: {deleted_label}]"),
            }
        }
    }
}
