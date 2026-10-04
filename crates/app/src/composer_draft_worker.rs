//! App-owned prompt persistence: one active snapshot and one coalesced latest snapshot.
//! No filesystem work or thread creation in render. A single writer commits revisions in order;
//! completed older revisions cannot overwrite a newer commit/status. Drop drains the latest
//! accepted snapshot before joining, rather than silently discarding a final edit.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::composer_drafts::{DraftError, DraftFileVersion, DraftSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DraftSaveStatus {
    Idle,
    Pending { revision: u64 },
    Saved { revision: u64 },
    Failed { revision: u64, error: DraftError },
    RecoveryRequired { error: DraftError },
}

impl DraftSaveStatus {
    pub(crate) fn write_blocking_error(self) -> Option<DraftError> {
        match self {
            Self::RecoveryRequired { .. } => Some(DraftError::RecoveryRequired),
            Self::Failed {
                error: DraftError::Conflict,
                ..
            } => Some(DraftError::Conflict),
            _ => None,
        }
    }
}

struct SaveRequest {
    revision: u64,
    snapshot: Arc<DraftSnapshot>,
}

struct State {
    pending: Option<SaveRequest>,
    latest_revision: u64,
    committed_revision: u64,
    status: DraftSaveStatus,
    stopping: bool,
}

pub(crate) struct DraftSaveWorker {
    path: PathBuf,
    file_version: DraftFileVersion,
    state: Arc<(Mutex<State>, Condvar)>,
    wake: Arc<dyn Fn() + Send + Sync>,
    handle: Option<JoinHandle<()>>,
}

impl DraftSaveWorker {
    pub(crate) fn new(
        path: PathBuf,
        recovery_error: Option<DraftError>,
        file_version: DraftFileVersion,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            path,
            file_version,
            state: Arc::new((
                Mutex::new(State {
                    pending: None,
                    latest_revision: 0,
                    committed_revision: 0,
                    status: recovery_error.map_or(DraftSaveStatus::Idle, |error| {
                        DraftSaveStatus::RecoveryRequired { error }
                    }),
                    stopping: false,
                }),
                Condvar::new(),
            )),
            wake: Arc::new(wake),
            handle: None,
        }
    }

    pub(crate) fn shutdown(&mut self) {
        {
            let mut guard = self
                .state
                .0
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            guard.stopping = true;
        }
        self.state.1.notify_one();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub(crate) fn status(&self) -> DraftSaveStatus {
        self.state
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .status
    }

    /// Completion status used by the host's submission checkpoint gate.
    pub(crate) fn checkpoint_status(&self) -> DraftSaveStatus {
        let state = self
            .state
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match state.status {
            DraftSaveStatus::Pending { .. } if state.committed_revision > 0 => {
                DraftSaveStatus::Saved {
                    revision: state.committed_revision,
                }
            }
            status => status,
        }
    }

    /// Known-unsent errors are visible to the host. At most two bounded snapshots are retained,
    /// regardless of edit count. Revisions must be strictly increasing across retries too.
    pub(crate) fn request(
        &mut self,
        revision: u64,
        snapshot: Arc<DraftSnapshot>,
    ) -> Result<(), DraftError> {
        snapshot.validate()?;
        {
            let mut state = self
                .state
                .0
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some(error) = state.status.write_blocking_error() {
                return Err(error);
            }
            if state.stopping || revision <= state.latest_revision {
                return Err(DraftError::StaleRevision);
            }
            state.latest_revision = revision;
            state.pending = Some(SaveRequest { revision, snapshot });
            state.status = DraftSaveStatus::Pending { revision };
        }
        if self.handle.is_none() {
            let state = Arc::clone(&self.state);
            let path = self.path.clone();
            let wake = Arc::clone(&self.wake);
            let file_version = self.file_version;
            match std::thread::Builder::new()
                .name("composer-draft-save".into())
                .spawn(move || run(state, path, file_version, wake))
            {
                Ok(handle) => self.handle = Some(handle),
                Err(_) => {
                    let mut state = self
                        .state
                        .0
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    state.pending = None;
                    state.status = DraftSaveStatus::Failed {
                        revision,
                        error: DraftError::WorkerUnavailable,
                    };
                    return Err(DraftError::WorkerUnavailable);
                }
            }
        }
        self.state.1.notify_one();
        Ok(())
    }
}

