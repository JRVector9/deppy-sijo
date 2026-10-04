//! App-owned prompt persistence: one active snapshot and one coalesced latest snapshot.
//! No filesystem work or thread creation in render. A single writer commits revisions in order;
//! completed older revisions cannot overwrite a newer commit/status. Drop drains the latest
//! accepted snapshot before joining, rather than silently discarding a final edit.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::prompt_library::{PromptLibrary, PromptLibraryError, PromptLibraryFileVersion};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptLibrarySaveStatus {
    Idle,
    Pending {
        revision: u64,
    },
    Saved {
        revision: u64,
    },
    Failed {
        revision: u64,
        error: PromptLibraryError,
    },
    RecoveryRequired {
        error: PromptLibraryError,
    },
}

impl PromptLibrarySaveStatus {
    pub(crate) fn write_blocking_error(self) -> Option<PromptLibraryError> {
        match self {
            Self::RecoveryRequired { .. } => Some(PromptLibraryError::RecoveryRequired),
            Self::Failed {
                error: PromptLibraryError::Conflict,
                ..
            } => Some(PromptLibraryError::Conflict),
            _ => None,
        }
    }
}

struct SaveRequest {
    revision: u64,
    library: Arc<PromptLibrary>,
}

struct State {
    pending: Option<SaveRequest>,
    latest_revision: u64,
    status: PromptLibrarySaveStatus,
    stopping: bool,
}

pub(crate) struct PromptLibrarySaveWorker {
    path: PathBuf,
    file_version: PromptLibraryFileVersion,
    state: Arc<(Mutex<State>, Condvar)>,
    wake: Arc<dyn Fn() + Send + Sync>,
    handle: Option<JoinHandle<()>>,
}

