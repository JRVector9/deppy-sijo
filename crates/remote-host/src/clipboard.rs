//! Bounded, account-private image staging. Clients never supply a filesystem path.
use crate::files::FileError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const IMAGE_BYTES_MAX: usize = 32 * 1024 * 1024;
pub const CHUNK_BYTES_MAX: usize = 256 * 1024;
const CACHE_FILES_MAX: usize = 64;
const CACHE_BYTES_MAX: u64 = 256 * 1024 * 1024;
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageChunk {
    pub upload_id: String,
    pub offset: u64,
    pub total: u64,
    pub sha256: String,
    pub extension: String,
    pub data: Vec<u8>,
}
impl std::fmt::Debug for ImageChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageChunk")
            .field("offset", &self.offset)
            .field("total", &self.total)
            .field("chunk_bytes", &self.data.len())
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    pub written: u64,
    /// Present only after complete checksum validation and atomic publication.
    pub path: Option<String>,
}

pub fn stage(workspace: &str, chunk: ImageChunk) -> Result<Receipt, FileError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(FileError::Io)?;
    stage_at(&home, workspace, chunk)
}

fn open_at(directory: &File, name: &CStr, flags: libc::c_int) -> Result<File, FileError> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(FileError::Changed);
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn cache_directory(home: &Path) -> Result<File, FileError> {
    // Pin the canonical HOME chain before creating anything. No HOME permission policy is
    // imposed here; only the three cache components must be owned and not writable by others.
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")
        .map_err(|_| FileError::Io)?;
    for component in home.components() {
        if let std::path::Component::Normal(name) = component {
            let name = CString::new(name.as_bytes()).map_err(|_| FileError::InvalidPath)?;
            directory = open_at(&directory, &name, libc::O_RDONLY | libc::O_DIRECTORY)?;
        }
    }
    for (name, private) in [
        (c".cache", false),
        (c"deppy", false),
        (c"ssh-clipboard", true),
    ] {
        if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } != 0
            && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(FileError::Io);
        }
        directory = open_at(&directory, name, libc::O_RDONLY | libc::O_DIRECTORY)
            .map_err(|_| FileError::PermissionDenied)?;
        let meta = directory.metadata().map_err(|_| FileError::Io)?;
        if meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o022 != 0
            || (private && meta.mode() & 0o077 != 0)
        {
            return Err(FileError::PermissionDenied);
        }
    }
    Ok(directory)
}

struct CacheLock(File);
impl CacheLock {
    fn acquire(directory: &File) -> Result<Self, FileError> {
        let file = open_at(directory, c".lock", libc::O_RDWR | libc::O_CREAT)?;
        private_file(&file)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(FileError::Io);
        }
        Ok(Self(file))
    }
}
impl Drop for CacheLock {
    fn drop(&mut self) {
        // Close alone leaves a flock held by any concurrent fork's inherited description
        // until exec. Explicit unlock releases it when this operation actually completes.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn unlink_at(directory: &File, name: &CStr) -> Result<(), FileError> {
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(FileError::Io);
    }
    Ok(())
}