fn run(
    state: Arc<(Mutex<State>, Condvar)>,
    path: PathBuf,
    file_version: DraftFileVersion,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    run_with_save(state, file_version, wake, |snapshot, version| {
        snapshot.save_checked(&path, version)
    });
}

fn run_with_save(
    state: Arc<(Mutex<State>, Condvar)>,
    mut file_version: DraftFileVersion,
    wake: Arc<dyn Fn() + Send + Sync>,
    mut save: impl FnMut(&DraftSnapshot, DraftFileVersion) -> Result<DraftFileVersion, DraftError>,
) {
    loop {
        let request = {
            let mut guard = state.0.lock().unwrap_or_else(|poison| poison.into_inner());
            while guard.pending.is_none() && !guard.stopping {
                guard = state
                    .1
                    .wait(guard)
                    .unwrap_or_else(|poison| poison.into_inner());
            }
            let Some(request) = guard.pending.take() else {
                return;
            };
            request
        };
        // No shared lock is held while validating, serializing, syncing or renaming.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            save(&request.snapshot, file_version)
        }))
        .unwrap_or(Err(DraftError::WorkerUnavailable));
        if let Ok(version) = result {
            file_version = version;
        }
        {
            let mut guard = state.0.lock().unwrap_or_else(|poison| poison.into_inner());
            if result.is_ok() {
                guard.committed_revision = request.revision;
            }
            if request.revision == guard.latest_revision {
                guard.status = match result {
                    Ok(_) => DraftSaveStatus::Saved {
                        revision: request.revision,
                    },
                    Err(error) => DraftSaveStatus::Failed {
                        revision: request.revision,
                        error,
                    },
                };
            }
        }
        wake();
    }
}

