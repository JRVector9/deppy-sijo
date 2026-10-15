//! Host-only proof and descriptor-relative permanent deletion. UI only carries the proof.
use super::{AppHostFileOperationBudget, app_host_file_operation_cancelled};
use crate::ui::file_tree::FileTreeIoErrorCode as Error;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct FileTreeDeleteGuard {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    proof: std::sync::Arc<unix::Proof>,
}

impl std::fmt::Debug for FileTreeDeleteGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileTreeDeleteGuard")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeleteStage {
    Move,
    Restore,
    Cleanup,
}

impl FileTreeDeleteGuard {
    pub(super) fn capture(root: &Path, target: &Path) -> Result<Self, Error> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            unix::capture(root, target).map(|proof| Self {
                proof: std::sync::Arc::new(proof),
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (root, target);
            Err(Error::NativeFailure)
        }
    }

    pub(super) fn validate(&self, target: &Path) -> Result<(), Error> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.proof.validate(target)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = target;
            Err(Error::NativeFailure)
        }
    }

    pub(super) fn remove(
        &self,
        target: &Path,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<(), Error> {
        self.remove_with_hook(target, cancel, |_, _| {})
    }

    pub(super) fn remove_with_hook(
        &self,
        target: &Path,
        cancel: &std::sync::atomic::AtomicBool,
        mut hook: impl FnMut(DeleteStage, &Path),
    ) -> Result<(), Error> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            unix::remove(&self.proof, target, cancel, &mut hook)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (target, cancel, &mut hook);
            Err(Error::NativeFailure)
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod unix {
    use super::*;
    use std::{
        ffi::{CStr, CString},
        fs::{File, OpenOptions},
        os::{
            fd::{AsRawFd, FromRawFd, RawFd},
            unix::{
                ffi::OsStrExt,
                fs::{MetadataExt, OpenOptionsExt},
            },
        },
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Identity {
        dev: u64,
        ino: u64,
        kind: u32,
    }
    impl Identity {
        fn from_metadata(metadata: &std::fs::Metadata) -> Self {
            Self {
                dev: metadata.dev(),
                ino: metadata.ino(),
                kind: metadata.mode() & u32::from(libc::S_IFMT),
            }
        }
        fn from_stat(stat: &libc::stat) -> Self {
            Self {
                dev: stat.st_dev as u64,
                ino: stat.st_ino,
                kind: stat.st_mode as u32 & u32::from(libc::S_IFMT),
            }
        }
    }

    #[derive(Debug)]
    pub(super) struct Proof {
        logical_root: PathBuf,
        logical_target: PathBuf,
        physical_root: PathBuf,
        physical_parent: PathBuf,
        root: File,
        parent: File,
        target: File,
        name: CString,
        root_identity: Identity,
        parent_identity: Identity,
        target_identity: Identity,
    }

    fn c_name(name: &std::ffi::OsStr) -> Result<CString, Error> {
        CString::new(name.as_bytes()).map_err(|_| Error::InvalidPath)
    }
    fn open_directory(path: &Path) -> Result<File, Error> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| Error::InvalidPath)
    }
    fn open_at(parent: RawFd, name: &CStr, flags: i32) -> Result<File, Error> {
        // SAFETY: parent is live and name is a terminated byte string; the returned fd is owned.
        let fd = unsafe { libc::openat(parent, name.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(Error::InvalidPath);
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    fn stat_at(parent: RawFd, name: &CStr) -> Result<libc::stat, Error> {
        let mut stat = std::mem::MaybeUninit::uninit();
        // SAFETY: output storage is valid and symlinks are inspected as objects.
        if unsafe {
            libc::fstatat(
                parent,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(Error::InvalidPath);
        }
        Ok(unsafe { stat.assume_init() })
    }

    pub(super) fn capture(root: &Path, target: &Path) -> Result<Proof, Error> {
        let physical_root = std::fs::canonicalize(root).map_err(|_| Error::InvalidPath)?;
        let physical_parent = std::fs::canonicalize(target.parent().ok_or(Error::InvalidPath)?)
            .map_err(|_| Error::InvalidPath)?;
        if !physical_parent.starts_with(&physical_root) {
            return Err(Error::OutsideRoot);
        }
        let root_fd = open_directory(&physical_root)?;
        let parent = open_directory(&physical_parent)?;
        let name = c_name(target.file_name().ok_or(Error::InvalidPath)?)?;
        let before = stat_at(parent.as_raw_fd(), &name)?;
        #[cfg(target_os = "macos")]
        let flags = if before.st_mode as u32 & u32::from(libc::S_IFMT) == u32::from(libc::S_IFLNK) {
            libc::O_SYMLINK
        } else {
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK
        };
        #[cfg(target_os = "linux")]
        let flags = libc::O_PATH | libc::O_NOFOLLOW;
        let target_fd = open_at(parent.as_raw_fd(), &name, flags)?;
        let proof = Proof {
            logical_root: root.to_path_buf(),
            logical_target: target.to_path_buf(),
            physical_root,
            physical_parent,
            root_identity: Identity::from_metadata(
                &root_fd.metadata().map_err(|_| Error::InvalidPath)?,
            ),
            parent_identity: Identity::from_metadata(
                &parent.metadata().map_err(|_| Error::InvalidPath)?,
            ),
            target_identity: Identity::from_metadata(
                &target_fd.metadata().map_err(|_| Error::InvalidPath)?,
            ),
            root: root_fd,
            parent,
            target: target_fd,
            name,
        };
        if proof.target_identity != Identity::from_stat(&before) {
            return Err(Error::Conflict);
        }
        proof.validate(target)?;
        Ok(proof)
    }

    impl Proof {
        pub(super) fn validate(&self, target: &Path) -> Result<(), Error> {
            if target != self.logical_target {
                return Err(Error::InvalidPath);
            }
            let root = std::fs::canonicalize(&self.logical_root).map_err(|_| Error::InvalidPath)?;
            let parent = std::fs::canonicalize(target.parent().ok_or(Error::InvalidPath)?)
                .map_err(|_| Error::InvalidPath)?;
            if root != self.physical_root
                || parent != self.physical_parent
                || !parent.starts_with(&root)
            {
                return Err(Error::OutsideRoot);
            }
            let identity = |path: &Path| {
                std::fs::metadata(path)
                    .map(|metadata| Identity::from_metadata(&metadata))
                    .map_err(|_| Error::InvalidPath)
            };
            if identity(&root)? != self.root_identity
                || identity(&parent)? != self.parent_identity
                || Identity::from_metadata(&self.root.metadata().map_err(|_| Error::InvalidPath)?)
                    != self.root_identity
                || Identity::from_metadata(&self.parent.metadata().map_err(|_| Error::InvalidPath)?)
                    != self.parent_identity
                || Identity::from_metadata(&self.target.metadata().map_err(|_| Error::InvalidPath)?)
                    != self.target_identity
                || Identity::from_stat(&stat_at(self.parent.as_raw_fd(), &self.name)?)
                    != self.target_identity
            {
                return Err(Error::Conflict);
            }
            Ok(())
        }
    }

    fn rename_no_replace(
        source_dir: RawFd,
        source: &CStr,
        destination_dir: RawFd,
        destination: &CStr,
    ) -> Result<(), Error> {
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::renameatx_np(
                source_dir,
                source.as_ptr(),
                destination_dir,
                destination.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                source_dir,
                source.as_ptr(),
                destination_dir,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            Err(Error::Conflict)
        } else {
            Ok(())
        }
    }

    struct DirectoryStream(*mut libc::DIR);
    impl Drop for DirectoryStream {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    fn directory_stream(file: &File) -> Result<DirectoryStream, Error> {
        let fd = unsafe { libc::dup(file.as_raw_fd()) };
        if fd < 0 {
            return Err(Error::NativeFailure);
        }
        let directory = unsafe { libc::fdopendir(fd) };
        if directory.is_null() {
            unsafe {
                libc::close(fd);
            }
            return Err(Error::NativeFailure);
        }
        Ok(DirectoryStream(directory))
    }
    fn errno_pointer() -> *mut libc::c_int {
        #[cfg(target_os = "macos")]
        {
            unsafe { libc::__error() }
        }
        #[cfg(target_os = "linux")]
        {
            unsafe { libc::__errno_location() }
        }
    }
    fn remove_at(
        parent: RawFd,
        name: &CStr,
        budget: &mut AppHostFileOperationBudget,
        cancel: &std::sync::atomic::AtomicBool,
        depth: usize,
    ) -> Result<(), Error> {
        app_host_file_operation_cancelled(cancel).map_err(|_| Error::NativeFailure)?;
        budget
            .consume_item(depth)
            .map_err(|_| Error::NativeFailure)?;
        let stat = stat_at(parent, name)?;
        let kind = stat.st_mode as u32 & u32::from(libc::S_IFMT);
        let is_dir = kind == u32::from(libc::S_IFDIR);
        if is_dir {
            let directory = open_at(
                parent,
                name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )?;
            if Identity::from_metadata(&directory.metadata().map_err(|_| Error::NativeFailure)?)
                != Identity::from_stat(&stat)
            {
                return Err(Error::Conflict);
            }
            let stream = directory_stream(&directory)?;
            loop {
                app_host_file_operation_cancelled(cancel).map_err(|_| Error::NativeFailure)?;
                unsafe {
                    *errno_pointer() = 0;
                }
                let entry = unsafe { libc::readdir(stream.0) };
                if entry.is_null() {
                    if unsafe { *errno_pointer() } != 0 {
                        return Err(Error::NativeFailure);
                    }
                    break;
                }
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_owned();
                if name.to_bytes() != b"." && name.to_bytes() != b".." {
                    remove_at(
                        directory.as_raw_fd(),
                        &name,
                        budget,
                        cancel,
                        depth.saturating_add(1),
                    )?;
                }
            }
        } else if kind == u32::from(libc::S_IFREG) || kind == u32::from(libc::S_IFLNK) {
            budget
                .consume_bytes(u64::try_from(stat.st_size).map_err(|_| Error::NativeFailure)?)
                .map_err(|_| Error::NativeFailure)?;
        } else {
            return Err(Error::NativeFailure);
        }
        app_host_file_operation_cancelled(cancel).map_err(|_| Error::NativeFailure)?;
        if unsafe {
            libc::unlinkat(
                parent,
                name.as_ptr(),
                if is_dir { libc::AT_REMOVEDIR } else { 0 },
            )
        } != 0
        {
            return Err(Error::NativeFailure);
        }
        Ok(())
    }

    pub(super) fn remove(
        proof: &Proof,
        target: &Path,
        cancel: &std::sync::atomic::AtomicBool,
        hook: &mut impl FnMut(DeleteStage, &Path),
    ) -> Result<(), Error> {
        proof.validate(target)?;
        // Scan limits before any mutation; the fd-relative removal also enforces them.
        let mut validation = AppHostFileOperationBudget::default();
        super::super::app_host_validate_tree(target, &mut validation, cancel, 0)
            .map_err(|_| Error::NativeFailure)?;
        proof.validate(target)?;
        app_host_file_operation_cancelled(cancel).map_err(|_| Error::NativeFailure)?;
        let name = CString::new(format!(".deppy-delete-{}", uuid::Uuid::new_v4())).unwrap();
        let directory_path = proof
            .physical_parent
            .join(std::ffi::OsStr::from_bytes(name.to_bytes()));
        if unsafe { libc::mkdirat(proof.parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(Error::NativeFailure);
        }
        let cleanup = || {
            if unsafe {
                libc::unlinkat(proof.parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR)
            } != 0
            {
                Err(Error::NativeFailure)
            } else {
                Ok(())
            }
        };
        let holder = match open_at(
            proof.parent.as_raw_fd(),
            &name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        ) {
            Ok(holder) => holder,
            Err(error) => {
                cleanup()?;
                return Err(error);
            }
        };
        let item = c"item";
        hook(DeleteStage::Move, &directory_path);
        if let Err(error) = rename_no_replace(
            proof.parent.as_raw_fd(),
            &proof.name,
            holder.as_raw_fd(),
            item,
        ) {
            cleanup()?;
            return Err(error);
        }
        let matched = stat_at(holder.as_raw_fd(), item)
            .map(|stat| Identity::from_stat(&stat) == proof.target_identity)
            .unwrap_or(false);
        let result = if matched {
            remove_at(
                holder.as_raw_fd(),
                item,
                &mut AppHostFileOperationBudget::default(),
                cancel,
                0,
            )
        } else {
            Err(Error::Conflict)
        };
        if let Err(error) = result {
            // Never unlink an unverified replacement, even if restoration is blocked.
            hook(DeleteStage::Restore, &directory_path);
            rename_no_replace(
                holder.as_raw_fd(),
                item,
                proof.parent.as_raw_fd(),
                &proof.name,
            )?;
            hook(DeleteStage::Cleanup, &directory_path);
            cleanup()?;
            return Err(error);
        }
        hook(DeleteStage::Cleanup, &directory_path);
        cleanup()
    }
}
