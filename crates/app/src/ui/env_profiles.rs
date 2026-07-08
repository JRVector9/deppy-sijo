use crate::env::{self, EnvLayer, EnvValue};
use crate::storage::{CredentialMeta, Db, EnvProfileRow, EnvVarRow};

/// 환경 UI에서 App으로 올라가는 액션.
pub enum EnvAction {
    /// 프로젝트 폴더(워크스페이스 path) 설정 — App이 .env 재동기화 + 파일트리 루트 갱신.
    SetProjectPath(std::path::PathBuf),
}

/// 프로젝트 환경(env profile) 관리 창.
pub struct EnvProfilesUi {
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

    #[allow(dead_code)]
    pub fn contents(
        &mut self,
        ui: &mut egui::Ui,
        db: &mut Db,
        workspace_id: &str,
        catalog: &i18n::Catalog,
    ) -> anyhow::Result<Option<EnvAction>> {
        let mut action = None;
        // ---- 프로젝트 폴더(워크스페이스 path) — 이 폴더의 .env가 자동 동기화된다 ----
        let current_path = db.workspace_path(workspace_id).ok().flatten();
        let has_path = current_path
            .as_deref()
            .is_some_and(|p| !p.trim().is_empty());
        ui.horizontal(|ui| {
            ui.label(catalog.t("env.project_folder", &[]));
            let shown = if has_path {
                current_path.clone().unwrap_or_default()
            } else {
                catalog.t("env.project_folder.unset", &[])
            };
            ui.monospace(shown);
            if ui
                .button(catalog.t("env.project_folder.choose", &[]))
                .clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder()
            {
                action = Some(EnvAction::SetProjectPath(dir));
            }
            if has_path
                && ui
                    .button(catalog.t("env.project_folder.clear", &[]))
                    .clicked()
            {
                action = Some(EnvAction::SetProjectPath(std::path::PathBuf::new()));
            }
        });
        ui.label(
            egui::RichText::new(catalog.t("env.project_folder.hint", &[]))
                .small()
                .weak(),
        );
        ui.separator();

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
            return Ok(action);
        };
        let Some(profile) = profiles.iter().find(|p| p.id == profile_id) else {
            return Ok(action);
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
        Ok(action)
    }