fn stage_at(home: &Path, workspace: &str, chunk: ImageChunk) -> Result<Receipt, FileError> {
    let id = uuid::Uuid::parse_str(&chunk.upload_id).map_err(|_| FileError::InvalidPath)?;
    let end = chunk
        .offset
        .checked_add(chunk.data.len() as u64)
        .ok_or(FileError::TooLarge)?;
    if !home.is_absolute()
        || home.to_str().is_none()
        || workspace.len() != 64
        || !workspace.bytes().all(|b| b.is_ascii_hexdigit())
        || id.to_string() != chunk.upload_id
        || chunk.sha256.len() != 64
        || !chunk.sha256.bytes().all(|b| b.is_ascii_hexdigit())
        || !["png", "jpg", "webp", "gif"].contains(&chunk.extension.as_str())
    {
        return Err(FileError::InvalidPath);
    }
    if chunk.total == 0
        || chunk.total > IMAGE_BYTES_MAX as u64
        || chunk.data.is_empty()
        || chunk.data.len() > CHUNK_BYTES_MAX
        || end > chunk.total
    {
        return Err(FileError::TooLarge);
    }
    let base = home.join(".cache/deppy/ssh-clipboard");
    let stem = format!("{}-{}", &workspace[..32], chunk.upload_id);
    let part = CString::new(format!("{stem}.part")).map_err(|_| FileError::InvalidPath)?;
    let target =
        CString::new(format!("{stem}.{}", chunk.extension)).map_err(|_| FileError::InvalidPath)?;
    let target_path = base.join(target.to_str().map_err(|_| FileError::InvalidPath)?);
    // Validate the receipt before any cache mutation. Preserve HOME's spelling in the receipt;
    // the canonical descriptor chain below supplies the filesystem authority.
    let path = target_path
        .to_str()
        .filter(|p| p.len() <= 4096)
        .ok_or(FileError::InvalidPath)?
        .to_owned();
    let home = home.canonicalize().map_err(|_| FileError::Io)?;
    let directory = cache_directory(&home)?;
    let _lock = CacheLock::acquire(&directory)?;
    let (count, bytes) = prune(&directory)?;
    if bytes.saturating_add(chunk.data.len() as u64) > CACHE_BYTES_MAX
        || (chunk.offset == 0 && count >= CACHE_FILES_MAX)
    {
        return Err(FileError::TooLarge);
    }
    let flags = libc::O_RDWR
        | if chunk.offset == 0 {
            libc::O_CREAT | libc::O_EXCL
        } else {
            0
        };
    let mut file = open_at(&directory, &part, flags)?;
    private_file(&file)?;
    if file.metadata().map_err(|_| FileError::Io)?.len() != chunk.offset {
        return Err(FileError::Changed);
    }
    file.seek(SeekFrom::Start(chunk.offset))
        .map_err(|_| FileError::Io)?;
    file.write_all(&chunk.data).map_err(|_| FileError::Io)?;
    if end < chunk.total {
        return Ok(Receipt {
            written: end,
            path: None,
        });
    }
    file.seek(SeekFrom::Start(0)).map_err(|_| FileError::Io)?;
    let mut magic = [0u8; 12];
    let n = file.read(&mut magic).map_err(|_| FileError::Io)?;
    let valid = match chunk.extension.as_str() {
        "png" => n >= 8 && magic.starts_with(b"\x89PNG\r\n\x1a\n"),
        "jpg" => n >= 3 && magic.starts_with(b"\xff\xd8\xff"),
        "gif" => n >= 6 && (magic.starts_with(b"GIF87a") || magic.starts_with(b"GIF89a")),
        "webp" => n >= 12 && &magic[..4] == b"RIFF" && &magic[8..12] == b"WEBP",
        _ => false,
    };
    file.seek(SeekFrom::Start(0)).map_err(|_| FileError::Io)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut reader = (&mut file).take(end + 1);
    let mut hashed = 0u64;
    loop {
        let n = reader.read(&mut buffer).map_err(|_| FileError::Io)?;
        if n == 0 {
            break;
        }
        hashed += n as u64;
        hash.update(&buffer[..n]);
    }
    let digest: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !valid || hashed != end || digest != chunk.sha256.to_ascii_lowercase() {
        let _ = unlink_at(&directory, &part);
        return Err(FileError::Changed);
    }
    // link is atomic and cannot replace any previously published destination.
    if unsafe {
        libc::linkat(
            directory.as_raw_fd(),
            part.as_ptr(),
            directory.as_raw_fd(),
            target.as_ptr(),
            0,
        )
    } != 0
    {
        let _ = unlink_at(&directory, &part);
        return Err(FileError::Changed);
    }
    if let Err(error) = unlink_at(&directory, &part) {
        let _ = unlink_at(&directory, &target);
        return Err(error);
    }
    Ok(Receipt {
        written: end,
        path: Some(path),
    })
}

