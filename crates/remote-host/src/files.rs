//! Root-scoped file operations. Paths on the wire are Unix bytes, never display labels.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const FILE_BYTES_MAX: usize = 256 * 1024;
pub const DIRECTORY_PAGE_MAX: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FileError {
    InvalidPath,
    OutsideRoot,
    NotFound,
    PermissionDenied,
    NotDirectory,
    NotRegularFile,
    TooLarge,
    Changed,
    Io,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileRevision {
    pub device: u64,
    pub inode: u64,
    pub length: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
    pub sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileDocument {
    pub content: Vec<u8>,
    pub revision: FileRevision,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub name: Vec<u8>,
    pub is_directory: bool,
    pub is_symlink: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DirectoryCursor {
    pub offset: u32,
    pub revision: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DirectoryPage {
    pub entries: Vec<DirectoryEntry>,
    pub next: Option<DirectoryCursor>,
}

pub struct FileRoot {
    canonical: PathBuf,
    directory: File,
}

impl FileRoot {
    pub fn open(path: &Path) -> Result<Self, FileError> {
        let canonical = path.canonicalize().map_err(io_error)?;
        let mut directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/")
            .map_err(io_error)?;
        // Pin every ancestor after canonicalization. A swapped parent symlink must not redirect
        // the final open between canonicalize and open, even when the last component is regular.
        for component in canonical.components() {
            let std::path::Component::Normal(name) = component else {
                continue;
            };
            use std::os::unix::ffi::OsStrExt;
            let name =
                std::ffi::CString::new(name.as_bytes()).map_err(|_| FileError::InvalidPath)?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io_error(std::io::Error::last_os_error()));
            }
            directory = unsafe { File::from_raw_fd(fd) };
        }
        Ok(Self {
            canonical,
            directory,
        })
    }
    pub fn canonical(&self) -> &Path {
        &self.canonical
    }
    pub fn read(&self, path: &[u8]) -> Result<FileDocument, FileError> {
        let mut file = self.open_relative(path, false)?;
        let before = file.metadata().map_err(io_error)?;
        if !before.is_file() {
            return Err(FileError::NotRegularFile);
        }
        if before.len() > FILE_BYTES_MAX as u64 {
            return Err(FileError::TooLarge);
        }
        let mut content = Vec::with_capacity(before.len() as usize);
        (&mut file)
            .take(FILE_BYTES_MAX as u64 + 1)
            .read_to_end(&mut content)
            .map_err(io_error)?;
        if content.len() > FILE_BYTES_MAX {
            return Err(FileError::TooLarge);
        }
        let after = file.metadata().map_err(io_error)?;
        if metadata_stamp(&before) != metadata_stamp(&after) || content.len() as u64 != after.len()
        {
            return Err(FileError::Changed);
        }
        Ok(FileDocument {
            revision: revision(&after, &content),
            content,
        })
    }
    pub fn write_if_version(
        &self,
        path: &[u8],
        expected: &FileRevision,
        content: &[u8],
    ) -> Result<FileRevision, FileError> {
        if content.len() > FILE_BYTES_MAX {
            return Err(FileError::TooLarge);
        }
        let parts = components(path)?;
        let (name, parents) = parts.split_last().ok_or(FileError::InvalidPath)?;
        let directory = self.open_components(parents, true)?;
        if self.read(path)?.revision != *expected {
            return Err(FileError::Changed);
        }
        let name = std::ffi::CString::new(*name).map_err(|_| FileError::InvalidPath)?;
        let temp_name =
            std::ffi::CString::new(format!(".deppy-save-{}", uuid::Uuid::new_v4())).unwrap();
        // Every parent is an already opened non-symlink directory; path replacement cannot
        // redirect the temporary file or rename to another root.
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                temp_name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        let mut temporary = unsafe { File::from_raw_fd(descriptor) };
        let result = (|| {
            let original = self.open_relative(path, false)?;
            let mode = original.metadata().map_err(io_error)?.mode() & 0o777;
            use std::os::unix::fs::PermissionsExt;
            temporary
                .set_permissions(std::fs::Permissions::from_mode(mode))
                .map_err(io_error)?;
            temporary.write_all(content).map_err(io_error)?;
            temporary.sync_all().map_err(io_error)?;
            if self.read(path)?.revision != *expected {
                return Err(FileError::Changed);
            }
            if unsafe {
                libc::renameat(
                    directory.as_raw_fd(),
                    temp_name.as_ptr(),
                    directory.as_raw_fd(),
                    name.as_ptr(),
                )
            } != 0
            {
                return Err(io_error(std::io::Error::last_os_error()));
            }
            directory.sync_all().map_err(io_error)?;
            self.read(path).map(|document| document.revision)
        })();
        // No-op after successful rename; removes only this request's exclusive temporary file.
        unsafe {
            libc::unlinkat(directory.as_raw_fd(), temp_name.as_ptr(), 0);
        }
        result
    }
    pub fn list(
        &self,
        path: &[u8],
        cursor: Option<&DirectoryCursor>,
        limit: usize,
    ) -> Result<DirectoryPage, FileError> {
        if limit == 0 || limit > DIRECTORY_PAGE_MAX {
            return Err(FileError::TooLarge);
        }
        let directory = self.open_relative(path, true)?;
        let stamp = directory_stamp(&directory.metadata().map_err(io_error)?);
        let offset = cursor.map_or(0, |cursor| cursor.offset as usize);
        if offset > 65_536 {
            return Err(FileError::TooLarge);
        }
        let mut iterator = DirectoryReader::open(&directory)?;
        let mut entries = Vec::with_capacity(limit);
        let mut digest = Sha256::new();
        digest.update(stamp.as_bytes());
        let mut count = 0usize;
        let mut name_bytes = 0usize;
        // Some remote filesystems expose coarse directory timestamps. Fingerprint the
        // bounded name/type stream too, so a new entry cannot reuse an old page cursor.
        while let Some(entry) = iterator.next()? {
            count += 1;
            name_bytes = name_bytes.saturating_add(entry.name.len());
            if count > 65_536 || name_bytes > 4 * 1024 * 1024 {
                return Err(FileError::TooLarge);
            }
            digest.update((entry.name.len() as u64).to_le_bytes());
            digest.update(&entry.name);
            digest.update([u8::from(entry.is_directory), u8::from(entry.is_symlink)]);
            if count > offset && entries.len() < limit {
                entries.push(entry);
            }
        }
        if directory_stamp(&directory.metadata().map_err(io_error)?) != stamp {
            return Err(FileError::Changed);
        }
        let revision: String = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if offset > count || cursor.is_some_and(|cursor| cursor.revision != revision) {
            return Err(FileError::Changed);
        }
        let more = offset + entries.len() < count;
        let next = more.then(|| DirectoryCursor {
            offset: (offset + entries.len()) as u32,
            revision,
        });
        Ok(DirectoryPage { entries, next })
    }
    fn open_relative(&self, path: &[u8], directory: bool) -> Result<File, FileError> {
        let parts = components(path)?;
        self.open_components(&parts, directory)
    }
    fn open_components(&self, parts: &[&[u8]], directory: bool) -> Result<File, FileError> {
        let mut parent = self.directory.try_clone().map_err(io_error)?;
        for (index, part) in parts.iter().enumerate() {
            let part = std::ffi::CString::new(*part).map_err(|_| FileError::InvalidPath)?;
            let is_directory = directory || index + 1 != parts.len();
            let flags = libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK
                | if is_directory { libc::O_DIRECTORY } else { 0 };
            let descriptor = unsafe { libc::openat(parent.as_raw_fd(), part.as_ptr(), flags) };
            if descriptor < 0 {
                return Err(io_error(std::io::Error::last_os_error()));
            }
            parent = unsafe { File::from_raw_fd(descriptor) };
        }
        Ok(parent)
    }
}

struct DirectoryReader(*mut libc::DIR);
impl DirectoryReader {
    fn open(directory: &File) -> Result<Self, FileError> {
        // A fresh open description starts at offset zero. dup/try_clone would share the
        // directory cursor and make a second page silently skip entries.
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        let stream = unsafe { libc::fdopendir(descriptor) };
        if stream.is_null() {
            unsafe {
                libc::close(descriptor);
            }
            return Err(io_error(std::io::Error::last_os_error()));
        }
        Ok(Self(stream))
    }
    fn next(&mut self) -> Result<Option<DirectoryEntry>, FileError> {
        loop {
            clear_errno();
            let entry = unsafe { libc::readdir(self.0) };
            if entry.is_null() {
                let error = std::io::Error::last_os_error();
                return if error.raw_os_error() == Some(0) {
                    Ok(None)
                } else {
                    Err(io_error(error))
                };
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            if name.to_bytes().len() > 4096 {
                return Err(FileError::TooLarge);
            }
            let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::fstatat(
                    libc::dirfd(self.0),
                    name.as_ptr(),
                    &mut metadata,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(io_error(std::io::Error::last_os_error()));
            }
            return Ok(Some(DirectoryEntry {
                name: name.to_bytes().to_vec(),
                is_directory: metadata.st_mode & libc::S_IFMT == libc::S_IFDIR,
                is_symlink: metadata.st_mode & libc::S_IFMT == libc::S_IFLNK,
            }));
        }
    }
}
impl Drop for DirectoryReader {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}
fn clear_errno() {
    #[cfg(target_os = "macos")]
    unsafe {
        *libc::__error() = 0;
    }
    #[cfg(target_os = "linux")]
    unsafe {
        *libc::__errno_location() = 0;
    }
}

use std::os::unix::fs::OpenOptionsExt;
pub(crate) fn components(path: &[u8]) -> Result<Vec<&[u8]>, FileError> {
    if path.len() > 4096 || path.starts_with(b"/") || path.contains(&0) {
        return Err(FileError::InvalidPath);
    }
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let parts: Vec<_> = path.split(|byte| *byte == b'/').collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == b"." || *part == b"..")
    {
        return Err(FileError::InvalidPath);
    }
    Ok(parts)
}
fn io_error(error: std::io::Error) -> FileError {
    match error.raw_os_error() {
        Some(libc::ELOOP) => FileError::OutsideRoot,
        Some(libc::ENOTDIR) => FileError::NotDirectory,
        _ => match error.kind() {
            std::io::ErrorKind::NotFound => FileError::NotFound,
            std::io::ErrorKind::PermissionDenied => FileError::PermissionDenied,
            _ => FileError::Io,
        },
    }
}
fn metadata_stamp(metadata: &std::fs::Metadata) -> (u64, u64, u64, i64, i64) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    )
}
fn directory_stamp(metadata: &std::fs::Metadata) -> String {
    format!("{:?}", metadata_stamp(metadata))
}
fn revision(metadata: &std::fs::Metadata, content: &[u8]) -> FileRevision {
    let digest = Sha256::digest(content);
    let sha256 = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    FileRevision {
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        sha256,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("deppy-remote-files-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn host(&self) -> FileRoot {
            FileRoot::open(&self.0).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn files_read_preserves_korean_spaces_quotes_and_raw_unix_names() {
        let fixture = Fixture::new();
        let mut paths = vec!["한글 문서 '인용'.md".as_bytes().to_vec()];
        // APFS refuses invalid UTF-8 filenames itself; Linux supports them and must preserve
        // their exact bytes instead of substituting a lossy display name in an operation.
        if cfg!(target_os = "linux") {
            paths.push(vec![b'r', 0xff, b'w']);
        }
        for path in paths {
            std::fs::write(
                fixture.0.join(std::ffi::OsString::from_vec(path.clone())),
                b"hello",
            )
            .unwrap();
            assert_eq!(fixture.host().read(&path).unwrap().content, b"hello");
        }
    }

    #[test]
    fn files_reject_traversal_absolute_and_symlink_escape() {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        std::fs::write(outside.0.join("secret"), b"private").unwrap();
        std::os::unix::fs::symlink(&outside.0, fixture.0.join("escape")).unwrap();
        for path in [
            b"../secret".as_slice(),
            b"/etc/passwd",
            b"escape/secret",
            b"a/../secret",
            b"a\0b",
        ] {
            assert!(fixture.host().read(path).is_err(), "unsafe path admitted");
        }
    }

    #[test]
    fn files_write_compares_revision_and_preserves_external_edit() {
        let fixture = Fixture::new();
        let path = fixture.0.join("note.md");
        std::fs::write(&path, b"one").unwrap();
        let host = fixture.host();
        let revision = host.read(b"note.md").unwrap().revision;
        let next = host
            .write_if_version(b"note.md", &revision, b"two")
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_ne!(revision, next);
        std::fs::write(&path, b"external").unwrap();
        assert_eq!(
            host.write_if_version(b"note.md", &next, b"lost"),
            Err(FileError::Changed)
        );
        assert_eq!(std::fs::read(path).unwrap(), b"external");
    }

    #[test]
    fn files_listing_is_paged_and_invalidates_changed_directory() {
        let fixture = Fixture::new();
        for name in ["a", "b", "c"] {
            std::fs::write(fixture.0.join(name), b"").unwrap();
        }
        let host = fixture.host();
        let first = host.list(b"", None, 2).unwrap();
        assert_eq!(first.entries.len(), 2);
        let cursor = first.next.unwrap();
        let second = host.list(b"", Some(&cursor), 2).unwrap();
        assert_eq!(second.entries.len(), 1);
        assert!(second.next.is_none());
        std::fs::write(fixture.0.join("d"), b"").unwrap();
        assert_eq!(host.list(b"", Some(&cursor), 2), Err(FileError::Changed));
    }

    #[test]
    fn files_reject_large_and_special_files_without_blocking() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("large"), vec![0; FILE_BYTES_MAX + 1]).unwrap();
        assert_eq!(fixture.host().read(b"large"), Err(FileError::TooLarge));
        assert_eq!(fixture.host().read(b""), Err(FileError::NotRegularFile));
    }
}
