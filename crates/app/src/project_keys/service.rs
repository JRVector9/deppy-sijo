use super::{contracts::*, sources};
use deppy_core::project_keys::*;
use secret::{
    LogicalCredentialId, PhysicalSecretSlot, SecretBundle, SecretBundleRef, SecretBundleStagePlan,
    SecretStore, SecretString,
};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use storage::{
    CredentialMeta, Db, ProjectKeyFileJournal, ProjectKeyFileJournalPhase, ProjectKeyValueCommit,
};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
pub fn storage_error<T>(result: anyhow::Result<T>) -> Result<T, ProjectKeysError> {
    result.map_err(|_| ProjectKeysError::Storage)
}
pub fn committed<T>(result: ProjectKeyCas<T>) -> Result<T, ProjectKeysError> {
    match result {
        ProjectKeyCas::Committed(value) => Ok(value),
        ProjectKeyCas::Stale { .. } => Err(ProjectKeysError::Stale),
    }
}

pub fn read_project_key_bundle(
    db: &Db,
    secrets: &dyn SecretStore,
    scope: &ProjectKeysScope,
    key: &ProjectKey,
) -> Result<SecretBundle, ProjectKeysError> {
    if key.workspace_id != scope.workspace_id || key.environment != scope.environment {
        return Err(ProjectKeysError::InvalidInput);
    }
    let id = key
        .credential_id
        .as_deref()
        .ok_or(ProjectKeysError::Locked)?;
    let (_, location) = storage_error(db.project_key_value_location(
        &scope.workspace_id,
        scope.environment,
        &key.id,
        key.revision,
    ))?
    .ok_or(ProjectKeysError::Stale)?;
    let value = if let Ok(slot) = PhysicalSecretSlot::parse(location.keyring_username.clone()) {
        if !slot.belongs_to(&LogicalCredentialId::new(id).map_err(|_| ProjectKeysError::Locked)?) {
            return Err(ProjectKeysError::Locked);
        }
        secret::read_secret_bundle(secrets, &slot).map_err(|_| ProjectKeysError::Locked)?
    } else {
        SecretBundle::new(
            secrets
                .get_secret(&location.keyring_username)
                .map_err(|_| ProjectKeysError::Locked)?,
            None,
            None,
        )
    };
    if value.access().expose().len() > SECRET_INPUT_BYTES_MAX {
        return Err(ProjectKeysError::Locked);
    }
    Ok(value)
}

pub type ServiceFactory =
    Arc<dyn Fn() -> Result<ProjectKeysService, ProjectKeysError> + Send + Sync>;

struct StagedPublication {
    key: ProjectKey,
    credential_id: String,
    previous: Option<PhysicalSecretSlot>,
    plan: SecretBundleStagePlan,
    commit: ProjectKeyValueCommit,
}

