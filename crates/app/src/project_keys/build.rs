use super::{
    contracts::*,
    process::{ProcessEnvironment, ProcessRunner},
    service::{ProjectKeysService, now, storage_error},
};
use deppy_core::project_keys::*;
use secret::SecretString;
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

struct TemporaryKeys {
    directory: PathBuf,
    files: Vec<PathBuf>,
}

impl TemporaryKeys {
    fn create() -> Result<Self, ProjectKeysError> {
        let directory =
            std::env::temp_dir().join(format!("deppy-build-keys-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&directory)
            .map_err(|_| ProjectKeysError::Process)?;
        Ok(Self {
            directory,
            files: vec![],
        })
    }
    fn write(&mut self, value: &SecretString) -> Result<String, ProjectKeysError> {
        let path = self.directory.join(format!("{}.pem", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&path).map_err(|_| ProjectKeysError::Process)?;
        self.files.push(path.clone());
        file.write_all(value.expose().as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|_| ProjectKeysError::Process)?;
        Ok(path.to_string_lossy().into())
    }
}
impl Drop for TemporaryKeys {
    fn drop(&mut self) {
        for path in &self.files {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir(&self.directory);
    }
}

impl ProjectKeysService {
    pub fn run_build(
        &self,
        scope: &ProjectKeysScope,
        recipe: Option<&ProjectBuildRecipe>,
        command: &[String],
        cwd: &Path,
        runner: &dyn ProcessRunner,
    ) -> Result<ProjectBuildReceipt, ProjectKeysError> {
        if command.is_empty()
            || command.len() > 128
            || command.iter().map(String::len).sum::<usize>() > 8192
            || command.iter().any(|arg| arg.contains('\0'))
        {
            return Err(ProjectKeysError::InvalidInput);
        }
        let root = self.root_handle(scope)?;
        let cwd = root.open_directory(cwd)?;
        let settings = storage_error(
            self.db
                .project_key_settings(&scope.workspace_id, scope.environment),
        )?;
        // Preflight performs the same bounded fresh-original scan as explicit Retry.
        self.refresh_scope(scope)?;
        let keys = storage_error(
            self.db
                .project_key_list(&scope.workspace_id, scope.environment),
        )?;
        let mut effective: BTreeMap<String, ProjectKey> = BTreeMap::new();
        let mut disconnected_required = Vec::new();
        for mut key in keys
            .into_iter()
            .filter(|key| matches!(key.usage, KeyUsage::Build | KeyUsage::LegacyAgent))
        {
            if key.source.is_file() {
                let Some(index) = settings
                    .sources
                    .iter()
                    .position(|source| *source == key.source)
                else {
                    if key.required {
                        disconnected_required.push(key.name.clone());
                    }
                    continue;
                };
                key.source_priority = index as i32 + 1;
            }
            if let Some(previous) = effective.get(&key.name) {
                if previous.source_priority == key.source_priority {
                    return Err(ProjectKeysError::Conflict);
                }
                if previous.source_priority > key.source_priority {
                    continue;
                }
            }
            effective.insert(key.name.clone(), key);
        }
        if disconnected_required
            .iter()
            .any(|name| !effective.contains_key(name))
        {
            return Err(ProjectKeysError::SourceMissing);
        }
        let mut temporary = TemporaryKeys::create()?;
        let mut environment = ProcessEnvironment::default();
        let mut used_keys = Vec::new();
        // Remove every registered name in either environment before injecting this snapshot.
        // This also removes optional/missing values from an old shell's ambient environment.
        for environment_kind in [KeyEnvironment::Development, KeyEnvironment::Production] {
            for key in storage_error(
                self.db
                    .project_key_list(&scope.workspace_id, environment_kind),
            )? {
                if !environment.remove_names.contains(&key.name) {
                    environment.remove_names.push(key.name);
                }
            }
        }
        for key in effective.into_values() {
            if matches!(key.source, KeySource::GitHubOnly) {
                if key.required {
                    return Err(ProjectKeysError::Locked);
                }
                continue;
            }
            if key.storage_state == KeyStorageState::Conflict {
                return Err(ProjectKeysError::Conflict);
            }
            if matches!(
                key.storage_state,
                KeyStorageState::MissingSource
                    | KeyStorageState::EmptySource
                    | KeyStorageState::Locked
                    | KeyStorageState::Failed
                    | KeyStorageState::UpdateNeeded
                    | KeyStorageState::NotStored
            ) {
                if key.required {
                    return Err(ProjectKeysError::Locked);
                }
                continue;
            }
            let bundle = match self.bundle(scope, &key) {
                Ok(bundle) => bundle,
                Err(
                    ProjectKeysError::Locked
                    | ProjectKeysError::SourceMissing
                    | ProjectKeysError::SourceEmpty,
                ) if !key.required => continue,
                Err(error) => return Err(error),
            };
            if bundle.access().expose().is_empty() {
                if key.required {
                    return Err(ProjectKeysError::Locked);
                }
                continue;
            }
            let value = if matches!(
                key.delivery,
                KeyDelivery::FilePath | KeyDelivery::TemporaryFile
            ) {
                SecretString::new(temporary.write(bundle.access())?)
            } else {
                SecretString::new(bundle.access().expose().into())
            };
            environment.values.push((key.name.clone(), value));
            used_keys.push(ProjectKeyUsedVersion {
                key_id: key.id,
                name: key.name,
                revision: key.last_good_revision.ok_or(ProjectKeysError::Locked)?,
            });
        }
        let mut receipt = ProjectBuildReceipt {
            owner: deppy_core::process_identity::ProcessIdentity::capture(std::process::id()),
            id: uuid::Uuid::new_v4().to_string(),
            workspace_id: scope.workspace_id.clone(),
            environment: scope.environment,
            recipe_id: recipe.map(|recipe| recipe.id.clone()),
            started_at: now(),
            finished_at: None,
            exit_code: None,
            used_keys,
        };
        // Receipt preflight rejects a rotation that happened while gathering the immutable values.
        storage_error(self.db.project_key_receipt_save(&receipt))?;
        // Every failure after publishing a running receipt must settle that receipt.
        let result = self
            .root_handle(scope)
            .and_then(|_| runner.run_in_directory(command, &cwd, &environment, None, true));
        // PEM files are needed only by the child; a busy receipt database must not retain them.
        drop(temporary);
        receipt.finished_at = Some(now());
        receipt.exit_code = Some(result.as_ref().map(|output| output.exit_code).unwrap_or(1));
        storage_error(self.db.project_key_receipt_save(&receipt))?;
        result.map(|_| receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_keys::{process::ProcessOutput, tests::Fixture};
    use std::sync::{Mutex, atomic::Ordering};
    struct BlockingReceiptRunner {
        db_path: PathBuf,
        lock: Mutex<Option<rusqlite::Connection>>,
        path: std::sync::mpsc::SyncSender<PathBuf>,
    }
    impl ProcessRunner for BlockingReceiptRunner {
        fn run(
            &self,
            _: &[String],
            _: &Path,
            environment: &ProcessEnvironment,
            _: Option<&SecretString>,
            _: bool,
        ) -> Result<ProcessOutput, ProjectKeysError> {
            let path = PathBuf::from(environment.values[0].1.expose());
            assert!(path.exists());
            let lock = rusqlite::Connection::open(&self.db_path).unwrap();
            lock.execute_batch("BEGIN IMMEDIATE").unwrap();
            *self.lock.lock().unwrap() = Some(lock);
            self.path.send(path).unwrap();
            Ok(ProcessOutput {
                exit_code: 0,
                stdout: vec![],
            })
        }
    }
    #[test]
    fn temporary_pem_is_removed_before_blocked_receipt_finalization() {
        let fixture = Fixture::new();
        let mut key = fixture.key("PRIVATE_KEY", KeySource::Vault);
        key.kind = KeyKind::PrivateKey;
        key.delivery = KeyDelivery::TemporaryFile;
        fixture.save(key, "fixture-pem-bytes");
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let runner = std::sync::Arc::new(BlockingReceiptRunner {
            db_path: fixture.root.join("metadata.sqlite3"),
            lock: Mutex::new(None),
            path: tx,
        });
        let running = runner.clone();
        let build = std::thread::spawn(move || {
            fixture.service.run_build(
                &fixture.scope,
                None,
                &["fixture".into()],
                Path::new("."),
                running.as_ref(),
            )
        });
        let path = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while path.exists() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let cleaned_before_publication = !path.exists();
        runner
            .lock
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .execute_batch("ROLLBACK")
            .unwrap();
        build.join().unwrap().unwrap();
        assert!(
            cleaned_before_publication,
            "plaintext PEM must be gone while final receipt DB save is blocked"
        );
    }
    struct ReplacingStore {
        inner: std::sync::Arc<crate::project_keys::tests::MemoryStore>,
        paths: Mutex<Option<(PathBuf, PathBuf)>>,
    }
    impl secret::SecretStore for ReplacingStore {
        fn set_secret(&self, id: &str, value: &SecretString) -> anyhow::Result<()> {
            self.inner.set_secret(id, value)
        }
        fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
            let value = self.inner.get_secret(id)?;
            if let Some((root, old)) = self.paths.lock().unwrap().take() {
                std::fs::rename(&root, old)?;
                std::fs::create_dir(root)?;
            }
            Ok(value)
        }
        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            self.inner.delete_secret(id)
        }
        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            self.inner.has_secret(id)
        }
        fn list_secret_ids(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
            self.inner.list_secret_ids(prefix)
        }
    }
    #[test]
    fn workspace_failure_after_receipt_publication_settles_receipt() {
        let mut fixture = Fixture::new();
        fixture.save(fixture.key("TOKEN", KeySource::Vault), "fixture-token");
        let old = fixture.root.with_extension("receipt-original");
        fixture.service.secrets = std::sync::Arc::new(ReplacingStore {
            inner: fixture.store.clone(),
            paths: Mutex::new(Some((fixture.root.clone(), old.clone()))),
        });
        let result = fixture.service.run_build(
            &fixture.scope,
            None,
            &["fixture".into()],
            Path::new("."),
            &ObservedRunner::default(),
        );
        let receipts = fixture
            .service
            .db
            .project_key_receipt_list(&fixture.scope.workspace_id, fixture.scope.environment, 10)
            .unwrap();
        std::fs::remove_dir_all(old).unwrap();
        assert!(matches!(result, Err(ProjectKeysError::SourceChanged)));
        assert_eq!(receipts.len(), 1);
        assert!(receipts[0].finished_at.is_some());
        assert_eq!(receipts[0].exit_code, Some(1));
        assert!(receipts[0].owner.is_some());
    }
    #[derive(Default)]
    struct ObservedRunner(Mutex<Vec<String>>);
    impl ProcessRunner for ObservedRunner {
        fn run(
            &self,
            _: &[String],
            _: &Path,
            environment: &ProcessEnvironment,
            _: Option<&SecretString>,
            _: bool,
        ) -> Result<ProcessOutput, ProjectKeysError> {
            assert!(environment.values.is_empty());
            *self.0.lock().unwrap() = environment.remove_names.clone();
            Ok(ProcessOutput {
                exit_code: 0,
                stdout: vec![],
            })
        }
    }
    #[test]
    fn unavailable_optional_vault_bundle_is_omitted_and_ambient_name_scrubbed() {
        let fixture = Fixture::new();
        let mut key = fixture.key("OPTIONAL_TOKEN", KeySource::Vault);
        key.required = false;
        fixture.save(key, "fixture-token");
        fixture.store.1.store(true, Ordering::Release);
        let runner = ObservedRunner::default();
        let receipt = fixture
            .service
            .run_build(
                &fixture.scope,
                None,
                &["fixture".into()],
                Path::new("."),
                &runner,
            )
            .unwrap();
        assert!(receipt.used_keys.is_empty());
        assert!(runner.0.lock().unwrap().contains(&"OPTIONAL_TOKEN".into()));
    }
    #[test]
    fn optional_empty_legacy_configuration_keeps_build_available_and_scrubs_name() {
        let fixture = Fixture::new();
        let profile = fixture
            .service
            .db
            .insert_env_profile(&fixture.scope.workspace_id, "custom", "custom")
            .unwrap();
        fixture
            .service
            .db
            .upsert_env_var(
                &profile,
                "API_URL",
                &storage::EnvValue::Plain(String::new()),
            )
            .unwrap();
        let runner = ObservedRunner::default();
        let receipt = fixture
            .service
            .run_build(
                &fixture.scope,
                None,
                &["fixture".into()],
                Path::new("."),
                &runner,
            )
            .unwrap();
        assert!(receipt.used_keys.is_empty());
        assert!(runner.0.lock().unwrap().contains(&"API_URL".into()));
        let keys = fixture
            .service
            .db
            .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
            .unwrap();
        assert_eq!(
            keys.iter()
                .find(|key| key.name == "API_URL")
                .unwrap()
                .storage_state,
            KeyStorageState::EmptySource
        );
    }
}
