//! Bounded ownership of per-launch project API access.
//!
//! App preparation retains a lease until the runtime acquires the same resource through
//! its local secret lifetime port. The concrete process then owns cleanup, independently
//! of agent configuration IDs, launch order, event loss, or archived respawns.
//! Guard release immediately retires the in-memory bootstrap reference. Filesystem cleanup
//! and durable capability/grant revocation run on the bounded worker and retry until success.

use crate::project_keys::agent_access::BootstrapArtifacts;
use crate::project_keys::repository::ProjectApiRepository;
use deppy_core::project_api_access::ProjectApiSessionScope;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

const MAX_PROJECT_API_LAUNCHES: usize = 256;

pub(super) struct ProjectApiLaunchLease(Arc<ApiLifetime>);
struct ApiLifetime {
    credential: EphemeralProjectApiCredential,
    retained: AtomicBool,
    cleanup: Mutex<Option<CleanupReservation>>,
}
struct RuntimeApiLifetime {
    owner: Arc<ApiLifetime>,
    _cleanup: CleanupReservation,
}
impl Drop for RuntimeApiLifetime {
    fn drop(&mut self) {
        // A still-pending UI acknowledgement must not retain the bootstrap reference.
        ephemeral_credentials()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&self.owner.credential.id);
    }
}

pub(super) struct PreparedProjectApiLaunch {
    pub(super) args: Vec<String>,
    pub(super) unavailable_reason: Option<&'static str>,
    pub(super) lease: Option<ProjectApiLaunchLease>,
}

pub(super) fn prepare(
    db: &dyn ProjectApiRepository,
    db_path: &Path,
    provider: &str,
    scope: ProjectApiSessionScope,
    executable: &Path,
    remote: bool,
) -> PreparedProjectApiLaunch {
    let unavailable = |reason| PreparedProjectApiLaunch {
        args: Vec::new(),
        unavailable_reason: Some(reason),
        lease: None,
    };
    if remote {
        return unavailable("project_api_remote_unavailable");
    }
    if scope.validate().is_err() {
        return unavailable("project_api_scope_invalid");
    }
    let Some(mut cleanup) =
        cleanup_pool().and_then(|pool| pool.reserve(db_path.to_owned(), scope.clone()))
    else {
        return unavailable("project_api_launch_capacity");
    };
    let bootstrap = match crate::project_keys::agent_access::prepare_local_bootstrap(
        db, provider, &scope, executable, false,
    ) {
        Ok(bootstrap) => bootstrap,
        Err(error) => return unavailable(error.code()),
    };
    cleanup.set_artifacts(bootstrap.artifacts);
    let Some(capability) = bootstrap.capability else {
        return unavailable(
            bootstrap
                .unavailable_reason
                .unwrap_or("project_api_provider_unavailable"),
        );
    };
    let credential = match EphemeralProjectApiCredential::register(capability) {
        Ok(credential) => credential,
        Err(_) => return unavailable("project_api_ephemeral_unavailable"),
    };
    PreparedProjectApiLaunch {
        args: bootstrap.args,
        unavailable_reason: bootstrap.unavailable_reason,
        lease: Some(ProjectApiLaunchLease::new(credential, cleanup)),
    }
}

impl ProjectApiLaunchLease {
    pub(super) fn credential_reference(&self) -> Option<&str> {
        Some(self.0.credential.id.as_str())
    }
    pub(super) fn was_retained(&self) -> bool {
        self.0.retained.load(Ordering::Acquire)
    }
    fn new(credential: EphemeralProjectApiCredential, cleanup: CleanupReservation) -> Self {
        let owner = Arc::new(ApiLifetime {
            credential,
            retained: AtomicBool::new(false),
            cleanup: Mutex::new(Some(cleanup)),
        });
        if let Some(entry) = ephemeral_credentials()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get_mut(&owner.credential.id)
        {
            entry.owner = Arc::downgrade(&owner);
        }
        Self(owner)
    }
}