    pub fn contents_compact(
        &mut self,
        ui: &mut egui::Ui,
        db: &mut Db,
        workspace_id: &str,
        catalog: &i18n::Catalog,
    ) -> anyhow::Result<Option<EnvAction>> {
        if self.cached_workspace.as_deref() != Some(workspace_id) {
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

        if self.selected.is_none()
            || !profiles
                .iter()
                .any(|p| Some(p.id.as_str()) == self.selected.as_deref())
        {
            self.selected = profiles.first().map(|p| p.id.clone());
            self.vars = None;
        }

        if profiles.is_empty() {
            env_api_section_header(ui, &catalog.t("env.env_vars", &[]), Some(0), None);
            compact_profile_form(ui, self, db, workspace_id, catalog)?;
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            return Ok(None);
        }

        let profile_id = self.selected.clone().unwrap_or_default();
        let Some(profile) = profiles.iter().find(|p| p.id == profile_id) else {
            return Ok(None);
        };

        let profile_is_production = profile.is_production;

        let credentials = db.list_credentials()?;
        let vars = match &self.vars {
            Some(v) => v.clone(),
            None => {
                let v = db.list_env_vars(&profile_id)?;
                self.vars = Some(v.clone());
                v
            }
        };
        let (api_key_vars, env_vars): (Vec<&EnvVarRow>, Vec<&EnvVarRow>) = vars
            .iter()
            .partition(|var| is_api_like_env_key(&var.key, &var.value));

        if env_api_section_header(
            ui,
            &catalog.t("env.env_vars", &[]),
            Some(env_vars.len()),
            Some(&catalog.t("env.add_key", &[])),
        ) {
            ui.memory_mut(|mem| mem.request_focus(env_var_key_input_id()));
        }
        env_table_header(
            ui,
            &[catalog.t("common.key", &[]), catalog.t("common.value", &[])],
        );
        let mut delete_key = None;
        for var in &env_vars {
            if env_table_row(ui, var, &credentials, catalog) {
                delete_key = Some(var.key.clone());
            }
            env_table_divider(ui);
        }
        if env_vars.is_empty() {
            env_empty_placeholder_row(ui, catalog);
            env_table_divider(ui);
        }

        compact_env_var_form(ui, self, db, &profile_id, &credentials, catalog)?;

        ui.add_space(14.0);
        env_api_section_header(
            ui,
            &catalog.t("credentials.api_keys", &[]),
            Some(api_key_vars.len()),
            None,
        );
        env_table_header(
            ui,
            &[catalog.t("common.key", &[]), catalog.t("common.value", &[])],
        );
        for var in &api_key_vars {
            if env_table_row(ui, var, &credentials, catalog) {
                delete_key = Some(var.key.clone());
            }
            env_table_divider(ui);
        }
        if let Some(key) = delete_key {
            db.delete_env_var(&profile_id, &key)?;
            self.vars = None;
            self.error = None;
        }

        if !vars.is_empty() {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(catalog.t("env.preview_heading", &[]))
                    .size(13.0)
                    .weak(),
            );
            let os_layer = EnvLayer {
                name: "OS".into(),
                vars: vars
                    .iter()
                    .filter_map(|v| {
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
                ui.label(
                    egui::RichText::new(format!(
                        "{} ← {}{}",
                        display_value(
                            &resolved.key,
                            &resolved.value,
                            &credentials,
                            &catalog.t("env.deleted_credential", &[]),
                        ),
                        resolved.source,
                        conflict
                    ))
                    .size(13.0)
                    .weak(),
                );
            }
        }

        let bottom_spacer = (ui.available_height() - 112.0).clamp(0.0, 260.0);
        if bottom_spacer > 0.0 {
            ui.add_space(bottom_spacer);
        }
        compact_profile_controls(
            ui,
            self,
            db,
            workspace_id,
            &profiles,
            profile_is_production,
            catalog,
        )?;

        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        Ok(None)
    }
}

/// 목록 표시용. secret은 credential 라벨/힌트만 노출한다.
#[allow(dead_code)]
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

fn env_api_section_header(
    ui: &mut egui::Ui,
    title: &str,
    count: Option<usize>,
    action_label: Option<&str>,
) -> bool {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), egui::Sense::hover());
    let painter = ui.painter();
    let y = rect.center().y;
    let mut x = rect.left();
    painter.text(
        egui::pos2(x, y),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(14.0),
        ui.visuals().text_color(),
    );
    x += painter
        .layout_no_wrap(
            title.to_owned(),
            egui::FontId::proportional(14.0),
            ui.visuals().text_color(),
        )
        .rect
        .width()
        + 10.0;
    if let Some(count) = count {
        let count_text = count.to_string();
        let count_rect =
            egui::Rect::from_center_size(egui::pos2(x + 14.0, y), egui::vec2(28.0, 28.0));
        painter.rect_filled(count_rect, 0.0, ui.visuals().faint_bg_color);
        painter.text(
            count_rect.center(),
            egui::Align2::CENTER_CENTER,
            count_text,
            egui::FontId::proportional(13.0),
            ui.visuals().hyperlink_color,
        );
    }

    if let Some(action_label) = action_label {
        let button_w = 72.0;
        let button_rect = egui::Rect::from_min_size(
            egui::pos2(rect.right() - button_w, rect.center().y - 14.0),
            egui::vec2(button_w, 28.0),
        );
        return ui
            .put(button_rect, egui::Button::new(action_label))
            .clicked();
    }
    false
}