fn private_file(file: &File) -> Result<(), FileError> {
    let meta = file.metadata().map_err(|_| FileError::Io)?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
    {
        return Err(FileError::PermissionDenied);
    }
    Ok(())
}

fn owned_name(name: &str) -> bool {
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    stem.is_ascii()
        && stem.len() == 69
        && stem.as_bytes()[32] == b'-'
        && stem[..32].bytes().all(|b| b.is_ascii_hexdigit())
        && uuid::Uuid::parse_str(&stem[33..]).is_ok()
        && ["part", "png", "jpg", "gif", "webp"].contains(&extension)
}
struct DirectoryReader(*mut libc::DIR);
impl DirectoryReader {
    fn open(directory: &File) -> Result<Self, FileError> {
        let file = open_at(directory, c".", libc::O_RDONLY | libc::O_DIRECTORY)?;
        use std::os::fd::IntoRawFd;
        let descriptor = file.into_raw_fd();
        let stream = unsafe { libc::fdopendir(descriptor) };
        if stream.is_null() {
            unsafe { libc::close(descriptor) };
            return Err(FileError::Io);
        }
        Ok(Self(stream))
    }
    fn next_name(&mut self) -> Result<Option<CString>, FileError> {
        loop {
            #[cfg(target_os = "macos")]
            unsafe {
                *libc::__error() = 0;
            }
            #[cfg(target_os = "linux")]
            unsafe {
                *libc::__errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(self.0) };
            if entry.is_null() {
                return if std::io::Error::last_os_error().raw_os_error() == Some(0) {
                    Ok(None)
                } else {
                    Err(FileError::Io)
                };
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name != c"." && name != c".." {
                return Ok(Some(name.to_owned()));
            }
        }
    }
}
impl Drop for DirectoryReader {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

fn prune(directory: &File) -> Result<(usize, u64), FileError> {
    let mut count = 0;
    let mut bytes = 0u64;
    let mut entries = DirectoryReader::open(directory)?;
    let mut i = 0;
    while let Some(name) = entries.next_name()? {
        if i >= 1024 {
            return Err(FileError::TooLarge);
        }
        i += 1;
        let Ok(name_str) = name.to_str() else {
            continue;
        };
        if !owned_name(name_str) {
            continue;
        }
        let file = open_at(directory, &name, libc::O_RDONLY)?;
        private_file(&file)?;
        let meta = file.metadata().map_err(|_| FileError::Io)?;
        if meta
            .modified()
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > CACHE_TTL)
        {
            unlink_at(directory, &name)?;
        } else {
            count += 1;
            bytes = bytes.saturating_add(meta.len());
        }
    }
    Ok((count, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let p =
                std::env::temp_dir().join(format!("deppy-image-stage-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn chunk(data: &[u8]) -> ImageChunk {
        ImageChunk {
            upload_id: uuid::Uuid::new_v4().to_string(),
            offset: 0,
            total: data.len() as u64,
            sha256: Sha256::digest(data)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            extension: "png".into(),
            data: data.to_vec(),
        }
    }
    #[test]
    fn chunks_publish_only_after_complete_digest_and_preserve_private_permissions() {
        let f = Fixture::new();
        let bytes = b"\x89PNG\r\n\x1a\nfixture-image";
        let mut first = chunk(bytes);
        first.data.truncate(8);
        assert_eq!(
            stage_at(&f.0, &"a".repeat(64), first.clone()).unwrap().path,
            None
        );
        assert_eq!(
            stage_at(&f.0, &"a".repeat(64), first.clone()),
            Err(FileError::Changed)
        );
        first.offset = 8;
        first.data = bytes[8..].to_vec();
        assert_eq!(
            stage_at(&f.0, &"b".repeat(64), first.clone()),
            Err(FileError::Changed)
        );
        let receipt = stage_at(&f.0, &"a".repeat(64), first).unwrap();
        let path = PathBuf::from(receipt.path.unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o777, 0o600);
    }
    #[test]
    fn traversal_oversize_digest_and_format_mismatch_are_rejected() {
        let f = Fixture::new();
        let mut c = chunk(b"\x89PNG\r\n\x1a\nfixture");
        c.upload_id = "../../elsewhere".into();
        assert_eq!(
            stage_at(&f.0, &"a".repeat(64), c),
            Err(FileError::InvalidPath)
        );
        let mut c = chunk(b"\x89PNG\r\n\x1a\nfixture");
        c.total = IMAGE_BYTES_MAX as u64 + 1;
        assert_eq!(stage_at(&f.0, &"a".repeat(64), c), Err(FileError::TooLarge));
        let mut c = chunk(b"\x89PNG\r\n\x1a\nfixture");
        c.sha256 = "0".repeat(64);
        assert_eq!(stage_at(&f.0, &"a".repeat(64), c), Err(FileError::Changed));
        assert_eq!(
            stage_at(&f.0, &"a".repeat(64), chunk(b"plain text, not an image")),
            Err(FileError::Changed)
        );
    }
    #[test]
    fn staging_never_follows_a_symlink_or_blocks_on_a_fifo() {
        let f = Fixture::new();
        let data = b"\x89PNG\r\n\x1a\nfixture";
        let mut c = chunk(data);
        c.data.truncate(8);
        stage_at(&f.0, &"a".repeat(64), c.clone()).unwrap();
        let part = f.0.join(".cache/deppy/ssh-clipboard").join(format!(
            "{}-{}.part",
            "a".repeat(32),
            c.upload_id
        ));
        std::fs::remove_file(&part).unwrap();
        let sentinel = f.0.join("sentinel");
        std::fs::write(&sentinel, b"secret").unwrap();
        std::os::unix::fs::symlink(&sentinel, &part).unwrap();
        c.offset = 8;
        c.data = data[8..].to_vec();
        assert!(stage_at(&f.0, &"a".repeat(64), c.clone()).is_err());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"secret");
        std::fs::remove_file(&part).unwrap();
        use std::os::unix::ffi::OsStrExt;
        let fifo = std::ffi::CString::new(part.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(stage_at(&f.0, &"a".repeat(64), c.clone()).is_err());
        std::fs::remove_file(&part).unwrap();
        std::fs::hard_link(&sentinel, &part).unwrap();
        assert!(stage_at(&f.0, &"a".repeat(64), c).is_err());
        assert_eq!(std::fs::read(sentinel).unwrap(), b"secret");
        assert!(!owned_name(&format!(
            "{}-{}.png",
            "가".repeat(10) + "ab",
            uuid::Uuid::new_v4()
        )));
    }
    #[test]
    fn abandoned_uploads_have_a_fixed_file_quota() {
        let f = Fixture::new();
        for _ in 0..CACHE_FILES_MAX {
            let mut c = chunk(b"12");
            c.data.truncate(1);
            stage_at(&f.0, &"a".repeat(64), c).unwrap();
        }
        let mut c = chunk(b"12");
        c.data.truncate(1);
        assert_eq!(stage_at(&f.0, &"a".repeat(64), c), Err(FileError::TooLarge));
    }

    #[test]
    fn cache_ancestors_cannot_redirect_staged_images() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let elsewhere = f.0.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, f.0.join(".cache")).unwrap();
        assert_eq!(
            stage_at(&f.0, &"a".repeat(64), chunk(b"\x89PNG\r\n\x1a\nimage")),
            Err(FileError::PermissionDenied)
        );
        assert!(!elsewhere.join("deppy").exists());

        std::fs::remove_file(f.0.join(".cache")).unwrap();
        std::fs::create_dir(f.0.join(".cache")).unwrap();
        std::fs::set_permissions(f.0.join(".cache"), std::fs::Permissions::from_mode(0o777))
            .unwrap();
        assert_eq!(
            stage_at(&f.0, &"a".repeat(64), chunk(b"\x89PNG\r\n\x1a\nimage")),
            Err(FileError::PermissionDenied)
        );
        assert!(!f.0.join(".cache/deppy").exists());
        std::fs::set_permissions(f.0.join(".cache"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert!(stage_at(&f.0, &"a".repeat(64), chunk(b"\x89PNG\r\n\x1a\nimage")).is_ok());
    }

    #[test]
    fn unrepresentable_receipt_paths_do_not_leave_image_files() {
        use std::os::unix::ffi::OsStringExt;
        let f = Fixture::new();
        let home =
            f.0.join(std::ffi::OsString::from_vec(b"home-\xff".to_vec()));
        // APFS rejects non-UTF8 names; Linux permits them and exercises the existing-home case.
        #[cfg(target_os = "linux")]
        std::fs::create_dir(&home).unwrap();
        assert_eq!(
            stage_at(&home, &"a".repeat(64), chunk(b"\x89PNG\r\n\x1a\nimage")),
            Err(FileError::InvalidPath)
        );
        assert!(!home.join(".cache").exists());
    }

    #[test]
    fn repeated_publication_does_not_leave_an_abandoned_duplicate() {
        let f = Fixture::new();
        let c = chunk(b"\x89PNG\r\n\x1a\nimage");
        let target = stage_at(&f.0, &"a".repeat(64), c.clone())
            .unwrap()
            .path
            .unwrap();
        assert_eq!(
            stage_at(&f.0, &"a".repeat(64), c.clone()),
            Err(FileError::Changed)
        );
        let part = f.0.join(".cache/deppy/ssh-clipboard").join(format!(
            "{}-{}.part",
            "a".repeat(32),
            c.upload_id
        ));
        assert!(!part.exists());
        assert_eq!(std::fs::read(target).unwrap(), c.data);
    }

    #[test]
    fn cache_lock_releases_while_fork_child_keeps_inherited_descriptor() {
        let f = Fixture::new();
        let directory = cache_directory(&f.0.canonicalize().unwrap()).unwrap();
        let lock = CacheLock::acquire(&directory).unwrap();
        let mut ready = [0; 2];
        let mut release = [0; 2];
        assert_eq!(unsafe { libc::pipe(ready.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::pipe(release.as_mut_ptr()) }, 0);
        for fd in ready.into_iter().chain(release) {
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            // Only async-signal-safe operations after fork; _exit deliberately keeps the
            // inherited lock description open until the parent's acquisition is attempted.
            unsafe {
                libc::close(ready[0]);
                libc::close(release[1]);
                let sent = libc::write(ready[1], b"x".as_ptr().cast(), 1);
                let mut signal = 0u8;
                let received = libc::read(release[0], (&mut signal as *mut u8).cast(), 1);
                libc::_exit(if sent == 1 && received == 1 { 0 } else { 2 });
            }
        }
        unsafe {
            libc::close(ready[1]);
            libc::close(release[0]);
        }
        let mut signal = 0u8;
        let ready_bytes = unsafe { libc::read(ready[0], (&mut signal as *mut u8).cast(), 1) };
        drop(lock);
        let next = CacheLock::acquire(&directory);
        let mut status = 0;
        let still_running = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) } == 0;
        unsafe {
            libc::write(release[1], b"x".as_ptr().cast(), 1);
            libc::close(release[1]);
            libc::close(ready[0]);
            libc::waitpid(child, &mut status, 0);
        }
        assert_eq!(ready_bytes, 1);
        assert!(still_running, "forked child exited before acquisition");
        assert_eq!(status, 0);
        assert!(
            next.is_ok(),
            "closed parent lock remained held by fork child"
        );
    }
}
