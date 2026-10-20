//! Task-owned worktrees. Only the lazy worker performs Git or filesystem operations.
//!
//! A durable reservation precedes creation. An interrupted/unknown creation is never replayed or
//! cleaned up automatically. The host must still revalidate the result's exact job and runtime cwd
//! immediately before launching an agent; this proof is not a runtime directory capability.

use crate::lazy_worker::{LazyBoundedWorker, LazyWorkerSubmitError};
use deppy_core::WorkspaceId;
use deppy_core::work_management::{AttemptId, ConfigRevision, OperationId, TaskId};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const RECORD_MAX_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnsureTaskWorktree {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub operation_id: OperationId,
    pub project_folder: String,
    pub config_revision: ConfigRevision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnedTaskWorktree {
    pub path: String,
    /// Canonical `.deppy/worktrees` directory for the dispatch policy.
    pub root: String,
    pub owner: TaskId,
    pub proof: TaskWorktreeProof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskWorktreeProof {
    workspace_id: WorkspaceId,
    project_folder: String,
    git_common_dir: String,
    branch: String,
}

/// A project-bound root validated on a background executor before planning is enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectWorktreeRootProof {
    project_folder: String,
    root: String,
    git_common_dir: String,
}

impl ProjectWorktreeRootProof {
    pub fn project_folder(&self) -> &str {
        &self.project_folder
    }
    pub fn root(&self) -> &str {
        &self.root
    }
}

/// Blocking Git/filesystem validation: call only from an existing background worker.
/// Does not create task worktrees, reserve tasks, launch agents, or remove anything.
pub(crate) fn prepare_project_worktree_root(
    project_folder: &str,
) -> Result<ProjectWorktreeRootProof, TaskWorktreeError> {
    validate_folder_path(project_folder)?;
    let project = canonical_folder(project_folder)?;
    let git_root = crate::git_cli::repo_root(&project, GIT_TIMEOUT)
        .map_err(|_| TaskWorktreeError::InvalidFolder)?;
    // Subdirectory projects need an explicit scope mapping; never silently widen their root.
    if git_root.to_str() != Some(project_folder) {
        return Err(TaskWorktreeError::InvalidFolder);
    }
    let common = common_dir(&project)?;
    let deppy = checked_directory(&project.join(".deppy"))?;
    let root = checked_directory(&deppy.join("worktrees"))?;
    Ok(ProjectWorktreeRootProof {
        project_folder: project_folder.into(),
        root: root
            .to_str()
            .ok_or(TaskWorktreeError::InvalidFolder)?
            .into(),
        git_common_dir: common
            .to_str()
            .ok_or(TaskWorktreeError::InvalidFolder)?
            .into(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskWorktreeError {
    InvalidJob,
    InvalidFolder,
    OwnershipConflict,
    CreationUncertain,
    Unavailable,
}

impl TaskWorktreeError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJob => "task_worktree_invalid_job",
            Self::InvalidFolder => "task_worktree_invalid_folder",
            Self::OwnershipConflict => "task_worktree_ownership_conflict",
            Self::CreationUncertain => "task_worktree_creation_uncertain",
            Self::Unavailable => "task_worktree_unavailable",
        }
    }
}

pub(crate) struct TaskWorktreeResult {
    /// Echoes every field, including reserved attempt/operation and configuration revision.
    pub job: EnsureTaskWorktree,
    pub result: Result<OwnedTaskWorktree, TaskWorktreeError>,
}

#[derive(Debug)]
pub(crate) enum TaskWorktreeSubmitError {
    Invalid {
        job: Box<EnsureTaskWorktree>,
        error: TaskWorktreeError,
    },
    Full(Box<EnsureTaskWorktree>),
    Unavailable(Box<EnsureTaskWorktree>),
}

impl TaskWorktreeSubmitError {
    pub fn into_job(self) -> EnsureTaskWorktree {
        match self {
            Self::Invalid { job, .. } | Self::Full(job) | Self::Unavailable(job) => *job,
        }
    }
}

pub(crate) struct TaskWorktreeWorker {
    worker: LazyBoundedWorker<EnsureTaskWorktree, TaskWorktreeResult>,
    pending: Option<EnsureTaskWorktree>,
}

impl TaskWorktreeWorker {
    /// Inert: no filesystem access, Git invocation, worker thread or wake until admission.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            worker: LazyBoundedWorker::new(
                "command-center-worktree",
                Duration::from_secs(20),
                || {
                    |job: EnsureTaskWorktree| TaskWorktreeResult {
                        result: ensure(&job),
                        job,
                    }
                },
                wake,
            ),
            pending: None,
        }
    }

    pub fn try_request(&mut self, job: EnsureTaskWorktree) -> Result<(), TaskWorktreeSubmitError> {
        if let Err(error) = validate_job(&job) {
            return Err(TaskWorktreeSubmitError::Invalid {
                job: Box::new(job),
                error,
            });
        }
        // Keep a separate immutable correlation copy, including when executor infrastructure fails.
        let pending = job.clone();
        self.worker.try_request(job).map_err(|error| match error {
            LazyWorkerSubmitError::Full(job) => TaskWorktreeSubmitError::Full(Box::new(job)),
            LazyWorkerSubmitError::Unavailable { job, .. } => {
                TaskWorktreeSubmitError::Unavailable(Box::new(job))
            }
        })?;
        self.pending = Some(pending);
        Ok(())
    }

    pub fn try_recv(&mut self) -> Option<TaskWorktreeResult> {
        let outcome = self.worker.try_recv()?;
        let job = self.pending.take()?;
        Some(outcome.into_result().unwrap_or(TaskWorktreeResult {
            job,
            result: Err(TaskWorktreeError::CreationUncertain),
        }))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerRecord {
    version: u8,
    reserved: EnsureTaskWorktree,
    owned: Option<StoredOwner>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredOwner {
    path: String,
    root: String,
    git_common_dir: String,
    branch: String,
}

fn ensure(job: &EnsureTaskWorktree) -> Result<OwnedTaskWorktree, TaskWorktreeError> {
    validate_job(job)?;
    let proof = prepare_project_worktree_root(&job.project_folder)?;
    let project = PathBuf::from(&proof.project_folder);
    let common = PathBuf::from(&proof.git_common_dir);
    let root = PathBuf::from(&proof.root);
    let owners = checked_directory(&project.join(".deppy/task-worktree-owners"))?;
    let record_path = owners.join(record_name(job));
    let (mut file, created) = open_record(&record_path)?;
    file.try_lock()
        .map_err(|_| TaskWorktreeError::OwnershipConflict)?;
    if !created {
        let record = read_record(&mut file)?;
        if record.version != 1
            || record.reserved.workspace_id != job.workspace_id
            || record.reserved.task_id != job.task_id
            || record.reserved.project_folder != job.project_folder
        {
            return Err(TaskWorktreeError::OwnershipConflict);
        }
        let owned = record.owned.ok_or(TaskWorktreeError::CreationUncertain)?;
        return verify_owned(job, &root, &common, owned);
    }

    let mut record = OwnerRecord {
        version: 1,
        reserved: job.clone(),
        owned: None,
    };
    write_record(&mut file, &record)?;
    // The name itself must survive a crash before Git can create a worktree.
    File::open(&owners)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| TaskWorktreeError::CreationUncertain)?;
    // The existing helper performs bounded Git and preserves repository excludes. No cleanup or
    // second creation is attempted after this point, including timeout or a result write failure.
    let path = crate::worktree::create_worktree(&project)
        .map_err(|_| TaskWorktreeError::CreationUncertain)?;
    let path = path
        .to_str()
        .ok_or(TaskWorktreeError::CreationUncertain)?
        .to_owned();
    let branch = git_scalar(Path::new(&path), &["symbolic-ref", "--quiet", "HEAD"])
        .map_err(|_| TaskWorktreeError::CreationUncertain)?;
    let owned = StoredOwner {
        path,
        root: root
            .to_str()
            .ok_or(TaskWorktreeError::InvalidFolder)?
            .into(),
        git_common_dir: common
            .to_str()
            .ok_or(TaskWorktreeError::InvalidFolder)?
            .into(),
        branch,
    };
    let verified = verify_owned(
        job,
        &root,
        &common,
        StoredOwner {
            path: owned.path.clone(),
            root: owned.root.clone(),
            git_common_dir: owned.git_common_dir.clone(),
            branch: owned.branch.clone(),
        },
    )
    .map_err(|_| TaskWorktreeError::CreationUncertain)?;
    record.owned = Some(owned);
    write_record(&mut file, &record)?;
    Ok(verified)
}

fn validate_job(job: &EnsureTaskWorktree) -> Result<(), TaskWorktreeError> {
    for id in [
        job.workspace_id.0.as_str(),
        job.task_id.as_str(),
        job.attempt_id.as_str(),
        job.operation_id.as_str(),
    ] {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
            || id == "."
            || id == ".."
        {
            return Err(TaskWorktreeError::InvalidJob);
        }
    }
    if job.config_revision.get() == 0 {
        return Err(TaskWorktreeError::InvalidJob);
    }
    validate_folder_path(&job.project_folder)
}

fn validate_folder_path(project_folder: &str) -> Result<(), TaskWorktreeError> {
    if project_folder.is_empty()
        || project_folder.len() > 4096
        || project_folder.chars().any(char::is_control)
        || !Path::new(project_folder).is_absolute()
        || Path::new(project_folder)
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(TaskWorktreeError::InvalidFolder);
    }
    Ok(())
}