impl PromptLibrarySaveWorker {
    pub(crate) fn new(
        path: PathBuf,
        recovery_error: Option<PromptLibraryError>,
        file_version: PromptLibraryFileVersion,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            path,
            file_version,
            state: Arc::new((
                Mutex::new(State {
                    pending: None,
                    latest_revision: 0,
                    status: recovery_error.map_or(PromptLibrarySaveStatus::Idle, |error| {
                        PromptLibrarySaveStatus::RecoveryRequired { error }
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

    pub(crate) fn status(&self) -> PromptLibrarySaveStatus {
        self.state
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .status
    }

    /// Known-unsent errors are visible to the host. At most two bounded snapshots are retained,
    /// regardless of edit count. Revisions must be strictly increasing across retries too.
    pub(crate) fn request(
        &mut self,
        revision: u64,
        library: Arc<PromptLibrary>,
    ) -> Result<(), PromptLibraryError> {
        library.validate()?;
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
                return Err(PromptLibraryError::StaleRevision);
            }
            state.latest_revision = revision;
            state.pending = Some(SaveRequest { revision, library });
            state.status = PromptLibrarySaveStatus::Pending { revision };
        }
        if self.handle.is_none() {
            let state = Arc::clone(&self.state);
            let path = self.path.clone();
            let wake = Arc::clone(&self.wake);
            let file_version = self.file_version;
            match std::thread::Builder::new()
                .name("prompt-library-save".into())
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
                    state.status = PromptLibrarySaveStatus::Failed {
                        revision,
                        error: PromptLibraryError::WorkerUnavailable,
                    };
                    return Err(PromptLibraryError::WorkerUnavailable);
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
    file_version: PromptLibraryFileVersion,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    run_with_save(state, file_version, wake, |library, version| {
        library.save_checked(&path, version)
    });
}

fn run_with_save(
    state: Arc<(Mutex<State>, Condvar)>,
    mut file_version: PromptLibraryFileVersion,
    wake: Arc<dyn Fn() + Send + Sync>,
    mut save: impl FnMut(
        &PromptLibrary,
        PromptLibraryFileVersion,
    ) -> Result<PromptLibraryFileVersion, PromptLibraryError>,
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
            save(&request.library, file_version)
        }))
        .unwrap_or(Err(PromptLibraryError::WorkerUnavailable));
        if let Ok(version) = result {
            file_version = version;
        }
        {
            let mut guard = state.0.lock().unwrap_or_else(|poison| poison.into_inner());
            if request.revision == guard.latest_revision {
                guard.status = match result {
                    Ok(_) => PromptLibrarySaveStatus::Saved {
                        revision: request.revision,
                    },
                    Err(error) => PromptLibrarySaveStatus::Failed {
                        revision: request.revision,
                        error,
                    },
                };
            }
        }
        wake();
    }
}

impl Drop for PromptLibrarySaveWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt_library::{Prompt, PromptLibraryLoad};

    fn dir() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("deppy-library-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        path
    }

    fn library(value: &str) -> Arc<PromptLibrary> {
        Arc::new(PromptLibrary {
            prompts: vec![Prompt {
                id: "one".into(),
                title: value.into(),
                body: value.into(),
                tags: vec![],
            }],
        })
    }

    fn wait(worker: &PromptLibrarySaveWorker) -> PromptLibrarySaveStatus {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let status = worker.status();
            if !matches!(status, PromptLibrarySaveStatus::Pending { .. }) {
                return status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "save worker completion deadline"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn pr3_worker_is_inert_until_admission() {
        let dir = dir();
        let path = dir.join("library.json");
        let worker = PromptLibrarySaveWorker::new(
            path.clone(),
            None,
            PromptLibraryFileVersion::Missing,
            || {},
        );
        assert!(worker.handle.is_none());
        assert!(!path.exists());
        drop(worker);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr3_worker_newest_revision_wins_and_stale_requests_cannot_replace_it() {
        let dir = dir();
        let path = dir.join("library.json");
        let mut worker = PromptLibrarySaveWorker::new(
            path.clone(),
            None,
            PromptLibraryFileVersion::Missing,
            || {},
        );
        for revision in 1..=64 {
            worker
                .request(revision, library(&revision.to_string()))
                .unwrap();
        }
        assert_eq!(
            wait(&worker),
            PromptLibrarySaveStatus::Saved { revision: 64 }
        );
        assert_eq!(
            worker.request(63, library("stale")),
            Err(PromptLibraryError::StaleRevision)
        );
        assert_eq!(
            PromptLibrary::load(&path),
            PromptLibraryLoad::Loaded((*library("64")).clone())
        );
        drop(worker);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no orphaned temporary files"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr3_worker_coalesces_while_real_save_is_paused_and_keeps_latest_pending_status() {
        let dir = dir();
        let path = dir.join("library.json");
        let mut worker = PromptLibrarySaveWorker::new(
            path.clone(),
            None,
            PromptLibraryFileVersion::Missing,
            || {},
        );
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let state = Arc::clone(&worker.state);
        let save_path = path.clone();
        let commits = Arc::new(Mutex::new(Vec::new()));
        let observed_commits = Arc::clone(&commits);
        worker.handle = Some(std::thread::spawn(move || {
            run_with_save(
                state,
                PromptLibraryFileVersion::Missing,
                Arc::new(|| {}),
                move |library, version| {
                    entered_tx.send(library.prompts[0].title.clone()).unwrap();
                    release_rx.recv().unwrap();
                    let result = library.save_checked(&save_path, version);
                    observed_commits
                        .lock()
                        .unwrap()
                        .push(library.prompts[0].title.clone());
                    result
                },
            )
        }));
        worker.request(1, library("1")).unwrap();
        assert_eq!(
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            "1"
        );
        let superseded = library("2");
        let weak = Arc::downgrade(&superseded);
        worker.request(2, superseded).unwrap();
        for revision in 3..=64 {
            worker
                .request(revision, library(&revision.to_string()))
                .unwrap();
        }
        assert!(
            weak.upgrade().is_none(),
            "superseded pending snapshots are released"
        );
        assert_eq!(
            worker.status(),
            PromptLibrarySaveStatus::Pending { revision: 64 }
        );
        release_tx.send(()).unwrap();
        assert_eq!(
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            "64"
        );
        assert_eq!(
            worker.status(),
            PromptLibrarySaveStatus::Pending { revision: 64 },
            "completion of revision 1 must not claim latest data saved"
        );
        release_tx.send(()).unwrap();
        assert_eq!(
            wait(&worker),
            PromptLibrarySaveStatus::Saved { revision: 64 }
        );
        assert_eq!(*commits.lock().unwrap(), vec!["1", "64"]);
        drop(worker);
        assert_eq!(
            PromptLibrary::load(&path),
            PromptLibraryLoad::Loaded((*library("64")).clone())
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr3_worker_drop_drains_final_accepted_edit() {
        let dir = dir();
        let path = dir.join("library.json");
        let mut worker = PromptLibrarySaveWorker::new(
            path.clone(),
            None,
            PromptLibraryFileVersion::Missing,
            || {},
        );
        for revision in 1..=32 {
            worker
                .request(revision, library(&revision.to_string()))
                .unwrap();
        }
        drop(worker);
        assert_eq!(
            PromptLibrary::load(&path),
            PromptLibraryLoad::Loaded((*library("32")).clone())
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr3_worker_failed_save_preserves_original_and_can_retry_new_revision() {
        let dir = dir();
        let path = dir.join("library.json");
        std::fs::create_dir(&path).unwrap();
        let preserved = path.join("recoverable");
        std::fs::write(&preserved, b"original").unwrap();
        let mut worker = PromptLibrarySaveWorker::new(
            path.clone(),
            None,
            PromptLibraryFileVersion::Missing,
            || {},
        );
        worker.request(1, library("first")).unwrap();
        assert_eq!(
            wait(&worker),
            PromptLibrarySaveStatus::Failed {
                revision: 1,
                error: PromptLibraryError::ReadFailed
            }
        );
        assert_eq!(std::fs::read(&preserved).unwrap(), b"original");
        std::fs::remove_dir_all(&path).unwrap();
        worker.request(2, library("retry")).unwrap();
        assert_eq!(
            wait(&worker),
            PromptLibrarySaveStatus::Saved { revision: 2 }
        );
        drop(worker);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pr3_worker_readonly_directory_failure_keeps_real_original_and_retries() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        } // root bypasses this OS permission fixture
        let dir = dir();
        let path = dir.join("library.json");
        library("original").save(&path).unwrap();
        let startup = PromptLibrary::load_startup(&path);
        let before = std::fs::read(&path).unwrap();
        let mut worker =
            PromptLibrarySaveWorker::new(path.clone(), None, startup.file_version, || {});
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        worker.request(1, library("changed")).unwrap();
        let status = wait(&worker);
        // Restore permissions before asserting so a failure does not leave an undeletable fixture.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            status,
            PromptLibrarySaveStatus::Failed {
                revision: 1,
                error: PromptLibraryError::WriteFailed
            }
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        worker.request(2, library("retry")).unwrap();
        assert_eq!(
            wait(&worker),
            PromptLibrarySaveStatus::Saved { revision: 2 }
        );
        drop(worker);
        assert_eq!(
            PromptLibrary::load(&path),
            PromptLibraryLoad::Loaded((*library("retry")).clone())
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr3_worker_external_conflict_stays_readonly_and_preserves_external_edit() {
        let dir = dir();
        let path = dir.join("library.json");
        library("original").save(&path).unwrap();
        let startup = PromptLibrary::load_startup(&path);
        let mut worker =
            PromptLibrarySaveWorker::new(path.clone(), None, startup.file_version, || {});
        library("external edit").save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        worker.request(1, library("unsaved edit")).unwrap();
        assert_eq!(
            wait(&worker),
            PromptLibrarySaveStatus::Failed {
                revision: 1,
                error: PromptLibraryError::Conflict
            }
        );
        assert_eq!(
            worker.request(2, library("later edit")),
            Err(PromptLibraryError::Conflict)
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(worker);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr3_worker_recovery_required_never_writes_even_after_edit() {
        let dir = dir();
        let path = dir.join("library.json");
        std::fs::write(&path, b"{ corrupt original").unwrap();
        let mut worker = PromptLibrarySaveWorker::new(
            path.clone(),
            Some(PromptLibraryError::Corrupt),
            PromptLibraryFileVersion::Missing,
            || {},
        );
        assert_eq!(
            worker.request(1, library("new edit")),
            Err(PromptLibraryError::RecoveryRequired)
        );
        assert!(worker.handle.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"{ corrupt original");
        drop(worker);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
