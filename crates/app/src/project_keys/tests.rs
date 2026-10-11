use super::{contracts::*, process::*, service::*};
use base64::{Engine, engine::general_purpose::STANDARD};
use deppy_core::project_keys::*;
use secret::{SecretStore, SecretString};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub(crate) fn open_service(
    path: &Path,
    secrets: Arc<dyn SecretStore>,
) -> Result<ProjectKeysService, ProjectKeysError> {
    crate::app::project_keys_test_service(path, secrets)
}

type StoreCallback = Box<dyn FnOnce() + Send>;
#[derive(Default)]
pub(crate) struct MemoryStore(
    Mutex<HashMap<String, String>>,
    pub std::sync::atomic::AtomicBool,
    pub std::sync::atomic::AtomicUsize,
    Mutex<Option<StoreCallback>>,
);
impl SecretStore for MemoryStore {
    fn set_secret(&self, id: &str, value: &SecretString) -> anyhow::Result<()> {
        self.2.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if self.1.load(std::sync::atomic::Ordering::Acquire) {
            anyhow::bail!("fixture_locked");
        }
        let callback = self.3.lock().unwrap().take();
        if let Some(callback) = callback {
            callback();
        }
        self.0
            .lock()
            .unwrap()
            .insert(id.into(), value.expose().into());
        Ok(())
    }
    fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
        self.2.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if self.1.load(std::sync::atomic::Ordering::Acquire) {
            anyhow::bail!("fixture_locked");
        }
        self.0
            .lock()
            .unwrap()
            .get(id)
            .map(|value| SecretString::new(value.clone()))
            .ok_or_else(|| anyhow::anyhow!("fixture_missing"))
    }
    fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().remove(id);
        Ok(())
    }
    fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
        Ok(self.0.lock().unwrap().contains_key(id))
    }
    fn list_secret_ids(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .keys()
            .filter(|id| id.starts_with(prefix))
            .cloned()
            .collect())
    }
}
pub(crate) struct Fixture {
    pub root: PathBuf,
    pub service: ProjectKeysService,
    pub scope: ProjectKeysScope,
    pub store: Arc<MemoryStore>,
}
impl Fixture {
    pub fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "deppy-project-keys-service-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&root).unwrap();
        let store = Arc::new(MemoryStore::default());
        let service = open_service(&root.join("metadata.sqlite3"), store.clone()).unwrap();
        let workspace_id = service.db.create_workspace("fixture").unwrap();
        let identity = crate::folder_identity::probe(&root).unwrap();
        service
            .db
            .set_workspace_path_and_anchor_with_volume(
                &workspace_id,
                &root.to_string_lossy(),
                Some(identity.anchor.dev),
                Some(identity.anchor.ino),
                identity.volume,
            )
            .unwrap();
        Self {
            root,
            service,
            scope: ProjectKeysScope {
                workspace_id,
                environment: KeyEnvironment::Development,
            },
            store,
        }
    }
    pub fn key(&self, name: &str, source: KeySource) -> ProjectKey {
        ProjectKey {
            id: uuid::Uuid::new_v4().to_string(),
            workspace_id: self.scope.workspace_id.clone(),
            environment: self.scope.environment,
            name: name.into(),
            credential_id: None,
            source_baseline_credential_id: None,
            source,
            kind: KeyKind::ApiToken,
            usage: KeyUsage::Build,
            delivery: KeyDelivery::Environment,
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
    pub fn save(&self, key: ProjectKey, value: &str) -> ProjectKey {
        self.service
            .save_key(
                &self.scope,
                key,
                None,
                ValueEdit::Replace(SensitiveDraft::new(value.into()).unwrap()),
            )
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
#[derive(Default)]
struct BuildRunner {
    seen: Mutex<Vec<(String, String)>>,
    removed: Mutex<Vec<String>>,
    paths: Mutex<Vec<PathBuf>>,
}
impl ProcessRunner for BuildRunner {
    fn run(
        &self,
        _command: &[String],
        _cwd: &Path,
        env: &ProcessEnvironment,
        stdin: Option<&SecretString>,
        inherit: bool,
    ) -> Result<ProcessOutput, ProjectKeysError> {
        assert!(inherit);
        assert!(stdin.is_none());
        *self.removed.lock().unwrap() = env.remove_names.clone();
        for (name, value) in &env.values {
            if name.ends_with("PRIVATE_KEY") {
                let path = PathBuf::from(value.expose());
                assert_eq!(std::fs::read_to_string(&path).unwrap(), "fixture-pem-bytes");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    assert_eq!(
                        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                        0o600
                    );
                    assert_eq!(
                        std::fs::metadata(path.parent().unwrap())
                            .unwrap()
                            .permissions()
                            .mode()
                            & 0o777,
                        0o700
                    );
                }
                self.paths.lock().unwrap().push(path);
            }
            self.seen
                .lock()
                .unwrap()
                .push((name.clone(), value.expose().into()));
        }
        Ok(ProcessOutput {
            exit_code: 0,
            stdout: vec![],
        })
    }
}
struct GitHubRunner {
    calls: Mutex<Vec<Vec<String>>>,
    sent: Mutex<bool>,
    remote_existing: bool,
    auth: Option<String>,
    organization: bool,
    private_key: crypto_box::SecretKey,
}
impl GitHubRunner {
    fn new(remote_existing: bool) -> Self {
        Self {
            calls: Mutex::new(vec![]),
            sent: Mutex::new(false),
            remote_existing,
            auth: None,
            organization: false,
            private_key: crypto_box::SecretKey::generate(&mut crypto_box::aead::OsRng),
        }
    }
}
impl ProcessRunner for GitHubRunner {
    fn run(
        &self,
        command: &[String],
        _cwd: &Path,
        environment: &ProcessEnvironment,
        stdin: Option<&SecretString>,
        inherit: bool,
    ) -> Result<ProcessOutput, ProjectKeysError> {
        assert!(!inherit);
        if let Some(auth) = &self.auth {
            assert_eq!(environment.values.len(), 1);
            assert_eq!(environment.values[0].0, "GH_TOKEN");
            assert_eq!(environment.values[0].1.expose(), auth);
        } else {
            assert!(environment.values.is_empty());
        }
        assert!(!command.iter().any(|arg| arg == "source-value-3"));
        self.calls.lock().unwrap().push(command.to_vec());
        assert_eq!(&command[..4], ["gh", "api", "--hostname", "github.com"]);
        let endpoint = &command[4];
        let stdout = if endpoint == "user" {
            assert!(stdin.is_none());
            b"{\"login\":\"fixture-account\"}".to_vec()
        } else if endpoint.ends_with("/public-key") {
            serde_json::to_vec(&serde_json::json!({"key_id":"fixture-key-id", "key":STANDARD.encode(self.private_key.public_key().as_bytes())})).unwrap()
        } else if command.windows(2).any(|args| args == ["--method", "PUT"]) {
            let body: serde_json::Value = serde_json::from_str(stdin.unwrap().expose()).unwrap();
            let ciphertext = STANDARD
                .decode(body["encrypted_value"].as_str().unwrap())
                .unwrap();
            assert_eq!(
                self.private_key.unseal(&ciphertext).unwrap(),
                b"source-value-3"
            );
            assert_eq!(body["key_id"], "fixture-key-id");
            if self.organization {
                assert_eq!(endpoint, "orgs/fixture-org/actions/secrets/TOKEN");
                assert_eq!(body["selected_repository_ids"], serde_json::json!([42]));
            } else {
                assert_eq!(endpoint, "repos/owner/repository/actions/secrets/TOKEN");
            }
            *self.sent.lock().unwrap() = true;
            vec![]
        } else if self.remote_existing || *self.sent.lock().unwrap() {
            b"HTTP/2.0 200 OK\r\n\r\n{\"name\":\"TOKEN\",\"updated_at\":\"2026-10-12T01:00:00Z\"}"
                .to_vec()
        } else {
            b"HTTP/2.0 404 Not Found\r\n\r\n{}".to_vec()
        };
        Ok(ProcessOutput {
            exit_code: 0,
            stdout,
        })
    }
}

fn target() -> GitHubTarget {
    GitHubTarget {
        auth_credential_id: None,
        account_identity: "fixture-account".into(),
        scope: GitHubScope::Repository {
            owner: "owner".into(),
            repository: "repository".into(),
        },
        secret_name: "TOKEN".into(),
        auto_send: false,
    }
}
fn link(fixture: &Fixture, mut key: ProjectKey, target: GitHubTarget) -> ProjectKey {
    key.github = Some(KeyGitHubStatus {
        target,
        state: GitHubRemoteState::Unlinked,
        last_sent_revision: None,
        remote_updated_at: None,
        error_code: None,
    });
    committed(
        fixture
            .service
            .db
            .project_key_save(
                &fixture.scope.workspace_id,
                fixture.scope.environment,
                &key,
                Some(key.revision),
            )
            .unwrap(),
    )
    .unwrap()
}
fn old_publish(fixture: &Fixture, key: &ProjectKey, value: &str) {
    let id = key.credential_id.as_ref().unwrap();
    let (_, location) = fixture
        .service
        .db
        .project_key_value_location(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &key.id,
            key.revision,
        )
        .unwrap()
        .unwrap();
    let plan = secret::SecretBundleStagePlan::allocate(
        secret::LogicalCredentialId::new(id).unwrap(),
        Some(secret::PhysicalSecretSlot::parse(location.keyring_username.clone()).unwrap()),
    )
    .unwrap();
    fixture
        .service
        .db
        .register_physical_secret_slot_staging(id, plan.new_slot().as_str())
        .unwrap();
    secret::stage_secret_bundle(
        fixture.store.as_ref(),
        &plan,
        secret::SecretBundleRef::new(&SecretString::new(value.into()), None, None),
    )
    .unwrap();
    assert!(
        fixture
            .service
            .db
            .publish_credential_secret_slot_cas(
                id,
                &location.keyring_username,
                plan.new_slot().as_str(),
                None,
                None
            )
            .unwrap()
    );
}

#[test]
fn old_credential_publication_recovers_durable_auto_queue_even_with_watch_disabled() {
    let fixture = Fixture::new();
    let key = fixture.save(fixture.key("TOKEN", KeySource::Vault), "before-old-edit");
    let mut destination = target();
    destination.auto_send = true;
    let key = link(&fixture, key, destination);
    let mut settings = fixture
        .service
        .db
        .project_key_settings(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap();
    settings.watch_enabled = false;
    fixture
        .service
        .db
        .project_key_save_settings(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &settings,
            Some(settings.revision),
        )
        .unwrap();
    old_publish(&fixture, &key, "source-value-3");
    assert!(matches!(
        fixture.service.bundle(&fixture.scope, &key),
        Err(ProjectKeysError::Stale)
    ));
    let runner = GitHubRunner::new(false);
    fixture.service.drain_github(&runner).unwrap();
    assert!(*runner.sent.lock().unwrap());
    let key = fixture
        .service
        .db
        .project_key_get(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &key.id,
        )
        .unwrap()
        .unwrap();
    assert_eq!(key.github.unwrap().last_sent_revision, Some(key.revision));
}
#[test]
fn staged_auth_bundle_and_selected_organization_ids_use_explicit_token_and_numeric_identity() {
    let fixture = Fixture::new();
    let mut auth = fixture.key("GITHUB_AUTH", KeySource::Vault);
    auth.usage = KeyUsage::StoreOnly;
    auth.required = false;
    let auth = fixture.save(auth, "github-auth-fixture");
    let key = fixture.save(fixture.key("TOKEN", KeySource::Vault), "source-value-3");
    let mut destination = target();
    destination.auth_credential_id = auth.credential_id;
    destination.scope = GitHubScope::Organization {
        organization: "fixture-org".into(),
        visibility: GitHubVisibility::Selected,
        selected_repository_ids: vec![42],
    };
    let key = link(&fixture, key, destination);
    fixture
        .service
        .db
        .project_key_github_enqueue(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &key.id,
            key.revision,
            false,
            now(),
        )
        .unwrap();
    let entry = fixture
        .service
        .db
        .project_key_github_claim(now(), 120, 1)
        .unwrap()
        .remove(0);
    let mut runner = GitHubRunner::new(false);
    runner.auth = Some("github-auth-fixture".into());
    runner.organization = true;
    assert!(matches!(
        fixture.service.github_send(&entry, &runner).unwrap(),
        GitHubSendOutcome::Sent { .. }
    ));
    assert!(*runner.sent.lock().unwrap());
}
#[test]
fn source_order_disconnect_and_manual_override_control_effective_build_values() {
    let fixture = Fixture::new();
    std::fs::write(fixture.root.join(".env"), "TOKEN=base-value\n").unwrap();
    std::fs::write(fixture.root.join(".env.local"), "TOKEN=local-value\n").unwrap();
    let base = KeySource::Dotenv {
        file: ".env".into(),
    };
    let local = KeySource::Dotenv {
        file: ".env.local".into(),
    };
    fixture
        .service
        .import_source(&fixture.scope, base.clone(), vec!["TOKEN".into()])
        .unwrap();
    fixture
        .service
        .import_source(&fixture.scope, local.clone(), vec!["TOKEN".into()])
        .unwrap();
    let run = || {
        let runner = BuildRunner::default();
        fixture
            .service
            .run_build(
                &fixture.scope,
                None,
                &["fixture".into()],
                Path::new("."),
                &runner,
            )
            .unwrap();
        runner.seen.lock().unwrap()[0].1.clone()
    };
    assert_eq!(run(), "local-value");
    let mut settings = fixture
        .service
        .db
        .project_key_settings(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap();
    settings.sources = vec![local.clone(), base.clone()];
    settings = committed(
        fixture
            .service
            .db
            .project_key_save_settings(
                &fixture.scope.workspace_id,
                fixture.scope.environment,
                &settings,
                Some(settings.revision),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(run(), "base-value");
    settings.sources = vec![base];
    fixture
        .service
        .db
        .project_key_save_settings(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &settings,
            Some(settings.revision),
        )
        .unwrap();
    std::fs::write(
        fixture.root.join(".env.local"),
        "TOKEN=disconnected-change\n",
    )
    .unwrap();
    assert_eq!(run(), "base-value");
    fixture.save(fixture.key("TOKEN", KeySource::Vault), "manual-override");
    assert_eq!(run(), "manual-override");
}
#[test]
fn legacy_shared_reference_edit_detaches_only_selected_binding_and_plain_build_reads_latest() {
    let fixture = Fixture::new();
    let profile = fixture
        .service
        .db
        .insert_env_profile(&fixture.scope.workspace_id, "custom", "custom")
        .unwrap();
    let fileprofile = fixture
        .service
        .db
        .insert_env_profile(&fixture.scope.workspace_id, ".env", "dotenv")
        .unwrap();
    let meta = storage::CredentialMeta {
        id: "fixture-shared".into(),
        provider: "fixture-provider".into(),
        label: "shared".into(),
        credential_kind: "api_key".into(),
        masked_hint: None,
        workspace_id: None,
    };
    fixture.service.db.insert_credential(&meta).unwrap();
    fixture
        .store
        .set_secret(&meta.id, &SecretString::new("shared-value".into()))
        .unwrap();
    for name in ["TOKEN_A", "TOKEN_B"] {
        fixture
            .service
            .db
            .upsert_env_var(
                &profile,
                name,
                &storage::EnvValue::Secret {
                    credential_id: meta.id.clone(),
                },
            )
            .unwrap();
    }
    fixture
        .service
        .db
        .upsert_env_var(
            &profile,
            "API_URL",
            &storage::EnvValue::Plain("https://fixture.invalid/old".into()),
        )
        .unwrap();
    fixture
        .service
        .db
        .upsert_env_var(
            &fileprofile,
            "NODE_ENV",
            &storage::EnvValue::Plain("stale-config".into()),
        )
        .unwrap();
    std::fs::write(fixture.root.join(".env"), "NODE_ENV=development\n").unwrap();
    let keys = fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap();
    let selected = keys
        .iter()
        .find(|key| {
            key.name == "TOKEN_A"
                && key
                    .legacy_reference
                    .as_deref()
                    .is_some_and(|reference| reference.starts_with("env_var:"))
        })
        .unwrap();
    let changed = fixture
        .service
        .save_key(
            &fixture.scope,
            selected.clone(),
            Some(selected.revision),
            ValueEdit::Replace(SensitiveDraft::new("selected-value".into()).unwrap()),
        )
        .unwrap();
    assert_ne!(changed.credential_id.as_deref(), Some("fixture-shared"));
    let rows = fixture.service.db.list_env_vars(&profile).unwrap();
    assert!(rows.iter().any(|row| row.key == "TOKEN_B"
        && row.value
            == storage::EnvValue::Secret {
                credential_id: "fixture-shared".into()
            }));
    assert!(rows.iter().any(|row| row.key == "TOKEN_A"
        && row.value
            == storage::EnvValue::Secret {
                credential_id: changed.credential_id.clone().unwrap()
            }));
    fixture
        .service
        .db
        .upsert_env_var(
            &profile,
            "API_URL",
            &storage::EnvValue::Plain("https://fixture.invalid/current".into()),
        )
        .unwrap();
    let runner = BuildRunner::default();
    fixture
        .service
        .run_build(
            &fixture.scope,
            None,
            &["fixture".into()],
            &fixture.root,
            &runner,
        )
        .unwrap();
    let values = runner.seen.lock().unwrap();
    assert!(
        values
            .iter()
            .any(|(name, value)| name == "API_URL" && value == "https://fixture.invalid/current")
    );
    assert!(
        values
            .iter()
            .any(|(name, value)| name == "NODE_ENV" && value == "development")
    );
    assert!(
        values
            .iter()
            .any(|(name, value)| name == "TOKEN_A" && value == "selected-value")
    );
}

#[test]
fn project_environment_value_slots_blank_edits_and_stale_replacements_are_isolated() {
    let fixture = Fixture::new();
    let dev = fixture.save(fixture.key("TOKEN", KeySource::Vault), "development-value");
    let prodscope = ProjectKeysScope {
        environment: KeyEnvironment::Production,
        ..fixture.scope.clone()
    };
    let mut prod = fixture.key("TOKEN", KeySource::Vault);
    prod.environment = KeyEnvironment::Production;
    let prod = fixture
        .service
        .save_key(
            &prodscope,
            prod,
            None,
            ValueEdit::Replace(SensitiveDraft::new("production-value".into()).unwrap()),
        )
        .unwrap();
    assert_ne!(dev.credential_id, prod.credential_id);
    assert_eq!(
        fixture
            .service
            .bundle(&fixture.scope, &dev)
            .unwrap()
            .access()
            .expose(),
        "development-value"
    );
    assert_eq!(
        fixture
            .service
            .bundle(&prodscope, &prod)
            .unwrap()
            .access()
            .expose(),
        "production-value"
    );
    let kept = fixture
        .service
        .save_key(
            &fixture.scope,
            dev.clone(),
            Some(dev.revision),
            ValueEdit::Replace(SensitiveDraft::default()),
        )
        .unwrap();
    assert_eq!(
        fixture
            .service
            .bundle(&fixture.scope, &kept)
            .unwrap()
            .access()
            .expose(),
        "development-value"
    );
    assert!(matches!(
        fixture.service.save_key(
            &fixture.scope,
            dev.clone(),
            Some(dev.revision),
            ValueEdit::Replace(SensitiveDraft::new("stale-value".into()).unwrap())
        ),
        Err(ProjectKeysError::Stale)
    ));
    let runner = BuildRunner::default();
    fixture
        .service
        .run_build(
            &fixture.scope,
            None,
            &["fixture".into()],
            &fixture.root,
            &runner,
        )
        .unwrap();
    assert_eq!(runner.seen.lock().unwrap()[0].1, "development-value");
    let json =
        serde_json::to_string(&fixture.service.snapshot(&fixture.scope, 1).unwrap().keys).unwrap();
    assert!(!json.contains("development-value"));
    assert!(!json.contains("production-value"));
}
#[test]
fn file_missing_restore_conflict_and_build_github_use_real_published_generations() {
    let fixture = Fixture::new();
    let source = KeySource::Dotenv {
        file: ".env".into(),
    };
    std::fs::write(fixture.root.join(".env"), "TOKEN=source-value-1\n").unwrap();
    fixture
        .service
        .import_source(&fixture.scope, source.clone(), vec!["TOKEN".into()])
        .unwrap();
    let mut key = fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap()
        .into_iter()
        .find(|key| key.name == "TOKEN")
        .unwrap();
    std::fs::write(fixture.root.join(".env"), "TOKEN=source-value-2\n").unwrap();
    fixture.service.observe_key(&fixture.scope, &key).unwrap();
    key = fixture
        .service
        .db
        .project_key_get(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &key.id,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        fixture
            .service
            .bundle(&fixture.scope, &key)
            .unwrap()
            .access()
            .expose(),
        "source-value-2"
    );
    key = fixture
        .service
        .publish(
            &fixture.scope,
            key.clone(),
            Some(key.revision),
            &SecretString::new("vault-value".into()),
            Some(&SecretString::new("source-value-2".into())),
        )
        .unwrap();
    std::fs::write(fixture.root.join(".env"), "TOKEN=source-value-3\n").unwrap();
    fixture.service.observe_key(&fixture.scope, &key).unwrap();
    key = fixture
        .service
        .db
        .project_key_get(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &key.id,
        )
        .unwrap()
        .unwrap();
    assert_eq!(key.storage_state, KeyStorageState::Conflict);
    assert_eq!(
        fixture
            .service
            .bundle(&fixture.scope, &key)
            .unwrap()
            .access()
            .expose(),
        "vault-value"
    );
    assert!(matches!(
        fixture.service.run_build(
            &fixture.scope,
            None,
            &["fixture".into()],
            &fixture.root,
            &BuildRunner::default()
        ),
        Err(ProjectKeysError::Conflict)
    ));
    key = fixture
        .service
        .resolve(
            &fixture.scope,
            &key.id,
            key.revision,
            ConflictResolution::KeepSource,
        )
        .unwrap();
    std::fs::remove_file(fixture.root.join(".env")).unwrap();
    fixture.service.observe_key(&fixture.scope, &key).unwrap();
    key = fixture
        .service
        .db
        .project_key_get(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &key.id,
        )
        .unwrap()
        .unwrap();
    assert_eq!(key.storage_state, KeyStorageState::MissingSource);
    std::fs::write(fixture.root.join(".env"), "TOKEN=source-value-3\n").unwrap();
    fixture.service.observe_key(&fixture.scope, &key).unwrap();
    key = fixture
        .service
        .db
        .project_key_get(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &key.id,
        )
        .unwrap()
        .unwrap();
    assert_eq!(key.storage_state, KeyStorageState::Stored);
    assert_eq!(key.last_good_revision, Some(key.revision));
    let runner = BuildRunner::default();
    let receipt = fixture
        .service
        .run_build(
            &fixture.scope,
            None,
            &["fixture".into()],
            &fixture.root,
            &runner,
        )
        .unwrap();
    assert_eq!(receipt.used_keys[0].revision, key.revision);
    assert_eq!(runner.seen.lock().unwrap()[0].1, "source-value-3");
    key.github = Some(KeyGitHubStatus {
        target: GitHubTarget {
            auth_credential_id: None,
            account_identity: "fixture-account".into(),
            scope: GitHubScope::Repository {
                owner: "owner".into(),
                repository: "repository".into(),
            },
            secret_name: "TOKEN".into(),
            auto_send: false,
        },
        state: GitHubRemoteState::Unlinked,
        last_sent_revision: None,
        remote_updated_at: None,
        error_code: None,
    });
    key = committed(
        fixture
            .service
            .db
            .project_key_save(
                &fixture.scope.workspace_id,
                fixture.scope.environment,
                &key,
                Some(key.revision),
            )
            .unwrap(),
    )
    .unwrap();
    assert!(
        fixture
            .service
            .db
            .project_key_github_enqueue(
                &fixture.scope.workspace_id,
                fixture.scope.environment,
                &key.id,
                key.revision,
                false,
                now()
            )
            .unwrap()
    );
    let entry = fixture
        .service
        .db
        .project_key_github_claim(now(), 120, 1)
        .unwrap()
        .remove(0);
    let runner = GitHubRunner::new(true);
    assert!(matches!(
        fixture.service.github_send(&entry, &runner).unwrap(),
        GitHubSendOutcome::ExternalChange { .. }
    ));
    assert!(!*runner.sent.lock().unwrap());
    let runner = GitHubRunner::new(false);
    let outcome = fixture.service.github_send(&entry, &runner).unwrap();
    assert!(*runner.sent.lock().unwrap());
    assert!(
        fixture
            .service
            .db
            .project_key_github_ack(&entry, outcome)
            .unwrap()
    );
}
#[test]
fn new_dotenv_name_quote_roundtrip_and_replaced_project_root_are_fenced() {
    let mut fixture = Fixture::new();
    std::fs::write(fixture.root.join(".env"), "EXISTING=fixture\n").unwrap();
    let key = fixture.save(
        fixture.key(
            "NEW_TOKEN",
            KeySource::Dotenv {
                file: ".env".into(),
            },
        ),
        "with\"quote",
    );
    assert_eq!(
        fixture
            .service
            .source_value(&fixture.scope, &key)
            .unwrap()
            .expose(),
        "with\"quote"
    );
    let old = fixture.root.with_extension("old");
    std::fs::rename(&fixture.root, &old).unwrap();
    std::fs::create_dir(&fixture.root).unwrap();
    std::fs::write(fixture.root.join(".env"), "NEW_TOKEN=wrong-project\n").unwrap();
    assert!(matches!(
        fixture.service.source_value(&fixture.scope, &key),
        Err(ProjectKeysError::SourceChanged)
    ));
    std::fs::remove_dir_all(&fixture.root).unwrap();
    fixture.root = old;
}
#[test]
fn pem_import_custom_name_restricted_child_file_is_removed_on_completion() {
    let fixture = Fixture::new();
    let path = fixture.root.join("input.pem");
    std::fs::write(&path, "fixture-pem-bytes").unwrap();
    fixture
        .service
        .import_source(
            &fixture.scope,
            KeySource::PemFile {
                path: path.to_string_lossy().into(),
            },
            vec!["GITHUB_PRIVATE_KEY".into()],
        )
        .unwrap();
    let runner = BuildRunner::default();
    fixture
        .service
        .run_build(
            &fixture.scope,
            None,
            &["fixture".into()],
            &fixture.root,
            &runner,
        )
        .unwrap();
    for path in runner.paths.lock().unwrap().iter() {
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }
}

#[test]
fn required_disconnected_sources_fail_and_optional_removed_values_never_inject() {
    let fixture = Fixture::new();
    std::fs::write(fixture.root.join(".env"), "TOKEN=file-value\n").unwrap();
    let source = KeySource::Dotenv {
        file: ".env".into(),
    };
    fixture
        .service
        .import_source(&fixture.scope, source, vec!["TOKEN".into()])
        .unwrap();
    let mut settings = fixture
        .service
        .db
        .project_key_settings(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap();
    settings.sources.clear();
    fixture
        .service
        .db
        .project_key_save_settings(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &settings,
            Some(settings.revision),
        )
        .unwrap();
    assert!(matches!(
        fixture.service.run_build(
            &fixture.scope,
            None,
            &["fixture".into()],
            Path::new("."),
            &BuildRunner::default()
        ),
        Err(ProjectKeysError::SourceMissing)
    ));
    let mut key = fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap()
        .remove(0);
    key.required = false;
    fixture
        .service
        .db
        .project_key_save(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &key,
            Some(key.revision),
        )
        .unwrap();
    let runner = BuildRunner::default();
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
    assert!(runner.seen.lock().unwrap().is_empty());
    assert!(
        runner
            .removed
            .lock()
            .unwrap()
            .iter()
            .any(|name| name == "TOKEN")
    );
    assert!(receipt.used_keys.is_empty());
}
#[test]
fn build_preflight_enrolls_new_selected_sources_without_watching_or_ui() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.root.join(".env"),
        "API_URL=https://fixture.invalid\nNODE_ENV=development\n",
    )
    .unwrap();
    let mut settings = fixture
        .service
        .db
        .project_key_settings(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap();
    settings.watch_enabled = false;
    fixture
        .service
        .db
        .project_key_save_settings(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &settings,
            Some(settings.revision),
        )
        .unwrap();
    let runner = BuildRunner::default();
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
    assert_eq!(receipt.used_keys.len(), 2);
    let values = runner.seen.lock().unwrap();
    assert!(
        values
            .iter()
            .any(|(name, value)| name == "API_URL" && value == "https://fixture.invalid")
    );
    assert!(
        values
            .iter()
            .any(|(name, value)| name == "NODE_ENV" && value == "development")
    );
}
#[cfg(unix)]
#[test]
fn actual_child_scrubs_optional_missing_and_other_environment_ambient_values() {
    // Put ambient synthetic values only in an isolated test subprocess; mutating the
    // multithreaded test runner's environment would be undefined behavior on Unix.
    const MARKER: &str = "DEPPY_FIXTURE_AMBIENT_TEST_CHILD";
    if std::env::var_os(MARKER).is_none() {
        let status=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","project_keys::tests::actual_child_scrubs_optional_missing_and_other_environment_ambient_values","--test-threads=1"]).env(MARKER,"1").env("DEPPY_FIXTURE_MISSING_AMBIENT","old-shell-token").env("DEPPY_FIXTURE_PRODUCTION_AMBIENT","old-production-token").status().unwrap();
        assert!(status.success());
        return;
    }
    let fixture = Fixture::new();
    let missing_name = "DEPPY_FIXTURE_MISSING_AMBIENT".to_string();
    let production_name = "DEPPY_FIXTURE_PRODUCTION_AMBIENT".to_string();
    let mut missing = fixture.key(
        &missing_name,
        KeySource::Dotenv {
            file: ".env".into(),
        },
    );
    missing.required = false;
    fixture
        .service
        .db
        .project_key_save(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &missing,
            None,
        )
        .unwrap();
    let prodscope = ProjectKeysScope {
        environment: KeyEnvironment::Production,
        ..fixture.scope.clone()
    };
    let mut prod = fixture.key(&production_name, KeySource::Vault);
    prod.environment = KeyEnvironment::Production;
    fixture
        .service
        .save_key(
            &prodscope,
            prod,
            None,
            ValueEdit::Replace(SensitiveDraft::new("production-only".into()).unwrap()),
        )
        .unwrap();
    let output = fixture.root.join("child-values");
    let script = format!(
        "test -z \"${{{missing_name}+x}}\" && test -z \"${{{production_name}+x}}\" && printf clean > \"$1\""
    );
    let command = vec![
        "/bin/sh".into(),
        "-c".into(),
        script,
        "fixture".into(),
        output.to_string_lossy().into(),
    ];
    let receipt = fixture
        .service
        .run_build(
            &fixture.scope,
            None,
            &command,
            Path::new("."),
            &NativeProcessRunner::default(),
        )
        .unwrap();
    assert_eq!(receipt.exit_code, Some(0));
    assert_eq!(std::fs::read_to_string(output).unwrap(), "clean");
    assert!(receipt.used_keys.is_empty());
}
#[test]
fn advanced_plain_and_secret_rotation_after_enrollment_uses_current_generation() {
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
            "CURRENT_CONFIG",
            &storage::EnvValue::Plain("original-config".into()),
        )
        .unwrap();
    let runner = BuildRunner::default();
    fixture
        .service
        .run_build(
            &fixture.scope,
            None,
            &["fixture".into()],
            Path::new("."),
            &runner,
        )
        .unwrap();
    assert!(
        runner
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|(name, value)| name == "CURRENT_CONFIG" && value == "original-config")
    );
    let old = fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap()
        .into_iter()
        .find(|key| key.name == "CURRENT_CONFIG")
        .unwrap();
    fixture
        .service
        .db
        .upsert_env_var(
            &profile,
            "CURRENT_CONFIG",
            &storage::EnvValue::Plain("edited-config".into()),
        )
        .unwrap();
    assert!(matches!(
        fixture.service.bundle(&fixture.scope, &old),
        Err(ProjectKeysError::Stale)
    ));
    let runner = BuildRunner::default();
    fixture
        .service
        .run_build(
            &fixture.scope,
            None,
            &["fixture".into()],
            Path::new("."),
            &runner,
        )
        .unwrap();
    assert!(
        runner
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|(name, value)| name == "CURRENT_CONFIG" && value == "edited-config")
    );
    let meta = storage::CredentialMeta {
        id: "fixture-replacement".into(),
        provider: "fixture".into(),
        label: "replacement".into(),
        credential_kind: "api_key".into(),
        masked_hint: None,
        workspace_id: Some(fixture.scope.workspace_id.clone()),
    };
    fixture.service.db.insert_credential(&meta).unwrap();
    fixture
        .store
        .set_secret(&meta.id, &SecretString::new("edited-secret".into()))
        .unwrap();
    fixture
        .service
        .db
        .upsert_env_var(
            &profile,
            "CURRENT_CONFIG",
            &storage::EnvValue::Secret {
                credential_id: meta.id,
            },
        )
        .unwrap();
    fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap();
    assert!(
        fixture
            .service
            .db
            .project_key_managed_ids(&fixture.scope.workspace_id, fixture.scope.environment,)
            .unwrap()
            .contains(&old.id),
        "protected enrollment stays managed after an Advanced credential rebind"
    );
    let runner = BuildRunner::default();
    fixture
        .service
        .run_build(
            &fixture.scope,
            None,
            &["fixture".into()],
            Path::new("."),
            &runner,
        )
        .unwrap();
    assert!(
        runner
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|(name, value)| name == "CURRENT_CONFIG" && value == "edited-secret")
    );
}

#[test]
fn explicit_source_import_reuses_existing_legacy_row_and_selected_binding() {
    let fixture = Fixture::new();
    let profile = fixture
        .service
        .db
        .insert_env_profile(&fixture.scope.workspace_id, ".env", "dotenv")
        .unwrap();
    fixture
        .service
        .db
        .upsert_env_var(
            &profile,
            "API_URL",
            &storage::EnvValue::Plain("https://fixture.invalid/old".into()),
        )
        .unwrap();
    let meta = storage::CredentialMeta {
        id: "fixture-old-token".into(),
        provider: "fixture".into(),
        label: "token".into(),
        credential_kind: "api_key".into(),
        masked_hint: None,
        workspace_id: Some(fixture.scope.workspace_id.clone()),
    };
    fixture.service.db.insert_credential(&meta).unwrap();
    fixture
        .store
        .set_secret(&meta.id, &SecretString::new("old-token".into()))
        .unwrap();
    fixture
        .service
        .db
        .upsert_env_var(
            &profile,
            "TOKEN",
            &storage::EnvValue::Secret {
                credential_id: meta.id,
            },
        )
        .unwrap();
    std::fs::write(
        fixture.root.join(".env"),
        "API_URL=https://fixture.invalid/current\nTOKEN=current-file-token\n",
    )
    .unwrap();
    let before = fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap();
    fixture
        .service
        .import_source(
            &fixture.scope,
            KeySource::Dotenv {
                file: ".env".into(),
            },
            vec!["API_URL".into(), "TOKEN".into()],
        )
        .unwrap();
    let after = fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap();
    assert_eq!(after.len(), 2);
    let rows = fixture.service.db.list_env_vars(&profile).unwrap();
    assert_eq!(rows.len(), 2);
    for before in before {
        let after = after.iter().find(|key| key.name == before.name).unwrap();
        assert_eq!(after.id, before.id);
        assert_eq!(after.kind, before.kind);
        assert_eq!(after.usage, before.usage);
        assert_eq!(after.legacy_reference, before.legacy_reference);
        let expected = if after.name == "TOKEN" {
            "current-file-token"
        } else {
            "https://fixture.invalid/current"
        };
        assert_eq!(
            fixture
                .service
                .bundle(&fixture.scope, after)
                .unwrap()
                .access()
                .expose(),
            expected
        );
        assert_eq!(
            rows.iter().find(|row| row.key == after.name).unwrap().value,
            storage::EnvValue::Secret {
                credential_id: after.credential_id.clone().unwrap()
            }
        );
    }
}

#[test]
fn unlinked_remote_only_registration_is_metadata_only_and_never_reads_a_local_value() {
    let fixture = Fixture::new();
    let before = fixture.store.2.load(std::sync::atomic::Ordering::Acquire);
    let key = fixture.key("REMOTE_TOKEN", KeySource::GitHubOnly);
    let key = fixture
        .service
        .save_key(&fixture.scope, key, None, ValueEdit::Keep)
        .unwrap();
    assert_eq!(key.usage, KeyUsage::StoreOnly);
    assert!(!key.required);
    assert!(key.credential_id.is_none());
    assert!(key.last_good_revision.is_none());
    assert!(key.last_good_at.is_none());
    assert!(key.github.is_none());
    let snapshot = fixture.service.snapshot(&fixture.scope, 1).unwrap();
    assert_eq!(snapshot.keys.len(), 1);
    assert_eq!(snapshot.keys[0].source, KeySource::GitHubOnly);
    assert!(matches!(
        fixture.service.bundle(&fixture.scope, &key),
        Err(ProjectKeysError::Locked)
    ));
    assert_eq!(
        fixture.store.2.load(std::sync::atomic::Ordering::Acquire),
        before
    );
    let key = link(&fixture, key, target());
    assert!(key.github.is_some());
    assert_eq!(
        fixture.store.2.load(std::sync::atomic::Ordering::Acquire),
        before
    );
}

#[test]
fn review_failed_file_save_preserves_original_for_duplicate_and_locked_store() {
    let fixture = Fixture::new();
    let source = KeySource::Dotenv {
        file: ".env".into(),
    };
    std::fs::write(fixture.root.join(".env"), "TOKEN=old-value\n").unwrap();
    fixture
        .service
        .import_source(&fixture.scope, source.clone(), vec!["TOKEN".into()])
        .unwrap();
    let duplicate = fixture.key("TOKEN", source.clone());
    assert!(
        fixture
            .service
            .save_key(
                &fixture.scope,
                duplicate,
                None,
                ValueEdit::Replace(SensitiveDraft::new("duplicate-value".into()).unwrap())
            )
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(".env")).unwrap(),
        "TOKEN=old-value\n"
    );
    let key = fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap()
        .pop()
        .unwrap();
    fixture
        .store
        .1
        .store(true, std::sync::atomic::Ordering::Release);
    assert!(
        fixture
            .service
            .save_key(
                &fixture.scope,
                key.clone(),
                Some(key.revision),
                ValueEdit::Replace(SensitiveDraft::new("locked-value".into()).unwrap())
            )
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(".env")).unwrap(),
        "TOKEN=old-value\n"
    );
}

