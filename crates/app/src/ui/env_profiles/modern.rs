//! 환경파일과 변수의 새 표시. 원본 변경 확인과 저장은 기존 worker가 수행한다.
use super::*;
use crate::ui::environment::field_label;

impl EnvProfilesUi {
    pub(crate) fn prefill_modern(
        &mut self,
        kind: crate::ui::environment::EnvironmentSelectionKind,
        value: crate::ui::credentials::SensitiveInput,
    ) -> egui::Id {
        self.reset_var_form();
        match kind {
            crate::ui::environment::EnvironmentSelectionKind::VariableName => {
                self.var_key = value.into_inner();
                env_var_value_input_id()
            }
            crate::ui::environment::EnvironmentSelectionKind::VariableValue => {
                self.var_plain_value = value.into_inner();
                env_var_key_input_id()
            }
            _ => unreachable!("환경변수 입력 필드만 전달한다"),
        }
    }

    pub(crate) fn prepare_modern(&mut self, snapshot: &EnvProfilesSnapshot) {
        self.sync_snapshot_state(snapshot);
        if let Some(sources) = snapshot.sources.as_ref()
            && self
                .write_source
                .as_ref()
                .is_none_or(|file| !sources.selected_files.contains(file))
        {
            self.write_source = sources.selected_files.last().cloned();
        }
    }

    pub(crate) fn reset_modern_draft(&mut self) {
        self.reset_var_form();
        self.source_draft = None;
        self.write_source = None;
    }

