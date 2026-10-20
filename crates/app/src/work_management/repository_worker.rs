//! 렌더 경로에서 SQLite를 열지 않는 단일 요청 저장 worker.

use crate::lazy_worker::{LazyBoundedWorker, LazyWorkerSubmitError};
use deppy_core::WorkspaceId;
use std::path::PathBuf;
use std::time::Duration;
use storage::work_management::{
    CommandCenterSnapshot, CommandCenterStore, ProjectRevision, SaveOutcome, StoredProject,
};

pub(crate) enum RepositoryJob {
    Load {
        workspace_id: WorkspaceId,
        token: u64,
    },
    Save {
        snapshot: Box<CommandCenterSnapshot>,
        expected: Option<ProjectRevision>,
        edit_generation: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepositoryError {
    Unavailable,
}

pub(crate) enum RepositoryResult {
    Loaded {
        workspace_id: WorkspaceId,
        token: u64,
        result: Result<Option<Box<StoredProject>>, RepositoryError>,
    },
    Saved {
        workspace_id: WorkspaceId,
        edit_generation: u64,
        result: Result<SaveOutcome, RepositoryError>,
    },
}

pub(crate) struct RepositoryWorker {
    worker: LazyBoundedWorker<RepositoryJob, RepositoryResult>,
}

impl RepositoryWorker {
    pub fn new(data_dir: PathBuf, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let worker = LazyBoundedWorker::new(
            "command-center-store",
            Duration::from_secs(20),
            move || {
                let data_dir = data_dir.clone();
                let mut store: Option<CommandCenterStore> = None;
                move |job| execute(&data_dir, &mut store, job)
            },
            wake,
        );
        Self { worker }
    }

    pub fn try_request(
        &mut self,
        job: RepositoryJob,
    ) -> Result<(), LazyWorkerSubmitError<RepositoryJob>> {
        self.worker.try_request(job)
    }

    pub fn try_recv(&mut self) -> Option<Result<RepositoryResult, RepositoryError>> {
        self.worker.try_recv().map(|outcome| {
            outcome
                .into_result()
                .map_err(|_| RepositoryError::Unavailable)
        })
    }
}

fn execute(
    data_dir: &std::path::Path,
    store: &mut Option<CommandCenterStore>,
    job: RepositoryJob,
) -> RepositoryResult {
    if store.is_none() {
        *store = CommandCenterStore::open(data_dir).ok();
    }
    match job {
        RepositoryJob::Load {
            workspace_id,
            token,
        } => {
            let result = store
                .as_ref()
                .ok_or(RepositoryError::Unavailable)
                .and_then(|store| {
                    store
                        .load_workspace(&workspace_id)
                        .map(|record| record.map(Box::new))
                        .map_err(|_| RepositoryError::Unavailable)
                });
            RepositoryResult::Loaded {
                workspace_id,
                token,
                result,
            }
        }
        RepositoryJob::Save {
            snapshot,
            expected,
            edit_generation,
        } => {
            let result = store
                .as_mut()
                .ok_or(RepositoryError::Unavailable)
                .and_then(|store| {
                    store
                        .save_workspace(&snapshot, expected)
                        .map_err(|_| RepositoryError::Unavailable)
                });
            RepositoryResult::Saved {
                workspace_id: snapshot.config.workspace_id,
                edit_generation,
                result,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deppy_core::work_management::{AgentSelection, ConfigRevision, ProjectConfig};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use storage::work_management::WorkDraft;

    fn recv(worker: &mut RepositoryWorker) -> RepositoryResult {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = worker.try_recv() {
                return result.expect("worker infrastructure");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "repository result deadline"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn repository_roundtrip_correlates_exact_edits_and_preserves_original_metadata() {
        let dir = std::env::temp_dir().join(format!("deppy-cc-worker-{}", uuid::Uuid::new_v4()));
        let wake_count = Arc::new(AtomicUsize::new(0));
        let signal = wake_count.clone();
        let mut worker = RepositoryWorker::new(dir.clone(), move || {
            signal.fetch_add(1, Ordering::SeqCst);
        });
        assert!(!dir.exists(), "constructor must not create a profile or DB");
        std::fs::create_dir_all(&dir).unwrap();
        let original = b"original fixture metadata stays untouched";
        std::fs::write(dir.join("metadata.sqlite3"), original).unwrap();
        let ws = WorkspaceId("workspace-a".into());
        let agent = AgentSelection {
            provider: "codex".into(),
            model: None,
            effort: None,
        };
        let snapshot = CommandCenterSnapshot {
            config: ProjectConfig {
                workspace_id: ws.clone(),
                revision: ConfigRevision(1),
                name: "프로젝트".into(),
                folder: "/fixture".into(),
                main_agent: agent.clone(),
                worker_agent: agent,
                max_workers: 2,
                confirm_plan: true,
                use_worktrees: true,
            },
            main_binding: None,
            goals: vec![],
            tasks: vec![],
            attempts: vec![],
            events: vec![],
            outbox: vec![],
            drafts: vec![WorkDraft {
                target_key: "main".into(),
                text: "첫 번째 초안".into(),
                delivery_uncertain: true,
            }],
        };
        assert!(
            worker
                .try_request(RepositoryJob::Save {
                    snapshot: Box::new(snapshot.clone()),
                    expected: None,
                    edit_generation: 17
                })
                .is_ok()
        );
        let RepositoryResult::Saved {
            workspace_id,
            edit_generation,
            result,
        } = recv(&mut worker)
        else {
            panic!("expected save result");
        };
        assert_eq!(workspace_id, ws);
        assert_eq!(edit_generation, 17);
        assert!(matches!(result.unwrap(), SaveOutcome::Saved { .. }));
        assert!(
            worker
                .try_request(RepositoryJob::Load {
                    workspace_id: ws.clone(),
                    token: 23
                })
                .is_ok()
        );
        let RepositoryResult::Loaded {
            workspace_id,
            token,
            result,
        } = recv(&mut worker)
        else {
            panic!("expected load result");
        };
        assert_eq!(workspace_id, ws);
        assert_eq!(token, 23);
        assert_eq!(result.unwrap().unwrap().snapshot, snapshot);
        assert_eq!(
            std::fs::read(dir.join("metadata.sqlite3")).unwrap(),
            original
        );
        assert!(wake_count.load(Ordering::SeqCst) >= 2);
        drop(worker);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