#[test]
fn review_fresh_empty_names_are_registered_and_later_empty_suppresses_fallback() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.root.join(".env"),
        "TOKEN=earlier-value\nFRESH_EMPTY=\n",
    )
    .unwrap();
    std::fs::write(fixture.root.join(".env.local"), "TOKEN=\n").unwrap();
    fixture.service.refresh_scope(&fixture.scope).unwrap();
    let keys = fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap();
    let fresh = keys
        .iter()
        .find(|key| key.name == "FRESH_EMPTY")
        .expect("empty source name must be metadata");
    assert_eq!(fresh.storage_state, KeyStorageState::EmptySource);
    assert!(fresh.credential_id.is_none());
    assert!(keys.iter().any(|key| key.name == "TOKEN"
        && matches!(&key.source, KeySource::Dotenv {file} if file == ".env.local")
        && key.storage_state == KeyStorageState::EmptySource));
    // Make the selected effective name optional: omission must still scrub the shell and must
    // not fall back to an earlier file's nonempty value.
    for mut key in keys.into_iter().filter(|key| key.name == "TOKEN") {
        let revision = key.revision;
        key.required = false;
        committed(
            fixture
                .service
                .db
                .project_key_save(
                    &fixture.scope.workspace_id,
                    fixture.scope.environment,
                    &key,
                    Some(revision),
                )
                .unwrap(),
        )
        .unwrap();
    }
    let runner = BuildRunner::default();
    fixture
        .service
        .run_build(
            &fixture.scope,
            None,
            &["fixture".into()],
            Path::new("."),
            &runner,
        )
        .unwrap();
    assert!(
        runner
            .removed
            .lock()
            .unwrap()
            .contains(&"FRESH_EMPTY".into())
    );
    assert!(runner.removed.lock().unwrap().contains(&"TOKEN".into()));
    assert!(
        !runner
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|(name, _)| name == "TOKEN")
    );
}