fn canonical_folder(path: &str) -> Result<PathBuf, TaskWorktreeError> {
    let canonical = std::fs::canonicalize(path).map_err(|_| TaskWorktreeError::InvalidFolder)?;
    if canonical.to_str() != Some(path) || !canonical.is_dir() {
        return Err(TaskWorktreeError::InvalidFolder);
    }
    Ok(canonical)
}

fn checked_directory(path: &Path) -> Result<PathBuf, TaskWorktreeError> {
    let created = match std::fs::create_dir(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(_) => return Err(TaskWorktreeError::Unavailable),
    };
    let metadata = std::fs::symlink_metadata(path).map_err(|_| TaskWorktreeError::InvalidFolder)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(TaskWorktreeError::InvalidFolder);
    }
    let canonical = canonical_folder(path.to_str().ok_or(TaskWorktreeError::InvalidFolder)?)?;
    if created {
        File::open(path.parent().ok_or(TaskWorktreeError::InvalidFolder)?)
            .and_then(|parent| parent.sync_all())
            .map_err(|_| TaskWorktreeError::Unavailable)?;
    }
    Ok(canonical)
}

fn common_dir(path: &Path) -> Result<PathBuf, TaskWorktreeError> {
    let answered = git_scalar(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    canonical_folder(&answered).map_err(|_| TaskWorktreeError::OwnershipConflict)
}

fn git_scalar(path: &Path, args: &[&str]) -> Result<String, TaskWorktreeError> {
    let output = crate::git_cli::run_git_bounded(path, args, GIT_TIMEOUT, 16 * 1024)
        .map_err(|_| TaskWorktreeError::OwnershipConflict)?;
    let value = output.trim_end_matches('\n');
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(TaskWorktreeError::OwnershipConflict);
    }
    Ok(value.into())
}

