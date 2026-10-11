use super::super::{draft_text_edit::DraftTextEditState, popup, text_input::BoundedTextBuffer};
use super::*;
use std::collections::BTreeSet;

pub(super) enum Dialog {
    Edit(EditDraft),
    Source(SourceDraft),
    GitHub(Box<GitHubDraft>),
    Recipe(RecipeDraft),
    Delete(ProjectKey),
    Conflict(ProjectKey),
}

pub(super) struct EditDraft {
    pub key: ProjectKey,
    pub value: SensitiveDraft,
    expected: Option<u64>,
    replacement_required: bool,
    replacement_submitted: bool,
    rejected: bool,
    input_state: DraftTextEditState,
    remote_source_allowed: bool,
}
impl EditDraft {
    pub fn new(key: ProjectKey) -> Self {
        Self {
            key,
            value: SensitiveDraft::default(),
            expected: None,
            replacement_required: false,
            replacement_submitted: false,
            rejected: false,
            input_state: DraftTextEditState::default(),
            remote_source_allowed: true,
        }
    }
    pub fn existing(key: ProjectKey) -> Self {
        let revision = key.revision;
        let remote_source_allowed = key.source == KeySource::GitHubOnly;
        let mut draft = Self::new(key);
        draft.expected = Some(revision);
        draft.remote_source_allowed = remote_source_allowed;
        draft
    }
    pub fn failed_submission(&mut self) {
        self.replacement_required = self.replacement_submitted;
    }
    pub(super) fn valid(&self) -> bool {
        !self.rejected
            && self.key.validate().is_ok()
            && (self.key.source == KeySource::GitHubOnly
                || !self.value.is_empty()
                || (self.expected.is_some() && !self.replacement_required))
    }
    pub(super) fn submit(&mut self) -> ProjectKeysAction {
        self.input_state.clear();
        self.replacement_submitted = !self.value.is_empty();
        let value = if self.replacement_submitted {
            ValueEdit::Replace(std::mem::take(&mut self.value))
        } else {
            ValueEdit::Keep
        };
        ProjectKeysAction::SaveKey {
            key: Box::new(self.key.clone()),
            expected_revision: self.expected,
            value,
        }
    }
}

pub(super) struct SourceDraft {
    pub source: KeySource,
    pub preview: Option<SourcePreview>,
    pub names: BTreeSet<String>,
    sources: Vec<KeySource>,
    expected_revision: u64,
    pem_name: String,
}
impl SourceDraft {
    pub fn new(snapshot: &ProjectKeysSnapshot) -> Self {
        Self {
            source: snapshot
                .settings
                .sources
                .iter()
                .find(|source| {
                    matches!(source, KeySource::Dotenv { .. } | KeySource::PemFile { .. })
                })
                .cloned()
                .unwrap_or(KeySource::Dotenv {
                    file: match snapshot.scope.environment {
                        KeyEnvironment::Development => ".env.development",
                        KeyEnvironment::Production => ".env.production",
                    }
                    .into(),
                }),
            preview: None,
            names: BTreeSet::new(),
            sources: snapshot.settings.sources.clone(),
            expected_revision: snapshot.settings.revision,
            pem_name: "PRIVATE_KEY".into(),
        }
    }
}