type Revoke = dyn Fn(&Path, &ProjectApiSessionScope) -> bool + Send + Sync;

struct CleanupPool {
    shared: Arc<CleanupShared>,
}
struct CleanupShared {
    capacity: usize,
    outstanding: AtomicUsize,
    shutdown: AtomicBool,
    pending: Mutex<Vec<CleanupRequest>>,
    changed: Condvar,
}
struct CleanupRequest {
    db_path: PathBuf,
    scope: ProjectApiSessionScope,
    artifacts: Option<BootstrapArtifacts>,
    reported_failure: bool,
    shared: Arc<CleanupShared>,
}
impl Drop for CleanupRequest {
    fn drop(&mut self) {
        self.shared.outstanding.fetch_sub(1, Ordering::AcqRel);
        self.shared.changed.notify_all();
    }
}
struct CleanupReservation {
    request: Option<CleanupRequest>,
}

impl CleanupPool {
    fn new(capacity: usize, revoke: Arc<Revoke>) -> anyhow::Result<Self> {
        let shared = Arc::new(CleanupShared {
            capacity,
            outstanding: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
            pending: Mutex::new(Vec::new()),
            changed: Condvar::new(),
        });
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("project-api-cleanup".into())
            .spawn(move || cleanup_worker(worker, revoke))?;
        Ok(Self { shared })
    }
    fn reserve(
        &self,
        db_path: PathBuf,
        scope: ProjectApiSessionScope,
    ) -> Option<CleanupReservation> {
        self.shared
            .outstanding
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < self.shared.capacity).then_some(current + 1)
            })
            .ok()?;
        Some(CleanupReservation {
            request: Some(CleanupRequest {
                db_path,
                scope,
                artifacts: None,
                reported_failure: false,
                shared: self.shared.clone(),
            }),
        })
    }
    #[cfg(test)]
    fn outstanding(&self) -> usize {
        self.shared.outstanding.load(Ordering::Acquire)
    }
    #[cfg(test)]
    fn wake(&self) {
        self.shared.changed.notify_all();
    }
}
impl Drop for CleanupPool {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.changed.notify_all();
    }
}

impl CleanupReservation {
    fn set_artifacts(&mut self, artifacts: Option<BootstrapArtifacts>) {
        self.request
            .as_mut()
            .expect("cleanup reservation is owned")
            .artifacts = artifacts;
    }
}
impl Drop for CleanupReservation {
    fn drop(&mut self) {
        let Some(request) = self.request.take() else {
            return;
        };
        let shared = request.shared.clone();
        shared
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(request);
        shared.changed.notify_one();
    }
}

fn cleanup_worker(shared: Arc<CleanupShared>, revoke: Arc<Revoke>) {
    let retry_delay = std::time::Duration::from_millis(250);
    let mut retrying = false;
    loop {
        let mut pending = shared
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if retrying {
            pending = shared
                .changed
                .wait_timeout(pending, retry_delay)
                .unwrap_or_else(|poison| poison.into_inner())
                .0;
        }
        while pending.is_empty() {
            if shared.shutdown.load(Ordering::Acquire)
                && shared.outstanding.load(Ordering::Acquire) == 0
            {
                return;
            }
            pending = shared
                .changed
                .wait(pending)
                .unwrap_or_else(|poison| poison.into_inner());
        }
        let requests = std::mem::take(&mut *pending);
        drop(pending);
        let mut failed = Vec::new();
        for mut request in requests {
            // Filesystem cleanup and SQLite work are owned by this worker, including failed
            // preparation and cancellation before runtime submission.
            drop(request.artifacts.take());
            let revoked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                revoke(&request.db_path, &request.scope)
            }))
            .unwrap_or(false);
            if !revoked {
                if !request.reported_failure {
                    tracing::warn!(
                        error_code = "project_api_revocation_retry",
                        "project API revocation will retry; launch capacity remains reserved"
                    );
                    request.reported_failure = true;
                }
                failed.push(request);
            }
        }
        retrying = !failed.is_empty();
        if retrying {
            shared
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .extend(failed);
        }
    }
}