fn record_name(job: &EnsureTaskWorktree) -> String {
    // These IDs are ASCII/control-free before this function is called, so the existing digest
    // helper's CRLF normalization cannot alias two identities. Lengths make the tuple unambiguous.
    let identity = format!(
        "cc-task-worktree-owner-v1:{}:{}:{}:{}",
        job.workspace_id.0.len(),
        job.workspace_id.0,
        job.task_id.as_str().len(),
        job.task_id.as_str()
    );
    format!("{}.json", storage::native_prompt_digest(&identity))
}

fn open_record(path: &Path) -> Result<(File, bool), TaskWorktreeError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let (file, created) = match options.create_new(true).open(path) {
        Ok(file) => (file, true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (
            options
                .create_new(false)
                .open(path)
                .map_err(|_| TaskWorktreeError::OwnershipConflict)?,
            false,
        ),
        Err(_) => return Err(TaskWorktreeError::Unavailable),
    };
    let metadata = file
        .metadata()
        .map_err(|_| TaskWorktreeError::OwnershipConflict)?;
    if !metadata.is_file() || metadata.len() > RECORD_MAX_BYTES {
        return Err(TaskWorktreeError::OwnershipConflict);
    }
    Ok((file, created))
}

fn read_record(file: &mut File) -> Result<OwnerRecord, TaskWorktreeError> {
    let mut bytes = Vec::new();
    file.take(RECORD_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| TaskWorktreeError::OwnershipConflict)?;
    if bytes.len() as u64 > RECORD_MAX_BYTES {
        return Err(TaskWorktreeError::OwnershipConflict);
    }
    serde_json::from_slice(&bytes).map_err(|_| TaskWorktreeError::OwnershipConflict)
}