pub(super) struct GitHubDraft {
    key: ProjectKey,
    auth: Option<String>,
    account: String,
    scope_kind: u8,
    owner: String,
    repository: String,
    environment: String,
    organization: String,
    visibility: GitHubVisibility,
    repositories: String,
    secret_name: String,
    auto_send: bool,
    allow_external: bool,
    credential_page: Option<Vec<CredentialChoice>>,
    credential_after: Option<String>,
    selected_credential_label: Option<String>,
}
impl GitHubDraft {
    pub fn existing(key: ProjectKey) -> Box<Self> {
        let mut draft = Self {
            auth: None,
            account: String::new(),
            scope_kind: 0,
            owner: String::new(),
            repository: String::new(),
            environment: String::new(),
            organization: String::new(),
            visibility: GitHubVisibility::Private,
            repositories: String::new(),
            secret_name: key.name.clone(),
            auto_send: false,
            allow_external: false,
            credential_page: None,
            credential_after: None,
            selected_credential_label: None,
            key,
        };
        if let Some(remote) = &draft.key.github {
            draft.auth = remote.target.auth_credential_id.clone();
            draft.account = remote.target.account_identity.clone();
            draft.secret_name = remote.target.secret_name.clone();
            draft.auto_send = remote.target.auto_send;
            match &remote.target.scope {
                GitHubScope::Repository { owner, repository } => {
                    draft.owner = owner.clone();
                    draft.repository = repository.clone();
                }
                GitHubScope::Environment {
                    owner,
                    repository,
                    environment,
                } => {
                    draft.scope_kind = 1;
                    draft.owner = owner.clone();
                    draft.repository = repository.clone();
                    draft.environment = environment.clone();
                }
                GitHubScope::Organization {
                    organization,
                    visibility,
                    selected_repository_ids,
                } => {
                    draft.scope_kind = 2;
                    draft.organization = organization.clone();
                    draft.visibility = *visibility;
                    draft.repositories = selected_repository_ids
                        .iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                }
            }
        }
        Box::new(draft)
    }
    pub(super) fn set_credentials_page(
        &mut self,
        choices: &[CredentialChoice],
        after: &Option<String>,
    ) {
        self.credential_page = Some(choices.iter().take(128).cloned().collect());
        self.credential_after = after.clone();
    }
    fn target(&self) -> Option<GitHubTarget> {
        let scope = match self.scope_kind {
            0 => GitHubScope::Repository {
                owner: self.owner.clone(),
                repository: self.repository.clone(),
            },
            1 => GitHubScope::Environment {
                owner: self.owner.clone(),
                repository: self.repository.clone(),
                environment: self.environment.clone(),
            },
            _ => GitHubScope::Organization {
                organization: self.organization.clone(),
                visibility: self.visibility,
                selected_repository_ids: if self.visibility == GitHubVisibility::Selected {
                    self.repositories
                        .split(',')
                        .map(|part| part.trim().parse::<u64>())
                        .collect::<Result<Vec<_>, _>>()
                        .ok()?
                } else {
                    Vec::new()
                },
            },
        };
        let target = GitHubTarget {
            auth_credential_id: self.auth.clone(),
            account_identity: self.account.clone(),
            scope,
            secret_name: self.secret_name.clone(),
            auto_send: self.auto_send,
        };
        target.validate().ok()?;
        Some(target)
    }
}

pub(super) struct RecipeDraft {
    recipe: ProjectBuildRecipe,
    expected: Option<u64>,
    program: String,
    arguments: Vec<String>,
}
impl RecipeDraft {
    pub fn new(scope: &ProjectKeysScope, cwd: String) -> Self {
        Self {
            recipe: ProjectBuildRecipe {
                id: uuid::Uuid::new_v4().to_string(),
                workspace_id: scope.workspace_id.clone(),
                environment: scope.environment,
                revision: 1,
                name: String::new(),
                command: Vec::new(),
                cwd,
            },
            expected: None,
            program: String::new(),
            arguments: Vec::new(),
        }
    }
    pub fn existing(recipe: ProjectBuildRecipe) -> Self {
        let program = recipe.command.first().cloned().unwrap_or_default();
        let arguments = recipe.command.iter().skip(1).cloned().collect();
        Self {
            expected: Some(recipe.revision),
            recipe,
            program,
            arguments,
        }
    }
    fn value(&self) -> Option<ProjectBuildRecipe> {
        let mut recipe = self.recipe.clone();
        recipe.command = std::iter::once(self.program.clone())
            .chain(self.arguments.iter().cloned())
            .collect();
        recipe.validate().ok()?;
        Some(recipe)
    }
}

fn input(ui: &mut egui::Ui, label: &str, value: &mut String, hint: Option<&str>) {
    popup::field(ui, label, hint, |ui| {
        // Metadata is bounded independently from secret input and is revalidated
        // by the worker before an operation is accepted.
        let mut rejected = false;
        let mut buffer = BoundedTextBuffer {
            text: value,
            max_bytes: 4096,
            rejected: &mut rejected,
            normalize_nfc: false,
        };
        popup::text_input(ui, &mut buffer, "");
    });
}

