//! API 저장 계약을 유지하는 목록과 세로 입력 패널.
use super::*;
use crate::ui::environment::field_label;

impl CredentialsUi {
    pub(crate) fn prepare_modern(&mut self, snapshot: &CredentialsSnapshot) {
        self.sync_snapshot(snapshot);
    }

    pub(crate) fn reset_modern_draft(&mut self) {
        clear_sensitive_string(&mut self.secret_input);
        self.env_name.clear();
        self.provider.clear();
        self.label.clear();
        self.modern_secret_visible = false;
        self.secret_input_overflowed = false;
        self.show_add_form = false;
        // 작업 중 여부는 화면 전환으로 풀지 않고 기존 완료 ACK가 정리한다.
    }

    pub(crate) fn begin_modern_add(&mut self) {
        if self.provider.is_empty() {
            self.provider = "openai".into();
            self.env_name = "OPENAI_API_KEY".into();
        }
    }

    pub(crate) fn modern_list(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &CredentialsSnapshot,
        catalog: &i18n::Catalog,
        query: &str,
        intent: &mut Option<CredentialsIntent>,
    ) {
        if !snapshot.is_available() {
            return;
        }
        ui.strong(catalog.t("credentials.api_keys", &[]));
        let rows: Vec<_> = snapshot
            .items()
            .iter()
            .filter(|row| {
                row.env_name
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(query)
                    || row.label().to_lowercase().contains(query)
                    || row.provider().to_lowercase().contains(query)
            })
            .collect();
        if rows.is_empty() {
            ui.weak(catalog.t("env.modern.no_items", &[]));
        }
        egui::ScrollArea::vertical()
            .id_salt("modern_api_rows")
            .max_height(CREDENTIAL_LIST_MAX_HEIGHT)
            .auto_shrink([false, true])
            .show_rows(ui, 88.0, rows.len(), |ui, range| {
                for meta in &rows[range] {
                    ui.push_id(meta.id(), |ui| {
                        ui.spacing_mut().item_spacing.y = 5.0;
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(
                                    meta.env_name.as_deref().unwrap_or(meta.label()),
                                )
                                .monospace(),
                            )
                            .truncate(),
                        )
                        .on_hover_text(meta.env_name.as_deref().unwrap_or(meta.label()));
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!(
                                    "{} · {}",
                                    meta.provider(),
                                    meta.label()
                                ))
                                .small()
                                .weak(),
                            )
                            .truncate(),
                        );
                        ui.horizontal(|ui| {
                            let revealed =
                                self.revealed.get(meta.id()).map(SensitiveDisplay::expose);
                            ui.add_sized(
                                [(ui.available_width() - 104.0).max(1.0), 18.0],
                                egui::Label::new(
                                    egui::RichText::new(revealed.unwrap_or(MASKED_SECRET))
                                        .monospace(),
                                )
                                .truncate(),
                            );
                            let reveal = ui
                                .add_enabled(
                                    !self.reveal_pending.contains(meta.id()),
                                    egui::Button::new(catalog.t(
                                        if revealed.is_some() {
                                            "env.modern.hide"
                                        } else {
                                            "env.modern.show"
                                        },
                                        &[],
                                    ))
                                    .small(),
                                )
                                .clicked();
                            if reveal {
                                if self.revealed.contains_key(meta.id()) {
                                    self.remove_revealed(meta.id());
                                } else if intent.is_none() {
                                    if self.revealed.len() >= CREDENTIAL_SENSITIVE_MAX_ITEMS {
                                        self.error =
                                            Some(CredentialsUiErrorCode::RevealCapacityExceeded);
                                    } else {
                                        self.reveal_pending.insert(meta.id().to_owned());
                                        *intent = Some(CredentialsIntent::Reveal {
                                            revision: snapshot.revision(),
                                            credential_id: meta.id().to_owned(),
                                        });
                                    }
                                }
                            }
                            if ui
                                .add_enabled(
                                    !self.delete_pending.contains(meta.id()),
                                    egui::Button::new(catalog.t("action.delete", &[])).small(),
                                )
                                .clicked()
                            {
                                self.delete_confirm =
                                    Some((meta.id().to_owned(), meta.label().to_owned()));
                            }
                        });
                        let source_key = if meta.overrides_dotenv {
                            "credentials.env_overrides_dotenv"
                        } else if meta.env_name.is_none() {
                            "env.modern.api_unbound"
                        } else {
                            "env.modern.secure_storage"
                        };
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(catalog.t(source_key, &[]))
                                    .small()
                                    .weak(),
                            )
                            .truncate(),
                        )
                        .on_hover_text(catalog.t(source_key, &[]));
                        ui.separator();
                    });
                }
            });
        ui.collapsing(catalog.t("env.modern.bindings", &[]), |ui| {
            self.render_env_bindings(ui, snapshot, catalog, intent);
        });
    }

    pub(crate) fn modern_form(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &CredentialsSnapshot,
        catalog: &i18n::Catalog,
        intent: &mut Option<CredentialsIntent>,
    ) {
        field_label(ui, catalog, "env.modern.service");
        let services = [
            ("openai", "OpenAI", "OPENAI_API_KEY"),
            ("anthropic", "Anthropic", "ANTHROPIC_API_KEY"),
            ("google", "Google AI", "GEMINI_API_KEY"),
            ("supabase", "Supabase", "SUPABASE_SERVICE_ROLE_KEY"),
        ];
        let direct = catalog.t("env.modern.direct", &[]);
        let display = services
            .iter()
            .find(|(id, _, _)| *id == self.provider)
            .map_or(direct.as_str(), |(_, name, _)| *name);
        egui::ComboBox::from_id_salt("modern_api_service")
            .width(ui.available_width())
            .selected_text(display)
            .show_ui(ui, |ui| {
                for (id, name, variable) in services {
                    if ui.selectable_label(self.provider == id, name).clicked() {
                        self.provider = id.into();
                        self.env_name = variable.into();
                    }
                }
                if ui
                    .selectable_label(
                        !services.iter().any(|(id, _, _)| *id == self.provider),
                        direct,
                    )
                    .clicked()
                {
                    self.provider.clear();
                    self.env_name.clear();
                }
            });
        if !services.iter().any(|(id, _, _)| *id == self.provider) {
            ui.add(
                egui::TextEdit::singleline(&mut self.provider)
                    .hint_text(catalog.t("credentials.provider", &[]))
                    .desired_width(f32::INFINITY),
            );
        }
        field_label(ui, catalog, "env.modern.variable_name");
        ui.add(
            egui::TextEdit::singleline(&mut self.env_name)
                .font(egui::TextStyle::Monospace)
                .hint_text("OPENAI_API_KEY")
                .desired_width(f32::INFINITY),
        );
        ui.label(
            egui::RichText::new(catalog.t("env.modern.binding_optional", &[]))
                .small()
                .weak(),
        );
        field_label(ui, catalog, "env.modern.api_value");
        let changed = ui
            .add(
                egui::TextEdit::singleline(&mut self.secret_input)
                    .password(!self.modern_secret_visible)
                    .hint_text(catalog.t("env.modern.paste_key", &[]))
                    .desired_width(f32::INFINITY),
            )
            .changed();
        if self.secret_input.len() > CREDENTIAL_SENSITIVE_ITEM_MAX_BYTES {
            clear_sensitive_string(&mut self.secret_input);
            self.secret_input_overflowed = true;
            self.error = Some(CredentialsUiErrorCode::DraftLimitExceeded);
        } else if changed {
            self.secret_input_overflowed = false;
        }
        ui.checkbox(
            &mut self.modern_secret_visible,
            catalog.t("action.show_secret", &[]),
        );
        ui.label(
            egui::RichText::new(catalog.t("env.modern.secure_storage", &[]))
                .small()
                .weak(),
        );
        ui.collapsing(catalog.t("env.modern.api_name_optional", &[]), |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.label)
                    .hint_text(catalog.t("env.modern.api_name_hint", &[]))
                    .desired_width(f32::INFINITY),
            );
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(
                    &mut self.kind,
                    "api_key",
                    catalog.t("credentials.api_keys", &[]),
                );
                ui.selectable_value(&mut self.kind, "token", "Token");
            });
        });
        truncate_utf8(&mut self.env_name, 256);
        truncate_utf8(&mut self.provider, CREDENTIAL_TEXT_INPUT_MAX_BYTES);
        truncate_utf8(&mut self.label, CREDENTIAL_TEXT_INPUT_MAX_BYTES);
        let name = self.env_name.trim();
        let valid_name = name.is_empty() || deppy_core::credential_env::valid_name(name);
        let duplicate = !name.is_empty()
            && snapshot
                .items()
                .iter()
                .any(|row| row.env_name.as_deref() == Some(name));
        if duplicate {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                catalog.t("env.modern.duplicate_api", &[]),
            );
        }
        if !valid_name {
            ui.colored_label(
                ui.visuals().error_fg_color,
                catalog.t("env.modern.invalid_name", &[]),
            );
        }
        let filled = valid_name
            && !duplicate
            && !self.provider.trim().is_empty()
            && !self.secret_input.is_empty()
            && !self.secret_input_overflowed
            && !self.add_pending
            && snapshot.is_available()
            && intent.is_none();
        if ui
            .add_enabled(filled, egui::Button::new(catalog.t("action.save", &[])))
            .clicked()
        {
            let secret = std::mem::take(&mut self.secret_input);
            match SensitiveInput::try_new(secret) {
                Ok(secret) => {
                    self.add_pending = true;
                    self.error = None;
                    self.modern_secret_visible = false;
                    *intent = Some(CredentialsIntent::Add {
                        revision: snapshot.revision(),
                        credential: NewCredential {
                            env_name: self.env_name.trim().to_owned(),
                            provider: self.provider.trim().to_owned(),
                            label: if self.label.trim().is_empty() {
                                self.provider.trim().to_owned()
                            } else {
                                self.label.trim().to_owned()
                            },
                            credential_kind: self.kind.to_owned(),
                            secret,
                        },
                    });
                }
                Err(_) => self.error = Some(CredentialsUiErrorCode::DraftLimitExceeded),
            }
        }
        if self.add_pending {
            ui.spinner();
        }
    }

    pub(crate) fn finish_modern(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &CredentialsSnapshot,
        catalog: &i18n::Catalog,
        intent: &mut Option<CredentialsIntent>,
    ) {
        if snapshot.state == SnapshotLoadState::Loading {
            ui.spinner();
        }
        if snapshot.is_available() {
            self.render_delete_confirmation(ui.ctx(), snapshot.revision(), catalog, intent);
        }
        if let Some(error) = self.error {
            ui.colored_label(ui.visuals().error_fg_color, error.message());
        }
    }

    pub(crate) fn modern_maintenance(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &CredentialsSnapshot,
        catalog: &i18n::Catalog,
        intent: &mut Option<CredentialsIntent>,
    ) {
        if snapshot.is_available() {
            let previous = self.show_add_form;
            self.show_add_form = true;
            self.render_orphan_controls(ui, snapshot, catalog, intent);
            self.show_add_form = previous;
        }
    }
}