fn env_table_header(ui: &mut egui::Ui, columns: &[String]) {
    ui.add_space(2.0);
    env_table_divider(ui);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
    let cols = env_table_columns(rect);
    let painter = ui.painter();
    let color = ui.visuals().weak_text_color();
    let font = egui::FontId::proportional(12.0);
    for (idx, column) in columns.iter().take(2).enumerate() {
        painter.text(
            egui::pos2(cols[idx].left() + 6.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            column,
            font.clone(),
            color,
        );
    }
    env_table_divider(ui);
}

fn env_table_row(
    ui: &mut egui::Ui,
    var: &EnvVarRow,
    credentials: &[CredentialMeta],
    catalog: &i18n::Catalog,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 46.0), egui::Sense::hover());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }

    let cols = env_table_columns(rect);
    let painter = ui.painter();
    let y = rect.center().y;
    painter.with_clip_rect(cols[0]).text(
        egui::pos2(cols[0].left() + 6.0, y),
        egui::Align2::LEFT_CENTER,
        &var.key,
        egui::FontId::proportional(14.0),
        ui.visuals().hyperlink_color,
    );
    painter.with_clip_rect(cols[1]).text(
        egui::pos2(cols[1].left() + 6.0, y),
        egui::Align2::LEFT_CENTER,
        display_env_value(
            &var.value,
            credentials,
            &catalog.t("env.deleted_credential", &[]),
        ),
        egui::FontId::proportional(14.0),
        ui.visuals().text_color(),
    );

    let override_center = egui::pos2(rect.right() - 72.0, y);
    let has_os_override = std::env::var_os(&var.key).is_some();
    let override_text = if has_os_override { "●" } else { "○" };
    let override_color = if has_os_override {
        ui.visuals().hyperlink_color
    } else {
        ui.visuals().weak_text_color()
    };
    painter.text(
        override_center,
        egui::Align2::CENTER_CENTER,
        override_text,
        egui::FontId::proportional(12.0),
        override_color,
    );
    let override_rect = egui::Rect::from_center_size(override_center, egui::vec2(20.0, 20.0));
    ui.interact(
        override_rect,
        ui.id().with(("env_override", &var.key)),
        egui::Sense::hover(),
    )
    .on_hover_text(catalog.t("env.os_override", &[]));

    let delete_rect =
        egui::Rect::from_center_size(egui::pos2(rect.right() - 40.0, y), egui::vec2(22.0, 18.0));
    let delete = ui
        .interact(
            delete_rect,
            ui.id().with(("env_var_delete", &var.key)),
            egui::Sense::click(),
        )
        .on_hover_text(catalog.t("action.delete", &[]));
    let hovered = delete.hovered();
    let stroke = if hovered {
        ui.visuals().error_fg_color
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke.color
    };
    let fill = if hovered {
        ui.visuals().error_fg_color
    } else {
        egui::Color32::TRANSPARENT
    };
    let text = if hovered {
        ui.visuals().window_fill
    } else {
        ui.visuals().weak_text_color()
    };
    painter.rect_filled(delete_rect, 0.0, fill);
    painter.rect_stroke(
        delete_rect,
        0.0,
        egui::Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    painter.text(
        delete_rect.center(),
        egui::Align2::CENTER_CENTER,
        "×",
        egui::FontId::proportional(11.0),
        text,
    );
    delete.clicked()
}

fn env_empty_placeholder_row(ui: &mut egui::Ui, catalog: &i18n::Catalog) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 46.0), egui::Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    if response.clicked() {
        ui.memory_mut(|mem| mem.request_focus(env_var_key_input_id()));
    }

    let cols = env_table_columns(rect);
    let y = rect.center().y;
    ui.painter().with_clip_rect(cols[0]).text(
        egui::pos2(cols[0].left() + 6.0, y),
        egui::Align2::LEFT_CENTER,
        "NEW_VAR",
        egui::FontId::proportional(14.0),
        ui.visuals().hyperlink_color,
    );
    ui.painter().with_clip_rect(cols[1]).text(
        egui::pos2(cols[1].left() + 6.0, y),
        egui::Align2::LEFT_CENTER,
        catalog.t("env.empty_value_placeholder", &[]),
        egui::FontId::proportional(14.0),
        ui.visuals().weak_text_color(),
    );

    let override_center = egui::pos2(rect.right() - 72.0, y);
    ui.painter().text(
        override_center,
        egui::Align2::CENTER_CENTER,
        "○",
        egui::FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );
    let delete_rect =
        egui::Rect::from_center_size(egui::pos2(rect.right() - 40.0, y), egui::vec2(22.0, 18.0));
    ui.painter().rect_stroke(
        delete_rect,
        0.0,
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        delete_rect.center(),
        egui::Align2::CENTER_CENTER,
        "×",
        egui::FontId::proportional(11.0),
        ui.visuals().weak_text_color(),
    );
}