impl ProjectKeysUi {
    pub(super) fn render_dialog(
        &mut self,
        ctx: &egui::Context,
        snapshot: &ProjectKeysSnapshot,
        catalog: &i18n::Catalog,
    ) -> Option<(ProjectKeysAction, PendingKind)> {
        let mut dialog = self.dialog.take()?;
        let busy = self.pending.is_some();
        let (case, title, subtitle, target) = match &dialog {
            Dialog::Edit(draft) => (
                "project_key_edit",
                "project_keys.edit_title",
                "project_keys.edit_hint",
                draft.key.id.as_str(),
            ),
            Dialog::Source(_) => (
                "project_key_source",
                "project_keys.source_title",
                "project_keys.source_hint",
                snapshot.scope.workspace_id.as_str(),
            ),
            Dialog::GitHub(draft) => (
                "project_key_github",
                "project_keys.github_title",
                "project_keys.github_hint",
                draft.key.id.as_str(),
            ),
            Dialog::Recipe(draft) => (
                "project_key_build",
                "project_keys.recipe_title",
                "project_keys.recipe_hint",
                draft.recipe.id.as_str(),
            ),
            Dialog::Delete(key) => (
                "project_key_delete",
                "project_keys.delete_title",
                "project_keys.delete_hint",
                key.id.as_str(),
            ),
            Dialog::Conflict(key) => (
                "project_key_conflict",
                "project_keys.conflict_title",
                "project_keys.conflict_hint",
                key.id.as_str(),
            ),
        };
        let action_scope = popup::prepare_target(ctx, egui::Id::new(case), egui::Id::new(target));
        let mut action = None;
        let mut cancel = false;
        let closed = popup::show(
            ctx,
            popup::PopupSpec {
                id: egui::Id::new(case),
                width: 480.0,
                title: &catalog.t(title, &[]),
                subtitle: &catalog.t(subtitle, &[]),
                close_label: &catalog.t("popup.dismiss", &[]),
                close_enabled: !busy,
            },
            |ui| {
                popup::body(ui, |ui| {
                    ui.push_id(action_scope, |ui| {
                        if let Some(Err(error)) = &self.notice {
                            popup::notice(
                                ui,
                                &error_label(*error, catalog),
                                popup::NoticeTone::Error,
                            );
                        }
                        ui.add_enabled_ui(!busy, |ui| match &mut dialog {
                            Dialog::Edit(draft) => edit_body(ui, draft, snapshot, catalog),
                            Dialog::Source(draft) => {
                                source_body(ui, draft, snapshot, catalog, &mut action)
                            }
                            Dialog::GitHub(draft) => {
                                github_body(ui, draft, snapshot, catalog, &mut action)
                            }
                            Dialog::Recipe(draft) => recipe_body(ui, draft, catalog),
                            Dialog::Delete(key) | Dialog::Conflict(key) => {
                                ui.label(egui::RichText::new(&key.name).strong());
                                ui.label(table::source_label(&key.source, catalog));
                            }
                        });
                        if busy {
                            popup::notice(
                                ui,
                                &catalog.t("project_keys.working", &[]),
                                popup::NoticeTone::Info,
                            );
                        }
                    });
                });
                popup::footer(ui, None, |ui| {
                    ui.push_id(action_scope, |ui| {
                        match &mut dialog {
                            Dialog::Edit(draft) => {
                                if popup::action_button(
                                    ui,
                                    &catalog.t("action.save", &[]),
                                    popup::ActionTone::Primary,
                                    !busy && draft.valid(),
                                )
                                .clicked()
                                {
                                    action = Some((draft.submit(), PendingKind::Edit));
                                }
                            }
                            Dialog::Source(draft) => {
                                let pem = matches!(draft.source, KeySource::PemFile { .. });
                                let import_ready = draft
                                    .preview
                                    .as_ref()
                                    .is_some_and(|p| p.source == draft.source)
                                    && if pem {
                                        deppy_core::credential_env::valid_name(&draft.pem_name)
                                    } else {
                                        !draft.names.is_empty()
                                    };
                                if popup::action_button(
                                    ui,
                                    &catalog.t("project_keys.import_selected", &[]),
                                    popup::ActionTone::Primary,
                                    !busy && import_ready,
                                )
                                .clicked()
                                {
                                    action = Some((
                                        ProjectKeysAction::ImportSource {
                                            source: draft.source.clone(),
                                            names: if pem {
                                                vec![draft.pem_name.clone()]
                                            } else {
                                                draft.names.iter().cloned().collect()
                                            },
                                        },
                                        PendingKind::Import,
                                    ));
                                }
                                if popup::action_button(
                                    ui,
                                    &catalog.t("project_keys.save_sources", &[]),
                                    popup::ActionTone::Secondary,
                                    !busy && draft.sources != snapshot.settings.sources,
                                )
                                .clicked()
                                {
                                    action = Some((
                                        ProjectKeysAction::SetSources {
                                            sources: draft.sources.clone(),
                                            expected_revision: draft.expected_revision,
                                        },
                                        PendingKind::Sources,
                                    ));
                                }
                            }
                            Dialog::GitHub(draft) => {
                                let target = draft.target();
                                let matches_saved =
                                    draft.key.github.as_ref().is_some_and(|remote| {
                                        Some(&remote.target) == target.as_ref()
                                    });
                                let external = draft.key.github.as_ref().is_some_and(|remote| {
                                    remote.state == GitHubRemoteState::ExternalChange
                                });
                                if popup::action_button(
                                    ui,
                                    &catalog.t("project_keys.send", &[]),
                                    popup::ActionTone::Primary,
                                    !busy
                                        && draft.key.source != KeySource::GitHubOnly
                                        && matches_saved
                                        && (!external || draft.allow_external),
                                )
                                .clicked()
                                {
                                    action = Some((
                                        ProjectKeysAction::SendGitHub {
                                            key_id: draft.key.id.clone(),
                                            expected_revision: draft.key.revision,
                                            allow_external_change: draft.allow_external,
                                        },
                                        PendingKind::GitHub,
                                    ));
                                }
                                if popup::action_button(
                                    ui,
                                    &catalog.t("action.save", &[]),
                                    popup::ActionTone::Secondary,
                                    !busy && target.is_some(),
                                )
                                .clicked()
                                {
                                    action = Some((
                                        ProjectKeysAction::SaveGitHubTarget {
                                            key_id: draft.key.id.clone(),
                                            expected_revision: draft.key.revision,
                                            target: target.map(Box::new),
                                        },
                                        PendingKind::GitHub,
                                    ));
                                }
                                if draft.key.github.is_some()
                                    && popup::action_button(
                                        ui,
                                        &catalog.t("project_keys.unlink", &[]),
                                        popup::ActionTone::Danger,
                                        !busy,
                                    )
                                    .clicked()
                                {
                                    action = Some((
                                        ProjectKeysAction::SaveGitHubTarget {
                                            key_id: draft.key.id.clone(),
                                            expected_revision: draft.key.revision,
                                            target: None,
                                        },
                                        PendingKind::GitHub,
                                    ));
                                }
                            }
                            Dialog::Recipe(draft) => {
                                if popup::action_button(
                                    ui,
                                    &catalog.t("action.save", &[]),
                                    popup::ActionTone::Primary,
                                    !busy && draft.value().is_some(),
                                )
                                .clicked()
                                {
                                    action = Some((
                                        ProjectKeysAction::SaveRecipe {
                                            recipe: draft.value().unwrap(),
                                            expected_revision: draft.expected,
                                        },
                                        PendingKind::Recipe,
                                    ));
                                }
                                if let Some(revision) = draft.expected
                                    && popup::action_button(
                                        ui,
                                        &catalog.t("action.delete", &[]),
                                        popup::ActionTone::Danger,
                                        !busy,
                                    )
                                    .clicked()
                                {
                                    action = Some((
                                        ProjectKeysAction::DeleteRecipe {
                                            recipe_id: draft.recipe.id.clone(),
                                            expected_revision: revision,
                                        },
                                        PendingKind::Recipe,
                                    ));
                                }
                            }
                            Dialog::Delete(key) => {
                                if popup::action_button(
                                    ui,
                                    &catalog.t("action.delete", &[]),
                                    popup::ActionTone::Danger,
                                    !busy,
                                )
                                .clicked()
                                {
                                    action = Some((
                                        ProjectKeysAction::DeleteKey {
                                            key_id: key.id.clone(),
                                            expected_revision: key.revision,
                                        },
                                        PendingKind::Delete,
                                    ));
                                }
                            }
                            Dialog::Conflict(key) => {
                                for (resolution, label) in [
                                    (ConflictResolution::KeepSource, "project_keys.keep_source"),
                                    (ConflictResolution::KeepVault, "project_keys.keep_vault"),
                                ] {
                                    if popup::action_button(
                                        ui,
                                        &catalog.t(label, &[]),
                                        popup::ActionTone::Primary,
                                        !busy,
                                    )
                                    .clicked()
                                    {
                                        action = Some((
                                            ProjectKeysAction::ResolveConflict {
                                                key_id: key.id.clone(),
                                                expected_revision: key.revision,
                                                resolution,
                                            },
                                            PendingKind::Conflict,
                                        ));
                                    }
                                }
                            }
                        }
                        cancel = popup::action_button(
                            ui,
                            &catalog.t("action.cancel", &[]),
                            popup::ActionTone::Ghost,
                            !busy,
                        )
                        .clicked();
                    });
                });
            },
        );
        if !(closed || cancel) {
            self.dialog = Some(dialog);
        }
        action
    }
}