fn write_record(file: &mut File, record: &OwnerRecord) -> Result<(), TaskWorktreeError> {
    let bytes = serde_json::to_vec(record).map_err(|_| TaskWorktreeError::CreationUncertain)?;
    if bytes.len() as u64 > RECORD_MAX_BYTES {
        return Err(TaskWorktreeError::CreationUncertain);
    }
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.set_len(0))
        .and_then(|_| file.write_all(&bytes))
        .and_then(|_| file.sync_all())
        .map_err(|_| TaskWorktreeError::CreationUncertain)
}

fn verify_owned(
    job: &EnsureTaskWorktree,
    root: &Path,
    common: &Path,
    owned: StoredOwner,
) -> Result<OwnedTaskWorktree, TaskWorktreeError> {
    let conflict = TaskWorktreeError::OwnershipConflict;
    let path = canonical_folder(&owned.path).map_err(|_| conflict)?;
    if path.parent() != Some(root)
        || Path::new(&owned.root) != root
        || Path::new(&owned.git_common_dir) != common
        || !owned.branch.starts_with("refs/heads/deppy/")
        || std::fs::symlink_metadata(path.join(".git"))
            .map_err(|_| conflict)?
            .file_type()
            .is_symlink()
        || !path.join(".git").is_file()
        || crate::git_cli::repo_root(&path, GIT_TIMEOUT).map_err(|_| conflict)? != path
        || common_dir(&path)? != common
        || git_scalar(&path, &["symbolic-ref", "--quiet", "HEAD"])? != owned.branch
    {
        return Err(conflict);
    }
    Ok(OwnedTaskWorktree {
        path: owned.path,
        root: owned.root,
        owner: job.task_id.clone(),
        proof: TaskWorktreeProof {
            workspace_id: job.workspace_id.clone(),
            project_folder: job.project_folder.clone(),
            git_common_dir: owned.git_common_dir,
            branch: owned.branch,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Instant;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let parent = std::fs::canonicalize(std::env::temp_dir()).unwrap();
            let path = parent.join(format!("deppy-task-worktree-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            git(&path, &["init", "--quiet"]);
            git(
                &path,
                &[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@localhost",
                    "commit",
                    "--allow-empty",
                    "-m",
                    "fixture",
                ],
            );
            Self(path)
        }
        fn job(&self) -> EnsureTaskWorktree {
            EnsureTaskWorktree {
                workspace_id: WorkspaceId("fixture-workspace".into()),
                task_id: TaskId::new(),
                attempt_id: AttemptId::new(),
                operation_id: OperationId::new(),
                project_folder: self.0.to_str().unwrap().into(),
                config_revision: ConfigRevision(1),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn git(path: &Path, args: &[&str]) -> String {
        crate::git_cli::run_git_bounded(path, args, Duration::from_secs(10), 64 * 1024).unwrap()
    }
    fn receive(worker: &mut TaskWorktreeWorker) -> TaskWorktreeResult {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if let Some(result) = worker.try_recv() {
                return result;
            }
            assert!(Instant::now() < deadline, "worktree result deadline");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn lazy_bounded_worker_correlates_exact_job_and_creates_a_real_worktree() {
        let fixture = Fixture::new();
        let job = fixture.job();
        let wakes = Arc::new(AtomicUsize::new(0));
        let signal = wakes.clone();
        let mut worker = TaskWorktreeWorker::new(move || {
            signal.fetch_add(1, Ordering::SeqCst);
        });
        assert!(!fixture.0.join(".deppy").exists());
        assert!(!worker.worker.has_live_worker());
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
        worker.try_request(job.clone()).unwrap();
        let mut other = job.clone();
        other.attempt_id = AttemptId::new();
        let rejected = worker.try_request(other.clone()).unwrap_err();
        assert!(matches!(&rejected, TaskWorktreeSubmitError::Full(_)));
        assert_eq!(rejected.into_job(), other);
        let result = receive(&mut worker);
        assert_eq!(result.job, job);
        let owned = result.result.unwrap();
        assert_eq!(owned.owner, job.task_id);
        assert_eq!(
            Path::new(&owned.path).parent(),
            Some(Path::new(&owned.root))
        );
        assert_eq!(
            git(Path::new(&owned.path), &["rev-parse", "--show-toplevel"]).trim(),
            owned.path
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while wakes.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "completion wake deadline");
            std::thread::yield_now();
        }
        drop(worker);
        // Unknown/abandoned runtime launch is not a request to clean up this worktree.
        assert!(Path::new(&owned.path).join(".git").is_file());
    }

    #[test]
    fn same_task_reuses_verified_owner_across_attempts_and_config_revisions() {
        let fixture = Fixture::new();
        let mut job = fixture.job();
        let first = ensure(&job).unwrap();
        job.attempt_id = AttemptId::new();
        job.operation_id = OperationId::new();
        job.config_revision = ConfigRevision(2);
        assert_eq!(ensure(&job).unwrap(), first);
        // A checkout changed outside this worker invalidates the original branch ownership proof.
        git(Path::new(&first.path), &["checkout", "--detach"]);
        assert_eq!(ensure(&job), Err(TaskWorktreeError::OwnershipConflict));
        assert!(Path::new(&first.path).exists());
    }

    #[test]
    fn existing_wrong_path_pending_reservation_and_symlink_alias_are_never_adopted() {
        let fixture = Fixture::new();
        let job = fixture.job();
        let first = ensure(&job).unwrap();
        // Replacing a formerly owned worktree with a plain directory must fail, without deletion.
        let relocated = fixture.0.join("preserved-original");
        std::fs::rename(&first.path, &relocated).unwrap();
        std::fs::create_dir(&first.path).unwrap();
        std::fs::write(Path::new(&first.path).join("user-file"), "keep").unwrap();
        assert_eq!(ensure(&job), Err(TaskWorktreeError::OwnershipConflict));
        assert_eq!(
            std::fs::read(Path::new(&first.path).join("user-file")).unwrap(),
            b"keep"
        );
        #[cfg(unix)]
        {
            let alias = fixture.0.join("alias");
            std::os::unix::fs::symlink(&fixture.0, &alias).unwrap();
            let mut aliased = job.clone();
            aliased.project_folder = alias.to_str().unwrap().into();
            assert_eq!(ensure(&aliased), Err(TaskWorktreeError::InvalidFolder));
        }
    }

    #[test]
    fn durable_pending_creation_is_not_replayed_and_other_workspace_cannot_reuse_owner() {
        let fixture = Fixture::new();
        let job = fixture.job();
        let first = ensure(&job).unwrap();
        let mut second_workspace = job.clone();
        second_workspace.workspace_id = WorkspaceId("different-workspace".into());
        let second = ensure(&second_workspace).unwrap();
        assert_ne!(first.path, second.path);
        let mut pending = job.clone();
        pending.task_id = TaskId::new();
        let owners = fixture.0.join(".deppy/task-worktree-owners");
        let (mut file, created) = open_record(&owners.join(record_name(&pending))).unwrap();
        assert!(created);
        write_record(
            &mut file,
            &OwnerRecord {
                version: 1,
                reserved: pending.clone(),
                owned: None,
            },
        )
        .unwrap();
        drop(file);
        let before = git(&fixture.0, &["worktree", "list", "--porcelain"]);
        assert_eq!(ensure(&pending), Err(TaskWorktreeError::CreationUncertain));
        assert_eq!(
            git(&fixture.0, &["worktree", "list", "--porcelain"]),
            before
        );
        assert!(Path::new(&first.path).exists());
        assert!(Path::new(&second.path).exists());
    }

    #[test]
    fn invalid_job_limits_and_traversal_are_rejected_before_creation() {
        let fixture = Fixture::new();
        let job = fixture.job();
        for value in ["", "../escape", "slash/task", "control\n"] {
            let mut invalid = job.clone();
            invalid.task_id = TaskId(value.into());
            assert_eq!(ensure(&invalid), Err(TaskWorktreeError::InvalidJob));
        }
        let mut invalid = job.clone();
        invalid.project_folder.push_str("/../escape");
        assert_eq!(ensure(&invalid), Err(TaskWorktreeError::InvalidFolder));
        invalid = job;
        invalid.config_revision = ConfigRevision(0);
        assert_eq!(ensure(&invalid), Err(TaskWorktreeError::InvalidJob));
        assert_eq!(
            TaskWorktreeError::InvalidJob.as_str(),
            "task_worktree_invalid_job"
        );
        let mut worker = TaskWorktreeWorker::new(|| {});
        assert!(matches!(
            worker.try_request(invalid),
            Err(TaskWorktreeSubmitError::Invalid {
                error: TaskWorktreeError::InvalidJob,
                ..
            })
        ));
        assert!(!worker.worker.has_live_worker());
        assert!(!fixture.0.join(".deppy").exists());
    }

    #[test]
    fn project_root_proof_is_bound_before_planning_without_creating_a_task() {
        let fixture = Fixture::new();
        let folder = fixture.0.to_str().unwrap();
        let before = git(&fixture.0, &["worktree", "list", "--porcelain"]);
        let proof = prepare_project_worktree_root(folder).unwrap();
        assert_eq!(proof.project_folder(), folder);
        assert_eq!(Path::new(proof.root()), fixture.0.join(".deppy/worktrees"));
        assert!(Path::new(proof.root()).is_dir());
        assert_eq!(prepare_project_worktree_root(folder).unwrap(), proof);
        assert_eq!(
            git(&fixture.0, &["worktree", "list", "--porcelain"]),
            before
        );
        assert!(!fixture.0.join(".deppy/task-worktree-owners").exists());
    }

    #[test]
    fn project_root_proof_rejects_subdirectory_traversal_and_symlink_roots() {
        let fixture = Fixture::new();
        let subdirectory = fixture.0.join("source");
        std::fs::create_dir(&subdirectory).unwrap();
        assert_eq!(
            prepare_project_worktree_root(subdirectory.to_str().unwrap()),
            Err(TaskWorktreeError::InvalidFolder)
        );
        assert_eq!(
            prepare_project_worktree_root(&format!("{}/../escape", fixture.0.display())),
            Err(TaskWorktreeError::InvalidFolder)
        );
        assert!(!fixture.0.join(".deppy").exists());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&subdirectory, fixture.0.join(".deppy")).unwrap();
            assert_eq!(
                prepare_project_worktree_root(fixture.0.to_str().unwrap()),
                Err(TaskWorktreeError::InvalidFolder)
            );
            assert!(!subdirectory.join("worktrees").exists());
        }
    }
}
