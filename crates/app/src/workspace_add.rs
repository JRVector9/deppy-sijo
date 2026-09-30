//! Workspace Add dialog and local GitHub clone operation.

use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

const CLONE_TIMEOUT: Duration = Duration::from_secs(300);
const GIT_OUTPUT_MAX_BYTES: usize = 4 * 1024;
static CLONE_STAGING_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Debug, Eq, PartialEq)]
struct RepoIdentity {
    owner: String,
    repo: String,
}

fn valid_repo_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn github_remote(raw: &str) -> Result<(RepoIdentity, String), CloneError> {
    let raw = raw.trim();
    if raw.len() > 2048 || raw.is_empty() {
        return Err(CloneError::InvalidUrl);
    }
    let (path, remote_url) = if let Some(path) = raw.strip_prefix("git@github.com:") {
        (path.to_owned(), raw.to_owned())
    } else {
        let parsed = url::Url::parse(raw).map_err(|_| CloneError::InvalidUrl)?;
        if !matches!(parsed.scheme(), "https" | "ssh")
            || parsed.host_str() != Some("github.com")
            || parsed.port().is_some()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || (parsed.scheme() == "https" && !parsed.username().is_empty())
            || (parsed.scheme() == "ssh" && parsed.username() != "git")
        {
            return Err(CloneError::InvalidUrl);
        }
        (parsed.path().to_owned(), raw.to_owned())
    };
    let segments: Vec<_> = path.trim_matches('/').split('/').collect();
    if segments.len() != 2 || !valid_repo_segment(segments[0]) {
        return Err(CloneError::InvalidUrl);
    }
    let repo = segments[1].strip_suffix(".git").unwrap_or(segments[1]);
    if !valid_repo_segment(repo) || path.contains(['?', '#', '\\']) {
        return Err(CloneError::InvalidUrl);
    }
    Ok((
        RepoIdentity {
            owner: segments[0].to_ascii_lowercase(),
            repo: repo.to_ascii_lowercase(),
        },
        remote_url,
    ))
}

pub fn suggested_folder(url: &str) -> Option<String> {
    github_remote(url).ok().map(|(identity, _)| identity.repo)
}

#[derive(Clone, Debug)]
pub struct CloneRequest {
    remote_url: String,
    identity: RepoIdentity,
    parent: PathBuf,
    directory_name: String,
}