fn edit_body(
    ui: &mut egui::Ui,
    draft: &mut EditDraft,
    snapshot: &ProjectKeysSnapshot,
    catalog: &i18n::Catalog,
) {
    if draft.expected.is_some() {
        ui.label(egui::RichText::new(&draft.key.name).strong());
    } else {
        input(
            ui,
            &catalog.t("project_keys.name", &[]),
            &mut draft.key.name,
            Some(&catalog.t("project_keys.name_hint", &[])),
        );
    }
    let old_source = draft.key.source.clone();
    popup::field(ui, &catalog.t("project_keys.source", &[]), None, |ui| {
        popup::choice_input(
            ui,
            "edit_source",
            &table::source_label(&draft.key.source, catalog),
            |ui| {
                ui.selectable_value(
                    &mut draft.key.source,
                    KeySource::Vault,
                    catalog.t("project_keys.source_vault", &[]),
                );
                if draft.remote_source_allowed {
                    ui.selectable_value(
                        &mut draft.key.source,
                        KeySource::GitHubOnly,
                        catalog.t("project_keys.source_github", &[]),
                    );
                }
                for source in &snapshot.settings.sources {
                    ui.selectable_value(
                        &mut draft.key.source,
                        source.clone(),
                        table::source_label(source, catalog),
                    );
                }
            },
        );
    });
    if draft.key.source != old_source {
        draft.key.source_baseline_credential_id = None;
    }
    let remote_only = draft.key.source == KeySource::GitHubOnly;
    if remote_only {
        draft.value.clear();
        draft.rejected = false;
        draft.replacement_required = false;
        draft.input_state.clear();
        draft.key.usage = KeyUsage::StoreOnly;
        draft.key.required = false;
        draft.key.delivery = KeyDelivery::Environment;
        draft.key.credential_id = None;
        draft.key.last_good_revision = None;
        draft.key.last_good_at = None;
        draft.key.storage_state = KeyStorageState::NotStored;
        draft.key.legacy_reference = None;
        if let Some(remote) = &mut draft.key.github {
            remote.target.auto_send = false;
        }
        popup::notice(
            ui,
            &catalog.t("project_keys.remote_only_hint", &[]),
            popup::NoticeTone::Info,
        );
    }
    popup::field(ui, &catalog.t("project_keys.kind", &[]), None, |ui| {
        popup::choice_input(
            ui,
            "edit_kind",
            &kind_label(draft.key.kind, catalog),
            |ui| {
                for kind in [
                    KeyKind::ApiToken,
                    KeyKind::ClientSecret,
                    KeyKind::PrivateKey,
                    KeyKind::Configuration,
                ] {
                    ui.selectable_value(&mut draft.key.kind, kind, kind_label(kind, catalog));
                }
            },
        );
    });
    let multiline = draft.key.kind == KeyKind::PrivateKey;
    if !remote_only {
        popup::field(
            ui,
            &catalog.t("project_keys.value", &[]),
            Some(&catalog.t(
                if draft.expected.is_some() {
                    "project_keys.keep_blank"
                } else {
                    "project_keys.value_hint"
                },
                &[],
            )),
            |ui| {
                let id = ui.make_persistent_id("project_key_secret");
                draft.input_state.track(ui.ctx(), id);
                let state = egui::TextEdit::load_state(ui.ctx(), id);
                let mut buffer =
                    popup::MaskedSecretBuffer::new(draft.value.text_mut(), SECRET_INPUT_BYTES_MAX);
                let response = popup::secret_input(ui, &mut buffer, id, "", multiline);
                let rejected = buffer.finish();
                if rejected {
                    draft.rejected = true;
                    if let Some(mut state) = state {
                        state.clear_undoer();
                        state.store(ui.ctx(), id);
                    }
                } else if response.changed() {
                    draft.rejected = false;
                }
            },
        );
    }
    if draft.replacement_required && draft.value.is_empty() {
        popup::notice(
            ui,
            &catalog.t("project_keys.reenter", &[]),
            popup::NoticeTone::Error,
        );
    }
    if draft.rejected {
        popup::notice(
            ui,
            &catalog.t("project_keys.too_long", &[]),
            popup::NoticeTone::Error,
        );
    }
    ui.add_enabled_ui(!remote_only, |ui| {
        popup::field(ui, &catalog.t("project_keys.usage", &[]), None, |ui| {
            popup::choice_input(
                ui,
                "edit_usage",
                &table::usage_label(draft.key.usage, catalog),
                |ui| {
                    for usage in [KeyUsage::Build, KeyUsage::StoreOnly, KeyUsage::LegacyAgent] {
                        ui.selectable_value(
                            &mut draft.key.usage,
                            usage,
                            table::usage_label(usage, catalog),
                        );
                    }
                },
            );
        });
        popup::field(ui, &catalog.t("project_keys.delivery", &[]), None, |ui| {
            popup::choice_input(
                ui,
                "edit_delivery",
                &delivery_label(draft.key.delivery, catalog),
                |ui| {
                    for delivery in [
                        KeyDelivery::Environment,
                        KeyDelivery::FilePath,
                        KeyDelivery::TemporaryFile,
                    ] {
                        ui.selectable_value(
                            &mut draft.key.delivery,
                            delivery,
                            delivery_label(delivery, catalog),
                        );
                    }
                },
            );
        });
        ui.checkbox(
            &mut draft.key.required,
            catalog.t("project_keys.required", &[]),
        );
    });
    if draft.key.validate().is_err() && !draft.key.name.is_empty() {
        popup::notice(
            ui,
            &catalog.t("project_keys.error_input", &[]),
            popup::NoticeTone::Error,
        );
    }
}