fn env_table_columns(rect: egui::Rect) -> [egui::Rect; 2] {
    let action_w = 92.0;
    let content = egui::Rect::from_min_max(
        rect.min,
        egui::pos2((rect.right() - action_w).max(rect.left()), rect.bottom()),
    );
    let key_w = content.width() * 0.45;
    let key = egui::Rect::from_min_size(content.min, egui::vec2(key_w, content.height()));
    let value = egui::Rect::from_min_max(
        egui::pos2(key.right(), content.top()),
        egui::pos2(content.right(), content.bottom()),
    );
    [key, value]
}

fn env_table_divider(ui: &mut egui::Ui) {
    let color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    let y = ui.painter().round_to_pixel_center(rect.center().y);
    ui.painter()
        .hline(rect.x_range(), y, egui::Stroke::new(1.0, color));
}

fn display_env_value(
    value: &EnvValue,
    credentials: &[CredentialMeta],
    deleted_label: &str,
) -> String {
    match value {
        EnvValue::Plain(v) => v.clone(),
        EnvValue::Secret { credential_id } => credentials
            .iter()
            .find(|c| &c.id == credential_id)
            .map(|c| {
                c.masked_hint
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("••••••••••••")
                    .to_owned()
            })
            .unwrap_or_else(|| format!("[{deleted_label}]")),
    }
}

pub(crate) fn is_api_like_env_key(key: &str, value: &EnvValue) -> bool {
    if matches!(value, EnvValue::Secret { .. }) {
        return true;
    }

    let key = key.to_ascii_uppercase();
    const MARKERS: &[&str] = &[
        "API_KEY",
        "APIKEY",
        "ACCESS_KEY",
        "SECRET_KEY",
        "SECRET",
        "TOKEN",
        "PASSWORD",
        "PASSCODE",
        "PRIVATE_KEY",
        "CLIENT_SECRET",
        "CLIENT_ID",
        "SIGNING_KEY",
        "SIGNING_SECRET",
        "WEBHOOK_SECRET",
    ];
    MARKERS.iter().any(|marker| key.contains(marker))
}

fn compact_profile_controls(
    ui: &mut egui::Ui,
    state: &mut EnvProfilesUi,
    db: &mut Db,
    workspace_id: &str,
    profiles: &[EnvProfileRow],
    selected_is_production: bool,
    catalog: &i18n::Catalog,
) -> anyhow::Result<()> {
    ui.add_space(16.0);
    env_table_divider(ui);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(catalog.t("env.profile", &[]))
                .size(13.0)
                .weak(),
        );
        let mut delete_profile = None;
        for profile in profiles {
            let label = if profile.is_production {
                format!("{} · {}", profile.name, profile.kind)
            } else {
                profile.name.clone()
            };
            if ui
                .selectable_label(
                    state.selected.as_deref() == Some(profile.id.as_str()),
                    label,
                )
                .clicked()
            {
                state.selected = Some(profile.id.clone());
                state.vars = None;
            }
            if ui
                .small_button("×")
                .on_hover_text(catalog.t("action.delete", &[]))
                .clicked()
            {
                delete_profile = Some(profile.id.clone());
            }
        }
        if let Some(id) = delete_profile {
            if let Err(e) = db.delete_env_profile(&id) {
                state.error = Some(format!("{e:#}"));
            } else {
                if state.selected.as_deref() == Some(id.as_str()) {
                    state.selected = None;
                }
                state.profiles = None;
                state.vars = None;
                state.error = None;
            }
        }
    });
    if selected_is_production {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            egui::RichText::new(catalog.t("env.production_warning", &[])).size(13.0),
        );
    }
    compact_profile_form(ui, state, db, workspace_id, catalog)
}