#[test]
fn review_deleted_file_name_stays_excluded_until_explicit_import() {
    let fixture = Fixture::new();
    let source = KeySource::Dotenv {
        file: ".env".into(),
    };
    std::fs::write(fixture.root.join(".env"), "TOKEN=source-value\n").unwrap();
    fixture
        .service
        .import_source(&fixture.scope, source.clone(), vec!["TOKEN".into()])
        .unwrap();
    let key = fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap()
        .pop()
        .unwrap();
    committed(
        fixture
            .service
            .db
            .project_key_delete(
                &fixture.scope.workspace_id,
                fixture.scope.environment,
                &key.id,
                key.revision,
            )
            .unwrap(),
    )
    .unwrap();
    fixture.service.refresh_scope(&fixture.scope).unwrap();
    assert!(
        fixture
            .service
            .db
            .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
            .unwrap()
            .is_empty()
    );
    fixture
        .service
        .import_source(&fixture.scope, source, vec!["TOKEN".into()])
        .unwrap();
    assert_eq!(
        fixture
            .service
            .db
            .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
            .unwrap()
            .len(),
        1
    );
    assert!(
        fixture
            .service
            .db
            .project_key_source_exclusions(&fixture.scope.workspace_id, fixture.scope.environment)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn legacy_sync_interop_keeps_managed_protected_baseline_and_unregistered_imports() {
    let mut fixture = Fixture::new();
    let file = fixture.root.join(".env");
    std::fs::write(&file, "TOKEN=old-source-value\n").unwrap();
    crate::app::sync_workspace_dotenv_at_root(
        &mut fixture.service.db,
        fixture.store.as_ref(),
        &secret::RedactionService::new(),
        &fixture.scope.workspace_id,
        &fixture.root,
    )
    .unwrap();
    fixture.service.refresh_scope(&fixture.scope).unwrap();
    std::fs::write(&file, "TOKEN=new-source-value\n").unwrap();
    fixture.service.refresh_scope(&fixture.scope).unwrap();
    let before = fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap()
        .into_iter()
        .find(|key| key.name == "TOKEN")
        .unwrap();
    assert_eq!(before.usage, KeyUsage::LegacyAgent);
    let pointer = fixture
        .service
        .db
        .credential_secret_location(before.credential_id.as_deref().unwrap())
        .unwrap()
        .unwrap()
        .keyring_username;
    crate::app::sync_workspace_dotenv_at_root(
        &mut fixture.service.db,
        fixture.store.as_ref(),
        &secret::RedactionService::new(),
        &fixture.scope.workspace_id,
        &fixture.root,
    )
    .unwrap();
    let after = fixture
        .service
        .db
        .project_key_get(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &before.id,
        )
        .unwrap()
        .unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(
        fixture
            .service
            .db
            .credential_secret_location(after.credential_id.as_deref().unwrap())
            .unwrap()
            .unwrap()
            .keyring_username,
        pointer
    );
    let bundle = fixture.service.bundle(&fixture.scope, &after).unwrap();
    assert_eq!(bundle.access().expose(), "new-source-value");
    assert_eq!(bundle.refresh().unwrap().expose(), "new-source-value");
}
#[test]
fn legacy_sync_interop_build_only_source_is_not_promoted_but_unregistered_name_is() {
    let mut fixture = Fixture::new();
    let source = KeySource::Dotenv {
        file: ".env".into(),
    };
    std::fs::write(
        fixture.root.join(".env"),
        "BUILD_ONLY=build-value\nUNREGISTERED_CONFIG=traditional-value\n",
    )
    .unwrap();
    fixture
        .service
        .import_source(&fixture.scope, source, vec!["BUILD_ONLY".into()])
        .unwrap();
    crate::app::sync_workspace_dotenv_at_root(
        &mut fixture.service.db,
        fixture.store.as_ref(),
        &secret::RedactionService::new(),
        &fixture.scope.workspace_id,
        &fixture.root,
    )
    .unwrap();
    let profile = fixture
        .service
        .db
        .list_env_profiles(&fixture.scope.workspace_id)
        .unwrap()
        .into_iter()
        .find(|profile| profile.kind == "dotenv")
        .unwrap();
    let vars = fixture.service.db.list_env_vars(&profile.id).unwrap();
    assert!(!vars.iter().any(|row| row.key == "BUILD_ONLY"));
    assert!(vars.iter().any(|row| row.key == "UNREGISTERED_CONFIG"));
}

#[test]
fn review_file_mark_applied_error_rolls_back_before_return() {
    let fixture = Fixture::new();
    let source = KeySource::Dotenv {
        file: ".env".into(),
    };
    std::fs::write(fixture.root.join(".env"), "TOKEN=old-value\n").unwrap();
    fixture
        .service
        .import_source(&fixture.scope, source, vec!["TOKEN".into()])
        .unwrap();
    let key = fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap()
        .pop()
        .unwrap();
    let conn = rusqlite::Connection::open(fixture.root.join("metadata.sqlite3")).unwrap();
    conn.execute_batch("CREATE TRIGGER fixture_mark_applied_failure BEFORE UPDATE OF phase ON project_key_file_journals WHEN NEW.phase='file_applied' BEGIN SELECT RAISE(ABORT,'fixture_failure'); END;").unwrap();
    assert_eq!(
        fixture.service.save_key(
            &fixture.scope,
            key.clone(),
            Some(key.revision),
            ValueEdit::Replace(SensitiveDraft::new("failed-value".into()).unwrap())
        ),
        Err(ProjectKeysError::Storage)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(".env")).unwrap(),
        "TOKEN=old-value\n"
    );
    let current = fixture
        .service
        .key(&fixture.scope, &key.id, key.revision)
        .unwrap();
    assert_eq!(
        fixture
            .service
            .bundle(&fixture.scope, &current)
            .unwrap()
            .access()
            .expose(),
        "old-value"
    );
    assert!(
        fixture
            .service
            .db
            .project_key_file_journals_page(16, None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn initial_legacy_merged_profile_uses_winning_source_and_exact_agent_default() {
    let mut fixture = Fixture::new();
    std::fs::write(fixture.root.join(".env"), "TOKEN=base-source-value\n").unwrap();
    std::fs::write(
        fixture.root.join(".env.local"),
        "TOKEN=winning-source-value\n",
    )
    .unwrap();
    crate::app::sync_workspace_dotenv_at_root(
        &mut fixture.service.db,
        fixture.store.as_ref(),
        &secret::RedactionService::new(),
        &fixture.scope.workspace_id,
        &fixture.root,
    )
    .unwrap();
    fixture.service.refresh_scope(&fixture.scope).unwrap();
    let legacy = fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap()
        .into_iter()
        .find(|key| key.usage == KeyUsage::LegacyAgent && key.name == "TOKEN")
        .unwrap();
    assert_eq!(
        legacy.source,
        KeySource::Dotenv {
            file: ".env.local".into()
        }
    );
    let runner = BuildRunner::default();
    fixture
        .service
        .run_build(
            &fixture.scope,
            None,
            &["fixture".into()],
            Path::new("."),
            &runner,
        )
        .unwrap();
    assert!(
        runner
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|(name, value)| name == "TOKEN" && value == "winning-source-value")
    );
    let project_defaults = |fixture: &Fixture| {
        let mut plain = vec![("TOKEN".into(), "stale-plain".into())];
        let mut secrets = vec![("TOKEN".into(), "stale-id".into())];
        let mut api = vec![];
        let excluded = crate::app::project_keys_legacy_default_overlay(
            (&fixture.service.db, fixture.store.as_ref()),
            &fixture.scope.workspace_id,
            &mut plain,
            &mut secrets,
            &mut api,
            false,
        )
        .unwrap();
        assert!(excluded.contains(&"TOKEN".into()));
        assert!(!plain.iter().any(|(name, _)| name == "TOKEN"));
        (secrets, api)
    };
    let (secret_defaults, _) = project_defaults(&fixture);
    assert!(
        secret_defaults
            .iter()
            .any(|(name, id)| name == "TOKEN" && Some(id) == legacy.credential_id.as_ref())
    );
    std::fs::write(
        fixture.root.join(".env.local"),
        "TOKEN=rotated-winning-value\n",
    )
    .unwrap();
    fixture.service.refresh_scope(&fixture.scope).unwrap();
    let key = fixture
        .service
        .db
        .project_key_get(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &legacy.id,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        fixture
            .service
            .bundle(&fixture.scope, &key)
            .unwrap()
            .access()
            .expose(),
        "rotated-winning-value"
    );
    project_defaults(&fixture);
    std::fs::write(fixture.root.join(".env.local"), "TOKEN=\n").unwrap();
    fixture.service.refresh_scope(&fixture.scope).unwrap();
    let (secrets, api) = project_defaults(&fixture);
    assert!(!secrets.iter().chain(&api).any(|(name, _)| name == "TOKEN"));
}

#[test]
fn review_file_publication_error_rolls_back_and_dead_owner_journal_recovers_before_import() {
    let fixture = Fixture::new();
    let source = KeySource::Dotenv {
        file: ".env".into(),
    };
    std::fs::write(fixture.root.join(".env"), "TOKEN=old-value\n").unwrap();
    fixture
        .service
        .import_source(&fixture.scope, source.clone(), vec!["TOKEN".into()])
        .unwrap();
    let key = fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap()
        .pop()
        .unwrap();
    let conn = rusqlite::Connection::open(fixture.root.join("metadata.sqlite3")).unwrap();
    conn.execute_batch("CREATE TRIGGER fixture_commit_failure BEFORE UPDATE ON project_keys BEGIN SELECT RAISE(ABORT,'fixture_failure'); END;").unwrap();
    assert_eq!(
        fixture.service.save_key(
            &fixture.scope,
            key.clone(),
            Some(key.revision),
            ValueEdit::Replace(SensitiveDraft::new("failed-value".into()).unwrap())
        ),
        Err(ProjectKeysError::Storage)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(".env")).unwrap(),
        "TOKEN=old-value\n"
    );
    assert!(
        fixture
            .service
            .db
            .project_key_file_journals_page(16, None)
            .unwrap()
            .is_empty()
    );
    conn.execute_batch("DROP TRIGGER fixture_commit_failure;")
        .unwrap();
    let root = fixture.service.root_handle(&fixture.scope).unwrap();
    let mut stage = super::sources::prepare_source_write(
        &root,
        &source,
        "TOKEN",
        &SecretString::new("interrupted-value".into()),
        Some(&SecretString::new("old-value".into())),
    )
    .unwrap();
    let identity = stage.recovery_identity();
    let id = uuid::Uuid::new_v4().to_string();
    let plan = secret::SecretBundleStagePlan::allocate(
        secret::LogicalCredentialId::new(&id).unwrap(),
        None,
    )
    .unwrap();
    fixture
        .service
        .db
        .register_physical_secret_slot_staging(&id, plan.new_slot().as_str())
        .unwrap();
    secret::stage_secret_bundle(
        fixture.store.as_ref(),
        &plan,
        secret::SecretBundleRef::new(stage.original(), Some(stage.replacement()), Some(&identity)),
    )
    .unwrap();
    committed(
        fixture
            .service
            .db
            .project_key_file_journal_begin(
                &fixture.scope.workspace_id,
                fixture.scope.environment,
                &key,
                Some(key.revision),
                &id,
                plan.new_slot().as_str(),
                now(),
            )
            .unwrap(),
    )
    .unwrap();
    stage.apply().unwrap();
    fixture
        .service
        .db
        .project_key_file_journal_mark_applied(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &id,
        )
        .unwrap();
    let mut cursor = None;
    fixture.service.recover_file_journals(&mut cursor).unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(".env")).unwrap(),
        "TOKEN=\"interrupted-value\"\n"
    );
    assert_eq!(
        fixture.service.preview(&fixture.scope, source.clone()),
        Err(ProjectKeysError::SourceChanged)
    );
    let legacy_profile = fixture
        .service
        .db
        .insert_env_profile(&fixture.scope.workspace_id, "unregistered", "local")
        .unwrap();
    fixture
        .service
        .db
        .upsert_env_var(
            &legacy_profile,
            "GREETING",
            &storage::EnvValue::Plain("hello".into()),
        )
        .unwrap();
    let mut legacy_service = open_service(
        &fixture.root.join("metadata.sqlite3"),
        fixture.store.clone(),
    )
    .unwrap();
    assert!(
        crate::dotenv_sync::migrate_legacy_profiles_to_dotenv(
            &mut legacy_service.db,
            fixture.store.as_ref(),
            &fixture.scope.workspace_id,
            &fixture.root,
        )
        .is_err(),
        "automatic migration cannot modify a journal-reserved source"
    );
    // A nonexistent maximum positive PID cannot identify a live execution. The exact stored
    // birth still participates in the recovery owner check; no epoch comparison is used.
    let dead = deppy_core::process_identity::ProcessIdentity {
        pid: i32::MAX as u32,
        birth: deppy_core::process_identity::ProcessBirth {
            seconds: 1,
            microseconds: 0,
        },
    };
    conn.execute(
        "UPDATE project_key_file_journals SET owner_json=?1 WHERE id=?2",
        rusqlite::params![serde_json::to_string(&dead).unwrap(), id],
    )
    .unwrap();
    cursor = None;
    fixture.service.recover_file_journals(&mut cursor).unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(".env")).unwrap(),
        "TOKEN=old-value\n"
    );
    assert!(
        fixture
            .service
            .db
            .project_key_file_journals_page(16, None)
            .unwrap()
            .is_empty()
    );
    fixture.service.refresh_scope(&fixture.scope).unwrap();
    let current = fixture
        .service
        .key(&fixture.scope, &key.id, key.revision)
        .unwrap();
    assert_eq!(
        fixture
            .service
            .bundle(&fixture.scope, &current)
            .unwrap()
            .access()
            .expose(),
        "old-value"
    );
}