    pub(crate) fn modern_list(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &EnvProfilesSnapshot,
        catalog: &i18n::Catalog,
        query: &str,
        intent: &mut Option<EnvAction>,
    ) {
        if !snapshot.is_available() {
            return;
        }
        ui.strong(catalog.t("env.env_vars", &[]));
        let rows: Vec<_> = snapshot
            .dotenv_vars()
            .iter()
            .filter(|row| row.key().to_lowercase().contains(query))
            .collect();
        if rows.is_empty() {
            ui.weak(catalog.t("env.modern.no_items", &[]));
        }
        let mut action = None;
        egui::ScrollArea::vertical()
            .id_salt("modern_env_rows")
            .max_height(ENV_LIST_MAX_HEIGHT)
            .auto_shrink([false, true])
            .show_rows(ui, 88.0, rows.len(), |ui, range| {
                for var in &rows[range] {
                    ui.push_id((var.profile_id(), var.key()), |ui| {
                        ui.spacing_mut().item_spacing.y = 5.0;
                        ui.add(
                            egui::Label::new(egui::RichText::new(var.key()).monospace()).truncate(),
                        )
                        .on_hover_text(var.key());
                        let source = snapshot
                            .sources
                            .as_ref()
                            .and_then(|s| s.keys.get(var.key()))
                            .map(|files| files.join(" → "))
                            .unwrap_or_else(|| catalog.t("env.modern.archived", &[]));
                        ui.add(
                            egui::Label::new(egui::RichText::new(&source).small().weak())
                                .truncate(),
                        )
                        .on_hover_text(&source);
                        let id = row_id(var);
                        let masked = self.masked.contains(&id) || self.reveal_pending.contains(&id);
                        let revealed = self.revealed.get(&id).map(SensitiveDisplay::expose);
                        let value = if masked {
                            MASKED_VALUE
                        } else {
                            revealed.unwrap_or_else(|| match var.value() {
                                EnvValueView::Plain { value, .. } => value.as_ref(),
                                EnvValueView::Secret { .. } => MASKED_VALUE,
                            })
                        };
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                [(ui.available_width() - 104.0).max(1.0), 18.0],
                                egui::Label::new(egui::RichText::new(value).monospace()).truncate(),
                            );
                            if ui
                                .add_enabled(
                                    !self.reveal_pending.contains(&id),
                                    egui::Button::new(catalog.t(
                                        if masked {
                                            "env.modern.show"
                                        } else {
                                            "env.modern.hide"
                                        },
                                        &[],
                                    ))
                                    .small(),
                                )
                                .clicked()
                            {
                                action = Some(EnvRowAction::Toggle {
                                    profile_id: var.profile_id().to_owned(),
                                    key: var.key().to_owned(),
                                });
                            }
                            if ui.small_button(catalog.t("action.delete", &[])).clicked() {
                                action = Some(EnvRowAction::ConfirmDelete {
                                    profile_id: var.profile_id().to_owned(),
                                    key: var.key().to_owned(),
                                });
                            }
                        });
                        ui.label(
                            egui::RichText::new(catalog.t(
                                if var.value().has_os_override() {
                                    "env.os_override"
                                } else {
                                    "env.modern.file_value"
                                },
                                &[],
                            ))
                            .small()
                            .weak(),
                        );
                        ui.separator();
                    });
                }
            });
        if let Some(action) = action {
            self.handle_row_action(action, snapshot.dotenv_vars(), intent);
        }
        self.render_legacy_vars(ui, snapshot, catalog, intent);
    }

    pub(crate) fn modern_form(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &EnvProfilesSnapshot,
        catalog: &i18n::Catalog,
        intent: &mut Option<EnvAction>,
    ) {
        if !snapshot.project_root_configured() {
            ui.label(catalog.t("env.no_project_path_note", &[]));
            if ui
                .button(catalog.t("env.project_folder.choose", &[]))
                .clicked()
            {
                *intent = Some(EnvAction::ChooseProjectFolder);
            }
            return;
        }
        field_label(ui, catalog, "env.modern.variable_name");
        ui.add(
            egui::TextEdit::singleline(&mut self.var_key)
                .id(env_var_key_input_id())
                .font(egui::TextStyle::Monospace)
                .hint_text("APP_ENV")
                .desired_width(f32::INFINITY),
        );
        field_label(ui, catalog, "common.value");
        let response = ui.add(
            egui::TextEdit::singleline(&mut self.var_plain_value)
                .id(env_var_value_input_id())
                .hint_text("development")
                .desired_width(f32::INFINITY),
        );
        self.var_value_input_state.track(ui.ctx(), response.id);
        field_label(ui, catalog, "env.modern.save_file");
        let files = snapshot
            .sources
            .as_ref()
            .map(|s| s.selected_files.as_slice())
            .unwrap_or(&[]);
        egui::ComboBox::from_id_salt("modern_env_write_source")
            .width(ui.available_width())
            .selected_text(self.write_source.as_deref().unwrap_or("—"))
            .show_ui(ui, |ui| {
                for file in files {
                    ui.selectable_value(&mut self.write_source, Some(file.clone()), file);
                }
            });
        if files.is_empty() {
            ui.weak(catalog.t("env.sources_disabled", &[]));
        }
        truncate_utf8(&mut self.var_key, ENV_KEY_INPUT_MAX_BYTES);
        truncate_utf8(&mut self.var_plain_value, ENV_VALUE_INPUT_MAX_BYTES);
        let key = self.var_key.trim();
        let valid = deppy_core::credential_env::valid_name(key);
        if !key.is_empty() && !valid {
            ui.colored_label(
                ui.visuals().error_fg_color,
                catalog.t("env.modern.invalid_name", &[]),
            );
        }
        let existing = snapshot.dotenv_vars().iter().any(|row| row.key() == key);
        if existing {
            ui.weak(catalog.t("env.modern.replace_value", &[]));
        }
        if ui
            .add_enabled(
                valid && !files.is_empty() && snapshot.is_available() && intent.is_none(),
                egui::Button::new(catalog.t(
                    if existing {
                        "env.modern.replace"
                    } else {
                        "action.save"
                    },
                    &[],
                )),
            )
            .clicked()
        {
            let key = key.to_owned();
            self.var_value_input_state.clear();
            let value = std::mem::take(&mut self.var_plain_value);
            self.remove_local_value(snapshot.dotenv_profile_id().unwrap_or_default(), &key);
            self.var_key.clear();
            *intent = Some(EnvAction::DotenvWrite {
                file: self.write_source.clone(),
                key,
                value: Some(value),
            });
        }
    }

    pub(crate) fn modern_files(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &EnvProfilesSnapshot,
        catalog: &i18n::Catalog,
        intent: &mut Option<EnvAction>,
    ) {
        if !snapshot.is_available() {
            return;
        }
        if !snapshot.project_root_configured() {
            ui.label(catalog.t("env.no_project_path_note", &[]));
            if ui
                .button(catalog.t("env.project_folder.choose", &[]))
                .clicked()
            {
                *intent = Some(EnvAction::ChooseProjectFolder);
            }
            return;
        }
        let Some(sources) = snapshot.sources.as_ref() else {
            return;
        };
        ui.strong(catalog.t("env.modern.files", &[]));
        for (index, file) in sources.selected_files.iter().enumerate() {
            egui::Frame::group(ui.style())
                .inner_margin(10)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.weak(format!("{:02}", index + 1));
                        ui.monospace(file);
                        if !sources.files.contains(file) {
                            ui.weak(catalog.t("env.modern.file_absent", &[]));
                        }
                    });
                });
        }
        if sources.selected_files.is_empty() {
            ui.weak(catalog.t("env.sources_disabled", &[]));
        }
        ui.label(
            egui::RichText::new(catalog.t("env.modern.file_order", &[]))
                .small()
                .weak(),
        );
        ui.collapsing(catalog.t("env.sources_title", &[]), |ui| {
            let draft = self
                .source_draft
                .get_or_insert_with(|| sources.selected_files.join("\n"));
            ui.add_enabled(
                !self.source_pending,
                egui::TextEdit::multiline(draft)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .char_limit(8192),
            );
            truncate_utf8(draft, 8192);
            let files: Vec<String> = draft
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        deppy_core::env_sources::valid_files(&files)
                            && !self.source_pending
                            && intent.is_none(),
                        egui::Button::new(catalog.t("action.save", &[])),
                    )
                    .clicked()
                {
                    self.source_pending = true;
                    *intent = Some(EnvAction::SetSources { files: Some(files) });
                }
                if ui
                    .add_enabled(
                        !self.source_pending && intent.is_none(),
                        egui::Button::new(catalog.t("env.sources_reset", &[])),
                    )
                    .clicked()
                {
                    self.source_pending = true;
                    *intent = Some(EnvAction::SetSources { files: None });
                }
            });
            ui.collapsing(catalog.t("env.modern.format_help", &[]), |ui| {
                ui.label(catalog.t("env.sources_hint", &[]));
            });
        });
    }

    pub(crate) fn finish_modern(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &EnvProfilesSnapshot,
        catalog: &i18n::Catalog,
        intent: &mut Option<EnvAction>,
    ) {
        if snapshot.state == SnapshotLoadState::Loading {
            ui.spinner();
        }
        if let Some(sources) = snapshot.sources.as_ref() {
            if sources.read_failed {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    catalog.t("env.source_read_failed", &[]),
                );
            } else if sources.files.is_empty() && !snapshot.dotenv_vars().is_empty() {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    catalog.t("env.source_missing", &[]),
                );
            }
        }
        if snapshot.is_available() {
            self.render_delete_confirmation(ui.ctx(), snapshot, catalog, intent);
        }
        self.render_error(ui);
    }
}
