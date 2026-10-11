//! Project-scoped key management. Rendering reads metadata and emits worker
//! intents; secrets exist only in an explicit, short-lived editor draft.
mod dialogs;
mod table;
#[cfg(test)]
mod tests;

use crate::project_keys::contracts::*;
use deppy_core::project_keys::*;
use dialogs::{Dialog, EditDraft};

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    General,
    Edit,
    SourcePick,
    Preview,
    Import,
    GitHub,
    Recipe,
    Delete,
    Conflict,
    Credentials,
    Sources,
}
struct Pending {
    fence: ProjectKeysFence,
    kind: PendingKind,
}
struct KeyHistory {
    key: ProjectKey,
    result: Option<Result<Vec<ProjectKeyChange>, ProjectKeysError>>,
}

#[derive(Default)]
pub(crate) struct ProjectKeysUi {
    scope: Option<ProjectKeysScope>,
    generation: u64,
    next_request: u64,
    selected: Option<String>,
    selected_recipe: Option<String>,
    query: String,
    dialog: Option<Dialog>,
    pending: Option<Pending>,
    build_pending: Option<ProjectKeysFence>,
    build_recipe: Option<String>,
    history_pending: Option<ProjectKeysFence>,
    history: Option<KeyHistory>,
    notice: Option<Result<ProjectKeysAckPayload, ProjectKeysError>>,
    prefill: Option<(super::environment::EnvironmentSelectionKind, SensitiveDraft)>,
}

impl ProjectKeysUi {
    pub(crate) fn reset(&mut self) {
        self.scope = None;
        self.generation = 0;
        self.selected = None;
        self.selected_recipe = None;
        self.query.clear();
        self.dialog = None;
        self.pending = None;
        self.notice = None;
        self.history_pending = None;
        self.history = None;
        self.prefill = None;
    }

    pub(crate) fn has_dialog(&self) -> bool {
        self.dialog.is_some()
    }

    pub(crate) fn prefill(
        &mut self,
        kind: super::environment::EnvironmentSelectionKind,
        value: SensitiveDraft,
    ) {
        self.prefill = Some((kind, value));
    }

    pub(crate) fn prepare(&mut self, snapshot: &ProjectKeysSnapshot) {
        if self.scope.as_ref() != Some(&snapshot.scope) || self.generation != snapshot.generation {
            let prefill = self.prefill.take();
            self.reset();
            self.scope = Some(snapshot.scope.clone());
            self.generation = snapshot.generation;
            self.prefill = prefill;
        }
        if let Some((kind, mut value)) = self.prefill.take() {
            let mut edit = EditDraft::new(new_key(&snapshot.scope));
            match kind {
                super::environment::EnvironmentSelectionKind::VariableName => {
                    edit.key.name = std::mem::take(value.text_mut())
                }
                super::environment::EnvironmentSelectionKind::VariableValue => edit.value = value,
                super::environment::EnvironmentSelectionKind::ApiName
                | super::environment::EnvironmentSelectionKind::ApiValue => return,
            }
            self.dialog = Some(Dialog::Edit(edit));
        }
        if self
            .selected
            .as_ref()
            .is_some_and(|id| !snapshot.keys.iter().any(|key| &key.id == id))
        {
            self.selected = None;
        }
    }

    fn request(
        &mut self,
        action: ProjectKeysAction,
        kind: PendingKind,
    ) -> Option<ProjectKeysIntent> {
        if self.pending.is_some() {
            return None;
        }
        let build = matches!(action, ProjectKeysAction::RunRecipe { .. });
        if build && self.build_pending.is_some() {
            return None;
        }
        let scope = self.scope.clone()?;
        self.next_request = self.next_request.checked_add(1)?;
        let fence = ProjectKeysFence {
            scope,
            generation: self.generation,
            request_id: self.next_request,
        };
        self.notice = None;
        if build {
            if let ProjectKeysAction::RunRecipe { recipe_id, .. } = &action {
                self.build_recipe = Some(recipe_id.clone());
            }
            self.build_pending = Some(fence.clone());
        } else {
            self.pending = Some(Pending {
                fence: fence.clone(),
                kind,
            });
        }
        Some(ProjectKeysIntent::Request { fence, action })
    }