pub struct ProjectKeysService {
    pub db: Db,
    pub secrets: Arc<dyn SecretStore>,
}
impl ProjectKeysService {
    pub fn root(&self, scope: &ProjectKeysScope) -> Result<PathBuf, ProjectKeysError> {
        Ok(self.root_handle(scope)?.path().to_path_buf())
    }
    pub fn root_handle(
        &self,
        scope: &ProjectKeysScope,
    ) -> Result<sources::AnchoredRoot, ProjectKeysError> {
        let (path, anchor, volume) =
            storage_error(self.db.project_key_workspace_identity(&scope.workspace_id))?
                .ok_or(ProjectKeysError::InvalidInput)?;
        if path.is_empty() {
            return Err(ProjectKeysError::SourceMissing);
        }
        let root = sources::AnchoredRoot::open(std::path::Path::new(&path))?;
        let current = root.identity();
        if anchor.is_some_and(|anchor| anchor != current.anchor)
            || volume.is_some_and(|volume| Some(volume) != current.volume)
        {
            return Err(ProjectKeysError::SourceChanged);
        }
        if anchor.is_none()
            && !storage_error(self.db.project_key_enroll_workspace_identity(
                &scope.workspace_id,
                &path,
                current.anchor,
                current.volume,
            ))?
        {
            return Err(ProjectKeysError::SourceChanged);
        }
        Ok(root)
    }
    fn read_source(
        &self,
        scope: &ProjectKeysScope,
        source: &KeySource,
    ) -> Result<std::collections::BTreeMap<String, SecretString>, ProjectKeysError> {
        if storage_error(self.db.project_key_file_journal_source_pending(
            &scope.workspace_id,
            scope.environment,
            source,
        ))? {
            return Err(ProjectKeysError::SourceChanged);
        }
        sources::read_source_anchored(&self.root_handle(scope)?, source)
    }
    pub fn snapshot(
        &self,
        scope: &ProjectKeysScope,
        generation: u64,
    ) -> Result<ProjectKeysSnapshot, ProjectKeysError> {
        let projects = storage_error(self.db.settings_workspace_projection_rows())?
            .into_iter()
            .map(|row| ProjectChoice {
                id: row.id,
                name: row.name,
                path: row.path,
            })
            .collect();
        let auth_credentials = storage_error(self.db.project_key_auth_credentials_page(
            &scope.workspace_id,
            scope.environment,
            128,
            None,
        ))?
        .into_iter()
        .map(|row| CredentialChoice {
            id: row.id,
            label: row.label,
            provider: row.provider,
        })
        .collect();
        Ok(ProjectKeysSnapshot {
            scope: scope.clone(),
            generation,
            keys: storage_error(
                self.db
                    .project_key_list(&scope.workspace_id, scope.environment),
            )?,
            api_access: storage_error(
                self.db
                    .project_api_access_list(&scope.workspace_id, scope.environment),
            )?,
            settings: storage_error(
                self.db
                    .project_key_settings(&scope.workspace_id, scope.environment),
            )?,
            recipes: storage_error(
                self.db
                    .project_key_recipe_list(&scope.workspace_id, scope.environment),
            )?,
            receipts: storage_error(self.db.project_key_receipt_list(
                &scope.workspace_id,
                scope.environment,
                PROJECT_KEY_RECEIPTS_MAX,
            ))?,
            projects,
            auth_credentials,
            source_preview: None,
            error: None,
        })
    }
    pub fn key(
        &self,
        scope: &ProjectKeysScope,
        id: &str,
        revision: u64,
    ) -> Result<ProjectKey, ProjectKeysError> {
        let key = storage_error(self.db.project_key_get(
            &scope.workspace_id,
            scope.environment,
            id,
        ))?
        .ok_or(ProjectKeysError::Stale)?;
        if key.revision != revision {
            return Err(ProjectKeysError::Stale);
        }
        Ok(key)
    }
    pub fn bundle(
        &self,
        scope: &ProjectKeysScope,
        key: &ProjectKey,
    ) -> Result<SecretBundle, ProjectKeysError> {
        read_project_key_bundle(&self.db, self.secrets.as_ref(), scope, key)
    }
    /// Staging happens before a single SQLite pointer+revision publication. A failed commit is
    /// indeterminate and remains in the existing durable reconciliation ledger.
    pub fn publish(
        &self,
        scope: &ProjectKeysScope,
        key: ProjectKey,
        expected: Option<u64>,
        value: &SecretString,
        baseline: Option<&SecretString>,
    ) -> Result<ProjectKey, ProjectKeysError> {
        let staged = self.stage_publication(scope, key, expected, value, baseline)?;
        self.commit_publication(scope, staged, expected, None)
    }
    fn stage_publication(
        &self,
        scope: &ProjectKeysScope,
        mut key: ProjectKey,
        expected: Option<u64>,
        value: &SecretString,
        baseline: Option<&SecretString>,
    ) -> Result<StagedPublication, ProjectKeysError> {
        if key.workspace_id != scope.workspace_id
            || key.environment != scope.environment
            || value.expose().is_empty()
            || value.expose().len() > SECRET_INPUT_BYTES_MAX
        {
            return Err(ProjectKeysError::InvalidInput);
        }
        if let Some(revision) = expected {
            self.key(scope, &key.id, revision)?;
        } else if storage_error(self.db.project_key_get(
            &scope.workspace_id,
            scope.environment,
            &key.id,
        ))?
        .is_some()
        {
            return Err(ProjectKeysError::Stale);
        }
        // File imports never repurpose a legacy/global or OAuth bundle's refresh token.
        let candidate = if expected.is_some() {
            key.credential_id
                .as_deref()
                .map(|id| storage_error(self.db.project_key_credential(&scope.workspace_id, id)))
                .transpose()?
                .flatten()
        } else {
            None
        };
        let existing = candidate.filter(|(meta, _)| {
            meta.provider == "project-key"
                && meta.workspace_id.as_deref() == Some(scope.workspace_id.as_str())
        });
        let credential_id = existing
            .as_ref()
            .map(|(meta, _)| meta.id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let previous_pointer = existing
            .as_ref()
            .map(|(_, location)| location.keyring_username.clone());
        let previous = previous_pointer
            .as_ref()
            .map(|pointer| PhysicalSecretSlot::parse(pointer.clone()))
            .transpose()
            .map_err(|_| ProjectKeysError::Locked)?;
        let logical = LogicalCredentialId::new(credential_id.clone())
            .map_err(|_| ProjectKeysError::InvalidInput)?;
        let plan = SecretBundleStagePlan::allocate(logical, previous.clone())
            .map_err(|_| ProjectKeysError::InvalidInput)?;
        storage_error(
            self.db
                .register_physical_secret_slot_staging(&credential_id, plan.new_slot().as_str()),
        )?;
        secret::stage_secret_bundle(
            self.secrets.as_ref(),
            &plan,
            SecretBundleRef::new(value, baseline, None),
        )
        .map_err(|_| ProjectKeysError::Locked)?;
        key.credential_id = Some(credential_id.clone());
        key.source_baseline_credential_id = baseline.map(|_| credential_id.clone());
        key.storage_state = KeyStorageState::NotStored;
        key.updated_at = now();
        // A protected Vault publication is also a verified source. Keep that ownership evidence
        // when an explicit legacy edit later rebinds the row to another credential.
        key.source_checked_at = Some(now());
        let meta = existing.is_none().then(|| CredentialMeta {
            id: credential_id.clone(),
            provider: "project-key".into(),
            label: key.name.clone(),
            credential_kind: "api_key".into(),
            masked_hint: None,
            workspace_id: Some(scope.workspace_id.clone()),
        });
        let commit = ProjectKeyValueCommit {
            credential_meta: meta,
            expected_previous_pointer: previous_pointer,
            physical_slot: plan.new_slot().as_str().into(),
            oauth_json: None,
            masked_hint: None,
        };
        Ok(StagedPublication {
            key,
            credential_id,
            previous,
            plan,
            commit,
        })
    }
    fn discard_publication(&self, staged: &StagedPublication) {
        if secret::delete_secret_bundle(self.secrets.as_ref(), staged.plan.new_slot()).is_ok() {
            let _ = self.db.acknowledge_physical_secret_slot_deleted(
                &staged.credential_id,
                staged.plan.new_slot().as_str(),
            );
        }
    }
    fn commit_publication(
        &self,
        scope: &ProjectKeysScope,
        staged: StagedPublication,
        expected: Option<u64>,
        journal: Option<&str>,
    ) -> Result<ProjectKey, ProjectKeysError> {
        let result = if let Some(journal) = journal {
            self.db.project_key_commit_value_with_file_journal(
                &scope.workspace_id,
                scope.environment,
                &staged.key,
                expected,
                &staged.commit,
                journal,
            )
        } else {
            self.db.project_key_commit_value(
                &scope.workspace_id,
                scope.environment,
                &staged.key,
                expected,
                &staged.commit,
            )
        };
        match storage_error(result)? {
            ProjectKeyCas::Committed(saved) => {
                if let Some(previous) = staged.previous
                    && secret::delete_secret_bundle(self.secrets.as_ref(), &previous).is_ok()
                {
                    let _ = self.db.acknowledge_physical_secret_slot_deleted(
                        &staged.credential_id,
                        previous.as_str(),
                    );
                }
                Ok(saved)
            }
            ProjectKeyCas::Stale { .. } => {
                self.discard_publication(&staged);
                Err(ProjectKeysError::Stale)
            }
        }
    }
    fn finish_file_journal(
        &self,
        journal: &ProjectKeyFileJournal,
        phase: ProjectKeyFileJournalPhase,
    ) -> Result<(), ProjectKeysError> {
        if !storage_error(self.db.project_key_file_journal_finish(
            &journal.workspace_id,
            journal.environment,
            &journal.id,
            phase,
        ))? {
            return Err(ProjectKeysError::Stale);
        }
        let slot = PhysicalSecretSlot::parse(journal.protected_slot.clone())
            .map_err(|_| ProjectKeysError::Storage)?;
        if secret::delete_secret_bundle(self.secrets.as_ref(), &slot).is_ok() {
            let _ = self
                .db
                .acknowledge_physical_secret_slot_deleted(&journal.id, slot.as_str());
        }
        Ok(())
    }
    fn publish_file_edit(
        &self,
        scope: &ProjectKeysScope,
        key: ProjectKey,
        expected: Option<u64>,
        value: &SecretString,
        before: Option<&SecretString>,
    ) -> Result<ProjectKey, ProjectKeysError> {
        let root = self.root_handle(scope)?;
        let mut file = sources::prepare_source_write(&root, &key.source, &key.name, value, before)?;
        let staged = self.stage_publication(scope, key, expected, value, Some(value))?;
        let id = uuid::Uuid::new_v4().to_string();
        let plan = SecretBundleStagePlan::allocate(
            LogicalCredentialId::new(&id).map_err(|_| ProjectKeysError::Storage)?,
            None,
        )
        .map_err(|_| ProjectKeysError::Storage)?;
        if let Err(error) = storage_error(
            self.db
                .register_physical_secret_slot_staging(&id, plan.new_slot().as_str()),
        ) {
            self.discard_publication(&staged);
            return Err(error);
        }
        let recovery_identity = file.recovery_identity();
        if secret::stage_secret_bundle(
            self.secrets.as_ref(),
            &plan,
            SecretBundleRef::new(
                file.original(),
                Some(file.replacement()),
                Some(&recovery_identity),
            ),
        )
        .is_err()
        {
            self.discard_publication(&staged);
            return Err(ProjectKeysError::Locked);
        }
        let journal = match storage_error(self.db.project_key_file_journal_begin(
            &scope.workspace_id,
            scope.environment,
            &staged.key,
            expected,
            &id,
            plan.new_slot().as_str(),
            now(),
        ))
        .and_then(committed)
        {
            Ok(journal) => journal,
            Err(error) => {
                self.discard_publication(&staged);
                if secret::delete_secret_bundle(self.secrets.as_ref(), plan.new_slot()).is_ok() {
                    let _ = self
                        .db
                        .acknowledge_physical_secret_slot_deleted(&id, plan.new_slot().as_str());
                }
                return Err(error);
            }
        };
        if let Err(error) = file.apply() {
            self.discard_publication(&staged);
            if file.rollback_if_unchanged().is_ok() {
                let _ = self.finish_file_journal(&journal, ProjectKeyFileJournalPhase::Prepared);
            }
            return Err(error);
        }
        match storage_error(self.db.project_key_file_journal_mark_applied(
            &scope.workspace_id,
            scope.environment,
            &id,
        )) {
            Ok(true) => {}
            result => {
                self.discard_publication(&staged);
                // No value commit has been attempted, so even an indeterminate phase write is
                // safe to undo while the file still matches this operation's replacement.
                if file.rollback_if_unchanged().is_ok()
                    && let Ok(Some(current)) = storage_error(self.db.project_key_file_journal_get(
                        &scope.workspace_id,
                        scope.environment,
                        &id,
                    ))
                {
                    let _ = self.finish_file_journal(&journal, current.phase);
                }
                return Err(result.err().unwrap_or(ProjectKeysError::Stale));
            }
        }
        let result = self.commit_publication(scope, staged, expected, Some(&id));
        match result {
            Ok(saved) => {
                file.finish();
                // Publication is already durable; cleanup failure must never roll back a saved value.
                let _ = self
                    .finish_file_journal(&journal, ProjectKeyFileJournalPhase::PublicationComplete);
                Ok(saved)
            }
            Err(error) => {
                // A storage error may be an indeterminate completed commit. Read its durable phase
                // before rollback; an unavailable DB leaves the journal for safe later recovery.
                let phase = storage_error(self.db.project_key_file_journal_get(
                    &scope.workspace_id,
                    scope.environment,
                    &id,
                ))
                .ok()
                .flatten()
                .map(|row| row.phase);
                if phase == Some(ProjectKeyFileJournalPhase::FileApplied)
                    && file.rollback_if_unchanged().is_ok()
                {
                    let _ =
                        self.finish_file_journal(&journal, ProjectKeyFileJournalPhase::FileApplied);
                }
                Err(error)
            }
        }
    }
    pub fn recover_file_journals(
        &self,
        cursor: &mut Option<String>,
    ) -> Result<(), ProjectKeysError> {
        let mut rows = storage_error(self.db.project_key_file_journals_page(1, cursor.as_deref()))?;
        if rows.is_empty() && cursor.is_some() {
            *cursor = None;
            rows = storage_error(self.db.project_key_file_journals_page(1, None))?;
        }
        for journal in rows {
            *cursor = Some(journal.id.clone());
            if journal.phase == ProjectKeyFileJournalPhase::PublicationComplete {
                self.finish_file_journal(&journal, journal.phase)?;
                continue;
            }
            let Some(owner) = journal.owner else {
                continue;
            };
            if owner.pid == 0 || owner.pid > i32::MAX as u32 {
                return Err(ProjectKeysError::Storage);
            }
            let dead = match deppy_core::process_identity::ProcessIdentity::capture(owner.pid) {
                Some(current) => current != owner,
                None => {
                    // SAFETY: signal zero only probes this bounded positive kernel PID.
                    (unsafe { libc::kill(owner.pid as i32, 0) }) == -1
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                }
            };
            if !dead {
                continue;
            }
            let scope = ProjectKeysScope {
                workspace_id: journal.workspace_id.clone(),
                environment: journal.environment,
            };
            let root = self.root_handle(&scope)?;
            let slot = PhysicalSecretSlot::parse(journal.protected_slot.clone())
                .map_err(|_| ProjectKeysError::Storage)?;
            if !slot.belongs_to(
                &LogicalCredentialId::new(&journal.id).map_err(|_| ProjectKeysError::Storage)?,
            ) {
                return Err(ProjectKeysError::Storage);
            }
            let bundle = secret::read_secret_bundle(self.secrets.as_ref(), &slot)
                .map_err(|_| ProjectKeysError::Locked)?;
            let replacement = bundle.refresh().ok_or(ProjectKeysError::Storage)?;
            let identity = bundle.dcr().ok_or(ProjectKeysError::SourceChanged)?;
            sources::recover_source_write_with_identity(
                &root,
                &journal.source,
                bundle.access(),
                replacement,
                identity,
            )?;
            self.finish_file_journal(&journal, journal.phase)?;
        }
        Ok(())
    }
    pub fn save_key(
        &self,
        scope: &ProjectKeysScope,
        mut key: ProjectKey,
        expected: Option<u64>,
        edit: ValueEdit,
    ) -> Result<ProjectKey, ProjectKeysError> {
        if key.workspace_id != scope.workspace_id || key.environment != scope.environment {
            return Err(ProjectKeysError::InvalidInput);
        }
        let current = expected
            .map(|revision| self.key(scope, &key.id, revision))
            .transpose()?;
        if key.source == KeySource::GitHubOnly {
            if matches!(&edit,ValueEdit::Replace(draft) if !draft.is_empty())
                || current
                    .as_ref()
                    .is_some_and(|current| current.source != KeySource::GitHubOnly)
            {
                return Err(ProjectKeysError::InvalidInput);
            }
            key.credential_id = None;
            key.source_baseline_credential_id = None;
            key.last_good_revision = None;
            key.last_good_at = None;
            key.usage = KeyUsage::StoreOnly;
            key.required = false;
            key.delivery = KeyDelivery::Environment;
            key.storage_state = KeyStorageState::NotStored;
            return committed(storage_error(self.db.project_key_save(
                &scope.workspace_id,
                scope.environment,
                &key,
                expected,
            ))?);
        }
        if key.source == KeySource::Vault
            && key.source_priority == 0
            && (current.is_none()
                || current
                    .as_ref()
                    .is_some_and(|current| current.source != KeySource::Vault))
        {
            key.source_priority = 1000;
        }
        match edit {
            ValueEdit::Keep => {
                if let Some(current) = current {
                    key.credential_id = current.credential_id.clone();
                    key.source_baseline_credential_id =
                        current.source_baseline_credential_id.clone();
                    key.last_good_revision = current.last_good_revision;
                    key.last_good_at = current.last_good_at;
                    key.storage_state = current.storage_state;
                    key.legacy_reference = current.legacy_reference.clone();
                    if key.source != current.source {
                        key.source_baseline_credential_id = None;
                        if key.source.is_file() {
                            let source = self.source_value(scope, &key)?;
                            let bundle = self.bundle(scope, &current)?;
                            if source.expose() != bundle.access().expose() {
                                return Err(ProjectKeysError::Conflict);
                            }
                            return self.publish(
                                scope,
                                key,
                                expected,
                                bundle.access(),
                                Some(&source),
                            );
                        }
                    }
                }
                committed(storage_error(self.db.project_key_save(
                    &scope.workspace_id,
                    scope.environment,
                    &key,
                    expected,
                ))?)
            }
            ValueEdit::Replace(draft) if draft.is_empty() => {
                self.save_key(scope, key, expected, ValueEdit::Keep)
            }
            ValueEdit::Replace(draft) => {
                let value = draft.into_secret()?;
                if let Some(current) = current {
                    key.credential_id = current.credential_id;
                    key.legacy_reference = current.legacy_reference;
                    key.source_baseline_credential_id = current.source_baseline_credential_id;
                }
                if key.source.is_file() {
                    let mut original = self.read_source(scope, &key.source)?;
                    let name = if matches!(key.source, KeySource::PemFile { .. }) {
                        "PRIVATE_KEY"
                    } else {
                        &key.name
                    };
                    let before = original.remove(name);
                    self.publish_file_edit(scope, key, expected, &value, before.as_ref())
                } else {
                    self.publish(scope, key, expected, &value, None)
                }
            }
        }
    }
    pub fn preview(
        &self,
        scope: &ProjectKeysScope,
        source: KeySource,
    ) -> Result<SourcePreview, ProjectKeysError> {
        let values = self.read_source(scope, &source)?;
        Ok(SourcePreview {
            source,
            names: values.into_keys().collect(),
        })
    }
    pub fn import_source(
        &self,
        scope: &ProjectKeysScope,
        source: KeySource,
        names: Vec<String>,
    ) -> Result<(), ProjectKeysError> {
        if names.is_empty()
            || names.len() > PROJECT_KEYS_MAX
            || (matches!(source, KeySource::PemFile { .. }) && names.len() != 1)
        {
            return Err(ProjectKeysError::InvalidInput);
        }
        let mut values = self.read_source(scope, &source)?;
        let existing = storage_error(
            self.db
                .project_key_list(&scope.workspace_id, scope.environment),
        )?;
        for (index, name) in names.iter().enumerate() {
            if !deppy_core::credential_env::valid_name(name) || names[..index].contains(name) {
                return Err(ProjectKeysError::InvalidInput);
            }
            if existing
                .iter()
                .filter(|key| key.name == *name && key.source == source)
                .count()
                > 1
            {
                return Err(ProjectKeysError::Conflict);
            }
            let source_name = if matches!(source, KeySource::PemFile { .. }) {
                "PRIVATE_KEY"
            } else {
                name
            };
            let value = values
                .get(source_name)
                .ok_or(ProjectKeysError::InvalidInput)?;
            if value.expose().is_empty() {
                return Err(ProjectKeysError::SourceEmpty);
            }
        }
        for name in names {
            if !deppy_core::credential_env::valid_name(&name) {
                return Err(ProjectKeysError::InvalidInput);
            }
            let source_name = if matches!(source, KeySource::PemFile { .. }) {
                "PRIVATE_KEY"
            } else {
                &name
            };
            let value = values
                .remove(source_name)
                .ok_or(ProjectKeysError::InvalidInput)?;
            let current = existing
                .iter()
                .find(|key| key.name == name && key.source == source)
                .cloned();
            let expected = current.as_ref().map(|key| key.revision);
            let key = current.unwrap_or_else(|| Self::new_source_key(scope, &source, name.clone()));
            self.publish(scope, key, expected, &value, Some(&value))?;
            storage_error(self.db.project_key_source_exclusion_clear(
                &scope.workspace_id,
                scope.environment,
                &source,
                &name,
            ))?;
        }
        let mut settings = storage_error(
            self.db
                .project_key_settings(&scope.workspace_id, scope.environment),
        )?;
        if !settings.sources.contains(&source) {
            let revision = settings.revision;
            settings.sources.push(source);
            committed(storage_error(self.db.project_key_save_settings(
                &scope.workspace_id,
                scope.environment,
                &settings,
                Some(revision),
            ))?)?;
        }
        Ok(())
    }
    pub fn enroll_source(
        &self,
        scope: &ProjectKeysScope,
        source: &KeySource,
    ) -> Result<(), ProjectKeysError> {
        self.enroll_source_limit(scope, source, 4)
    }
    pub fn enroll_source_limit(
        &self,
        scope: &ProjectKeysScope,
        source: &KeySource,
        limit: usize,
    ) -> Result<(), ProjectKeysError> {
        // Discovery is restricted to persisted, explicitly selected sources and a fixed write budget.
        let values = self.read_source(scope, source)?;
        let keys = storage_error(
            self.db
                .project_key_list(&scope.workspace_id, scope.environment),
        )?;
        let exclusions = storage_error(
            self.db
                .project_key_source_exclusions(&scope.workspace_id, scope.environment),
        )?;
        for (name, value) in values
            .into_iter()
            .filter(|(name, _)| {
                !keys.iter().any(|key| {
                    key.source == *source
                        && (matches!(source, KeySource::PemFile { .. }) || key.name == *name)
                }) && !exclusions
                    .iter()
                    .any(|excluded| excluded.source == *source && excluded.name == *name)
            })
            .take(limit.min(PROJECT_KEYS_MAX))
        {
            let mut key = Self::new_source_key(scope, source, name);
            if value.expose().is_empty() {
                // A selected empty name is still an effective override and an ambient-env removal.
                // No value is fabricated or persisted for its metadata row.
                key.required = false;
                key.storage_state = KeyStorageState::EmptySource;
                key.source_checked_at = Some(now());
                key.error_code = Some(ProjectKeysError::SourceEmpty.code().into());
                committed(storage_error(self.db.project_key_save(
                    &scope.workspace_id,
                    scope.environment,
                    &key,
                    None,
                ))?)?;
            } else {
                self.publish(scope, key, None, &value, Some(&value))?;
            }
        }
        Ok(())
    }
    /// Old dotenv profiles contain a merged value. Correct their initial origin using the
    /// selected files before publishing a baseline; explicitly managed sources stay fixed.
    pub fn prepare_legacy_source_origins(
        &self,
        scope: &ProjectKeysScope,
    ) -> Result<(), ProjectKeysError> {
        let settings = storage_error(
            self.db
                .project_key_settings(&scope.workspace_id, scope.environment),
        )?;
        let keys = storage_error(
            self.db
                .project_key_list(&scope.workspace_id, scope.environment),
        )?;
        let mut initial = Vec::new();
        for key in keys.into_iter().filter(|key| {
            key.usage == KeyUsage::LegacyAgent && matches!(key.source, KeySource::Dotenv { .. })
        }) {
            let owned = key
                .credential_id
                .as_deref()
                .map(|id| storage_error(self.db.project_key_credential(&scope.workspace_id, id)))
                .transpose()?
                .flatten()
                .is_some_and(|(meta, _)| meta.provider == "project-key");
            if !owned || key.source_baseline_credential_id.is_none() {
                initial.push(key);
            }
        }
        if initial.is_empty() {
            return Ok(());
        }
        let exclusions = storage_error(
            self.db
                .project_key_source_exclusions(&scope.workspace_id, scope.environment),
        )?;
        let mut winning = std::collections::BTreeMap::new();
        for (index, source) in settings
            .sources
            .iter()
            .enumerate()
            .filter(|(_, source)| matches!(source, KeySource::Dotenv { .. }))
        {
            match self.read_source(scope, source) {
                Ok(values) => {
                    for name in values.into_keys() {
                        if !exclusions
                            .iter()
                            .any(|excluded| excluded.source == *source && excluded.name == name)
                        {
                            winning.insert(name, (source.clone(), index as i32 + 1));
                        }
                    }
                }
                Err(ProjectKeysError::SourceMissing) => {}
                Err(error) => return Err(error),
            }
        }
        for mut key in initial {
            if let Some((source, priority)) = winning.get(&key.name)
                && (key.source != *source || key.source_priority != *priority)
            {
                let revision = key.revision;
                key.source = source.clone();
                key.source_priority = *priority;
                committed(storage_error(self.db.project_key_save(
                    &scope.workspace_id,
                    scope.environment,
                    &key,
                    Some(revision),
                ))?)?;
            }
        }
        Ok(())
    }
    pub fn refresh_scope(&self, scope: &ProjectKeysScope) -> Result<(), ProjectKeysError> {
        self.root(scope)?;
        self.prepare_legacy_source_origins(scope)?;
        let settings = storage_error(
            self.db
                .project_key_settings(&scope.workspace_id, scope.environment),
        )?;
        for source in &settings.sources {
            // Missing originals remain visible on their existing rows. Unregistered optional files
            // may be absent; a failed enrollment never prevents rechecking the remaining keys.
            match self.enroll_source_limit(scope, source, PROJECT_KEYS_MAX) {
                Ok(()) | Err(ProjectKeysError::SourceMissing | ProjectKeysError::SourceEmpty) => {}
                Err(error) => return Err(error),
            }
        }
        let keys = storage_error(
            self.db
                .project_key_list(&scope.workspace_id, scope.environment),
        )?;
        for key in &keys {
            if key.source.is_file() {
                if settings.sources.contains(&key.source) {
                    self.observe_key(scope, key)?;
                }
            } else {
                match self.enroll_configuration(scope, key) {
                    Ok(()) => {}
                    Err(
                        error @ (ProjectKeysError::Locked
                        | ProjectKeysError::SourceMissing
                        | ProjectKeysError::SourceEmpty),
                    ) if !key.required => {
                        self.record_key_error(scope, key, error)?;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }
    fn new_source_key(scope: &ProjectKeysScope, source: &KeySource, name: String) -> ProjectKey {
        ProjectKey {
            id: uuid::Uuid::new_v4().to_string(),
            workspace_id: scope.workspace_id.clone(),
            environment: scope.environment,
            name,
            credential_id: None,
            source_baseline_credential_id: None,
            source: source.clone(),
            kind: if matches!(source, KeySource::PemFile { .. }) {
                KeyKind::PrivateKey
            } else {
                KeyKind::ApiToken
            },
            usage: KeyUsage::Build,
            delivery: if matches!(source, KeySource::PemFile { .. }) {
                KeyDelivery::TemporaryFile
            } else {
                KeyDelivery::Environment
            },
            source_priority: 0,
            required: true,
            revision: 1,
            last_good_revision: None,
            last_good_at: None,
            storage_state: KeyStorageState::NotStored,
            created_at: now(),
            updated_at: now(),
            source_checked_at: None,
            error_code: None,
            github: None,
            legacy_reference: None,
        }
    }
    fn record_key_error(
        &self,
        scope: &ProjectKeysScope,
        key: &ProjectKey,
        error: ProjectKeysError,
    ) -> Result<(), ProjectKeysError> {
        let state = match error {
            ProjectKeysError::SourceMissing => KeyStorageState::MissingSource,
            ProjectKeysError::SourceEmpty => KeyStorageState::EmptySource,
            ProjectKeysError::Locked => KeyStorageState::Locked,
            ProjectKeysError::Conflict => KeyStorageState::Conflict,
            _ => KeyStorageState::Failed,
        };
        if key.storage_state != state || key.error_code.as_deref() != Some(error.code()) {
            let mut next = key.clone();
            next.storage_state = state;
            next.error_code = Some(error.code().into());
            next.source_checked_at = Some(now());
            let _ = storage_error(self.db.project_key_save(
                &scope.workspace_id,
                scope.environment,
                &next,
                Some(key.revision),
            ))?;
        }
        Ok(())
    }
    pub fn source_value(
        &self,
        scope: &ProjectKeysScope,
        key: &ProjectKey,
    ) -> Result<SecretString, ProjectKeysError> {
        let mut values = self.read_source(scope, &key.source)?;
        let name = if matches!(key.source, KeySource::PemFile { .. }) {
            "PRIVATE_KEY"
        } else {
            &key.name
        };
        let value = values.remove(name).ok_or(ProjectKeysError::SourceMissing)?;
        if value.expose().is_empty() {
            return Err(ProjectKeysError::SourceEmpty);
        }
        Ok(value)
    }
    pub fn observe_key(
        &self,
        scope: &ProjectKeysScope,
        key: &ProjectKey,
    ) -> Result<(), ProjectKeysError> {
        if !key.source.is_file() {
            return Ok(());
        }
        let result = (|| {
            let source = self.source_value(scope, key)?;
            if key.credential_id.is_none() {
                self.publish(
                    scope,
                    key.clone(),
                    Some(key.revision),
                    &source,
                    Some(&source),
                )?;
                return Ok(());
            }
            let bundle = self.bundle(scope, key)?;
            let baseline = if key.source_baseline_credential_id == key.credential_id {
                bundle.refresh()
            } else {
                None
            };
            match baseline {
                Some(base) if source.expose() == base.expose() => {
                    if key.storage_state != KeyStorageState::Stored {
                        self.publish(
                            scope,
                            key.clone(),
                            Some(key.revision),
                            bundle.access(),
                            Some(&source),
                        )?;
                    }
                    return Ok(());
                }
                Some(base)
                    if bundle.access().expose() == base.expose()
                        || source.expose() == bundle.access().expose() =>
                {
                    self.publish(
                        scope,
                        key.clone(),
                        Some(key.revision),
                        &source,
                        Some(&source),
                    )?;
                }
                None if source.expose() == bundle.access().expose() => {
                    // Initial legacy file values are detached into a workspace-owned bundle.
                    // Its protected baseline is not an OAuth/legacy refresh token.
                    self.publish(
                        scope,
                        key.clone(),
                        Some(key.revision),
                        &source,
                        Some(&source),
                    )?;
                }
                _ => return Err(ProjectKeysError::Conflict),
            }
            Ok(())
        })();
        let desired = match result {
            Ok(()) => {
                if key.storage_state != KeyStorageState::Stored {
                    Some((KeyStorageState::Stored, None))
                } else {
                    None
                }
            }
            Err(ProjectKeysError::Stale) => return Ok(()),
            Err(error) => Some((
                match error {
                    ProjectKeysError::SourceMissing => KeyStorageState::MissingSource,
                    ProjectKeysError::SourceEmpty => KeyStorageState::EmptySource,
                    ProjectKeysError::Locked => KeyStorageState::Locked,
                    ProjectKeysError::Conflict => KeyStorageState::Conflict,
                    _ => KeyStorageState::Failed,
                },
                Some(error),
            )),
        };
        if let Some((state, error)) = desired
            && (key.storage_state != state || key.error_code.as_deref() != error.map(|e| e.code()))
        {
            let mut next = key.clone();
            next.storage_state = state;
            next.error_code = error.map(|e| e.code().into());
            next.source_checked_at = Some(now());
            let _ = storage_error(self.db.project_key_save(
                &scope.workspace_id,
                scope.environment,
                &next,
                Some(key.revision),
            ))?;
        }
        Ok(())
    }
    pub fn resolve(
        &self,
        scope: &ProjectKeysScope,
        id: &str,
        revision: u64,
        resolution: ConflictResolution,
    ) -> Result<ProjectKey, ProjectKeysError> {
        let key = self.key(scope, id, revision)?;
        let source = self.source_value(scope, &key)?;
        match resolution {
            ConflictResolution::KeepSource => {
                self.publish(scope, key, Some(revision), &source, Some(&source))
            }
            ConflictResolution::KeepVault => {
                let bundle = self.bundle(scope, &key)?;
                self.publish_file_edit(scope, key, Some(revision), bundle.access(), Some(&source))
            }
        }
    }
    pub fn enroll_configuration(
        &self,
        scope: &ProjectKeysScope,
        key: &ProjectKey,
    ) -> Result<(), ProjectKeysError> {
        if key.source != KeySource::Vault {
            return Ok(());
        }
        // Legacy Plain/Secret edits carry a durable key generation. Read the current selected
        // legacy row only on initial enrollment or when that generation requires refresh.
        if key.credential_id.is_none() || key.storage_state == KeyStorageState::UpdateNeeded {
            if let Some(value) = storage_error(self.db.project_key_legacy_configuration_value(
                &scope.workspace_id,
                scope.environment,
                &key.id,
                key.revision,
            ))? {
                let value = SecretString::new(value);
                if value.expose().is_empty() {
                    return Err(ProjectKeysError::SourceEmpty);
                }
                let mut next = key.clone();
                next.kind = KeyKind::Configuration;
                self.publish(scope, next, Some(key.revision), &value, None)?;
            } else if key.usage == KeyUsage::LegacyAgent
                && let Some((meta, location)) =
                    storage_error(self.db.project_key_legacy_value_location(
                        &scope.workspace_id,
                        scope.environment,
                        &key.id,
                        key.revision,
                    ))?
            {
                let bundle = if let Ok(slot) =
                    PhysicalSecretSlot::parse(location.keyring_username.clone())
                {
                    if !slot.belongs_to(
                        &LogicalCredentialId::new(&meta.id)
                            .map_err(|_| ProjectKeysError::Locked)?,
                    ) {
                        return Err(ProjectKeysError::Locked);
                    }
                    secret::read_secret_bundle(self.secrets.as_ref(), &slot)
                        .map_err(|_| ProjectKeysError::Locked)?
                } else {
                    SecretBundle::new(
                        self.secrets
                            .get_secret(&location.keyring_username)
                            .map_err(|_| ProjectKeysError::Locked)?,
                        None,
                        None,
                    )
                };
                let mut next = key.clone();
                next.kind = KeyKind::ApiToken;
                self.publish(scope, next, Some(key.revision), bundle.access(), None)?;
            }
        }
        Ok(())
    }
}