fn source_body(
    ui: &mut egui::Ui,
    draft: &mut SourceDraft,
    snapshot: &ProjectKeysSnapshot,
    catalog: &i18n::Catalog,
    action: &mut Option<(ProjectKeysAction, PendingKind)>,
) {
    ui.label(egui::RichText::new(catalog.t("project_keys.source_order", &[])).strong());
    let mut change = None;
    for (index, source) in draft.sources.iter().enumerate() {
        ui.horizontal_wrapped(|ui| {
            ui.label(table::source_label(source, catalog));
            if ui
                .add_enabled(
                    index > 0,
                    egui::Button::new("↑").min_size(egui::vec2(28.0, 28.0)),
                )
                .clicked()
            {
                change = Some((index, -1));
            }
            if ui
                .add_enabled(
                    index + 1 < draft.sources.len(),
                    egui::Button::new("↓").min_size(egui::vec2(28.0, 28.0)),
                )
                .clicked()
            {
                change = Some((index, 1));
            }
            if ui
                .button(catalog.t("project_keys.remove_source", &[]))
                .clicked()
            {
                change = Some((index, 0));
            }
        });
    }
    if let Some((index, direction)) = change {
        if direction == 0 {
            draft.sources.remove(index);
        } else {
            draft
                .sources
                .swap(index, (index as isize + direction) as usize);
        }
    }
    super::super::hairline(ui);
    let mut pem = matches!(draft.source, KeySource::PemFile { .. });
    ui.horizontal(|ui| {
        if ui
            .selectable_value(&mut pem, false, catalog.t("project_keys.dotenv", &[]))
            .changed()
        {
            draft.source = KeySource::Dotenv {
                file: match snapshot.scope.environment {
                    KeyEnvironment::Development => ".env.development",
                    KeyEnvironment::Production => ".env.production",
                }
                .into(),
            };
            draft.preview = None;
            draft.names.clear();
        }
        if ui
            .selectable_value(&mut pem, true, catalog.t("project_keys.pem", &[]))
            .changed()
        {
            draft.source = KeySource::PemFile {
                path: String::new(),
            };
            draft.preview = None;
            draft.names.clear();
        }
    });
    popup::field(
        ui,
        &catalog.t("project_keys.file", &[]),
        Some(&catalog.t("project_keys.selected_only", &[])),
        |ui| {
            let value = match &mut draft.source {
                KeySource::Dotenv { file } => file,
                KeySource::PemFile { path } => path,
                _ => unreachable!(),
            };
            let mut rejected = false;
            let mut buffer = BoundedTextBuffer {
                text: value,
                max_bytes: PROJECT_KEY_PATH_BYTES_MAX,
                rejected: &mut rejected,
                normalize_nfc: false,
            };
            let response =
                popup::path_input(ui, &mut buffer, &catalog.t("project_keys.browse", &[]));
            if response.input.changed() {
                draft.preview = None;
                draft.names.clear();
            }
            if response.browse.clicked() {
                *action = Some((
                    ProjectKeysAction::PickSource { pem },
                    PendingKind::SourcePick,
                ));
            }
        },
    );
    ui.horizontal_wrapped(|ui| {
        if popup::action_button(
            ui,
            &catalog.t("project_keys.attach", &[]),
            popup::ActionTone::Secondary,
            draft.source.validate().is_ok()
                && draft.sources.len() < PROJECT_KEY_SOURCES_MAX
                && !draft.sources.contains(&draft.source),
        )
        .clicked()
        {
            draft.sources.push(draft.source.clone());
        }
        if popup::action_button(
            ui,
            &catalog.t("project_keys.preview", &[]),
            popup::ActionTone::Secondary,
            draft.source.validate().is_ok(),
        )
        .clicked()
        {
            *action = Some((
                ProjectKeysAction::PreviewSource {
                    source: draft.source.clone(),
                },
                PendingKind::Preview,
            ));
        }
    });
    if pem {
        input(
            ui,
            &catalog.t("project_keys.name", &[]),
            &mut draft.pem_name,
            Some(&catalog.t("project_keys.name_hint", &[])),
        );
    }
    if let Some(preview) = &draft.preview {
        for name in preview.names.iter().filter(|_| !pem) {
            let mut checked = draft.names.contains(name);
            if ui.checkbox(&mut checked, name).changed() {
                if checked {
                    draft.names.insert(name.clone());
                } else {
                    draft.names.remove(name);
                }
            }
        }
        if preview.names.is_empty() {
            popup::notice(
                ui,
                &catalog.t("project_keys.source_empty", &[]),
                popup::NoticeTone::Error,
            );
        }
    }
    if draft.sources != snapshot.settings.sources {
        ui.label(egui::RichText::new(catalog.t("project_keys.sources_unsaved", &[])).weak());
    }
}