    pub(crate) fn apply_ack(&mut self, ack: &ProjectKeysAck) {
        if self.history_pending.as_ref() == Some(&ack.fence) {
            self.history_pending = None;
            if self.scope.as_ref() == Some(&ack.fence.scope)
                && self.generation == ack.fence.generation
                && let Some(history) = &mut self.history
            {
                history.result = Some(match &ack.result {
                    Ok(ProjectKeysAckPayload::KeyChanges { key_id, changes })
                        if key_id == &history.key.id =>
                    {
                        Ok(changes
                            .iter()
                            .filter(|change| {
                                change.key_id == history.key.id
                                    && change.workspace_id == ack.fence.scope.workspace_id
                                    && change.environment == ack.fence.scope.environment
                                    && change.validate().is_ok()
                            })
                            .take(32)
                            .cloned()
                            .collect())
                    }
                    Err(error) => Err(*error),
                    _ => Err(ProjectKeysError::Storage),
                });
            }
            return;
        }
        if self.build_pending.as_ref() == Some(&ack.fence) {
            self.build_pending = None;
            self.build_recipe = None;
            if self.scope.as_ref() == Some(&ack.fence.scope)
                && self.generation == ack.fence.generation
            {
                self.notice = Some(ack.result.clone());
            }
            return;
        }
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        if pending.fence != ack.fence
            || self.scope.as_ref() != Some(&ack.fence.scope)
            || self.generation != ack.fence.generation
        {
            return;
        }
        let kind = pending.kind;
        self.pending = None;
        match &ack.result {
            Ok(ProjectKeysAckPayload::CredentialsPage { choices, after }) => {
                if let Some(Dialog::GitHub(draft)) = &mut self.dialog {
                    draft.set_credentials_page(choices, after);
                }
            }
            Ok(ProjectKeysAckPayload::SourcePicked(source)) => {
                if let Some(Dialog::Source(draft)) = &mut self.dialog {
                    draft.source = source.clone();
                    draft.preview = None;
                    draft.names.clear();
                }
            }
            Ok(ProjectKeysAckPayload::Preview(preview)) => {
                if let Some(Dialog::Source(draft)) = &mut self.dialog {
                    draft.preview = Some(preview.clone());
                    draft.names = preview.names.iter().cloned().collect();
                }
            }
            Ok(_) => {
                if matches!(
                    kind,
                    PendingKind::Edit
                        | PendingKind::Import
                        | PendingKind::GitHub
                        | PendingKind::Recipe
                        | PendingKind::Delete
                        | PendingKind::Conflict
                        | PendingKind::Sources
                ) {
                    self.dialog = None;
                }
            }
            Err(_) => {
                if let Some(Dialog::Edit(draft)) = &mut self.dialog {
                    draft.failed_submission();
                }
            }
        }
        self.notice = Some(ack.result.clone());
    }