fn cleanup_pool() -> Option<&'static CleanupPool> {
    static POOL: std::sync::OnceLock<Option<CleanupPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        CleanupPool::new(
            MAX_PROJECT_API_LAUNCHES,
            Arc::new(super::revoke_project_api_scope),
        )
        .ok()
    })
    .as_ref()
}

const EPHEMERAL_PROJECT_API_PREFIX: &str = "project-api-ephemeral:";
struct EphemeralEntry {
    value: Option<secret::SecretString>,
    owner: std::sync::Weak<ApiLifetime>,
}
fn ephemeral_credentials() -> &'static Mutex<BTreeMap<String, EphemeralEntry>> {
    static CREDENTIALS: std::sync::OnceLock<Mutex<BTreeMap<String, EphemeralEntry>>> =
        std::sync::OnceLock::new();
    CREDENTIALS.get_or_init(Default::default)
}
struct EphemeralProjectApiCredential {
    id: String,
}
impl EphemeralProjectApiCredential {
    fn register(value: secret::SecretString) -> anyhow::Result<Self> {
        let mut credentials = ephemeral_credentials()
            .lock()
            .map_err(|_| anyhow::anyhow!("project_api_ephemeral_unavailable"))?;
        anyhow::ensure!(
            credentials.len() < MAX_PROJECT_API_LAUNCHES,
            "project_api_ephemeral_capacity"
        );
        let id = format!("{EPHEMERAL_PROJECT_API_PREFIX}{}", uuid::Uuid::new_v4());
        credentials.insert(
            id.clone(),
            EphemeralEntry {
                value: Some(value),
                owner: std::sync::Weak::new(),
            },
        );
        Ok(Self { id })
    }
}
impl Drop for EphemeralProjectApiCredential {
    fn drop(&mut self) {
        ephemeral_credentials()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&self.id);
    }
}
pub(super) fn resolve_runtime_secret(
    reference: &str,
) -> Option<anyhow::Result<runtime::RuntimeSecret>> {
    if !reference.starts_with(EPHEMERAL_PROJECT_API_PREFIX) {
        return None;
    }
    Some((|| {
        let value = ephemeral_credentials()
            .lock()
            .map_err(|_| anyhow::anyhow!("project_api_ephemeral_unavailable"))?
            .get_mut(reference)
            .and_then(|entry| entry.value.take())
            .ok_or_else(|| anyhow::anyhow!("project_api_ephemeral_missing"))?;
        Ok(runtime::RuntimeSecret::new(value.into_string()))
    })())
}