fn github_body(
    ui: &mut egui::Ui,
    draft: &mut GitHubDraft,
    snapshot: &ProjectKeysSnapshot,
    catalog: &i18n::Catalog,
    action: &mut Option<(ProjectKeysAction, PendingKind)>,
) {
    popup::notice(
        ui,
        &catalog.t("project_keys.github_outbound", &[]),
        popup::NoticeTone::Info,
    );
    popup::field(ui, &catalog.t("project_keys.auth", &[]), None, |ui| {
        let choices = draft
            .credential_page
            .as_deref()
            .unwrap_or(&snapshot.auth_credentials);
        let selected = match &draft.auth {
            None => catalog.t("project_keys.gh_account", &[]),
            Some(id) => choices
                .iter()
                .find(|c| &c.id == id)
                .map(|c| c.label.clone())
                .or_else(|| draft.selected_credential_label.clone())
                .unwrap_or_else(|| catalog.t("project_keys.selected_credential", &[("id", id)])),
        };
        popup::choice_input(ui, "github_auth", &selected, |ui| {
            ui.selectable_value(
                &mut draft.auth,
                None,
                catalog.t("project_keys.gh_account", &[]),
            );
            for credential in choices {
                if ui
                    .selectable_value(
                        &mut draft.auth,
                        Some(credential.id.clone()),
                        &credential.label,
                    )
                    .changed()
                {
                    draft.selected_credential_label = Some(credential.label.clone());
                }
            }
        });
    });
    ui.horizontal_wrapped(|ui| {
        let after = match &draft.credential_page {
            Some(_) => draft.credential_after.clone(),
            None if snapshot.auth_credentials.len() == 128 => snapshot
                .auth_credentials
                .last()
                .map(|credential| credential.id.clone()),
            None => None,
        };
        if after.is_some()
            && popup::action_button(
                ui,
                &catalog.t("project_keys.credentials_next", &[]),
                popup::ActionTone::Secondary,
                true,
            )
            .clicked()
        {
            *action = Some((
                ProjectKeysAction::LoadCredentials { after },
                PendingKind::Credentials,
            ));
        }
        if draft.credential_page.is_some()
            && popup::action_button(
                ui,
                &catalog.t("project_keys.credentials_first", &[]),
                popup::ActionTone::Ghost,
                true,
            )
            .clicked()
        {
            draft.credential_page = None;
            draft.credential_after = None;
        }
    });
    input(
        ui,
        &catalog.t("project_keys.account", &[]),
        &mut draft.account,
        None,
    );
    popup::field(
        ui,
        &catalog.t("project_keys.github_scope", &[]),
        None,
        |ui| {
            let labels = [
                catalog.t("project_keys.repository", &[]),
                catalog.t("project_keys.environment", &[]),
                catalog.t("project_keys.organization", &[]),
            ];
            popup::choice_input(
                ui,
                "github_scope",
                &labels[draft.scope_kind as usize],
                |ui| {
                    for (index, label) in labels.iter().enumerate() {
                        ui.selectable_value(&mut draft.scope_kind, index as u8, label);
                    }
                },
            );
        },
    );
    if draft.scope_kind < 2 {
        input(
            ui,
            &catalog.t("project_keys.owner", &[]),
            &mut draft.owner,
            None,
        );
        input(
            ui,
            &catalog.t("project_keys.repository", &[]),
            &mut draft.repository,
            None,
        );
        if draft.scope_kind == 1 {
            input(
                ui,
                &catalog.t("project_keys.environment", &[]),
                &mut draft.environment,
                None,
            );
        }
    } else {
        input(
            ui,
            &catalog.t("project_keys.organization", &[]),
            &mut draft.organization,
            None,
        );
        popup::field(ui, &catalog.t("project_keys.visibility", &[]), None, |ui| {
            let choices = [
                (GitHubVisibility::All, "project_keys.all"),
                (GitHubVisibility::Private, "project_keys.private"),
                (GitHubVisibility::Selected, "project_keys.selected"),
            ];
            let label = choices
                .iter()
                .find(|(value, _)| *value == draft.visibility)
                .unwrap()
                .1;
            popup::choice_input(ui, "github_visibility", &catalog.t(label, &[]), |ui| {
                for (value, label) in choices {
                    ui.selectable_value(&mut draft.visibility, value, catalog.t(label, &[]));
                }
            });
        });
        if draft.visibility == GitHubVisibility::Selected {
            input(
                ui,
                &catalog.t("project_keys.repository_ids", &[]),
                &mut draft.repositories,
                Some(&catalog.t("project_keys.repository_ids_hint", &[])),
            );
        }
    }
    input(
        ui,
        &catalog.t("project_keys.secret_name", &[]),
        &mut draft.secret_name,
        Some(&catalog.t("project_keys.secret_name_hint", &[])),
    );
    if draft.key.source == KeySource::GitHubOnly {
        draft.auto_send = false;
    }
    ui.add_enabled(
        draft.key.source != KeySource::GitHubOnly,
        egui::Checkbox::new(
            &mut draft.auto_send,
            catalog.t("project_keys.auto_send", &[]),
        ),
    );
    if draft
        .key
        .github
        .as_ref()
        .is_some_and(|remote| remote.state == GitHubRemoteState::ExternalChange)
    {
        popup::notice(
            ui,
            &catalog.t("project_keys.external_warning", &[]),
            popup::NoticeTone::Error,
        );
        ui.checkbox(
            &mut draft.allow_external,
            catalog.t("project_keys.external_confirm", &[]),
        );
    }
    ui.label(egui::RichText::new(catalog.t("project_keys.save_before_send", &[])).weak());
}

