use super::{
    contracts::*,
    process::{Clipboard, ProcessRunner},
    service::{ProjectKeysService, ServiceFactory, committed, now, storage_error},
};
use deppy_core::project_keys::*;
#[cfg(test)]
use secret::SecretStore;
#[cfg(test)]
use std::path::PathBuf;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::Duration,
};

const QUEUE_MAX: usize = 32;
struct Completion(SyncSender<()>);
impl Drop for Completion {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

#[derive(Default)]
struct Selection {
    scope: Option<ProjectKeysScope>,
    generation: u64,
}
struct Request {
    fence: ProjectKeysFence,
    action: ProjectKeysAction,
}
pub struct ProjectKeysWorker {
    sender: Option<SyncSender<Request>>,
    acks: Receiver<ProjectKeysAck>,
    snapshot: Arc<Mutex<Option<Arc<ProjectKeysSnapshot>>>>,
    selection: Arc<Mutex<Selection>>,
    outstanding: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    runner: Arc<dyn ProcessRunner>,
    completions: Receiver<()>,
    remaining_workers: usize,
}
impl ProjectKeysWorker {
    #[cfg(test)]
    pub fn dormant_for_tests() -> Self {
        let (sender, requests) = mpsc::sync_channel(QUEUE_MAX);
        drop(requests);
        let (_ack, acks) = mpsc::sync_channel(QUEUE_MAX);
        let (_done, completions) = mpsc::sync_channel(4);
        Self {
            sender: Some(sender),
            acks,
            snapshot: Arc::new(Mutex::new(None)),
            selection: Arc::new(Mutex::new(Selection::default())),
            outstanding: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            runner: Arc::new(super::process::NativeProcessRunner::default()),
            completions,
            remaining_workers: 0,
        }
    }
    #[cfg(test)]
    pub fn spawn_with(
        db_path: PathBuf,
        secrets: Arc<dyn SecretStore>,
        clipboard: Arc<dyn Clipboard>,
        runner: Arc<dyn ProcessRunner>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self::spawn_with_factory(
            Arc::new(move || crate::project_keys::tests::open_service(&db_path, secrets.clone())),
            clipboard,
            runner,
            wake,
        )
    }
    pub fn spawn_with_factory(
        factory: ServiceFactory,
        clipboard: Arc<dyn Clipboard>,
        runner: Arc<dyn ProcessRunner>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let (sender, requests) = mpsc::sync_channel::<Request>(QUEUE_MAX);
        let (ack_tx, acks) = mpsc::sync_channel(QUEUE_MAX);
        let (done_tx, completions) = mpsc::sync_channel(4);
        let snapshot = Arc::new(Mutex::new(None));
        let selection = Arc::new(Mutex::new(Selection::default()));
        let outstanding = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(wake);
        let (build_tx, build_rx) = mpsc::sync_channel::<(ProjectKeysFence, ProjectBuildRecipe)>(1);
        {
            let factory = factory.clone();
            let stop = stop.clone();
            let ack = ack_tx.clone();
            let wake = wake.clone();
            let runner = runner.clone();
            // Exactly one build runs and at most one waits. Source watching and commands keep running.
            let done = done_tx.clone();
            std::thread::Builder::new()
                .name("project-key-build".into())
                .spawn(move || {
                    let _completion = Completion(done);
                    while !stop.load(Ordering::Acquire) {
                        let Ok((fence, recipe)) = build_rx.recv_timeout(Duration::from_millis(250))
                        else {
                            continue;
                        };
                        let result = (|| {
                            let service = factory()?;
                            let current = storage_error(service.db.project_key_recipe_list(
                                &fence.scope.workspace_id,
                                fence.scope.environment,
                            ))?
                            .into_iter()
                            .find(|row| row.id == recipe.id && row.revision == recipe.revision)
                            .ok_or(ProjectKeysError::Stale)?;
                            service
                                .run_build(
                                    &fence.scope,
                                    Some(&current),
                                    &current.command,
                                    std::path::Path::new(&current.cwd),
                                    runner.as_ref(),
                                )
                                .map(ProjectKeysAckPayload::BuildFinished)
                        })();
                        let _ = ack.try_send(ProjectKeysAck { fence, result });
                        wake();
                    }
                })
                .expect("project key build worker spawn");
        }
        {
            let factory = factory.clone();
            let stop = stop.clone();
            let selection = selection.clone();
            let snapshot = snapshot.clone();
            let wake = wake.clone();
            let done = done_tx.clone();
            std::thread::Builder::new()
                .name("project-key-sources".into())
                .spawn(move || {
                    let _completion = Completion(done);
                    let mut service = None;
                    let mut enrolled = std::collections::HashSet::new();
                    let mut journal_cursor = None;
                    let mut cursor: Option<(String, KeyEnvironment)> = None;
                    let mut offsets =
                        std::collections::HashMap::<(String, KeyEnvironment), usize>::new();
                    while !stop.load(Ordering::Acquire) {
                        if service.is_none() {
                            service = factory().ok();
                        }
                        if let Some(service) = &service {
                            // One durable journal is recovered per pass before discovery. Pending
                            // sources remain guarded, so a failed edit cannot become a new value.
                            let _ = service.recover_file_journals(&mut journal_cursor);
                            // Startup/new workspace enrollment is independent of selected UI scope.
                            if let Ok(projects) = service.db.settings_workspace_projection_rows() {
                                for project in projects {
                                    if enrolled.insert(project.id.clone()) {
                                        for environment in [
                                            KeyEnvironment::Development,
                                            KeyEnvironment::Production,
                                        ] {
                                            let _ = service
                                                .db
                                                .project_key_settings(&project.id, environment);
                                        }
                                    }
                                }
                            }
                            if let Ok(scopes) = service.db.project_key_watch_scopes_page(
                                4,
                                cursor.as_ref().map(|(id, env)| (id.as_str(), *env)),
                            ) {
                                if scopes.is_empty() {
                                    cursor = None;
                                } else {
                                    for settings in scopes {
                                        cursor = Some((
                                            settings.workspace_id.clone(),
                                            settings.environment,
                                        ));
                                        let scope = ProjectKeysScope {
                                            workspace_id: settings.workspace_id,
                                            environment: settings.environment,
                                        };
                                        let _ = service.prepare_legacy_source_origins(&scope);
                                        let offset = offsets
                                            .entry((scope.workspace_id.clone(), scope.environment))
                                            .or_default();
                                        if !settings.sources.is_empty() {
                                            let source = &settings.sources
                                                [(*offset / 4) % settings.sources.len()];
                                            let _ = service.enroll_source(&scope, source);
                                        }
                                        if let Ok(keys) = service.db.project_key_list(
                                            &scope.workspace_id,
                                            scope.environment,
                                        ) {
                                            for key in keys
                                                .iter()
                                                .cycle()
                                                .skip(*offset % keys.len().max(1))
                                                .take(keys.len().min(4))
                                            {
                                                if settings.sources.contains(&key.source) {
                                                    let _ = service.observe_key(&scope, key);
                                                } else if !key.source.is_file() {
                                                    let _ =
                                                        service.enroll_configuration(&scope, key);
                                                }
                                            }
                                        }
                                        *offset = offset.wrapping_add(4);
                                    }
                                }
                            }
                        }
                        let selected = selection.lock().ok().and_then(|state| {
                            state.scope.clone().map(|scope| (scope, state.generation))
                        });
                        if let Some((scope, generation)) = selected {
                            publish_snapshot(
                                &snapshot,
                                &scope,
                                generation,
                                service.as_ref(),
                                wake.as_ref(),
                            );
                        }
                        for _ in 0..8 {
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(250));
                        }
                    }
                })
                .expect("project key source worker spawn");
        }
        {
            let factory = factory.clone();
            let stop = stop.clone();
            let runner = runner.clone();
            let done = done_tx.clone();
            std::thread::Builder::new()
                .name("project-key-github".into())
                .spawn(move || {
                    let _completion = Completion(done);
                    let mut service = None;
                    let mut observer_cursor = None;
                    let mut next_observe = std::time::Instant::now();
                    while !stop.load(Ordering::Acquire) {
                        if service.is_none() {
                            service = factory().ok();
                        }
                        if let Some(service) = &service {
                            if std::time::Instant::now() >= next_observe {
                                let _ =
                                    service.poll_github_next(&mut observer_cursor, runner.as_ref());
                                next_observe = std::time::Instant::now() + Duration::from_secs(30);
                            }
                            let _ = service.drain_github(runner.as_ref());
                        }
                        for _ in 0..8 {
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(250));
                        }
                    }
                })
                .expect("project key github worker spawn");
        }
        {
            let factory = factory.clone();
            let runner = runner.clone();
            let stop = stop.clone();
            let selection = selection.clone();
            let snapshot = snapshot.clone();
            let wake = wake.clone();
            let done = done_tx.clone();
            std::thread::Builder::new()
                .name("project-key-commands".into())
                .spawn(move || {
                    let _completion = Completion(done);
                    let mut service = None;
                    while !stop.load(Ordering::Acquire) {
                        let Ok(request) = requests.recv_timeout(Duration::from_millis(250)) else {
                            continue;
                        };
                        if service.is_none() {
                            service = factory().ok();
                        }
                        let result = match &service {
                            None => Err(ProjectKeysError::Storage),
                            Some(service) => {
                                if let ProjectKeysAction::RunRecipe {
                                    recipe_id,
                                    expected_revision,
                                } = &request.action
                                {
                                    (|| {
                                        let recipe =
                                            storage_error(service.db.project_key_recipe_list(
                                                &request.fence.scope.workspace_id,
                                                request.fence.scope.environment,
                                            ))?
                                            .into_iter()
                                            .find(|recipe| {
                                                recipe.id == *recipe_id
                                                    && recipe.revision == *expected_revision
                                            })
                                            .ok_or(ProjectKeysError::Stale)?;
                                        build_tx
                                            .try_send((request.fence.clone(), recipe))
                                            .map_err(|_| ProjectKeysError::Busy)?;
                                        Ok(None)
                                    })()
                                } else {
                                    execute(
                                        service,
                                        &request.fence,
                                        request.action,
                                        &selection,
                                        clipboard.as_ref(),
                                        runner.as_ref(),
                                    )
                                    .map(Some)
                                }
                            }
                        };
                        let selected = selection.lock().ok().and_then(|state| {
                            state.scope.clone().map(|scope| (scope, state.generation))
                        });
                        if let Some((scope, generation)) = selected {
                            publish_snapshot(
                                &snapshot,
                                &scope,
                                generation,
                                service.as_ref(),
                                wake.as_ref(),
                            );
                        }
                        match result {
                            Ok(None) => {}
                            result => {
                                let _ = ack_tx.try_send(ProjectKeysAck {
                                    fence: request.fence,
                                    result: result.and_then(|payload| {
                                        payload.ok_or(ProjectKeysError::Process)
                                    }),
                                });
                            }
                        }
                        wake();
                    }
                })
                .expect("project key command worker spawn");
        }
        Self {
            sender: Some(sender),
            acks,
            snapshot,
            selection,
            outstanding,
            stop,
            runner,
            completions,
            remaining_workers: 4,
        }
    }
    /// Used only by actual exit. Rendering and Drop merely signal cancellation.
    pub fn shutdown(&mut self, timeout: Duration) -> bool {
        self.request_stop();
        let deadline = std::time::Instant::now() + timeout;
        while self.remaining_workers > 0 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match self.completions.recv_timeout(remaining) {
                Ok(()) => self.remaining_workers -= 1,
                Err(_) => return false,
            }
        }
        self.runner
            .wait_for_idle(deadline.saturating_duration_since(std::time::Instant::now()))
    }
    fn request_stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.runner.cancel();
        self.sender.take();
    }
    pub fn select_scope(&self, scope: ProjectKeysScope) -> Result<u64, ProjectKeysError> {
        let mut selected = self
            .selection
            .try_lock()
            .map_err(|_| ProjectKeysError::Busy)?;
        if selected.scope.as_ref() != Some(&scope) {
            selected.generation = selected.generation.wrapping_add(1).max(1);
            selected.scope = Some(scope);
            if let Ok(mut snapshot) = self.snapshot.try_lock() {
                *snapshot = None;
            }
        }
        Ok(selected.generation)
    }
    pub fn try_request(
        &self,
        fence: ProjectKeysFence,
        action: ProjectKeysAction,
    ) -> Result<(), Box<ProjectKeysAck>> {
        if self
            .outstanding
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < QUEUE_MAX).then_some(count + 1)
            })
            .is_err()
        {
            return Err(Box::new(ProjectKeysAck {
                fence,
                result: Err(ProjectKeysError::Busy),
            }));
        }
        match self
            .sender
            .as_ref()
            .unwrap()
            .try_send(Request { fence, action })
        {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(request) | mpsc::TrySendError::Disconnected(request)) => {
                self.outstanding.fetch_sub(1, Ordering::AcqRel);
                Err(Box::new(ProjectKeysAck {
                    fence: request.fence,
                    result: Err(ProjectKeysError::Busy),
                }))
            }
        }
    }
    pub fn try_ack(&self) -> Option<ProjectKeysAck> {
        let result = self.acks.try_recv().ok()?;
        self.outstanding.fetch_sub(1, Ordering::AcqRel);
        Some(result)
    }
    pub fn snapshot(&self) -> Option<Arc<ProjectKeysSnapshot>> {
        self.snapshot.try_lock().ok()?.clone()
    }
}
impl Drop for ProjectKeysWorker {
    fn drop(&mut self) {
        self.request_stop();
    }
}