#[test]
fn protected_legacy_configuration_keeps_short_plain_runtime_semantics() {
    let fixture = Fixture::new();
    let profile = fixture
        .service
        .db
        .insert_env_profile(&fixture.scope.workspace_id, "custom", "custom")
        .unwrap();
    fixture
        .service
        .db
        .upsert_env_var(&profile, "FLAG", &storage::EnvValue::Plain("1".into()))
        .unwrap();
    fixture.service.refresh_scope(&fixture.scope).unwrap();
    let mut plain = Vec::new();
    let mut secrets = Vec::new();
    let mut api = Vec::new();
    crate::app::project_keys_legacy_default_overlay(
        (&fixture.service.db, fixture.store.as_ref()),
        &fixture.scope.workspace_id,
        &mut plain,
        &mut secrets,
        &mut api,
        false,
    )
    .unwrap();
    assert_eq!(plain, vec![("FLAG".into(), "1".into())]);
    assert!(secrets.is_empty() && api.is_empty());
}

#[test]
fn legacy_migration_preserves_production_and_managed_profiles() {
    let mut fixture = Fixture::new();
    let production = fixture
        .service
        .db
        .insert_env_profile(&fixture.scope.workspace_id, "production", "production")
        .unwrap();
    fixture
        .service
        .db
        .upsert_env_var(
            &production,
            "NODE_ENV",
            &storage::EnvValue::Plain("production".into()),
        )
        .unwrap();
    let managed = fixture
        .service
        .db
        .insert_env_profile(&fixture.scope.workspace_id, "managed", "custom")
        .unwrap();
    fixture
        .service
        .db
        .upsert_env_var(&managed, "FLAG", &storage::EnvValue::Plain("1".into()))
        .unwrap();
    fixture.service.refresh_scope(&fixture.scope).unwrap();
    let ordinary = fixture
        .service
        .db
        .insert_env_profile(&fixture.scope.workspace_id, "old-unregistered", "local")
        .unwrap();
    fixture
        .service
        .db
        .upsert_env_var(
            &ordinary,
            "GREETING",
            &storage::EnvValue::Plain("hello".into()),
        )
        .unwrap();
    let migrated = crate::dotenv_sync::migrate_legacy_profiles_to_dotenv(
        &mut fixture.service.db,
        fixture.store.as_ref(),
        &fixture.scope.workspace_id,
        &fixture.root,
    )
    .unwrap();
    assert_eq!(
        migrated, 1,
        "only the unregistered development row migrates"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(".env")).unwrap(),
        "GREETING=hello\n"
    );
    assert_eq!(
        fixture.service.db.list_env_vars(&production).unwrap().len(),
        1
    );
    assert_eq!(fixture.service.db.list_env_vars(&managed).unwrap().len(), 1);
}