fn recipe_body(ui: &mut egui::Ui, draft: &mut RecipeDraft, catalog: &i18n::Catalog) {
    input(
        ui,
        &catalog.t("project_keys.recipe_name", &[]),
        &mut draft.recipe.name,
        None,
    );
    input(
        ui,
        &catalog.t("project_keys.program", &[]),
        &mut draft.program,
        None,
    );
    ui.label(catalog.t("project_keys.arguments_hint", &[]));
    let mut remove = None;
    for (index, argument) in draft.arguments.iter_mut().enumerate() {
        popup::field(
            ui,
            &catalog.t(
                "project_keys.argument",
                &[("index", &(index + 1).to_string())],
            ),
            None,
            |ui| {
                let mut rejected = false;
                let mut buffer = BoundedTextBuffer {
                    text: argument,
                    max_bytes: 8192,
                    rejected: &mut rejected,
                    normalize_nfc: false,
                };
                if buffer.text.contains('\n') {
                    ui.add_sized(
                        [ui.available_width(), 80.0],
                        egui::TextEdit::multiline(&mut buffer)
                            .desired_rows(1)
                            .font(egui::FontId::proportional(13.0))
                            .margin(egui::Margin::symmetric(10, 8)),
                    );
                } else {
                    popup::text_input(ui, &mut buffer, "");
                }
            },
        );
        if popup::action_button(
            ui,
            &catalog.t("project_keys.argument_remove", &[]),
            popup::ActionTone::Ghost,
            true,
        )
        .clicked()
        {
            remove = Some(index);
        }
    }
    if let Some(index) = remove {
        draft.arguments.remove(index);
    }
    if popup::action_button(
        ui,
        &catalog.t("project_keys.argument_add", &[]),
        popup::ActionTone::Secondary,
        draft.arguments.len() < 127,
    )
    .clicked()
    {
        draft.arguments.push(String::new());
    }
    input(
        ui,
        &catalog.t("project_keys.cwd", &[]),
        &mut draft.recipe.cwd,
        None,
    );
    ui.label(catalog.t(
        match draft.recipe.environment {
            KeyEnvironment::Development => "project_keys.development",
            KeyEnvironment::Production => "project_keys.production",
        },
        &[],
    ));
}

fn kind_label(kind: KeyKind, catalog: &i18n::Catalog) -> String {
    catalog.t(
        match kind {
            KeyKind::ApiToken => "project_keys.kind_token",
            KeyKind::ClientSecret => "project_keys.kind_client",
            KeyKind::PrivateKey => "project_keys.kind_private",
            KeyKind::Configuration => "project_keys.kind_config",
        },
        &[],
    )
}
fn delivery_label(delivery: KeyDelivery, catalog: &i18n::Catalog) -> String {
    catalog.t(
        match delivery {
            KeyDelivery::Environment => "project_keys.delivery_env",
            KeyDelivery::FilePath => "project_keys.delivery_path",
            KeyDelivery::TemporaryFile => "project_keys.delivery_temp",
        },
        &[],
    )
}