impl CloneRequest {
    pub fn prepare(url: &str, parent: &Path, directory_name: &str) -> Result<Self, CloneError> {
        let (identity, remote_url) = github_remote(url)?;
        let directory_name = directory_name.trim();
        if directory_name.is_empty()
            || directory_name.len() > 255
            || directory_name.contains('\0')
            || directory_name.contains('/')
            || directory_name.contains('\\')
            || directory_name == "."
            || directory_name == ".."
            || !matches!(
                Path::new(directory_name).components().next(),
                Some(Component::Normal(_))
            )
        {
            return Err(CloneError::InvalidName);
        }
        if !parent.is_absolute() {
            return Err(CloneError::ParentMissing);
        }
        if parent.to_str().is_none() {
            return Err(CloneError::InvalidPathEncoding);
        }
        Ok(Self {
            remote_url,
            identity,
            parent: parent.to_owned(),
            directory_name: directory_name.to_owned(),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_test_remote(mut self, path: &Path) -> Self {
        self.remote_url = path.to_string_lossy().into_owned();
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CloneError {
    InvalidUrl,
    InvalidName,
    ParentMissing,
    InvalidPathEncoding,
    DestinationExists,
    Filesystem,
    GitFailed,
    TimedOut,
    Cancelled,
    CancelledAfterCompletion,
    WorkerUnavailable,
    RegistrationFailed,
    SwitchFailed,
}

fn classify_destination_io_error(error: &std::io::Error) -> CloneError {
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        CloneError::DestinationExists
    } else {
        CloneError::Filesystem
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloneOutcome {
    pub path: PathBuf,
    pub reused: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CloneStagingJournal {
    staging: String,
    owner: String,
}

struct StagingDir {
    staging: PathBuf,
    journal: Option<PathBuf>,
}

fn recover_staging_journal(path: &Path) -> std::io::Result<bool> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > 4096 {
        return Ok(false);
    }
    let entry: CloneStagingJournal = serde_json::from_slice(&std::fs::read(path)?)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let id = uuid::Uuid::parse_str(&entry.owner)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if path.file_name().and_then(|name| name.to_str())
        != Some(format!("clone-staging-{id}.json").as_str())
    {
        return Ok(false);
    }
    let staging = Path::new(&entry.staging);
    if !staging.is_absolute()
        || staging.file_name().and_then(|name| name.to_str())
            != Some(format!(".deppy-clone-{id}").as_str())
    {
        return Ok(false);
    }
    let Some(parent) = staging.parent() else {
        return Ok(false);
    };
    if std::fs::canonicalize(parent).ok().as_deref() != Some(parent) {
        return Ok(false);
    }
    let marker = parent.join(format!(".deppy-clone-{id}.owner"));
    let marker_metadata = match std::fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !marker_metadata.file_type().is_file()
        || marker_metadata.len() > 64
        || std::fs::read_to_string(&marker)? != entry.owner
    {
        return Ok(false);
    }
    match std::fs::symlink_metadata(staging) {
        Ok(metadata) if metadata.file_type().is_dir() => std::fs::remove_dir_all(staging)?,
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::remove_file(marker)?;
    std::fs::remove_file(path)?;
    Ok(true)
}

pub fn recover_abandoned_clone_staging(data_dir: &Path) -> std::io::Result<usize> {
    let _guard = CLONE_STAGING_LOCK
        .lock()
        .map_err(|_| std::io::Error::other("clone staging lock poisoned"))?;
    let mut recovered = 0;
    let mut inspected = 0;
    for item in std::fs::read_dir(data_dir)? {
        let path = item?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with("clone-staging-") && name.ends_with(".json") {
            inspected += 1;
            if inspected > 256 {
                break;
            }
            match recover_staging_journal(&path) {
                Ok(true) => recovered += 1,
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(path = %path.display(), error = %error, "clone staging recovery failed")
                }
            }
        }
    }
    Ok(recovered)
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if let Some(journal) = &self.journal {
            if let Err(error) = recover_staging_journal(journal) {
                tracing::warn!(path = %journal.display(), error = %error, "clone staging cleanup failed");
            }
        } else if let Err(error) = std::fs::remove_dir_all(&self.staging)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.staging.display(), error = %error, "clone staging cleanup failed");
        }
    }
}

/// Publish a complete clone only if the requested name is still unused.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn rename_staging_no_replace(staging: &Path, target: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let from = CString::new(staging.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let to = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: both C strings are NUL terminated and remain alive through the syscall.
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    // SAFETY: both C strings are NUL terminated and remain alive through the syscall.
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_staging_no_replace(staging: &Path, target: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(target).is_ok() {
        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    }
    std::fs::rename(staging, target)
}

#[cfg(test)]
pub fn clone_or_reuse(
    request: &CloneRequest,
    cancelled: &AtomicBool,
) -> Result<CloneOutcome, CloneError> {
    clone_or_reuse_with_journal(request, cancelled, None)
}

fn clone_or_reuse_with_journal(
    request: &CloneRequest,
    cancelled: &AtomicBool,
    data_dir: Option<&Path>,
) -> Result<CloneOutcome, CloneError> {
    let _guard = CLONE_STAGING_LOCK
        .lock()
        .map_err(|_| CloneError::Filesystem)?;
    if cancelled.load(Ordering::Acquire) {
        return Err(CloneError::Cancelled);
    }
    let parent = std::fs::canonicalize(&request.parent).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CloneError::ParentMissing
        } else {
            CloneError::Filesystem
        }
    })?;
    match std::fs::metadata(&parent) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(CloneError::ParentMissing),
        Err(_) => return Err(CloneError::Filesystem),
    }
    let target = parent.join(&request.directory_name);
    if match std::fs::symlink_metadata(&target) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => return Err(CloneError::Filesystem),
    } {
        if target.is_dir() && !target.is_symlink() && target.join(".git").exists() {
            let existing = crate::git_cli::run_git_bounded(
                &target,
                &["remote", "get-url", "origin"],
                Duration::from_secs(10),
                GIT_OUTPUT_MAX_BYTES,
            )
            .ok();
            if existing
                .as_deref()
                .and_then(|url| github_remote(url).ok())
                .is_some_and(|(identity, _)| identity == request.identity)
            {
                return Ok(CloneOutcome {
                    path: target,
                    reused: true,
                });
            }
        }
        return Err(CloneError::DestinationExists);
    }
    let owner = uuid::Uuid::new_v4().to_string();
    let staging = parent.join(format!(".deppy-clone-{owner}"));
    let journal = if let Some(data_dir) = data_dir {
        let entry = CloneStagingJournal {
            staging: staging
                .to_str()
                .ok_or(CloneError::InvalidPathEncoding)?
                .to_owned(),
            owner: owner.clone(),
        };
        let bytes = serde_json::to_vec(&entry).map_err(|_| CloneError::Filesystem)?;
        let marker = parent.join(format!(".deppy-clone-{owner}.owner"));
        let mut marker_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .map_err(|_| CloneError::Filesystem)?;
        if marker_file.write_all(owner.as_bytes()).is_err() {
            drop(marker_file);
            let _ = std::fs::remove_file(marker);
            return Err(CloneError::Filesystem);
        }
        let journal = data_dir.join(format!("clone-staging-{owner}.json"));
        if deppy_core::fs::atomic_write(&journal, &bytes).is_err() {
            let _ = std::fs::remove_file(marker);
            return Err(CloneError::Filesystem);
        }
        Some(journal)
    } else {
        None
    };
    let _cleanup = StagingDir {
        staging: staging.clone(),
        journal,
    };
    std::fs::create_dir(&staging).map_err(|error| classify_destination_io_error(&error))?;
    let stage_name = staging
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(CloneError::GitFailed)?;
    let result = crate::git_cli::run_git_bounded_cancellable(
        &parent,
        &["clone", "--", &request.remote_url, stage_name],
        CLONE_TIMEOUT,
        GIT_OUTPUT_MAX_BYTES,
        cancelled,
    );
    if let Err(error) = result {
        return Err(match error.to_string().as_str() {
            "git_cancelled" => CloneError::Cancelled,
            "git_timeout" => CloneError::TimedOut,
            _ => CloneError::GitFailed,
        });
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(CloneError::Cancelled);
    }
    if match std::fs::symlink_metadata(&target) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => return Err(CloneError::Filesystem),
    } {
        return Err(CloneError::DestinationExists);
    }
    rename_staging_no_replace(&staging, &target)
        .map_err(|error| classify_destination_io_error(&error))?;
    Ok(CloneOutcome {
        path: target,
        reused: false,
    })
}