#[test]
fn development_sync_and_unlink_preserve_production_dotenv_profile() {
    let mut fixture = Fixture::new();
    let production = fixture
        .service
        .db
        .insert_env_profile(&fixture.scope.workspace_id, ".env.production", "dotenv")
        .unwrap();
    let conn = rusqlite::Connection::open(fixture.root.join("metadata.sqlite3")).unwrap();
    conn.execute(
        "UPDATE env_profiles SET is_production=1 WHERE id=?1",
        [&production],
    )
    .unwrap();
    fixture
        .service
        .db
        .upsert_env_var(
            &production,
            "NODE_ENV",
            &storage::EnvValue::Plain("production".into()),
        )
        .unwrap();
    std::fs::write(fixture.root.join(".env"), "NODE_ENV=development\n").unwrap();
    crate::app::sync_workspace_dotenv_at_root(
        &mut fixture.service.db,
        fixture.store.as_ref(),
        &secret::RedactionService::new(),
        &fixture.scope.workspace_id,
        &fixture.root,
    )
    .unwrap();
    assert_eq!(
        fixture.service.db.list_env_vars(&production).unwrap()[0].value,
        storage::EnvValue::Plain("production".into())
    );
    let profiles = fixture
        .service
        .db
        .list_env_profiles(&fixture.scope.workspace_id)
        .unwrap();
    let development = profiles
        .iter()
        .find(|profile| profile.kind == "dotenv" && !profile.is_production)
        .unwrap();
    assert_eq!(
        fixture.service.db.list_env_vars(&development.id).unwrap()[0].value,
        storage::EnvValue::Plain("development".into())
    );
    assert_eq!(
        crate::dotenv_sync::remove_workspace_dotenv(
            &mut fixture.service.db,
            fixture.store.as_ref(),
            &fixture.scope.workspace_id,
        )
        .unwrap(),
        1
    );
    assert_eq!(
        fixture.service.db.list_env_vars(&production).unwrap()[0].value,
        storage::EnvValue::Plain("production".into())
    );
    assert!(
        fixture
            .service
            .db
            .list_env_profiles(&fixture.scope.workspace_id)
            .unwrap()
            .iter()
            .any(|profile| profile.id == production)
    );
}