fn execute(
    service: &ProjectKeysService,
    fence: &ProjectKeysFence,
    action: ProjectKeysAction,
    selection: &Mutex<Selection>,
    clipboard: &dyn Clipboard,
    runner: &dyn ProcessRunner,
) -> Result<ProjectKeysAckPayload, ProjectKeysError> {
    let scope = &fence.scope;
    match action {
        ProjectKeysAction::Refresh { key_id } => {
            service.refresh_scope(scope)?;
            if let Some(key_id) = key_id {
                let key = storage_error(service.db.project_key_get(
                    &scope.workspace_id,
                    scope.environment,
                    &key_id,
                ))?
                .ok_or(ProjectKeysError::Stale)?;
                if key.github.is_some() && !service.observe_github(&key, runner)? {
                    return Err(ProjectKeysError::Stale);
                }
            }
            let snapshot = service.snapshot(scope, fence.generation)?;
            if let Some(key) = snapshot.keys.iter().find(|key| {
                snapshot.settings.sources.contains(&key.source)
                    && matches!(
                        key.storage_state,
                        KeyStorageState::MissingSource
                            | KeyStorageState::EmptySource
                            | KeyStorageState::Locked
                            | KeyStorageState::Conflict
                            | KeyStorageState::Failed
                    )
            }) {
                return Err(match key.storage_state {
                    KeyStorageState::MissingSource => ProjectKeysError::SourceMissing,
                    KeyStorageState::EmptySource => ProjectKeysError::SourceEmpty,
                    KeyStorageState::Locked => ProjectKeysError::Locked,
                    KeyStorageState::Conflict => ProjectKeysError::Conflict,
                    _ => ProjectKeysError::Storage,
                });
            }
        }
        ProjectKeysAction::LoadKeyChanges {
            key_id,
            expected_revision,
        } => {
            service.key(scope, &key_id, expected_revision)?;
            let changes = storage_error(service.db.project_key_changes(
                &scope.workspace_id,
                scope.environment,
                &key_id,
            ))?;
            service.key(scope, &key_id, expected_revision)?;
            return Ok(ProjectKeysAckPayload::KeyChanges { key_id, changes });
        }
        ProjectKeysAction::LoadCredentials { after } => {
            let choices = storage_error(service.db.project_key_auth_credentials_page(
                &scope.workspace_id,
                scope.environment,
                128,
                after.as_deref(),
            ))?
            .into_iter()
            .map(|meta| CredentialChoice {
                id: meta.id,
                label: meta.label,
                provider: meta.provider,
            })
            .collect::<Vec<_>>();
            let after = if choices.len() == 128 {
                choices.last().map(|choice| choice.id.clone())
            } else {
                None
            };
            return Ok(ProjectKeysAckPayload::CredentialsPage { choices, after });
        }
        ProjectKeysAction::SetWatch {
            enabled,
            expected_revision,
        } => {
            let mut settings = storage_error(
                service
                    .db
                    .project_key_settings(&scope.workspace_id, scope.environment),
            )?;
            settings.watch_enabled = enabled;
            committed(storage_error(service.db.project_key_save_settings(
                &scope.workspace_id,
                scope.environment,
                &settings,
                Some(expected_revision),
            ))?)?;
        }
        ProjectKeysAction::SetSources {
            sources,
            expected_revision,
        } => {
            let mut settings = storage_error(
                service
                    .db
                    .project_key_settings(&scope.workspace_id, scope.environment),
            )?;
            settings.sources = sources;
            committed(storage_error(service.db.project_key_save_settings(
                &scope.workspace_id,
                scope.environment,
                &settings,
                Some(expected_revision),
            ))?)?;
            if scope.environment == KeyEnvironment::Development {
                return Ok(ProjectKeysAckPayload::RuntimeChanged);
            }
        }
        ProjectKeysAction::PickSource { pem } => {
            let mut picker = rfd::FileDialog::new();
            if pem {
                picker = picker.add_filter("PEM", &["pem", "key"]);
            }
            if let Some(path) = picker.pick_file() {
                let source = if pem {
                    KeySource::PemFile {
                        path: path.to_string_lossy().into(),
                    }
                } else {
                    let root = service.root(scope)?;
                    if path.parent() != Some(root.as_path()) {
                        return Err(ProjectKeysError::SourceUnsafe);
                    }
                    KeySource::Dotenv {
                        file: path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .ok_or(ProjectKeysError::SourceUnsafe)?
                            .into(),
                    }
                };
                return Ok(ProjectKeysAckPayload::SourcePicked(source));
            }
            return Err(ProjectKeysError::ScopeChanged);
        }
        ProjectKeysAction::PreviewSource { source } => {
            return service
                .preview(scope, source)
                .map(ProjectKeysAckPayload::Preview);
        }
        ProjectKeysAction::ImportSource { source, names } => {
            service.import_source(scope, source, names)?;
            if scope.environment == KeyEnvironment::Development {
                return Ok(ProjectKeysAckPayload::RuntimeChanged);
            }
        }
        ProjectKeysAction::SaveKey {
            key,
            expected_revision,
            value,
        } => {
            return service
                .save_key(scope, *key, expected_revision, value)
                .map(|key| ProjectKeysAckPayload::SavedKey(Box::new(key)));
        }
        ProjectKeysAction::DeleteKey {
            key_id,
            expected_revision,
        } => {
            let key = service.key(scope, &key_id, expected_revision)?;
            let usage = key.usage;
            let location = storage_error(service.db.project_key_value_location(
                &scope.workspace_id,
                scope.environment,
                &key_id,
                expected_revision,
            ))?;
            committed(storage_error(service.db.project_key_delete(
                &scope.workspace_id,
                scope.environment,
                &key_id,
                expected_revision,
            ))?)?;
            if let (Some(id), Some((meta, location))) = (key.credential_id, location)
                && meta.provider == "project-key"
                && storage_error(service.db.project_key_credential(&scope.workspace_id, &id))?
                    .is_none()
                && let Ok(slot) = secret::PhysicalSecretSlot::parse(location.keyring_username)
                && secret::delete_secret_bundle(service.secrets.as_ref(), &slot).is_ok()
            {
                let _ = service
                    .db
                    .acknowledge_physical_secret_slot_deleted(&id, slot.as_str());
            }
            return Ok(ProjectKeysAckPayload::DeletedKey { key_id, usage });
        }
        ProjectKeysAction::CopyKey {
            key_id,
            expected_revision,
        } => {
            {
                let selected = selection
                    .lock()
                    .map_err(|_| ProjectKeysError::ScopeChanged)?;
                if selected.scope.as_ref() != Some(scope) || selected.generation != fence.generation
                {
                    return Err(ProjectKeysError::ScopeChanged);
                }
            }
            let key = service.key(scope, &key_id, expected_revision)?;
            if key.source == KeySource::GitHubOnly {
                return Err(ProjectKeysError::Locked);
            }
            let bundle = service.bundle(scope, &key)?;
            // Keychain reads happen outside the selection lock. This final fence linearizes Copy
            // against a project/environment switch before the clipboard side effect starts.
            service.key(scope, &key_id, expected_revision)?;
            {
                let selected = selection
                    .lock()
                    .map_err(|_| ProjectKeysError::ScopeChanged)?;
                if selected.scope.as_ref() != Some(scope) || selected.generation != fence.generation
                {
                    return Err(ProjectKeysError::ScopeChanged);
                }
            }
            clipboard.copy(bundle.access())?;
            let still_current = selection.lock().is_ok_and(|selected| {
                selected.scope.as_ref() == Some(scope) && selected.generation == fence.generation
            });
            if !still_current {
                let _ = clipboard.clear_if_equal(bundle.access());
                return Err(ProjectKeysError::ScopeChanged);
            }
            if let Err(error) = service.key(scope, &key_id, expected_revision) {
                let _ = clipboard.clear_if_equal(bundle.access());
                return Err(error);
            }
            return Ok(ProjectKeysAckPayload::Copied);
        }
        ProjectKeysAction::ResolveConflict {
            key_id,
            expected_revision,
            resolution,
        } => {
            return service
                .resolve(scope, &key_id, expected_revision, resolution)
                .map(|key| ProjectKeysAckPayload::SavedKey(Box::new(key)));
        }
        ProjectKeysAction::SaveGitHubTarget {
            key_id,
            expected_revision,
            target,
        } => {
            let mut key = service.key(scope, &key_id, expected_revision)?;
            key.github = target.map(|target| KeyGitHubStatus {
                target: *target,
                state: GitHubRemoteState::Unlinked,
                last_sent_revision: None,
                remote_updated_at: None,
                error_code: None,
            });
            return committed(storage_error(service.db.project_key_save(
                &scope.workspace_id,
                scope.environment,
                &key,
                Some(expected_revision),
            ))?)
            .map(|key| ProjectKeysAckPayload::SavedKey(Box::new(key)));
        }
        ProjectKeysAction::SendGitHub {
            key_id,
            expected_revision,
            allow_external_change,
        } => {
            let key = service.key(scope, &key_id, expected_revision)?;
            if key.source == KeySource::GitHubOnly {
                return Err(ProjectKeysError::Locked);
            }
            if !allow_external_change
                && key
                    .github
                    .as_ref()
                    .is_some_and(|remote| remote.state == GitHubRemoteState::ExternalChange)
            {
                return Err(ProjectKeysError::ExternalChange);
            }
            if !storage_error(service.db.project_key_github_enqueue(
                &scope.workspace_id,
                scope.environment,
                &key_id,
                expected_revision,
                allow_external_change,
                now(),
            ))? {
                return Err(ProjectKeysError::Stale);
            }
        }
        ProjectKeysAction::SaveRecipe {
            recipe,
            expected_revision,
        } => {
            return committed(storage_error(service.db.project_key_recipe_save(
                &scope.workspace_id,
                scope.environment,
                &recipe,
                expected_revision,
            ))?)
            .map(ProjectKeysAckPayload::SavedRecipe);
        }
        ProjectKeysAction::DeleteRecipe {
            recipe_id,
            expected_revision,
        } => {
            committed(storage_error(service.db.project_key_recipe_delete(
                &scope.workspace_id,
                scope.environment,
                &recipe_id,
                expected_revision,
            ))?)?;
        }
        ProjectKeysAction::RunRecipe { .. } | ProjectKeysAction::LegacyAdvanced => {
            return Err(ProjectKeysError::InvalidInput);
        }
    }
    Ok(ProjectKeysAckPayload::Done)
}