pub struct CloneTask {
    result_rx: mpsc::Receiver<Result<CloneOutcome, CloneError>>,
    handle: Option<thread::JoinHandle<()>>,
    cancelled: Arc<AtomicBool>,
}

impl CloneTask {
    pub fn spawn(
        request: CloneRequest,
        ctx: egui::Context,
        data_dir: PathBuf,
    ) -> Result<Self, CloneError> {
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_for_worker = Arc::clone(&cancelled);
        let handle = thread::Builder::new()
            .name("workspace-github-clone".to_owned())
            .spawn(move || {
                let result =
                    clone_or_reuse_with_journal(&request, &cancel_for_worker, Some(&data_dir));
                let _ = result_tx.send(result);
                ctx.request_repaint();
            })
            .map_err(|_| CloneError::WorkerUnavailable)?;
        Ok(Self {
            result_rx,
            handle: Some(handle),
            cancelled,
        })
    }

    pub fn poll(&mut self) -> Option<Result<CloneOutcome, CloneError>> {
        match self.result_rx.try_recv() {
            Ok(result) => {
                if let Some(handle) = self.handle.take() {
                    let _ = handle.join();
                }
                Some(match result {
                    Ok(_) if self.cancelled.load(Ordering::Acquire) => {
                        Err(CloneError::CancelledAfterCompletion)
                    }
                    other => other,
                })
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                if let Some(handle) = self.handle.take() {
                    let _ = handle.join();
                }
                Some(Err(CloneError::WorkerUnavailable))
            }
            Err(mpsc::TryRecvError::Empty) => None,
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl Drop for CloneTask {
    fn drop(&mut self) {
        self.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceAddPurpose {
    SwitchRuntime,
    SelectInSettings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkspaceSource {
    Local,
    GitHub,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceAddStatus {
    Ready,
    Picking,
    Cloning,
    Cancelling,
    Registering,
}

pub enum WorkspaceAddIntent {
    PickLocal,
    PickDestination,
    Clone(CloneRequest),
    CancelClone,
    Close,
}

pub struct WorkspaceAddUi {
    purpose: WorkspaceAddPurpose,
    source: WorkspaceSource,
    remote_url: String,
    destination_parent: String,
    selected_parent: Option<PathBuf>,
    directory_name: String,
    name_edited: bool,
    status: WorkspaceAddStatus,
    error: Option<CloneError>,
}

impl WorkspaceAddUi {
    pub fn new(purpose: WorkspaceAddPurpose, destination_parent: PathBuf) -> Self {
        Self {
            purpose,
            source: WorkspaceSource::Local,
            remote_url: String::new(),
            destination_parent: destination_parent.to_string_lossy().into_owned(),
            selected_parent: Some(destination_parent),
            directory_name: String::new(),
            name_edited: false,
            status: WorkspaceAddStatus::Ready,
            error: None,
        }
    }

    pub fn purpose(&self) -> WorkspaceAddPurpose {
        self.purpose
    }

    pub fn set_destination(&mut self, path: &Path) {
        self.destination_parent = path.to_string_lossy().into_owned();
        self.selected_parent = Some(path.to_owned());
        self.error = None;
    }

    pub fn set_cloning(&mut self) {
        self.status = WorkspaceAddStatus::Cloning;
        self.error = None;
    }

    pub fn set_picking(&mut self) {
        self.status = WorkspaceAddStatus::Picking;
    }

    pub fn finish_picking(&mut self) {
        self.status = WorkspaceAddStatus::Ready;
    }

    pub fn set_cancelling(&mut self) {
        self.status = WorkspaceAddStatus::Cancelling;
    }

    pub fn set_registering(&mut self) {
        self.status = WorkspaceAddStatus::Registering;
    }

    pub fn set_error(&mut self, error: CloneError) {
        self.status = WorkspaceAddStatus::Ready;
        self.error = Some(error);
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        catalog: &i18n::Catalog,
    ) -> Option<WorkspaceAddIntent> {
        let mut action = None;
        let busy = self.status != WorkspaceAddStatus::Ready;
        let title = catalog.t("workspace.add.title", &[]);
        let intro = catalog.t("workspace.add.intro", &[]);
        let close_label = catalog.t("action.close", &[]);
        let modal = crate::ui::popup::show(
            ctx,
            crate::ui::popup::PopupSpec {
                id: egui::Id::new("workspace_add_source"),
                width: 560.0,
                title: &title,
                subtitle: &intro,
                close_label: &close_label,
                close_enabled: !busy,
            },
            |ui| {
                crate::ui::popup::body(ui, |ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        crate::ui::popup::segmented_choice(
                            ui,
                            &mut self.source,
                            [
                                (
                                    WorkspaceSource::Local,
                                    &catalog.t("workspace.add.local", &[]),
                                ),
                                (
                                    WorkspaceSource::GitHub,
                                    &catalog.t("workspace.add.github", &[]),
                                ),
                            ],
                        );
                        ui.add_space(14.0);
                        match self.source {
                            WorkspaceSource::Local => {
                                crate::ui::popup::notice(
                                    ui,
                                    &catalog.t("workspace.add.local_hint", &[]),
                                    crate::ui::popup::NoticeTone::Info,
                                );
                            }
                            WorkspaceSource::GitHub => {
                                crate::ui::popup::field(
                                    ui,
                                    &catalog.t("workspace.add.url", &[]),
                                    None,
                                    |ui| {
                                        let url = ui.add(
                                            egui::TextEdit::singleline(&mut self.remote_url)
                                                .hint_text("https://github.com/owner/repo")
                                                .desired_width(ui.available_width()),
                                        );
                                        if url.changed() {
                                            if !self.name_edited {
                                                self.directory_name =
                                                    suggested_folder(&self.remote_url)
                                                        .unwrap_or_default();
                                            }
                                            self.error = None;
                                        }
                                    },
                                );
                                crate::ui::popup::field(
                                    ui,
                                    &catalog.t("workspace.add.destination", &[]),
                                    None,
                                    |ui| {
                                        ui.horizontal(|ui| {
                                            let path_width =
                                                (ui.available_width() - 105.0).max(120.0);
                                            if ui
                                                .add(
                                                    egui::TextEdit::singleline(
                                                        &mut self.destination_parent,
                                                    )
                                                    .desired_width(path_width),
                                                )
                                                .changed()
                                            {
                                                self.selected_parent = None;
                                                self.error = None;
                                            }
                                            if ui
                                                .button(catalog.t("workspace.add.browse", &[]))
                                                .clicked()
                                            {
                                                action = Some(WorkspaceAddIntent::PickDestination);
                                            }
                                        });
                                    },
                                );
                                crate::ui::popup::field(
                                    ui,
                                    &catalog.t("workspace.add.folder_name", &[]),
                                    None,
                                    |ui| {
                                        let name = ui.add(
                                            egui::TextEdit::singleline(&mut self.directory_name)
                                                .desired_width(ui.available_width()),
                                        );
                                        if name.changed() {
                                            self.name_edited = true;
                                            self.error = None;
                                        }
                                    },
                                );
                                crate::ui::popup::notice(
                                    ui,
                                    &catalog.t("workspace.add.github_hint", &[]),
                                    crate::ui::popup::NoticeTone::Info,
                                );
                            }
                        }
                    });
                    let status_key = match self.status {
                        WorkspaceAddStatus::Ready => None,
                        WorkspaceAddStatus::Picking => Some("workspace.add.choosing"),
                        WorkspaceAddStatus::Cloning => Some("workspace.add.cloning"),
                        WorkspaceAddStatus::Cancelling => Some("workspace.add.cancelling"),
                        WorkspaceAddStatus::Registering => Some("workspace.add.opening"),
                    };
                    if let Some(key) = status_key {
                        crate::ui::popup::notice(
                            ui,
                            &catalog.t(key, &[]),
                            crate::ui::popup::NoticeTone::Info,
                        );
                    }
                    if let Some(error) = &self.error {
                        crate::ui::popup::notice(
                            ui,
                            &catalog.t(error.message_key(), &[]),
                            crate::ui::popup::NoticeTone::Error,
                        );
                    }
                });
                crate::ui::popup::footer(ui, |ui| {
                    use crate::ui::popup::{ActionTone, action_button};
                    if self.status == WorkspaceAddStatus::Cloning {
                        if action_button(
                            ui,
                            &catalog.t("workspace.add.cancel_clone", &[]),
                            ActionTone::Secondary,
                            true,
                        )
                        .clicked()
                        {
                            action = Some(WorkspaceAddIntent::CancelClone);
                        }
                    } else if self.status == WorkspaceAddStatus::Ready {
                        let primary_key = if self.source == WorkspaceSource::Local {
                            "workspace.add.choose_folder"
                        } else {
                            "workspace.add.clone_open"
                        };
                        if action_button(
                            ui,
                            &catalog.t(primary_key, &[]),
                            ActionTone::Primary,
                            true,
                        )
                        .clicked()
                        {
                            match self.source {
                                WorkspaceSource::Local => {
                                    action = Some(WorkspaceAddIntent::PickLocal);
                                }
                                WorkspaceSource::GitHub => {
                                    match CloneRequest::prepare(
                                        &self.remote_url,
                                        self.selected_parent
                                            .as_deref()
                                            .unwrap_or_else(|| Path::new(&self.destination_parent)),
                                        &self.directory_name,
                                    ) {
                                        Ok(request) => {
                                            action = Some(WorkspaceAddIntent::Clone(request));
                                        }
                                        Err(error) => self.error = Some(error),
                                    }
                                }
                            }
                        }
                        if action_button(
                            ui,
                            &catalog.t("action.cancel", &[]),
                            ActionTone::Secondary,
                            true,
                        )
                        .clicked()
                        {
                            action = Some(WorkspaceAddIntent::Close);
                        }
                    }
                });
            },
        );
        if modal && !busy && action.is_none() {
            action = Some(WorkspaceAddIntent::Close);
        }
        action
    }
}

impl CloneError {
    fn message_key(&self) -> &'static str {
        match self {
            Self::InvalidUrl => "workspace.add.invalid_url",
            Self::InvalidName => "workspace.add.invalid_name",
            Self::ParentMissing => "workspace.add.parent_missing",
            Self::InvalidPathEncoding => "workspace.add.invalid_path_encoding",
            Self::DestinationExists => "workspace.add.destination_exists",
            Self::Filesystem => "workspace.add.filesystem_error",
            Self::GitFailed => "workspace.add.clone_failed",
            Self::TimedOut => "workspace.add.timeout",
            Self::Cancelled => "workspace.add.cancelled",
            Self::CancelledAfterCompletion => "workspace.add.cancelled_after_completion",
            Self::WorkerUnavailable => "workspace.add.worker_failed",
            Self::RegistrationFailed => "workspace.add.registration_failed",
            Self::SwitchFailed => "workspace.add.switch_failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn add_dialog_selects_local_picker_only_after_a_click_and_exposes_git_fields() {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 640.0))
            .build_ui_state(
                |ui, state: &mut (Option<WorkspaceAddUi>, Vec<WorkspaceAddIntent>)| {
                    if let Some(intent) = state.0.as_mut().and_then(|dialog| {
                        dialog.show(ui.ctx(), &i18n::Catalog::load("en-US").unwrap())
                    }) {
                        state.1.push(intent);
                    }
                },
                (None, Vec::new()),
            );
        harness.state_mut().0 = Some(WorkspaceAddUi::new(
            WorkspaceAddPurpose::SwitchRuntime,
            std::env::temp_dir(),
        ));
        harness.run();
        assert!(harness.state().1.is_empty());
        harness.get_by_label("My folder");
        harness.get_by_label("GitHub repository").click();
        harness.run();
        harness.get_by_label("Repository URL");
        harness.get_by_label("Clone and open");
        harness.get_by_label("My folder").click();
        harness.run();
        harness.get_by_label("Choose folder…").click();
        harness.run();
        assert!(
            harness
                .state()
                .1
                .iter()
                .any(|intent| matches!(intent, WorkspaceAddIntent::PickLocal))
        );
    }

    #[test]
    fn add_dialog_uses_roomy_shared_form_width() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut dialog =
            WorkspaceAddUi::new(WorkspaceAddPurpose::SwitchRuntime, std::env::temp_dir());
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 640.0))
            .build_ui(move |ui| {
                dialog.show(ui.ctx(), &catalog);
            });
        harness.run();
        let rect = harness
            .ctx
            .memory(|memory| memory.area_rect(egui::Id::new("workspace_add_source")))
            .expect("workspace add modal should be visible");
        assert!(rect.width() >= 550.0, "dialog too narrow: {rect:?}");
    }

    #[test]
    fn github_add_dialog_stays_inside_a_narrow_window() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut dialog =
            WorkspaceAddUi::new(WorkspaceAddPurpose::SwitchRuntime, std::env::temp_dir());
        dialog.source = WorkspaceSource::GitHub;
        dialog.error = Some(CloneError::DestinationExists);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(280.0, 360.0))
            .build_ui(move |ui| {
                dialog.show(ui.ctx(), &catalog);
            });
        harness.run();
        let rect = harness
            .ctx
            .memory(|memory| memory.area_rect(egui::Id::new("workspace_add_source")))
            .expect("workspace add modal should be visible");
        assert!(rect.left() >= 0.0 && rect.right() <= 280.0, "{rect:?}");
    }

    #[test]
    fn github_https_and_ssh_urls_identify_the_same_repository() {
        let parent = std::env::temp_dir();
        let https =
            CloneRequest::prepare("https://github.com/Example/project.git", &parent, "project")
                .unwrap();
        let ssh = CloneRequest::prepare("git@github.com:Example/project.git", &parent, "project")
            .unwrap();
        assert_eq!(https.identity, ssh.identity);
        assert_eq!(https.directory_name, "project");
    }

    #[test]
    fn github_clone_request_rejects_credentials_extra_path_and_traversal() {
        let parent = std::env::temp_dir();
        for url in [
            "https://token@github.com/owner/repo",
            "https://github.com/owner/repo/tree/main",
            "https://github.com/owner/repo?tab=readme",
            "https://evil.example/owner/repo",
        ] {
            assert!(
                CloneRequest::prepare(url, &parent, "repo").is_err(),
                "{url}"
            );
        }
        assert!(
            CloneRequest::prepare("https://github.com/owner/repo", &parent, "../repo").is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_picker_destination_is_rejected_before_clone_without_path_substitution() {
        use std::os::unix::ffi::OsStringExt;
        let parent = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/deppy-\xff".to_vec()));
        let mut dialog =
            WorkspaceAddUi::new(WorkspaceAddPurpose::SwitchRuntime, PathBuf::from("/tmp"));
        dialog.set_destination(&parent);
        assert_eq!(dialog.selected_parent.as_deref(), Some(parent.as_path()));
        assert!(matches!(
            CloneRequest::prepare("https://github.com/owner/repo", &parent, "repo"),
            Err(CloneError::InvalidPathEncoding)
        ));
    }

    #[test]
    fn local_fixture_clones_then_reuses_matching_origin_without_overwriting() {
        let root =
            std::env::temp_dir().join(format!("deppy-workspace-clone-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.git");
        crate::git_cli::run_git_bounded(
            &root,
            &["init", "--bare", source.to_str().unwrap()],
            std::time::Duration::from_secs(10),
            1024,
        )
        .unwrap();
        let parent = root.join("projects");
        std::fs::create_dir(&parent).unwrap();
        let mut request =
            CloneRequest::prepare("https://github.com/owner/repo", &parent, "repo").unwrap();
        request.remote_url = source.to_string_lossy().into_owned();
        let cancel = AtomicBool::new(false);
        let first = clone_or_reuse_with_journal(&request, &cancel, Some(&root)).unwrap();
        assert!(!first.reused);
        assert!(!std::fs::read_dir(&root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("clone-staging-")
        }));
        assert_eq!(
            first.path,
            std::fs::canonicalize(&parent).unwrap().join("repo")
        );
        crate::git_cli::run_git_bounded(
            &first.path,
            &[
                "remote",
                "set-url",
                "origin",
                "git@github.com:owner/repo.git",
            ],
            std::time::Duration::from_secs(10),
            1024,
        )
        .unwrap();
        let second = clone_or_reuse(&request, &cancel).unwrap();
        assert!(second.reused);
        assert_eq!(second.path, first.path);
        std::fs::write(first.path.join("keep.txt"), "preserve").unwrap();
        let other =
            CloneRequest::prepare("https://github.com/elsewhere/repo", &parent, "repo").unwrap();
        assert_eq!(
            clone_or_reuse(&other, &cancel).unwrap_err(),
            CloneError::DestinationExists
        );
        assert_eq!(
            std::fs::read_to_string(first.path.join("keep.txt")).unwrap(),
            "preserve"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn final_move_never_replaces_a_folder_created_during_clone() {
        let root = std::env::temp_dir().join(format!("deppy-clone-race-{}", uuid::Uuid::new_v4()));
        let staging = root.join(".deppy-clone-stage");
        let target = root.join("repo");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("clone.txt"), "clone").unwrap();
        std::fs::create_dir(&target).unwrap();
        assert!(rename_staging_no_replace(&staging, &target).is_err());
        assert!(target.is_dir());
        assert!(!target.join("clone.txt").exists());
        assert!(staging.join("clone.txt").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn abandoned_clone_recovery_removes_only_owned_staging_directories() {
        let root =
            std::env::temp_dir().join(format!("deppy-clone-recovery-{}", uuid::Uuid::new_v4()));
        let data = root.join("data");
        let projects = root.join("projects");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        let projects = std::fs::canonicalize(projects).unwrap();
        let owned = "11111111-1111-4111-8111-111111111111";
        let foreign = "22222222-2222-4222-8222-222222222222";
        for (id, marker_present) in [(owned, true), (foreign, false)] {
            let stage = projects.join(format!(".deppy-clone-{id}"));
            std::fs::create_dir(&stage).unwrap();
            std::fs::write(stage.join("payload"), "partial clone").unwrap();
            if marker_present {
                std::fs::write(projects.join(format!(".deppy-clone-{id}.owner")), id).unwrap();
            }
            let manifest = serde_json::json!({"staging": stage.to_str().unwrap(), "owner": id});
            std::fs::write(
                data.join(format!("clone-staging-{id}.json")),
                manifest.to_string(),
            )
            .unwrap();
        }
        recover_abandoned_clone_staging(&data).unwrap();
        assert!(!projects.join(format!(".deppy-clone-{owned}")).exists());
        assert!(!data.join(format!("clone-staging-{owned}.json")).exists());
        assert!(
            projects
                .join(format!(".deppy-clone-{foreign}/payload"))
                .exists()
        );
        assert!(data.join(format!("clone-staging-{foreign}.json")).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn destination_io_errors_keep_conflicts_distinct_from_other_failures() {
        assert_eq!(
            classify_destination_io_error(&std::io::Error::from(std::io::ErrorKind::AlreadyExists)),
            CloneError::DestinationExists
        );
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::StorageFull,
        ] {
            assert_eq!(
                classify_destination_io_error(&std::io::Error::from(kind)),
                CloneError::Filesystem
            );
        }
    }

    #[test]
    fn cancel_after_worker_success_prevents_opening_while_preserving_clone() {
        let root = std::env::temp_dir().join(format!(
            "deppy-clone-finished-cancel-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&root).unwrap();
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        result_tx
            .send(Ok(CloneOutcome {
                path: root.clone(),
                reused: false,
            }))
            .unwrap();
        let mut task = CloneTask {
            result_rx,
            handle: None,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        task.cancel();
        assert_eq!(task.poll(), Some(Err(CloneError::CancelledAfterCompletion)));
        assert!(root.is_dir());
        std::fs::remove_dir(root).unwrap();
    }
}