    pub(crate) fn show(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &ProjectKeysSnapshot,
        catalog: &i18n::Catalog,
    ) -> Vec<ProjectKeysIntent> {
        self.prepare(snapshot);
        let mut intents = Vec::new();
        let modal = self.dialog.is_some();
        super::popup::set_pending_modal(ui.ctx(), modal);
        egui::ScrollArea::vertical()
            .id_salt("project_keys_page")
            .show(ui, |ui| {
                ui.add_enabled_ui(!modal, |ui| {
                    if self.toolbar(ui, snapshot, catalog, &mut intents) {
                        ui.label(catalog.t("project_keys.loading", &[]));
                        return;
                    }
                    self.status(ui, snapshot, catalog);
                    ui.add_space(10.0);
                    let wide = ui.available_width() >= 960.0;
                    if wide {
                        ui.horizontal_top(|ui| {
                            let table_width = (ui.available_width() - 316.0).max(240.0);
                            ui.allocate_ui_with_layout(
                                egui::vec2(table_width, 320.0),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    self.table(ui, snapshot, catalog, &mut intents);
                                },
                            );
                            ui.add_space(8.0);
                            ui.allocate_ui_with_layout(
                                egui::vec2(300.0, 320.0),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    self.detail(ui, snapshot, catalog, &mut intents);
                                },
                            );
                        });
                    } else {
                        self.table(ui, snapshot, catalog, &mut intents);
                        ui.add_space(12.0);
                        self.detail(ui, snapshot, catalog, &mut intents);
                    }
                    ui.add_space(12.0);
                    self.build(ui, snapshot, catalog, &mut intents);
                });
            });
        if let Some((action, kind)) = self.render_dialog(ui.ctx(), snapshot, catalog)
            && let Some(intent) = self.request(action, kind)
        {
            intents.push(intent);
        }
        intents
    }

    fn toolbar(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &ProjectKeysSnapshot,
        catalog: &i18n::Catalog,
        intents: &mut Vec<ProjectKeysIntent>,
    ) -> bool {
        ui.label(
            egui::RichText::new(catalog.t("project_keys.title", &[]))
                .size(19.0)
                .strong()
                .color(super::popup::palette(ui).text),
        );
        ui.label(
            egui::RichText::new(catalog.t("project_keys.subtitle", &[]))
                .size(12.0)
                .weak(),
        );
        ui.add_space(12.0);
        let mut scope = snapshot.scope.clone();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let selected = snapshot
                .projects
                .iter()
                .find(|p| p.id == scope.workspace_id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| scope.workspace_id.clone());
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width().min(240.0), 36.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    super::popup::choice_input(ui, "project_keys_project", &selected, |ui| {
                        for project in &snapshot.projects {
                            ui.selectable_value(
                                &mut scope.workspace_id,
                                project.id.clone(),
                                &project.name,
                            )
                            .on_hover_text(&project.path);
                        }
                    });
                },
            );
            for (environment, label) in [
                (KeyEnvironment::Development, "project_keys.development"),
                (KeyEnvironment::Production, "project_keys.production"),
            ] {
                if ui
                    .add_sized(
                        [100.0, 28.0],
                        egui::Button::new(catalog.t(label, &[]))
                            .selected(scope.environment == environment),
                    )
                    .clicked()
                {
                    scope.environment = environment;
                }
            }
        });
        if scope != snapshot.scope {
            self.reset();
            intents.push(ProjectKeysIntent::SelectScope(scope));
            return true;
        }
        ui.add_space(10.0);
        super::hairline(ui);
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            for source in &snapshot.settings.sources {
                let colors = super::popup::palette(ui);
                egui::Frame::new()
                    .fill(colors.input)
                    .stroke(egui::Stroke::new(1.0, colors.line))
                    .corner_radius(3)
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(table::source_label(source, catalog))
                                .size(11.0)
                                .color(colors.muted),
                        );
                    });
            }
            if super::popup::action_button(
                ui,
                &catalog.t("project_keys.sources", &[]),
                super::popup::ActionTone::Secondary,
                self.pending.is_none(),
            )
            .clicked()
            {
                self.dialog = Some(Dialog::Source(dialogs::SourceDraft::new(snapshot)));
            }
            let mut watch = snapshot.settings.watch_enabled;
            if ui
                .add_enabled(
                    self.pending.is_none(),
                    egui::Checkbox::new(&mut watch, catalog.t("project_keys.watch", &[])),
                )
                .changed()
                && let Some(intent) = self.request(
                    ProjectKeysAction::SetWatch {
                        enabled: watch,
                        expected_revision: snapshot.settings.revision,
                    },
                    PendingKind::General,
                )
            {
                intents.push(intent);
            }
            if super::popup::action_button(
                ui,
                &catalog.t("project_keys.refresh", &[]),
                super::popup::ActionTone::Secondary,
                self.pending.is_none(),
            )
            .clicked()
                && let Some(intent) =
                    self.request(self.refresh_action(snapshot), PendingKind::General)
            {
                intents.push(intent);
            }
        });
        ui.add_space(8.0);
        super::hairline(ui);
        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width().min(260.0), 36.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    let mut rejected = false;
                    let mut buffer = super::text_input::BoundedTextBuffer {
                        text: &mut self.query,
                        max_bytes: 256,
                        rejected: &mut rejected,
                        normalize_nfc: false,
                    };
                    super::popup::text_input(
                        ui,
                        &mut buffer,
                        &catalog.t("project_keys.search", &[]),
                    );
                },
            );
            if super::popup::action_button(
                ui,
                &catalog.t("project_keys.add", &[]),
                super::popup::ActionTone::Primary,
                self.pending.is_none(),
            )
            .clicked()
            {
                self.dialog = Some(Dialog::Edit(EditDraft::new(new_key(&snapshot.scope))));
            }
            if super::popup::action_button(
                ui,
                &catalog.t("project_keys.import", &[]),
                super::popup::ActionTone::Secondary,
                self.pending.is_none(),
            )
            .clicked()
            {
                self.dialog = Some(Dialog::Source(dialogs::SourceDraft::new(snapshot)));
            }
            if super::popup::action_button(
                ui,
                &catalog.t("project_keys.advanced", &[]),
                super::popup::ActionTone::Ghost,
                self.pending.is_none(),
            )
            .clicked()
                && let Some(intent) =
                    self.request(ProjectKeysAction::LegacyAdvanced, PendingKind::General)
            {
                intents.push(intent);
            }
        });
        false
    }

    fn refresh_action(&self, snapshot: &ProjectKeysSnapshot) -> ProjectKeysAction {
        let key_id = self
            .selected
            .as_ref()
            .and_then(|id| {
                snapshot
                    .keys
                    .iter()
                    .find(|key| &key.id == id && key.github.is_some())
            })
            .map(|key| key.id.clone());
        ProjectKeysAction::Refresh { key_id }
    }

    fn status(&self, ui: &mut egui::Ui, snapshot: &ProjectKeysSnapshot, catalog: &i18n::Catalog) {
        if self.pending.is_some() {
            super::popup::notice(
                ui,
                &catalog.t("project_keys.working", &[]),
                super::popup::NoticeTone::Info,
            );
        }
        if let Some(result) = &self.notice {
            let (text, tone) = match result {
                Ok(ProjectKeysAckPayload::Copied) => (
                    catalog.t("project_keys.copied", &[]),
                    super::popup::NoticeTone::Info,
                ),
                Ok(
                    ProjectKeysAckPayload::SourcePicked(_)
                    | ProjectKeysAckPayload::Preview(_)
                    | ProjectKeysAckPayload::CredentialsPage { .. }
                    | ProjectKeysAckPayload::KeyChanges { .. },
                ) => {
                    return;
                }
                Ok(_) => (
                    catalog.t("project_keys.done", &[]),
                    super::popup::NoticeTone::Info,
                ),
                Err(error) => (
                    error_label(*error, catalog),
                    super::popup::NoticeTone::Error,
                ),
            };
            super::popup::notice(ui, &text, tone);
        } else if let Some(error) = snapshot.error {
            super::popup::notice(
                ui,
                &error_label(error, catalog),
                super::popup::NoticeTone::Error,
            );
        }
    }

    fn table(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &ProjectKeysSnapshot,
        catalog: &i18n::Catalog,
        intents: &mut Vec<ProjectKeysIntent>,
    ) {
        ui.label(
            egui::RichText::new(catalog.t("project_keys.list", &[]))
                .size(13.0)
                .strong(),
        );
        table::header(ui, catalog);
        let query = self.query.to_lowercase();
        let keys = snapshot
            .keys
            .iter()
            .filter(|key| key.name.to_lowercase().contains(&query));
        let mut count = 0;
        egui::ScrollArea::vertical()
            .id_salt("project_keys_list")
            .max_height(360.0)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for key in keys {
                    count += 1;
                    let (_, action) = table::row(
                        ui,
                        key,
                        self.selected.as_deref() == Some(&key.id),
                        self.pending.is_none(),
                        catalog,
                    );
                    match action {
                        Some(table::RowAction::Select) => self.selected = Some(key.id.clone()),
                        Some(table::RowAction::Edit) => {
                            self.selected = Some(key.id.clone());
                            self.dialog = Some(Dialog::Edit(EditDraft::existing(key.clone())));
                        }
                        Some(table::RowAction::Copy) => {
                            if let Some(intent) = self.request(
                                ProjectKeysAction::CopyKey {
                                    key_id: key.id.clone(),
                                    expected_revision: key.revision,
                                },
                                PendingKind::General,
                            ) {
                                intents.push(intent);
                            }
                        }
                        None => {}
                    }
                }
            });
        if count == 0 {
            super::popup::notice(
                ui,
                &catalog.t("project_keys.empty", &[]),
                super::popup::NoticeTone::Info,
            );
        }
    }

    fn detail(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &ProjectKeysSnapshot,
        catalog: &i18n::Catalog,
        intents: &mut Vec<ProjectKeysIntent>,
    ) {
        let Some(key) = self
            .selected
            .as_ref()
            .and_then(|id| snapshot.keys.iter().find(|key| &key.id == id))
        else {
            ui.label(egui::RichText::new(catalog.t("project_keys.select_key", &[])).weak());
            return;
        };
        if self
            .history
            .as_ref()
            .is_none_or(|history| history.key != *key)
        {
            self.history = Some(KeyHistory {
                key: key.clone(),
                result: None,
            });
            self.next_request = self.next_request.saturating_add(1);
            let fence = ProjectKeysFence {
                scope: snapshot.scope.clone(),
                generation: snapshot.generation,
                request_id: self.next_request,
            };
            self.history_pending = Some(fence.clone());
            intents.push(ProjectKeysIntent::Request {
                fence,
                action: ProjectKeysAction::LoadKeyChanges {
                    key_id: key.id.clone(),
                    expected_revision: key.revision,
                },
            });
        }
        egui::Frame::group(ui.style())
            .inner_margin(12)
            .show(ui, |ui| {
                ui.label(egui::RichText::new(&key.name).size(15.0).strong());
                ui.add_space(8.0);
                detail_pair(
                    ui,
                    &catalog.t("project_keys.source", &[]),
                    &table::source_label(&key.source, catalog),
                );
                detail_pair(
                    ui,
                    &catalog.t("project_keys.usage", &[]),
                    &table::usage_label(key.usage, catalog),
                );
                detail_pair(
                    ui,
                    &catalog.t("project_keys.state_column", &[]),
                    &table::storage_label(key.storage_state, catalog),
                );
                detail_pair(
                    ui,
                    &catalog.t("project_keys.stored_revision", &[]),
                    &key.last_good_revision
                        .map(|v| format!("v{v}"))
                        .unwrap_or_else(|| "—".into()),
                );
                detail_pair(
                    ui,
                    &catalog.t("project_keys.saved_column", &[]),
                    &key.last_good_at
                        .map(table::saved_time)
                        .unwrap_or_else(|| "—".into()),
                );
                detail_pair(
                    ui,
                    &catalog.t("project_keys.last_change", &[]),
                    &table::saved_time(key.updated_at),
                );
                ui.add_space(8.0);
                if key.error_code.is_some() {
                    super::popup::notice(
                        ui,
                        &catalog.t("project_keys.source_repair", &[]),
                        super::popup::NoticeTone::Error,
                    );
                }
                ui.horizontal_wrapped(|ui| {
                    if super::popup::action_button(
                        ui,
                        &catalog.t("project_keys.edit", &[]),
                        super::popup::ActionTone::Primary,
                        self.pending.is_none(),
                    )
                    .clicked()
                    {
                        self.dialog = Some(Dialog::Edit(EditDraft::existing(key.clone())));
                    }
                    if super::popup::action_button(
                        ui,
                        &catalog.t("action.copy", &[]),
                        super::popup::ActionTone::Secondary,
                        self.pending.is_none() && key.source != KeySource::GitHubOnly,
                    )
                    .clicked()
                        && let Some(intent) = self.request(
                            ProjectKeysAction::CopyKey {
                                key_id: key.id.clone(),
                                expected_revision: key.revision,
                            },
                            PendingKind::General,
                        )
                    {
                        intents.push(intent);
                    }
                });
                if matches!(key.storage_state, KeyStorageState::Conflict) {
                    if super::popup::action_button(
                        ui,
                        &catalog.t("project_keys.resolve", &[]),
                        super::popup::ActionTone::Primary,
                        self.pending.is_none(),
                    )
                    .clicked()
                    {
                        self.dialog = Some(Dialog::Conflict(key.clone()));
                    }
                } else if matches!(
                    key.storage_state,
                    KeyStorageState::MissingSource
                        | KeyStorageState::EmptySource
                        | KeyStorageState::Failed
                        | KeyStorageState::UpdateNeeded
                        | KeyStorageState::Locked
                ) && super::popup::action_button(
                    ui,
                    &catalog.t("project_keys.retry", &[]),
                    super::popup::ActionTone::Secondary,
                    self.pending.is_none(),
                )
                .clicked()
                    && let Some(intent) =
                        self.request(self.refresh_action(snapshot), PendingKind::General)
                {
                    intents.push(intent);
                }
                super::hairline(ui);
                ui.label(egui::RichText::new(catalog.t("project_keys.github", &[])).strong());
                if let Some(remote) = &key.github {
                    detail_pair(
                        ui,
                        &catalog.t("project_keys.remote_status", &[]),
                        &table::remote_label(remote.state, catalog),
                    );
                    detail_pair(
                        ui,
                        &catalog.t("project_keys.account", &[]),
                        &remote.target.account_identity,
                    );
                    detail_pair(
                        ui,
                        &catalog.t("project_keys.destination", &[]),
                        &github_destination(&remote.target.scope, catalog),
                    );
                    detail_pair(
                        ui,
                        &catalog.t("project_keys.secret_name", &[]),
                        &remote.target.secret_name,
                    );
                    detail_pair(
                        ui,
                        &catalog.t("project_keys.sent_revision", &[]),
                        &remote
                            .last_sent_revision
                            .map(|v| format!("v{v}"))
                            .unwrap_or_else(|| "—".into()),
                    );
                    detail_pair(
                        ui,
                        &catalog.t("project_keys.remote_updated", &[]),
                        remote.remote_updated_at.as_deref().unwrap_or("—"),
                    );
                    detail_pair(
                        ui,
                        &catalog.t("project_keys.auto_send", &[]),
                        &catalog.t(
                            if remote.target.auto_send {
                                "project_keys.enabled"
                            } else {
                                "project_keys.disabled"
                            },
                            &[],
                        ),
                    );
                } else {
                    ui.label(
                        egui::RichText::new(table::remote_label(
                            GitHubRemoteState::Unlinked,
                            catalog,
                        ))
                        .weak(),
                    );
                }
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if key.github.is_some()
                        && super::popup::action_button(
                            ui,
                            &catalog.t("project_keys.send", &[]),
                            super::popup::ActionTone::Secondary,
                            self.pending.is_none() && key.source != KeySource::GitHubOnly,
                        )
                        .clicked()
                    {
                        self.dialog =
                            Some(Dialog::GitHub(dialogs::GitHubDraft::existing(key.clone())));
                    }
                    if super::popup::action_button(
                        ui,
                        &catalog.t("project_keys.github_configure", &[]),
                        super::popup::ActionTone::Secondary,
                        self.pending.is_none(),
                    )
                    .clicked()
                    {
                        self.dialog =
                            Some(Dialog::GitHub(dialogs::GitHubDraft::existing(key.clone())));
                    }
                });
                super::hairline(ui);
                ui.label(egui::RichText::new(catalog.t("project_keys.history", &[])).strong());
                if let Some(history) = &self.history {
                    match &history.result {
                        None => {
                            ui.label(catalog.t("project_keys.history_loading", &[]));
                        }
                        Some(Ok(changes)) if changes.is_empty() => {
                            ui.label(catalog.t("project_keys.history_empty", &[]));
                        }
                        Some(Ok(changes)) => {
                            for change in changes.iter().take(4) {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} · v{}",
                                        table::change_label(change.kind, catalog),
                                        change.revision
                                    ))
                                    .size(11.0),
                                );
                                ui.label(
                                    egui::RichText::new(table::saved_time(change.occurred_at))
                                        .size(10.0)
                                        .weak(),
                                );
                            }
                        }
                        Some(Err(error)) => {
                            ui.label(error_label(*error, catalog));
                            if super::popup::action_button(
                                ui,
                                &catalog.t("project_keys.history_retry", &[]),
                                super::popup::ActionTone::Secondary,
                                self.history_pending.is_none(),
                            )
                            .clicked()
                            {
                                self.history = None;
                            }
                        }
                    }
                }
                ui.add_space(8.0);
                if super::popup::action_button(
                    ui,
                    &catalog.t("action.delete", &[]),
                    super::popup::ActionTone::Danger,
                    self.pending.is_none(),
                )
                .clicked()
                {
                    self.dialog = Some(Dialog::Delete(key.clone()));
                }
            });
    }

    fn build(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &ProjectKeysSnapshot,
        catalog: &i18n::Catalog,
        intents: &mut Vec<ProjectKeysIntent>,
    ) {
        super::hairline(ui);
        ui.label(
            egui::RichText::new(catalog.t("project_keys.build", &[]))
                .size(14.0)
                .strong(),
        );
        ui.label(
            egui::RichText::new(catalog.t("project_keys.build_hint", &[]))
                .size(11.0)
                .weak(),
        );
        for recipe in &snapshot.recipes {
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(
                    &mut self.selected_recipe,
                    Some(recipe.id.clone()),
                    &recipe.name,
                );
                if super::popup::action_button(
                    ui,
                    &catalog.t("project_keys.copy_command", &[]),
                    super::popup::ActionTone::Secondary,
                    true,
                )
                .clicked()
                {
                    ui.ctx()
                        .copy_text(build_command(&snapshot.scope, Some(&recipe.id)));
                }
                if super::popup::action_button(
                    ui,
                    &catalog.t("project_keys.edit", &[]),
                    super::popup::ActionTone::Secondary,
                    self.pending.is_none(),
                )
                .clicked()
                {
                    self.dialog = Some(Dialog::Recipe(dialogs::RecipeDraft::existing(
                        recipe.clone(),
                    )));
                }
                if super::popup::action_button(
                    ui,
                    &catalog.t("action.run", &[]),
                    super::popup::ActionTone::Primary,
                    self.pending.is_none() && self.build_pending.is_none(),
                )
                .clicked()
                    && let Some(intent) = self.request(
                        ProjectKeysAction::RunRecipe {
                            recipe_id: recipe.id.clone(),
                            expected_revision: recipe.revision,
                        },
                        PendingKind::General,
                    )
                {
                    intents.push(intent);
                }
            });
            ui.label(
                egui::RichText::new(build_command(&snapshot.scope, Some(&recipe.id)))
                    .monospace()
                    .size(11.0),
            );
            if self
                .build_pending
                .as_ref()
                .is_some_and(|fence| fence.scope == snapshot.scope)
                && self.build_recipe.as_deref() == Some(&recipe.id)
            {
                ui.label(catalog.t("project_keys.build_running", &[]));
            }
        }
        if super::popup::action_button(
            ui,
            &catalog.t("project_keys.recipe_add", &[]),
            super::popup::ActionTone::Secondary,
            self.pending.is_none(),
        )
        .clicked()
        {
            let cwd = snapshot
                .projects
                .iter()
                .find(|p| p.id == snapshot.scope.workspace_id)
                .map(|p| p.path.clone())
                .unwrap_or_default();
            self.dialog = Some(Dialog::Recipe(dialogs::RecipeDraft::new(
                &snapshot.scope,
                cwd,
            )));
        }
        ui.collapsing(catalog.t("project_keys.cli_command", &[]), |ui| {
            let command = build_command(&snapshot.scope, None);
            ui.label(egui::RichText::new(&command).monospace().size(11.0));
            if super::popup::action_button(
                ui,
                &catalog.t("project_keys.copy_command", &[]),
                super::popup::ActionTone::Secondary,
                true,
            )
            .clicked()
            {
                ui.ctx().copy_text(command);
            }
        });
        for receipt in snapshot.receipts.iter().take(5) {
            let status = match receipt.exit_code {
                Some(0) => catalog.t("project_keys.build_success", &[]),
                Some(code) => catalog.t("project_keys.build_exit", &[("code", &code.to_string())]),
                None => catalog.t("project_keys.build_running", &[]),
            };
            ui.collapsing(
                format!("{status} · {}", table::saved_time(receipt.started_at)),
                |ui| {
                    for used in &receipt.used_keys {
                        ui.label(format!("{} · v{}", used.name, used.revision));
                    }
                    if receipt.used_keys.is_empty() {
                        ui.label(catalog.t("project_keys.build_no_keys", &[]));
                    }
                },
            );
        }
    }
}