pub(super) fn retain_runtime_credentials(
    references: &[String],
) -> anyhow::Result<Option<Box<dyn Send>>> {
    let mut guards = Vec::new();
    for reference in references
        .iter()
        .filter(|reference| reference.starts_with(EPHEMERAL_PROJECT_API_PREFIX))
    {
        let owner = {
            let credentials = ephemeral_credentials()
                .lock()
                .map_err(|_| anyhow::anyhow!("project_api_ephemeral_unavailable"))?;
            credentials
                .get(reference)
                .and_then(|entry| entry.owner.upgrade())
        }
        .ok_or_else(|| anyhow::anyhow!("project_api_ephemeral_missing"))?;
        anyhow::ensure!(
            owner
                .retained
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "project_api_ephemeral_already_retained"
        );
        let cleanup = owner
            .cleanup
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
            .ok_or_else(|| anyhow::anyhow!("project_api_ephemeral_already_retained"))?;
        guards.push(RuntimeApiLifetime {
            owner,
            _cleanup: cleanup,
        });
    }
    Ok((!guards.is_empty()).then(|| Box::new(guards) as Box<dyn Send>))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_api_launch_cleanup_retries_off_ui_and_retains_bounded_credit() {
        use deppy_core::{project_api_access::ProjectApiActorKind, project_keys::KeyEnvironment};
        let ui_thread = std::thread::current().id();
        let allow_revoke = Arc::new(AtomicBool::new(false));
        let gate = allow_revoke.clone();
        let (called_tx, called_rx) = std::sync::mpsc::channel();
        let pool = CleanupPool::new(
            1,
            Arc::new(move |_, _| {
                let _ = called_tx.send(std::thread::current().id());
                gate.load(Ordering::Acquire)
            }),
        )
        .unwrap();
        let scope = ProjectApiSessionScope {
            workspace_id: "fixture-project".into(),
            environment: KeyEnvironment::Development,
            principal_id: "local-agent".into(),
            session_id: "fixture-launch".into(),
            generation: "fixture-generation".into(),
            actor_kind: ProjectApiActorKind::LocalSession,
        };
        let directory = std::env::temp_dir().join(format!(
            "deppy-api-cleanup-fixture-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&directory).unwrap();
        let mut reservation = pool
            .reserve(PathBuf::from("unused-fixture-db"), scope.clone())
            .expect("first credit");
        reservation.set_artifacts(Some(BootstrapArtifacts {
            directory: directory.clone(),
        }));
        assert!(pool.reserve(PathBuf::new(), scope.clone()).is_none());
        drop(reservation);
        assert_ne!(
            called_rx
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap(),
            ui_thread
        );
        assert!(
            !directory.exists(),
            "artifact cleanup belongs to the cleanup worker"
        );
        assert_eq!(
            pool.outstanding(),
            1,
            "failed revoke retains its bounded retry credit"
        );
        assert!(pool.reserve(PathBuf::new(), scope).is_none());
        allow_revoke.store(true, Ordering::Release);
        pool.wake();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while pool.outstanding() != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(pool.outstanding(), 0);
    }

    #[test]
    fn project_api_launch_ephemeral_reference_is_one_use_and_drop_removes_reference() {
        let credential = EphemeralProjectApiCredential::register(secret::SecretString::new(
            "synthetic-api-capability".into(),
        ))
        .unwrap();
        assert!(!credential.id.contains("synthetic-api-capability"));
        assert!(resolve_runtime_secret(&credential.id).unwrap().is_ok());
        assert!(resolve_runtime_secret(&credential.id).unwrap().is_err());
        let canceled = EphemeralProjectApiCredential::register(secret::SecretString::new(
            "synthetic-canceled-capability".into(),
        ))
        .unwrap();
        let reference = canceled.id.clone();
        drop(canceled);
        assert!(resolve_runtime_secret(&reference).unwrap().is_err());
        assert!(resolve_runtime_secret("ordinary-credential-id").is_none());
    }

    #[test]
    fn project_api_launch_runtime_guard_owns_resources_after_app_handoff() {
        use deppy_core::{project_api_access::ProjectApiActorKind, project_keys::KeyEnvironment};
        let pool = CleanupPool::new(1, Arc::new(|_, _| true)).unwrap();
        let scope = ProjectApiSessionScope {
            workspace_id: "fixture-project".into(),
            environment: KeyEnvironment::Development,
            principal_id: "local-agent".into(),
            session_id: "fixture-launch".into(),
            generation: "fixture-generation".into(),
            actor_kind: ProjectApiActorKind::LocalSession,
        };
        let cleanup = pool
            .reserve(PathBuf::from("unused-fixture-db"), scope)
            .unwrap();
        let credential = EphemeralProjectApiCredential::register(secret::SecretString::new(
            "synthetic-guard-capability".into(),
        ))
        .unwrap();
        let lease = ProjectApiLaunchLease::new(credential, cleanup);
        let reference = lease.credential_reference().unwrap().to_owned();
        let guard = retain_runtime_credentials(std::slice::from_ref(&reference))
            .unwrap()
            .expect("runtime must retain the exact launch resources");
        assert!(lease.was_retained());
        assert!(
            retain_runtime_credentials(std::slice::from_ref(&reference)).is_err(),
            "a second process cannot replay one launch resource"
        );
        drop(lease);
        assert!(
            resolve_runtime_secret(&reference).unwrap().is_ok(),
            "App handoff must not remove a runtime-owned capability"
        );
        assert_eq!(pool.outstanding(), 1);
        drop(guard);
        assert!(resolve_runtime_secret(&reference).unwrap().is_err());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while pool.outstanding() != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(pool.outstanding(), 0);
    }

    #[test]
    fn project_api_launch_preparation_adds_only_transient_native_flags_and_secret_reference() {
        let fixture = crate::project_keys::tests::Fixture::new();
        let db = &fixture.service.db;
        let workspace = &fixture.scope.workspace_id;
        db.upsert_builtin_agent_config("deppy-builtin-claude", "Claude fixture", "/fixture/claude")
            .unwrap();
        let profile = db
            .insert_env_profile(workspace, "Production fixture", "production")
            .unwrap();
        db.upsert_env_var(
            &profile,
            "DEPPY_PROJECT_API_CAPABILITY",
            &storage::EnvValue::Plain("synthetic-profile-shadow".into()),
        )
        .unwrap();
        let mut prepared = super::super::prepare_agent_launch(
            (db, fixture.store.as_ref()),
            workspace,
            "deppy-builtin-claude",
            Some(&profile),
            workspace.clone(),
            Some(Arc::from("fixture-batch-prompt")),
        )
        .unwrap();
        prepared.env_secrets.push((
            "DEPPY_PROJECT_API_CAPABILITY".into(),
            "synthetic-stale-reference".into(),
        ));
        let persisted_args = prepared.args.clone();
        super::super::add_project_api_launch(
            db,
            &fixture.root.join("metadata.sqlite3"),
            workspace,
            Some(&profile),
            &mut prepared,
        );
        assert_eq!(
            prepared.args, persisted_args,
            "model discovery must not persist expired bootstrap flags"
        );
        assert_eq!(
            prepared.transient_args.first().map(String::as_str),
            Some("--append-system-prompt")
        );
        assert_eq!(
            prepared.transient_args.last().map(String::as_str),
            Some("fixture-batch-prompt")
        );
        let lease = prepared
            .project_api
            .as_ref()
            .expect("local supported provider has scoped API discovery");
        assert!(
            prepared
                .env_secrets
                .iter()
                .any(|(name, reference)| name == "DEPPY_PROJECT_API_CAPABILITY"
                    && Some(reference.as_str()) == lease.credential_reference())
        );
        assert_eq!(
            prepared
                .env_secrets
                .iter()
                .filter(|(name, _)| name == "DEPPY_PROJECT_API_CAPABILITY")
                .count(),
            1,
            "the reserved capability name has exactly one authoritative reference"
        );
        assert!(
            prepared
                .env_plain
                .iter()
                .all(|(name, _)| name != "DEPPY_PROJECT_API_CAPABILITY")
        );
        assert!(prepared.transient_args.join(" ").contains("production"));
    }

    #[test]
    fn project_api_launch_quick_wrapper_keeps_bootstrap_out_of_persisted_arguments() {
        let fixture = crate::project_keys::tests::Fixture::new();
        let workspace = &fixture.scope.workspace_id;
        let detected = crate::agent_launcher::DetectionSnapshot::from_test_agents([(
            crate::agent_launcher::AgentKind::Codex,
            PathBuf::from("/fixture/codex"),
        )]);
        let spec = crate::agent_launcher::build_launch_spec(
            detected
                .find(crate::agent_launcher::AgentKind::Codex)
                .unwrap(),
            crate::agent_launcher::LaunchOptions {
                model: String::new(),
                effort: None,
                yolo: false,
            },
            None,
        )
        .unwrap();
        let mut prepared = super::super::prepare_quick_agent_launch(
            &fixture.service.db,
            workspace,
            spec,
            workspace.clone(),
        )
        .unwrap();
        let persisted_args = prepared.args.clone();
        super::super::add_project_api_launch(
            &fixture.service.db,
            &fixture.root.join("metadata.sqlite3"),
            workspace,
            None,
            &mut prepared,
        );
        assert!(prepared.project_api.is_some());
        assert_eq!(prepared.args, persisted_args);
        assert!(
            prepared
                .transient_args
                .join(" ")
                .contains("mcp_servers.deppy_api_")
        );
        assert!(
            prepared
                .transient_args
                .iter()
                .all(|arg| !arg.contains("developer_instructions"))
        );
    }

    #[test]
    fn project_api_launch_does_not_grant_settings_project_to_another_runtime() {
        let fixture = crate::project_keys::tests::Fixture::new();
        let db = &fixture.service.db;
        db.upsert_builtin_agent_config("deppy-builtin-claude", "Claude fixture", "/fixture/claude")
            .unwrap();
        let runtime_workspace = db.create_workspace("Other runtime").unwrap();
        let mut prepared = super::super::prepare_agent_launch(
            (db, fixture.store.as_ref()),
            &fixture.scope.workspace_id,
            "deppy-builtin-claude",
            None,
            runtime_workspace,
            None,
        )
        .unwrap();
        super::super::add_project_api_launch(
            db,
            &fixture.root.join("metadata.sqlite3"),
            &fixture.scope.workspace_id,
            None,
            &mut prepared,
        );
        assert!(
            prepared.project_api.is_none(),
            "the Settings project must not grant API authority to another project's process"
        );
        assert!(prepared.transient_args.is_empty());
        assert!(prepared.env_secrets.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn project_api_launch_quick_split_preserves_native_bootstrap_without_archiving_it() {
        use runtime::{RuntimeCommandSink, RuntimeEventStream, RuntimeHost};
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};
        let fixture = crate::project_keys::tests::Fixture::new();
        let workspace = &fixture.scope.workspace_id;
        let db_path = fixture.root.join("metadata.sqlite3");
        let adapter = fixture.root.join("codex");
        let received_args = fixture.root.join("adapter-args");
        std::fs::write(
            &adapter,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > adapter-args\nsleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o700)).unwrap();
        let detected = crate::agent_launcher::DetectionSnapshot::from_test_agents([(
            crate::agent_launcher::AgentKind::Codex,
            adapter,
        )]);
        let spec = crate::agent_launcher::build_launch_spec(
            detected
                .find(crate::agent_launcher::AgentKind::Codex)
                .unwrap(),
            crate::agent_launcher::LaunchOptions {
                model: String::new(),
                effort: None,
                yolo: false,
            },
            None,
        )
        .unwrap();
        let mut prepared = super::super::prepare_quick_agent_launch(
            &fixture.service.db,
            workspace,
            spec,
            workspace.clone(),
        )
        .unwrap();
        super::super::add_project_api_launch(
            &fixture.service.db,
            &db_path,
            workspace,
            None,
            &mut prepared,
        );
        assert!(!prepared.transient_args.is_empty());
        let persisted_args = prepared.args.clone();
        let lease = prepared.project_api.take().unwrap();
        let factory = runtime::InProcessRuntimeHostFactory::new(
            Arc::new(super::super::AppRuntimeSecretResolver::new(db_path.clone())),
            secret::RedactionService::new(),
        );
        let mut client = factory
            .create_client(runtime::RuntimeHostConfig {
                scrollback_policy: None,
                output_batch_ms: 5,
                logs_root: fixture.root.join("logs"),
                persist: Some(runtime::PersistConfig {
                    db_path: db_path.clone(),
                    workspace_id: workspace.clone(),
                }),
                cwd: Some(fixture.root.clone()),
                extra_env: Vec::new(),
            })
            .unwrap();
        let events = client.subscribe();
        let wait = |predicate: &mut dyn FnMut(&runtime::RuntimeEvent) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                for event in events.drain() {
                    if predicate(&event) {
                        return event;
                    }
                }
                assert!(Instant::now() < deadline, "fixture runtime outcome timeout");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        client
            .send_command(runtime::RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let runtime::RuntimeEvent::MuxUpdated { snapshot } = wait(
            &mut |event| matches!(event, runtime::RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.len() == 1 && snapshot.tabs[0].panes.len() == 1),
        ) else {
            unreachable!()
        };
        let anchor = snapshot.tabs[0].panes[0].id.clone();
        client
            .submit_agent_beside(
                runtime::RuntimeCommand::SpawnAgent {
                    agent_config_id: Some(prepared.agent_config_id),
                    cols: 80,
                    rows: 24,
                    scrollback_lines: 100,
                    command: prepared.command,
                    args: prepared.args,
                    transient_args: prepared.transient_args,
                    env_plain: prepared.env_plain,
                    env_secrets: prepared.env_secrets,
                    waiting_regex: prepared.waiting_regex,
                    approval_regex: prepared.approval_regex,
                    error_regex: prepared.error_regex,
                    done_regex: prepared.done_regex,
                },
                anchor.clone(),
            )
            .expect("supported quick launch must admit native bootstrap beside the clicked pane");
        let runtime::RuntimeEvent::MuxUpdated { snapshot } = wait(
            &mut |event| matches!(event, runtime::RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.len() == 1 && snapshot.tabs[0].panes.len() == 2),
        ) else {
            unreachable!()
        };
        assert!(snapshot.tabs[0].panes.iter().any(|pane| pane.id == anchor));
        assert!(lease.was_retained());
        let deadline = Instant::now() + Duration::from_secs(15);
        let recorded = loop {
            if let Ok(recorded) = std::fs::read_to_string(&received_args)
                && recorded.contains("mcp_servers.deppy_api_")
            {
                break recorded;
            }
            assert!(
                Instant::now() < deadline,
                "native adapter did not receive bootstrap"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(recorded.contains(".env_vars="));
        client.shutdown();
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        let sessions = persist::load_sessions(&connection, workspace).unwrap();
        let agent = sessions
            .iter()
            .find(|session| session.session_kind == "agent")
            .unwrap();
        assert_eq!(
            agent.args, persisted_args,
            "resume must not reuse expired API bootstrap"
        );
        assert!(
            agent
                .args
                .iter()
                .all(|arg| !arg.contains("mcp_servers.deppy_api_"))
        );
    }

    #[test]
    fn project_api_launch_runtime_drop_retires_reference_before_app_observes_handoff() {
        use deppy_core::{project_api_access::ProjectApiActorKind, project_keys::KeyEnvironment};
        let pool = CleanupPool::new(1, Arc::new(|_, _| true)).unwrap();
        let scope = ProjectApiSessionScope {
            workspace_id: "fixture-project".into(),
            environment: KeyEnvironment::Development,
            principal_id: "local-agent".into(),
            session_id: "fixture-launch".into(),
            generation: "fixture-generation".into(),
            actor_kind: ProjectApiActorKind::LocalSession,
        };
        let cleanup = pool
            .reserve(PathBuf::from("unused-fixture-db"), scope)
            .unwrap();
        let credential = EphemeralProjectApiCredential::register(secret::SecretString::new(
            "synthetic-exit-capability".into(),
        ))
        .unwrap();
        let lease = ProjectApiLaunchLease::new(credential, cleanup);
        let reference = lease.credential_reference().unwrap().to_owned();
        let guard = retain_runtime_credentials(std::slice::from_ref(&reference))
            .unwrap()
            .unwrap();
        drop(guard);
        assert!(
            lease.was_retained(),
            "App has not processed its handoff notification yet"
        );
        assert!(
            resolve_runtime_secret(&reference).unwrap().is_err(),
            "real process exit retires its bootstrap reference without depending on an App event"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while pool.outstanding() != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(pool.outstanding(), 0);
        drop(lease);
    }
}