impl Drop for DraftSaveWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer_drafts::DraftRecord;
    fn directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!("deppy-pr5-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        path
    }
    fn snapshot(text: &str) -> Arc<DraftSnapshot> {
        Arc::new(DraftSnapshot {
            drafts: vec![DraftRecord {
                key: "session".into(),
                workspace_id: "ws".into(),
                delivery_uncertain: false,
                text: Arc::from(text),
            }],
        })
    }
    fn wait(worker: &DraftSaveWorker) -> DraftSaveStatus {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let status = worker.status();
            if !matches!(status, DraftSaveStatus::Pending { .. }) {
                return status;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    #[test]
    fn composer_checkpoint_completion_is_not_hidden_by_a_later_autosave() {
        let dir = directory();
        let path = dir.join("drafts.json");
        let mut worker = DraftSaveWorker::new(path.clone(), None, DraftFileVersion::Missing, || {});
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let state = Arc::clone(&worker.state);
        let save_path = path.clone();
        worker.handle = Some(std::thread::spawn(move || {
            run_with_save(
                state,
                DraftFileVersion::Missing,
                Arc::new(|| {}),
                move |snapshot, version| {
                    entered_tx.send(()).unwrap();
                    release_rx
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                    snapshot.save_checked(&save_path, version)
                },
            )
        }));
        worker
            .request(1, snapshot("submission checkpoint"))
            .unwrap();
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        worker.request(2, snapshot("later editing")).unwrap();
        release_tx.send(()).unwrap();
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(worker.status(), DraftSaveStatus::Pending { revision: 2 });
        assert_eq!(
            DraftSnapshot::load_startup(&path).snapshot,
            *snapshot("submission checkpoint")
        );
        let completed = worker.checkpoint_status();
        // Release before assertion so a failing test never leaves a blocked worker.
        release_tx.send(()).unwrap();
        worker.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(completed, DraftSaveStatus::Saved { revision: 1 });
    }

    #[test]
    fn pr5_worker_latest_coalesces_and_drop_drains_original_order() {
        let dir = directory();
        let path = dir.join("drafts.json");
        let mut worker = DraftSaveWorker::new(path.clone(), None, DraftFileVersion::Missing, || {});
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let state = Arc::clone(&worker.state);
        let save_path = path.clone();
        let commits = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&commits);
        worker.handle = Some(std::thread::spawn(move || {
            run_with_save(
                state,
                DraftFileVersion::Missing,
                Arc::new(|| {}),
                move |snapshot, version| {
                    entered_tx
                        .send(snapshot.drafts[0].text.to_string())
                        .unwrap();
                    release_rx.recv().unwrap();
                    let result = snapshot.save_checked(&save_path, version);
                    observed
                        .lock()
                        .unwrap()
                        .push(snapshot.drafts[0].text.to_string());
                    result
                },
            )
        }));
        worker.request(1, snapshot("first")).unwrap();
        assert_eq!(
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            "first"
        );
        let replaced = snapshot("replaced");
        let weak = Arc::downgrade(&replaced);
        worker.request(2, replaced).unwrap();
        for revision in 3..=32 {
            worker
                .request(revision, snapshot(&revision.to_string()))
                .unwrap()
        }
        assert!(weak.upgrade().is_none());
        release_tx.send(()).unwrap();
        assert_eq!(
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            "32"
        );
        assert_eq!(worker.status(), DraftSaveStatus::Pending { revision: 32 });
        release_tx.send(()).unwrap();
        assert_eq!(wait(&worker), DraftSaveStatus::Saved { revision: 32 });
        assert_eq!(
            worker.request(31, snapshot("stale")),
            Err(DraftError::StaleRevision)
        );
        assert_eq!(*commits.lock().unwrap(), vec!["first", "32"]);
        drop(worker);
        assert_eq!(DraftSnapshot::load_startup(&path).snapshot, *snapshot("32"));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn pr5_worker_recovery_conflict_and_read_failure_preserve_original_and_visible_status() {
        let dir = directory();
        let path = dir.join("drafts.json");
        std::fs::write(&path, b"{ corrupt original").unwrap();
        let mut worker = DraftSaveWorker::new(
            path.clone(),
            Some(DraftError::Corrupt),
            DraftFileVersion::Missing,
            || {},
        );
        assert_eq!(
            worker.request(1, snapshot("edit")),
            Err(DraftError::RecoveryRequired)
        );
        assert!(worker.handle.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"{ corrupt original");
        drop(worker);
        std::fs::remove_file(&path).unwrap();
        let version = snapshot("original")
            .save_checked(&path, DraftFileVersion::Missing)
            .unwrap();
        let mut worker = DraftSaveWorker::new(path.clone(), None, version, || {});
        std::fs::write(&path, b"{\"drafts\":[]}").unwrap();
        let before = std::fs::read(&path).unwrap();
        worker.request(1, snapshot("unsaved")).unwrap();
        assert_eq!(
            wait(&worker),
            DraftSaveStatus::Failed {
                revision: 1,
                error: DraftError::Conflict
            }
        );
        assert_eq!(
            worker.request(2, snapshot("later")),
            Err(DraftError::Conflict)
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(worker);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let mut worker = DraftSaveWorker::new(path.clone(), None, DraftFileVersion::Missing, || {});
        worker.request(1, snapshot("retry")).unwrap();
        assert_eq!(
            wait(&worker),
            DraftSaveStatus::Failed {
                revision: 1,
                error: DraftError::ReadFailed
            }
        );
        std::fs::remove_dir(&path).unwrap();
        worker.request(2, snapshot("retry")).unwrap();
        drop(worker);
        assert_eq!(
            DraftSnapshot::load_startup(&path).snapshot,
            *snapshot("retry")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn pr5_worker_drop_commits_latest_without_prior_poll() {
        let dir = directory();
        let path = dir.join("drafts.json");
        let mut worker = DraftSaveWorker::new(path.clone(), None, DraftFileVersion::Missing, || {});
        for revision in 1..=32 {
            worker
                .request(revision, snapshot(&revision.to_string()))
                .unwrap()
        }
        drop(worker);
        assert_eq!(DraftSnapshot::load_startup(&path).snapshot, *snapshot("32"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