fn publish_snapshot(
    target: &Mutex<Option<Arc<ProjectKeysSnapshot>>>,
    scope: &ProjectKeysScope,
    generation: u64,
    service: Option<&ProjectKeysService>,
    wake: &dyn Fn(),
) {
    let result = service
        .ok_or(ProjectKeysError::Storage)
        .and_then(|service| service.snapshot(scope, generation));
    let projects = if result.is_err() {
        service
            .and_then(|service| service.db.settings_workspace_projection_rows().ok())
            .map(|rows| {
                rows.into_iter()
                    .map(|row| ProjectChoice {
                        id: row.id,
                        name: row.name,
                        path: row.path,
                    })
                    .collect::<Vec<_>>()
            })
    } else {
        None
    };
    let changed = if let Ok(mut previous) = target.lock() {
        let next = match result {
            Ok(next) => next,
            Err(error) => {
                let mut next = previous
                    .as_deref()
                    .filter(|snapshot| {
                        snapshot.scope == *scope && snapshot.generation == generation
                    })
                    .cloned()
                    .unwrap_or_else(|| {
                        ProjectKeysSnapshot::unavailable(scope.clone(), generation, error)
                    });
                next.error = Some(error);
                if let Some(projects) = projects {
                    next.projects = projects;
                }
                next
            }
        };
        if previous.as_deref() != Some(&next) {
            *previous = Some(Arc::new(next));
            true
        } else {
            false
        }
    } else {
        false
    };
    if changed {
        wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_keys::{
        process::{ProcessEnvironment, ProcessOutput},
        tests::Fixture,
    };
    use secret::SecretString;
    use std::time::Instant;
    struct NoProcess;
    impl ProcessRunner for NoProcess {
        fn run(
            &self,
            _: &[String],
            _: &std::path::Path,
            _: &ProcessEnvironment,
            _: Option<&SecretString>,
            _: bool,
        ) -> Result<ProcessOutput, ProjectKeysError> {
            Err(ProjectKeysError::Process)
        }
    }
    struct NoClipboard;
    impl Clipboard for NoClipboard {
        fn copy(&self, _: &SecretString) -> Result<(), ProjectKeysError> {
            Err(ProjectKeysError::Clipboard)
        }
    }
    struct SlowClipboard {
        entered: SyncSender<()>,
        release: Mutex<Receiver<()>>,
        value: Mutex<Option<SecretString>>,
    }
    impl Clipboard for SlowClipboard {
        fn copy(&self, value: &SecretString) -> Result<(), ProjectKeysError> {
            let _ = self.entered.send(());
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .map_err(|_| ProjectKeysError::Clipboard)?;
            *self.value.lock().unwrap() = Some(SecretString::new(value.expose().into()));
            Ok(())
        }
        fn clear_if_equal(&self, value: &SecretString) -> Result<(), ProjectKeysError> {
            let mut current = self.value.lock().unwrap();
            if current
                .as_ref()
                .is_some_and(|current| current.expose() == value.expose())
            {
                *current = None;
            }
            Ok(())
        }
    }
    #[test]
    fn slow_clipboard_never_blocks_scope_selection_and_stale_completion_is_fenced() {
        let fixture = Fixture::new();
        let key = fixture.save(fixture.key("TOKEN", KeySource::Vault), "fixture-copy");
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let clipboard = Arc::new(SlowClipboard {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            value: Mutex::new(None),
        });
        let worker = ProjectKeysWorker::spawn_with(
            fixture.root.join("metadata.sqlite3"),
            fixture.store.clone(),
            clipboard.clone(),
            Arc::new(NoProcess),
            || {},
        );
        let generation = worker.select_scope(fixture.scope.clone()).unwrap();
        let fence = ProjectKeysFence {
            scope: fixture.scope.clone(),
            generation,
            request_id: 1,
        };
        assert!(
            worker
                .try_request(
                    fence,
                    ProjectKeysAction::CopyKey {
                        key_id: key.id.clone(),
                        expected_revision: key.revision
                    }
                )
                .is_ok()
        );
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let start = Instant::now();
        worker
            .select_scope(ProjectKeysScope {
                environment: KeyEnvironment::Production,
                ..fixture.scope.clone()
            })
            .unwrap();
        assert!(start.elapsed() < Duration::from_millis(100));
        let _ = worker.snapshot();
        assert!(start.elapsed() < Duration::from_millis(100));
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(ack) = worker.try_ack() {
                assert_eq!(ack.result, Err(ProjectKeysError::ScopeChanged));
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(clipboard.value.lock().unwrap().is_none());
    }
    #[test]
    fn independent_enrollment_and_restart_scan_both_environments_without_ui_selection() {
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join(".env"), "TOKEN=development-source\n").unwrap();
        std::fs::write(
            fixture.root.join(".env.production"),
            "TOKEN=production-source\n",
        )
        .unwrap();
        let mut production = fixture
            .service
            .db
            .project_key_settings(&fixture.scope.workspace_id, KeyEnvironment::Production)
            .unwrap();
        production.watch_enabled = true;
        production.sources = vec![KeySource::Dotenv {
            file: ".env.production".into(),
        }];
        fixture
            .service
            .db
            .project_key_save_settings(
                &fixture.scope.workspace_id,
                KeyEnvironment::Production,
                &production,
                Some(production.revision),
            )
            .unwrap();
        let spawn = || {
            ProjectKeysWorker::spawn_with(
                fixture.root.join("metadata.sqlite3"),
                fixture.store.clone(),
                Arc::new(NoClipboard),
                Arc::new(NoProcess),
                || {},
            )
        };
        let worker = spawn();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let dev = fixture
                .service
                .db
                .project_key_list(&fixture.scope.workspace_id, KeyEnvironment::Development)
                .unwrap();
            let prod = fixture
                .service
                .db
                .project_key_list(&fixture.scope.workspace_id, KeyEnvironment::Production)
                .unwrap();
            if let (Some(dev), Some(prod)) = (
                dev.iter().find(|key| key.name == "TOKEN"),
                prod.iter().find(|key| key.name == "TOKEN"),
            ) {
                assert_ne!(dev.credential_id, prod.credential_id);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(30));
        }
        assert!(worker.snapshot().is_none());
        drop(worker);
        std::fs::write(fixture.root.join(".env"), "TOKEN=after-restart\n").unwrap();
        let worker = spawn();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let key = fixture
                .service
                .db
                .project_key_list(&fixture.scope.workspace_id, KeyEnvironment::Development)
                .unwrap()
                .into_iter()
                .find(|key| key.name == "TOKEN")
                .unwrap();
            if fixture
                .service
                .bundle(&fixture.scope, &key)
                .unwrap()
                .access()
                .expose()
                == "after-restart"
            {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(30));
        }
        drop(worker);
    }
    fn wait_ack(worker: &ProjectKeysWorker) -> ProjectKeysAck {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(ack) = worker.try_ack() {
                return ack;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn wait_snapshot(
        worker: &ProjectKeysWorker,
        condition: impl Fn(&ProjectKeysSnapshot) -> bool,
    ) -> Arc<ProjectKeysSnapshot> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(snapshot) = worker.snapshot()
                && condition(&snapshot)
            {
                return snapshot;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn explicit_retry_repairs_missing_and_locked_sources_with_watching_disabled_and_history_is_real()
     {
        let fixture = Fixture::new();
        let source = KeySource::Dotenv {
            file: ".env".into(),
        };
        std::fs::write(fixture.root.join(".env"), "TOKEN=retry-value\n").unwrap();
        fixture
            .service
            .import_source(&fixture.scope, source, vec!["TOKEN".into()])
            .unwrap();
        let key = fixture
            .service
            .db
            .project_key_list(&fixture.scope.workspace_id, fixture.scope.environment)
            .unwrap()
            .remove(0);
        let saved_at = key.last_good_at;
        assert!(saved_at.is_some());
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
        std::fs::remove_file(fixture.root.join(".env")).unwrap();
        fixture.service.observe_key(&fixture.scope, &key).unwrap();
        let missing = fixture
            .service
            .db
            .project_key_get(
                &fixture.scope.workspace_id,
                fixture.scope.environment,
                &key.id,
            )
            .unwrap()
            .unwrap();
        assert_eq!(missing.storage_state, KeyStorageState::MissingSource);
        assert_eq!(missing.last_good_at, saved_at);
        let worker = ProjectKeysWorker::spawn_with(
            fixture.root.join("metadata.sqlite3"),
            fixture.store.clone(),
            Arc::new(NoClipboard),
            Arc::new(NoProcess),
            || {},
        );
        let generation = worker.select_scope(fixture.scope.clone()).unwrap();
        std::fs::write(fixture.root.join(".env"), "TOKEN=retry-value\n").unwrap();
        let send = |request_id, action| {
            assert!(
                worker
                    .try_request(
                        ProjectKeysFence {
                            scope: fixture.scope.clone(),
                            generation,
                            request_id
                        },
                        action
                    )
                    .is_ok()
            );
            wait_ack(&worker)
        };
        assert_eq!(
            send(1, ProjectKeysAction::Refresh { key_id: None }).result,
            Ok(ProjectKeysAckPayload::Done)
        );
        let snapshot = wait_snapshot(&worker, |snapshot| {
            snapshot
                .keys
                .iter()
                .any(|key| key.id == missing.id && key.storage_state == KeyStorageState::Stored)
        });
        let restored = snapshot
            .keys
            .iter()
            .find(|key| key.id == missing.id)
            .unwrap();
        assert_eq!(restored.last_good_revision, Some(restored.revision));
        fixture.store.1.store(true, Ordering::Release);
        assert_eq!(
            send(2, ProjectKeysAction::Refresh { key_id: None }).result,
            Err(ProjectKeysError::Locked)
        );
        wait_snapshot(&worker, |snapshot| {
            snapshot
                .keys
                .iter()
                .any(|key| key.id == missing.id && key.storage_state == KeyStorageState::Locked)
        });
        fixture.store.1.store(false, Ordering::Release);
        assert_eq!(
            send(3, ProjectKeysAction::Refresh { key_id: None }).result,
            Ok(ProjectKeysAckPayload::Done)
        );
        let snapshot = wait_snapshot(&worker, |snapshot| {
            snapshot
                .keys
                .iter()
                .any(|key| key.id == missing.id && key.storage_state == KeyStorageState::Stored)
        });
        let restored = snapshot
            .keys
            .iter()
            .find(|key| key.id == missing.id)
            .unwrap();
        let ack = send(
            4,
            ProjectKeysAction::LoadKeyChanges {
                key_id: restored.id.clone(),
                expected_revision: restored.revision,
            },
        );
        let Ok(ProjectKeysAckPayload::KeyChanges { key_id, changes }) = ack.result else {
            panic!("history acknowledgement missing")
        };
        assert_eq!(key_id, restored.id);
        assert!(changes.len() <= 32);
        assert!(
            changes
                .iter()
                .any(|event| event.storage_state == KeyStorageState::MissingSource)
        );
        assert!(
            changes
                .iter()
                .any(|event| event.storage_state == KeyStorageState::Locked)
        );
        assert!(
            changes
                .windows(2)
                .all(|pair| pair[0].sequence > pair[1].sequence)
        );
        let serialized = serde_json::to_string(&changes).unwrap();
        assert!(!serialized.contains("retry-value"));
    }
    #[test]
    fn scoped_loading_failure_has_visible_error_retry_and_other_scope_recovers() {
        let fixture = Fixture::new();
        fixture.save(fixture.key("TOKEN", KeySource::Vault), "fixture-value");
        let fail = Arc::new(AtomicBool::new(true));
        let factory: ServiceFactory = {
            let fail = fail.clone();
            let path = fixture.root.join("metadata.sqlite3");
            let store = fixture.store.clone();
            Arc::new(move || {
                if fail.load(Ordering::Acquire) {
                    Err(ProjectKeysError::Storage)
                } else {
                    crate::project_keys::tests::open_service(&path, store.clone())
                }
            })
        };
        let worker = ProjectKeysWorker::spawn_with_factory(
            factory,
            Arc::new(NoClipboard),
            Arc::new(NoProcess),
            || {},
        );
        let generation = worker.select_scope(fixture.scope.clone()).unwrap();
        let snapshot = wait_snapshot(&worker, |snapshot| {
            snapshot.error == Some(ProjectKeysError::Storage)
        });
        assert_eq!(snapshot.generation, generation);
        assert_eq!(snapshot.scope, fixture.scope);
        let request = |scope, generation, id| {
            assert!(
                worker
                    .try_request(
                        ProjectKeysFence {
                            scope,
                            generation,
                            request_id: id
                        },
                        ProjectKeysAction::Refresh { key_id: None }
                    )
                    .is_ok()
            );
            wait_ack(&worker)
        };
        assert_eq!(
            request(fixture.scope.clone(), generation, 1).result,
            Err(ProjectKeysError::Storage)
        );
        fail.store(false, Ordering::Release);
        assert_eq!(
            request(fixture.scope.clone(), generation, 2).result,
            Ok(ProjectKeysAckPayload::Done)
        );
        wait_snapshot(&worker, |snapshot| {
            snapshot.error.is_none() && snapshot.keys.len() == 1
        });
        let invalid = ProjectKeysScope {
            workspace_id: "fixture-missing-project".into(),
            ..fixture.scope.clone()
        };
        let other_generation = worker.select_scope(invalid.clone()).unwrap();
        assert_eq!(
            request(invalid.clone(), other_generation, 3).result,
            Err(ProjectKeysError::InvalidInput)
        );
        let snapshot = wait_snapshot(&worker, |snapshot| {
            snapshot.scope == invalid && snapshot.error.is_some()
        });
        assert!(
            snapshot
                .projects
                .iter()
                .any(|project| project.id == fixture.scope.workspace_id)
        );
        let generation = worker.select_scope(fixture.scope.clone()).unwrap();
        assert_eq!(
            request(fixture.scope.clone(), generation, 4).result,
            Ok(ProjectKeysAckPayload::Done)
        );
        wait_snapshot(&worker, |snapshot| {
            snapshot.scope == fixture.scope
                && snapshot.generation == generation
                && snapshot.error.is_none()
        });
    }
    #[cfg(unix)]
    #[test]
    fn dropping_worker_cancels_actual_build_and_removes_its_restricted_pem() {
        let fixture = Fixture::new();
        let source = fixture.root.join("input.pem");
        std::fs::write(&source, "fixture-pem-bytes").unwrap();
        fixture
            .service
            .import_source(
                &fixture.scope,
                KeySource::PemFile {
                    path: source.to_string_lossy().into(),
                },
                vec!["GITHUB_PRIVATE_KEY".into()],
            )
            .unwrap();
        let marker = fixture.root.join("child-marker");
        let recipe = ProjectBuildRecipe {
            id: uuid::Uuid::new_v4().to_string(),
            workspace_id: fixture.scope.workspace_id.clone(),
            environment: fixture.scope.environment,
            revision: 1,
            name: "cancellable fixture".into(),
            cwd: ".".into(),
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf '%s\\n%s\\n' \"$$\" \"$GITHUB_PRIVATE_KEY\" > \"$1\"; exec /bin/sleep 60"
                    .into(),
                "fixture".into(),
                marker.to_string_lossy().into(),
            ],
        };
        let recipe = committed(
            fixture
                .service
                .db
                .project_key_recipe_save(
                    &fixture.scope.workspace_id,
                    fixture.scope.environment,
                    &recipe,
                    None,
                )
                .unwrap(),
        )
        .unwrap();
        let runner = Arc::new(super::super::process::NativeProcessRunner::default());
        let worker = ProjectKeysWorker::spawn_with(
            fixture.root.join("metadata.sqlite3"),
            fixture.store.clone(),
            Arc::new(NoClipboard),
            runner,
            || {},
        );
        let generation = worker.select_scope(fixture.scope.clone()).unwrap();
        assert!(
            worker
                .try_request(
                    ProjectKeysFence {
                        scope: fixture.scope.clone(),
                        generation,
                        request_id: 1
                    },
                    ProjectKeysAction::RunRecipe {
                        recipe_id: recipe.id,
                        expected_revision: recipe.revision
                    }
                )
                .is_ok()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let (pid, path) = loop {
            if let Ok(text) = std::fs::read_to_string(&marker) {
                let mut lines = text.lines();
                if let (Some(pid), Some(path)) = (lines.next(), lines.next()) {
                    break (pid.parse::<i32>().unwrap(), PathBuf::from(path));
                }
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(path.exists());
        let start = Instant::now();
        drop(worker);
        assert!(start.elapsed() < Duration::from_millis(100));
        let deadline = Instant::now() + Duration::from_secs(3);
        while path.exists() || unsafe { libc::kill(pid, 0) } == 0 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!path.parent().unwrap().exists());
    }
    #[cfg(unix)]
    #[test]
    fn actual_host_exit_waits_for_build_cleanup() {
        if std::env::var_os("DEPPY_KEYS_SHUTDOWN_FIXTURE").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "project_keys::worker::tests::actual_host_exit_waits_for_build_cleanup",
                    "--nocapture",
                ])
                .env("DEPPY_KEYS_SHUTDOWN_FIXTURE", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout).unwrap();
            let path = stdout
                .lines()
                .find_map(|line| line.strip_prefix("KEY_SHUTDOWN_PATH="))
                .expect("child cleanup evidence");
            assert!(!std::path::Path::new(path).exists());
            let pid = stdout
                .lines()
                .find_map(|line| line.strip_prefix("KEY_SHUTDOWN_PID="))
                .unwrap()
                .parse::<i32>()
                .unwrap();
            // SAFETY: signal zero probes only the controlled synthetic child PID.
            assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
            return;
        }

        let fixture = Fixture::new();
        let source = fixture.root.join("input.pem");
        std::fs::write(&source, "fixture-pem-bytes").unwrap();
        fixture
            .service
            .import_source(
                &fixture.scope,
                KeySource::PemFile {
                    path: source.to_string_lossy().into(),
                },
                vec!["GITHUB_PRIVATE_KEY".into()],
            )
            .unwrap();
        let marker = fixture.root.join("child-marker");
        let recipe = ProjectBuildRecipe {
            id: uuid::Uuid::new_v4().to_string(),
            workspace_id: fixture.scope.workspace_id.clone(),
            environment: fixture.scope.environment,
            revision: 1,
            name: "cancellable fixture".into(),
            cwd: ".".into(),
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf '%s\\n%s\\n' \"$$\" \"$GITHUB_PRIVATE_KEY\" > \"$1\"; exec /bin/sleep 60"
                    .into(),
                "fixture".into(),
                marker.to_string_lossy().into(),
            ],
        };
        let recipe = committed(
            fixture
                .service
                .db
                .project_key_recipe_save(
                    &fixture.scope.workspace_id,
                    fixture.scope.environment,
                    &recipe,
                    None,
                )
                .unwrap(),
        )
        .unwrap();
        let runner = Arc::new(super::super::process::NativeProcessRunner::default());
        let mut worker = ProjectKeysWorker::spawn_with(
            fixture.root.join("metadata.sqlite3"),
            fixture.store.clone(),
            Arc::new(NoClipboard),
            runner,
            || {},
        );
        let generation = worker.select_scope(fixture.scope.clone()).unwrap();
        assert!(
            worker
                .try_request(
                    ProjectKeysFence {
                        scope: fixture.scope.clone(),
                        generation,
                        request_id: 1
                    },
                    ProjectKeysAction::RunRecipe {
                        recipe_id: recipe.id,
                        expected_revision: recipe.revision
                    }
                )
                .is_ok()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let (pid, path) = loop {
            if let Ok(text) = std::fs::read_to_string(&marker) {
                let mut lines = text.lines();
                if let (Some(pid), Some(path)) = (lines.next(), lines.next()) {
                    break (pid.parse::<i32>().unwrap(), PathBuf::from(path));
                }
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(path.exists());
        assert!(worker.shutdown(Duration::from_secs(5)));
        assert!(!path.exists());
        assert!(
            fixture
                .service
                .db
                .project_key_receipt_list(
                    &fixture.scope.workspace_id,
                    fixture.scope.environment,
                    10
                )
                .unwrap()
                .iter()
                .all(|receipt| receipt.finished_at.is_some())
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while path.exists() || unsafe { libc::kill(pid, 0) } == 0 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!path.parent().unwrap().exists());
        println!("KEY_SHUTDOWN_PATH={}", path.display());
        println!("KEY_SHUTDOWN_PID={pid}");
        drop(fixture);
        // This models the actual host lifetime: no background thread may be relied on after exit.
        std::process::exit(0);
    }

    #[test]
    fn clipboard_completion_rejects_a_value_rotation_and_remote_only_copy_send_are_read_only() {
        let fixture = Fixture::new();
        let key = fixture.save(fixture.key("TOKEN", KeySource::Vault), "before-copy");
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let clipboard = Arc::new(SlowClipboard {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            value: Mutex::new(None),
        });
        let worker = ProjectKeysWorker::spawn_with(
            fixture.root.join("metadata.sqlite3"),
            fixture.store.clone(),
            clipboard.clone(),
            Arc::new(NoProcess),
            || {},
        );
        let generation = worker.select_scope(fixture.scope.clone()).unwrap();
        assert!(
            worker
                .try_request(
                    ProjectKeysFence {
                        scope: fixture.scope.clone(),
                        generation,
                        request_id: 1
                    },
                    ProjectKeysAction::CopyKey {
                        key_id: key.id.clone(),
                        expected_revision: key.revision
                    }
                )
                .is_ok()
        );
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        fixture
            .service
            .save_key(
                &fixture.scope,
                key.clone(),
                Some(key.revision),
                ValueEdit::Replace(SensitiveDraft::new("after-copy".into()).unwrap()),
            )
            .unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(wait_ack(&worker).result, Err(ProjectKeysError::Stale));
        assert!(clipboard.value.lock().unwrap().is_none());
        let remote = fixture
            .service
            .save_key(
                &fixture.scope,
                fixture.key("REMOTE_TOKEN", KeySource::GitHubOnly),
                None,
                ValueEdit::Keep,
            )
            .unwrap();
        let before = fixture.store.2.load(Ordering::Acquire);
        for (request_id, action) in [
            (
                2,
                ProjectKeysAction::CopyKey {
                    key_id: remote.id.clone(),
                    expected_revision: remote.revision,
                },
            ),
            (
                3,
                ProjectKeysAction::SendGitHub {
                    key_id: remote.id.clone(),
                    expected_revision: remote.revision,
                    allow_external_change: false,
                },
            ),
        ] {
            assert!(
                worker
                    .try_request(
                        ProjectKeysFence {
                            scope: fixture.scope.clone(),
                            generation,
                            request_id
                        },
                        action
                    )
                    .is_ok()
            );
            assert_eq!(wait_ack(&worker).result, Err(ProjectKeysError::Locked));
        }
        assert_eq!(fixture.store.2.load(Ordering::Acquire), before);
    }
    #[test]
    fn development_source_mutations_ack_actual_legacy_runtime_change_and_production_stays_separate()
    {
        let fixture = Fixture::new();
        let selection = Mutex::new(Selection::default());
        let fence = ProjectKeysFence {
            scope: fixture.scope.clone(),
            generation: 1,
            request_id: 1,
        };
        let settings = fixture
            .service
            .db
            .project_key_settings(&fence.scope.workspace_id, fence.scope.environment)
            .unwrap();
        assert_eq!(
            execute(
                &fixture.service,
                &fence,
                ProjectKeysAction::SetSources {
                    sources: vec![KeySource::Dotenv {
                        file: ".env.local".into()
                    }],
                    expected_revision: settings.revision
                },
                &selection,
                &NoClipboard,
                &NoProcess
            ),
            Ok(ProjectKeysAckPayload::RuntimeChanged)
        );
        assert_eq!(
            fixture
                .service
                .db
                .env_source_files(&fence.scope.workspace_id)
                .unwrap(),
            vec![".env.local"]
        );
        let production = ProjectKeysFence {
            scope: ProjectKeysScope {
                environment: KeyEnvironment::Production,
                ..fence.scope.clone()
            },
            ..fence
        };
        let settings = fixture
            .service
            .db
            .project_key_settings(&production.scope.workspace_id, production.scope.environment)
            .unwrap();
        assert_eq!(
            execute(
                &fixture.service,
                &production,
                ProjectKeysAction::SetSources {
                    sources: vec![KeySource::Dotenv {
                        file: ".env.production".into()
                    }],
                    expected_revision: settings.revision
                },
                &selection,
                &NoClipboard,
                &NoProcess
            ),
            Ok(ProjectKeysAckPayload::Done)
        );
        assert_eq!(
            fixture
                .service
                .db
                .env_source_files(&production.scope.workspace_id)
                .unwrap(),
            vec![".env.local"]
        );
    }
}