#[test]
fn explicit_keep_claims_lazy_legacy_metadata_without_claiming_source_validation() {
    let mut fixture = Fixture::new();
    std::fs::write(
        fixture.root.join(".env"),
        "API_URL=https://fixture.invalid/current\n",
    )
    .unwrap();
    crate::app::sync_workspace_dotenv_at_root(
        &mut fixture.service.db,
        fixture.store.as_ref(),
        &secret::RedactionService::new(),
        &fixture.scope.workspace_id,
        &fixture.root,
    )
    .unwrap();
    let mut key = fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap()
        .into_iter()
        .find(|key| key.name == "API_URL")
        .unwrap();
    assert!(key.source_checked_at.is_none());
    let revision = key.revision;
    key.required = true;
    let saved = fixture
        .service
        .save_key(&fixture.scope, key, Some(revision), ValueEdit::Keep)
        .unwrap();
    assert!(
        saved.source_checked_at.is_none(),
        "metadata Keep does not verify an original or read a secret"
    );
    std::fs::write(fixture.root.join(".env"), "").unwrap();
    crate::app::sync_workspace_dotenv_at_root(
        &mut fixture.service.db,
        fixture.store.as_ref(),
        &secret::RedactionService::new(),
        &fixture.scope.workspace_id,
        &fixture.root,
    )
    .unwrap();
    let current = fixture
        .service
        .db
        .project_key_get(
            &fixture.scope.workspace_id,
            fixture.scope.environment,
            &saved.id,
        )
        .unwrap()
        .expect("ordinary legacy prune must retain explicitly saved metadata");
    assert!(current.required);
    assert!(current.source_checked_at.is_none());
}