fn detail_pair(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.add_sized(
            [88.0, 16.0],
            egui::Label::new(egui::RichText::new(label).size(11.0).weak()).wrap(),
        );
        ui.add(egui::Label::new(egui::RichText::new(value).size(11.0)).wrap());
    });
}

fn github_destination(scope: &GitHubScope, catalog: &i18n::Catalog) -> String {
    match scope {
        GitHubScope::Repository { owner, repository } => format!("{owner}/{repository}"),
        GitHubScope::Environment {
            owner,
            repository,
            environment,
        } => format!("{owner}/{repository} · {environment}"),
        GitHubScope::Organization {
            organization,
            visibility,
            selected_repository_ids,
        } => {
            let access = catalog.t(
                match visibility {
                    GitHubVisibility::All => "project_keys.all",
                    GitHubVisibility::Private => "project_keys.private",
                    GitHubVisibility::Selected => "project_keys.selected",
                },
                &[],
            );
            let ids = selected_repository_ids
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{organization} · {access}{}",
                if ids.is_empty() {
                    String::new()
                } else {
                    format!(" · {ids}")
                }
            )
        }
    }
}

fn build_command(scope: &ProjectKeysScope, recipe_id: Option<&str>) -> String {
    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
    let environment = match scope.environment {
        KeyEnvironment::Development => "dev",
        KeyEnvironment::Production => "prod",
    };
    let base = format!(
        "deppy-sijo env run --workspace {} --environment {environment}",
        quote(&scope.workspace_id)
    );
    match recipe_id {
        Some(id) => format!("{base} --recipe {}", quote(id)),
        None => format!("{base} -- <program> <arguments>"),
    }
}