fn compact_env_var_form(
    ui: &mut egui::Ui,
    state: &mut EnvProfilesUi,
    db: &mut Db,
    profile_id: &str,
    credentials: &[CredentialMeta],
    catalog: &i18n::Catalog,
) -> anyhow::Result<()> {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.var_key)
                .hint_text(catalog.t("common.key", &[]))
                .id_source(env_var_key_input_id())
                .desired_width(180.0),
        );
        ui.selectable_value(&mut state.var_is_secret, false, "plain");
        ui.selectable_value(&mut state.var_is_secret, true, "secret");
        if state.var_is_secret {
            let current = state
                .var_credential_id
                .as_ref()
                .and_then(|id| credentials.iter().find(|c| &c.id == id))
                .map(|c| c.label.clone())
                .unwrap_or_else(|| catalog.t("common.select", &[]));
            egui::ComboBox::from_id_salt("var_credential_compact")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for cred in credentials {
                        ui.selectable_value(
                            &mut state.var_credential_id,
                            Some(cred.id.clone()),
                            format!("{} ({})", cred.label, cred.provider),
                        );
                    }
                });
        } else {
            ui.add(
                egui::TextEdit::singleline(&mut state.var_plain_value)
                    .hint_text(catalog.t("common.value", &[]))
                    .desired_width(240.0),
            );
        }
        let key = state.var_key.trim();
        let key_valid = !key.is_empty() && !key.contains('=') && !key.contains('\0');
        let filled = key_valid
            && if state.var_is_secret {
                state.var_credential_id.is_some()
            } else {
                true
            };
        if ui
            .add_enabled(filled, egui::Button::new(catalog.t("env.add_var", &[])))
            .clicked()
        {
            let value = if state.var_is_secret {
                EnvValue::Secret {
                    credential_id: state.var_credential_id.clone().unwrap_or_default(),
                }
            } else {
                EnvValue::Plain(state.var_plain_value.clone())
            };
            match Db::validate_env_var_for_persistence(key, &value)
                .and_then(|_| db.upsert_env_var(profile_id, key, &value))
            {
                Ok(()) => {
                    state.var_key.clear();
                    state.var_plain_value.clear();
                    state.vars = None;
                    state.error = None;
                }
                Err(e) => state.error = Some(format!("{e:#}")),
            }
        }
    });
    Ok(())
}

fn env_var_key_input_id() -> egui::Id {
    egui::Id::new("env_var_key_input_compact")
}

fn compact_profile_form(
    ui: &mut egui::Ui,
    state: &mut EnvProfilesUi,
    db: &mut Db,
    workspace_id: &str,
    catalog: &i18n::Catalog,
) -> anyhow::Result<()> {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.new_name)
                .hint_text(catalog.t("env.create_profile", &[]))
                .desired_width(180.0),
        );
        for kind in ["local", "staging", "production", "custom"] {
            ui.selectable_value(&mut state.new_kind, kind, kind);
        }
        let name_filled = !state.new_name.trim().is_empty();
        if ui
            .add_enabled(
                name_filled,
                egui::Button::new(catalog.t("env.create_profile", &[])),
            )
            .clicked()
        {
            match db.insert_env_profile(workspace_id, state.new_name.trim(), state.new_kind) {
                Ok(_) => {
                    state.new_name.clear();
                    state.profiles = None;
                    state.error = None;
                }
                Err(e) => state.error = Some(format!("{e:#}")),
            }
        }
    });
    Ok(())
}