#[test]
fn late_prepared_legacy_writer_cannot_promote_or_leave_an_orphan_credential() {
    let mut fixture = Fixture::new();
    let source = KeySource::Dotenv {
        file: ".env".into(),
    };
    std::fs::write(fixture.root.join(".env"), "TOKEN=source-value\n").unwrap();
    let store = fixture.store.clone();
    let path = fixture.root.join("metadata.sqlite3");
    let scope = fixture.scope.clone();
    *fixture.store.3.lock().unwrap() = Some(Box::new(move || {
        // Enrollment happens after the old writer has prepared and inspected an unowned row,
        // at its actual secret-store seam. The SQL binding guard must observe the new ownership.
        let service = open_service(&path, store).unwrap();
        service
            .import_source(&scope, source, vec!["TOKEN".into()])
            .unwrap();
    }));
    crate::app::sync_workspace_dotenv_at_root(
        &mut fixture.service.db,
        fixture.store.as_ref(),
        &secret::RedactionService::new(),
        &fixture.scope.workspace_id,
        &fixture.root,
    )
    .unwrap();
    let profiles = fixture
        .service
        .db
        .list_env_profiles(&fixture.scope.workspace_id)
        .unwrap();
    for profile in profiles {
        assert!(
            fixture
                .service
                .db
                .list_env_vars(&profile.id)
                .unwrap()
                .is_empty()
        );
    }
    let keys = fixture
        .service
        .db
        .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
        .unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].usage, KeyUsage::Build);
    let records = fixture
        .service
        .db
        .list_credential_secret_records(256)
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].meta.provider, "project-key");
}