fn new_key(scope: &ProjectKeysScope) -> ProjectKey {
    ProjectKey {
        id: uuid::Uuid::new_v4().to_string(),
        workspace_id: scope.workspace_id.clone(),
        environment: scope.environment,
        name: String::new(),
        credential_id: None,
        source_baseline_credential_id: None,
        source: KeySource::Vault,
        kind: KeyKind::ApiToken,
        usage: KeyUsage::Build,
        delivery: KeyDelivery::Environment,
        source_priority: 0,
        required: false,
        revision: 1,
        last_good_revision: None,
        last_good_at: None,
        storage_state: KeyStorageState::NotStored,
        created_at: 0,
        updated_at: 0,
        source_checked_at: None,
        error_code: None,
        github: None,
        legacy_reference: None,
    }
}

fn error_label(error: ProjectKeysError, catalog: &i18n::Catalog) -> String {
    catalog.t(
        match error {
            ProjectKeysError::Locked => "project_keys.error_locked",
            ProjectKeysError::Stale | ProjectKeysError::ScopeChanged => "project_keys.error_stale",
            ProjectKeysError::Busy => "project_keys.error_busy",
            ProjectKeysError::Conflict | ProjectKeysError::SourceChanged => {
                "project_keys.error_conflict"
            }
            ProjectKeysError::SourceMissing
            | ProjectKeysError::SourceEmpty
            | ProjectKeysError::SourceUnsafe => "project_keys.error_source",
            ProjectKeysError::Clipboard => "project_keys.error_clipboard",
            ProjectKeysError::GitHubAuth => "project_keys.error_auth",
            ProjectKeysError::GitHubRemote | ProjectKeysError::ExternalChange => {
                "project_keys.error_github"
            }
            ProjectKeysError::Process => "project_keys.error_build",
            ProjectKeysError::InvalidInput => "project_keys.error_input",
            ProjectKeysError::Storage => "project_keys.error_storage",
        },
        &[],
    )
}
