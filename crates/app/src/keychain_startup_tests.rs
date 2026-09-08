#[cfg(test)]
mod tests {
    use super::super::*;
    use keyring_core::api::CredentialStoreApi;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static STORE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct CountingStore {
        calls: AtomicUsize,
        on_access: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
        inner: Arc<keyring_core::mock::Store>,
    }

    impl CountingStore {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                on_access: std::sync::Mutex::new(None),
                inner: keyring_core::mock::Store::new().unwrap(),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl CredentialStoreApi for CountingStore {
        fn vendor(&self) -> String {
            "deppy-test-counter".into()
        }
        fn id(&self) -> String {
            "startup-keychain".into()
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn build(
            &self,
            service: &str,
            user: &str,
            modifiers: Option<&HashMap<&str, &str>>,
        ) -> keyring_core::Result<keyring_core::Entry> {
            if let Some(callback) = self.on_access.lock().unwrap().take() {
                callback();
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.build(service, user, modifiers)
        }
        fn search(
            &self,
            spec: &HashMap<&str, &str>,
        ) -> keyring_core::Result<Vec<keyring_core::Entry>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.search(spec)
        }
    }

    struct StoreGuard(Option<Arc<keyring_core::CredentialStore>>);
    impl StoreGuard {
        fn install(store: Arc<CountingStore>) -> Self {
            let old = keyring_core::get_default_store();
            keyring_core::set_default_store(store);
            Self(old)
        }
    }
    impl Drop for StoreGuard {
        fn drop(&mut self) {
            if let Some(old) = self.0.take() {
                keyring_core::set_default_store(old);
            } else {
                keyring_core::unset_default_store();
            }
        }
    }

    #[test]
    fn keychain_startup_actual_app_relay_off_and_settings_load_do_not_access_store() {
        let _serial = STORE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir =
            std::env::temp_dir().join(format!("deppy-keychain-startup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        let db = Db::open(&path).unwrap();
        let workspace_id = db.ensure_default_workspace().unwrap();
        db.insert_credential(&storage::CredentialMeta {
            id: uuid::Uuid::new_v4().to_string(),
            provider: "legacy".into(),
            label: "startup".into(),
            credential_kind: "api_key".into(),
            masked_hint: None,
            workspace_id: None,
        })
        .unwrap();
        let spy = CountingStore::new();
        let _store_guard = StoreGuard::install(Arc::clone(&spy));
        let mut config = Config::default();
        config.ui.agent_status_hooks = false;
        config.web.enabled = false;
        config.relay.enabled = false;
        let run_lock = Arc::new(persist::LockFile::acquire(&dir.join("deppy.lock")).unwrap());
        let mut app = App::new(
            config,
            dir.join("config.toml"),
            db,
            workspace_id.clone(),
            dir.join("logs"),
            path.clone(),
            egui::Context::default(),
            None,
            run_lock,
        );
        let startup_calls = spy.calls();
        assert!(app.agent_sessions_secrets_snapshot.is_available());
        assert!(app.agent_sessions_secrets_snapshot.can_delete_api_key());
        app.relay_disable();
        let off_calls = spy.calls();
        let outcome = execute_settings_job(
            &mut app.db,
            &path,
            &secret::RedactionService::new(),
            SettingsJob {
                generation: 1,
                revision: 1,
                workspace_id: workspace_id.clone(),
                project_root: None,
                action: SettingsJobAction::Load,
            },
        );
        assert!(matches!(outcome.kind, SettingsOutcomeKind::Loaded));
        let context = egui::Context::default();
        app.agent_sessions_ui.open();
        for _ in 0..3 {
            context
                .run_ui(egui::RawInput::default(), |_| {
                    let output = app.agent_sessions_ui.show(
                        &context,
                        ui::agent_sessions::AgentSessionsFrameInput {
                            workspace_id: &workspace_id,
                            workspace_cwd: None,
                            pty_surfaces: Vec::new(),
                            agents_config: &mut app.config.agents,
                            secrets_snapshot: &app.agent_sessions_secrets_snapshot,
                            ollama_models: None,
                        },
                    );
                    assert!(output.secret_intent.is_none());
                    let remote = ui::settings::RemoteView {
                        running: false,
                        addr: None,
                        fingerprint: None,
                        token: None,
                        error: None,
                        known_hosts_path: String::new(),
                        known_hosts: &[],
                    };
                    let web = ui::settings::WebRemoteView {
                        running: false,
                        addr: None,
                        url: None,
                        error: None,
                        ts_detect: ui::settings::TsDetectView::Idle,
                        serve: ui::settings::ServeView::Idle,
                    };
                    let relay = ui::settings::RelayView {
                        running: false,
                        connection: ui::settings::RelayConnectionView::Disabled,
                        error: None,
                        pairing: ui::settings::RelayPairingView::Idle,
                        devices: &[],
                        now: 0,
                    };
                    let mut relay_qr = None;
                    app.settings_open = true;
                    ui::settings::show(
                        &context,
                        &mut app.settings_open,
                        &mut app.settings_category,
                        &mut app.config,
                        &remote,
                        &mut app.remote_reveal_token,
                        &web,
                        &mut app.web_reveal_url,
                        &mut app.web_qr,
                        &relay,
                        &mut relay_qr,
                        0,
                        &mut app.settings_search,
                        &app.i18n,
                        crate::scrollback_policy::View::default(),
                        |_, _| {},
                    );
                })
                .drop_without_applying_deltas();
        }
        let settings_calls = spy.calls();
        let saved = execute_settings_job(
            &mut app.db,
            &path,
            &secret::RedactionService::new(),
            SettingsJob {
                generation: 1,
                revision: 2,
                workspace_id: workspace_id.clone(),
                project_root: None,
                action: SettingsJobAction::SaveCodexLlmApiKey {
                    value: secret::SecretString::new("keychain-positive-control".into()),
                },
            },
        );
        assert!(matches!(
            saved.kind,
            SettingsOutcomeKind::CodexLlmApiKeySaved(Ok(()))
        ));
        assert!(spy.calls() > settings_calls);
        let deleted = execute_settings_job(
            &mut app.db,
            &path,
            &secret::RedactionService::new(),
            SettingsJob {
                generation: 1,
                revision: 3,
                workspace_id,
                project_root: None,
                action: SettingsJobAction::DeleteCodexLlmApiKey,
            },
        );
        assert!(matches!(
            deleted.kind,
            SettingsOutcomeKind::CodexLlmApiKeyDeleted(Ok(()))
        ));
        app.shutdown_on_exit();
        drop(app);
        // 실제 KeyringSecretStore가 같은 계수기에 도달하는 양성 대조다.
        let _ =
            secret::SecretStore::get_secret(&KeyringSecretStore, "startup-test-positive-control");
        assert!(spy.calls() > settings_calls);
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(startup_calls, 0, "앱 생성은 Keychain에 접근하면 안 된다");
        assert_eq!(off_calls, 0, "Relay OFF는 Keychain에 접근하면 안 된다");
        assert_eq!(settings_calls, 0, "설정 열기는 Keychain에 접근하면 안 된다");
    }

    #[test]
    fn keychain_startup_deferred_cleanup_preserves_new_staging() {
        let _serial = STORE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir =
            std::env::temp_dir().join(format!("deppy-secret-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("db.sqlite3")).unwrap();
        let logical = secret::LogicalCredentialId::new(uuid::Uuid::new_v4().to_string()).unwrap();
        let old = secret::SecretBundleStagePlan::allocate(logical.clone(), None).unwrap();
        db.register_physical_secret_slot_staging(logical.as_str(), old.new_slot().as_str())
            .unwrap();
        let startup_snapshot = DeferredSecretRepair::capture(&db);
        let current = secret::SecretBundleStagePlan::allocate(logical.clone(), None).unwrap();
        db.register_physical_secret_slot_staging(logical.as_str(), current.new_slot().as_str())
            .unwrap();
        let _guard = StoreGuard::install(CountingStore::new());
        let store = KeyringSecretStore;
        let value = secret::SecretString::new("current-test-value".into());
        secret::stage_secret_bundle(
            &store,
            &current,
            secret::SecretBundleRef::new(&value, None, None),
        )
        .unwrap();
        startup_snapshot.reconcile(&db, &store).unwrap();
        assert!(
            secret::SecretStore::has_secret(&store, current.new_slot().as_str()).unwrap(),
            "현재 실행 Staging은 정리 대상이 아니다"
        );
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn keychain_startup_explicit_credential_use_migrates_and_retries_legacy() {
        let _serial = STORE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir =
            std::env::temp_dir().join(format!("deppy-keychain-explicit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.sqlite3");
        let mut db = Db::open(&path).unwrap();
        let workspace_id = db.ensure_default_workspace().unwrap();
        let logical = uuid::Uuid::new_v4().to_string();
        db.insert_credential(&storage::CredentialMeta {
            id: logical.clone(),
            provider: "legacy".into(),
            label: "test".into(),
            credential_kind: "api_key".into(),
            masked_hint: None,
            workspace_id: None,
        })
        .unwrap();
        let repair = DeferredSecretRepair::capture(&db);
        let spy = CountingStore::new();
        let _guard = StoreGuard::install(Arc::clone(&spy));
        let reveal = || SettingsJob {
            generation: 1,
            revision: 1,
            workspace_id: workspace_id.clone(),
            project_root: None,
            action: SettingsJobAction::RevealCredential {
                credential_id: logical.clone(),
            },
        };
        let first = execute_settings_job_with_repair(
            &mut db,
            &path,
            &secret::RedactionService::new(),
            &repair,
            reveal(),
        );
        assert!(matches!(
            first.kind,
            SettingsOutcomeKind::CredentialRevealed { result: Err(_), .. }
        ));
        assert_eq!(
            db.credential_secret_location(&logical)
                .unwrap()
                .unwrap()
                .keyring_username,
            logical
        );
        secret::SecretStore::set_secret(
            &KeyringSecretStore,
            &logical,
            &secret::SecretString::new("explicit-test-value".into()),
        )
        .unwrap();
        let before = spy.calls();
        let second = execute_settings_job_with_repair(
            &mut db,
            &path,
            &secret::RedactionService::new(),
            &repair,
            reveal(),
        );
        assert!(matches!(
            second.kind,
            SettingsOutcomeKind::CredentialRevealed { result: Ok(_), .. }
        ));
        assert!(spy.calls() > before);
        assert_ne!(
            db.credential_secret_location(&logical)
                .unwrap()
                .unwrap()
                .keyring_username,
            logical
        );
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[derive(Default)]
    struct MigrationFaultStore {
        values: std::sync::Mutex<HashMap<String, String>>,
        deny_delete: std::sync::atomic::AtomicBool,
        deny_refresh_write: std::sync::atomic::AtomicBool,
    }
    impl secret::SecretStore for MigrationFaultStore {
        fn set_secret(&self, id: &str, value: &secret::SecretString) -> anyhow::Result<()> {
            anyhow::ensure!(
                !(self.deny_refresh_write.load(Ordering::SeqCst)
                    && id.starts_with("deppy.oauth.v1.")
                    && id.ends_with(".refresh")),
                "test refresh write denied"
            );
            self.values
                .lock()
                .unwrap()
                .insert(id.into(), value.expose().into());
            Ok(())
        }
        fn get_secret(&self, id: &str) -> anyhow::Result<secret::SecretString> {
            self.values
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .map(secret::SecretString::new)
                .ok_or_else(|| anyhow::anyhow!("missing test secret"))
        }
        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            Ok(self.values.lock().unwrap().contains_key(id))
        }
        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            anyhow::ensure!(
                !self.deny_delete.load(Ordering::SeqCst),
                "test keychain denied"
            );
            self.values.lock().unwrap().remove(id);
            Ok(())
        }
    }

    #[test]
    fn keychain_startup_failed_legacy_cleanup_retries_without_restart() {
        let path = std::env::temp_dir().join(format!(
            "deppy-migration-retry-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let db = Db::open(&path).unwrap();
        let logical = uuid::Uuid::new_v4().to_string();
        db.insert_credential(&storage::CredentialMeta {
            id: logical.clone(),
            provider: "legacy".into(),
            label: "retry".into(),
            credential_kind: "oauth_token".into(),
            masked_hint: None,
            workspace_id: None,
        })
        .unwrap();
        let store = MigrationFaultStore::default();
        secret::SecretStore::set_secret(
            &store,
            &logical,
            &secret::SecretString::new("migration-test-value".into()),
        )
        .unwrap();
        let repair = DeferredSecretRepair::capture(&db);
        store.deny_delete.store(true, Ordering::SeqCst);
        assert!(repair.reconcile(&db, &store).is_err());
        assert!(
            db.physical_secret_slots_for_reconciliation(8).unwrap()[0]
                .legacy_cleanup_username
                .is_some()
        );
        store.deny_delete.store(false, Ordering::SeqCst);
        repair.reconcile(&db, &store).unwrap();
        assert!(
            !secret::SecretStore::has_secret(&store, &logical).unwrap(),
            "같은 실행에서 미완료 legacy 삭제를 재시도해야 한다"
        );
        assert!(
            db.physical_secret_slots_for_reconciliation(8).unwrap()[0]
                .legacy_cleanup_username
                .is_none()
        );
    }

    #[test]
    fn keychain_startup_partial_migration_retries_its_own_slot_without_growth() {
        let path = std::env::temp_dir().join(format!(
            "deppy-migration-partial-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let db = Db::open(&path).unwrap();
        let logical = uuid::Uuid::new_v4().to_string();
        db.insert_credential(&storage::CredentialMeta {
            id: logical.clone(),
            provider: "legacy".into(),
            label: "partial".into(),
            credential_kind: "oauth_token".into(),
            masked_hint: None,
            workspace_id: None,
        })
        .unwrap();
        let store = MigrationFaultStore::default();
        let value = secret::SecretString::new("partial-test-value".into());
        secret::SecretStore::set_secret(&store, &logical, &value).unwrap();
        secret::SecretStore::set_secret(&store, &auth::refresh_entry_id(&logical), &value).unwrap();
        let repair = DeferredSecretRepair::capture(&db);
        store.deny_delete.store(true, Ordering::SeqCst);
        store.deny_refresh_write.store(true, Ordering::SeqCst);
        for _ in 0..3 {
            assert!(repair.reconcile(&db, &store).is_err());
            let rows = db.physical_secret_slots_for_reconciliation(8).unwrap();
            assert_eq!(
                rows.len(),
                1,
                "자신의 미완료 슬롯을 먼저 재시도해 backlog를 늘리지 않는다"
            );
            assert_eq!(rows[0].state, storage::PhysicalSecretSlotState::Staging);
        }
        store.deny_delete.store(false, Ordering::SeqCst);
        store.deny_refresh_write.store(false, Ordering::SeqCst);
        let before = db.connector_config_revision().unwrap().get();
        let mut publications = 0;
        repair
            .reconcile_counted(&db, &store, &mut publications)
            .unwrap();
        assert_eq!(publications, 1);
        assert_eq!(
            db.connector_config_revision().unwrap().get(),
            before + publications
        );
        let rows = db.physical_secret_slots_for_reconciliation(8).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, storage::PhysicalSecretSlotState::Published);
        drop(db);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn keychain_startup_connector_revision_receipt_excludes_an_actual_other_writer() {
        use connector_service::ConnectorRepository;
        let _serial = STORE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for other_writer in [false, true] {
            let path = std::env::temp_dir().join(format!(
                "deppy-migration-revision-{}.sqlite3",
                uuid::Uuid::new_v4()
            ));
            let db = Db::open(&path).unwrap();
            let logical = uuid::Uuid::new_v4().to_string();
            let metadata = |id: String| storage::CredentialMeta {
                id,
                provider: "legacy".into(),
                label: "revision".into(),
                credential_kind: "api_key".into(),
                masked_hint: None,
                workspace_id: None,
            };
            db.insert_credential(&metadata(logical.clone())).unwrap();
            db.insert_mcp_server(&mcp_store::McpServerRow {
                id: "server".into(),
                name: "server".into(),
                kind: "stdio".into(),
                command: Some("fixture-mcp".into()),
                args: Vec::new(),
                env_plain: Vec::new(),
                env_secrets: Vec::new(),
                inherit_env: false,
                url: None,
                enabled: true,
            })
            .unwrap();
            let spy = CountingStore::new();
            let _guard = StoreGuard::install(Arc::clone(&spy));
            secret::SecretStore::set_secret(
                &KeyringSecretStore,
                &logical,
                &secret::SecretString::new("revision-test-value".into()),
            )
            .unwrap();
            let repair = Arc::new(DeferredSecretRepair::capture(&db));
            let mut repository = AppConnectorRepository {
                secret_migration_revision: None,
                secret_repair: repair,
                db,
                redaction: secret::RedactionService::new(),
                authorization_owner: None,
            };
            let before_calls = spy.calls();
            let expected = repository.load_overview().unwrap().config_revision;
            assert_eq!(
                spy.calls(),
                before_calls,
                "Connector 화면 조회는 Keychain을 만지지 않는다"
            );
            if other_writer {
                let writer = Db::open(&path).unwrap();
                *spy.on_access.lock().unwrap() = Some(Box::new(move || {
                    writer
                        .insert_credential(&metadata(uuid::Uuid::new_v4().to_string()))
                        .unwrap();
                }));
            }
            let target = repository
                .load_mcp_target(&connector_contract::ServerId::new("server"))
                .unwrap();
            assert_eq!(
                target.revision.0,
                expected.0 + if other_writer { 2 } else { 1 }
            );
            assert_eq!(
                repository.accept_secret_migration_revision(expected, target.revision),
                !other_writer
            );
            assert!(
                !repository.accept_secret_migration_revision(expected, target.revision),
                "증명은 한 번만 소비한다"
            );
            drop(repository);
            std::fs::remove_file(path).unwrap();
        }
    }
}
