//! Pure, bounded projections between the SSH service protocol and app IO values.
//! Paths remain remote POSIX bytes; this module never touches the local filesystem.
use crate::{
    document_io::*,
    ui::{diff_viewer, file_tree::*, git_panel::*},
};
use deppy_core::workspace_target::SshWorkspaceTarget;
use deppy_remote_host::{
    files,
    services::{self, Reply, Request, Response},
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    ffi::OsString,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteProviderError {
    InvalidPath,
    OutsideRoot,
    TooLarge,
    InvalidResponse,
    Stale,
    Disconnected,
    Unsupported,
    TargetMismatch,
    PermissionDenied,
}
impl std::fmt::Display for RemoteProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidPath => "ssh_remote_path_invalid",
            Self::OutsideRoot => "ssh_remote_path_outside_root",
            Self::TooLarge => "ssh_remote_response_too_large",
            Self::InvalidResponse => "ssh_remote_response_invalid",
            Self::Stale => "ssh_remote_response_stale",
            Self::Disconnected => "ssh_remote_disconnected",
            Self::Unsupported => "ssh_remote_operation_unsupported",
            Self::TargetMismatch => "ssh_remote_target_mismatch",
            Self::PermissionDenied => "ssh_remote_permission_denied",
        })
    }
}
impl std::error::Error for RemoteProviderError {}
#[derive(Clone)]
pub struct RemoteWorkspaceFiles {
    target: SshWorkspaceTarget,
}
pub struct RemoteReplyFence {
    identity: runtime::SshTargetIdentity,
    runtime_instance: String,
    generation: u64,
    request_id: u64,
}
pub struct RemoteDirectoryListing {
    path: Vec<u8>,
    cursor: Option<files::DirectoryCursor>,
    items: Vec<FileTreeListingItem>,
    names: HashSet<Vec<u8>>,
    bytes: usize,
    max_items: usize,
    max_bytes: usize,
    done: bool,
}
pub enum RemoteListingProgress {
    Next(Request),
    Complete(FileTreeListingSnapshot),
}
impl RemoteWorkspaceFiles {
    pub fn new(target: SshWorkspaceTarget) -> Result<Self, RemoteProviderError> {
        target
            .validate()
            .map_err(|_| RemoteProviderError::TargetMismatch)?;
        Ok(Self { target })
    }
    pub fn target(&self) -> &SshWorkspaceTarget {
        &self.target
    }
    /// Lexical containment preserves Unix bytes rather than display labels.
    pub fn relative_path(&self, path: &Path) -> Result<Vec<u8>, RemoteProviderError> {
        let bytes = path.as_os_str().as_encoded_bytes();
        if bytes.len() > 4096 || bytes.contains(&0) || !bytes.starts_with(b"/") {
            return Err(RemoteProviderError::InvalidPath);
        }
        let root = self.target.canonical_root.as_str().as_bytes();
        let relative = if bytes == root {
            &[][..]
        } else if root == b"/" {
            &bytes[1..]
        } else {
            bytes
                .strip_prefix(root)
                .and_then(|tail| tail.strip_prefix(b"/"))
                .ok_or(RemoteProviderError::OutsideRoot)?
        };
        validate_relative(relative, true)?;
        Ok(relative.to_vec())
    }
    pub fn read_request(&self, path: &Path) -> Result<Request, RemoteProviderError> {
        let path = self.relative_path(path)?;
        validate_relative(&path, false)?;
        Ok(Request::ReadFile { path })
    }
    pub fn write_request(
        &self,
        request: &DocumentSaveRequest,
    ) -> Result<Request, RemoteProviderError> {
        let path = self.relative_path(&request.path)?;
        validate_relative(&path, false)?;
        let DocumentRevision::Remote { scope, revision } = request.expected_revision else {
            return Err(RemoteProviderError::TargetMismatch);
        };
        if scope != self.document_scope(&path) {
            return Err(RemoteProviderError::TargetMismatch);
        }
        if request.contents.len() > files::FILE_BYTES_MAX
            || revision.length > files::FILE_BYTES_MAX as u64
        {
            return Err(RemoteProviderError::TooLarge);
        }
        if !(0..1_000_000_000).contains(&revision.modified_nanoseconds) {
            return Err(RemoteProviderError::InvalidResponse);
        }
        Ok(Request::WriteFileIfVersion {
            path,
            expected: files::FileRevision {
                device: revision.device,
                inode: revision.inode,
                length: revision.length,
                modified_seconds: revision.modified_seconds,
                modified_nanoseconds: revision.modified_nanoseconds,
                sha256: hex(&revision.sha256),
            },
            content: request.contents.as_bytes().to_vec(),
        })
    }
    pub fn load_response(&self, path: &Path, reply: Reply) -> DocumentLoadOutcome {
        let result = (|| {
            let relative = self
                .relative_path(path)
                .map_err(|_| DocumentIoErrorCode::TargetMismatch)?;
            validate_relative(&relative, false).map_err(|_| DocumentIoErrorCode::TargetMismatch)?;
            let document = match reply {
                Ok(Response::File(document)) => document,
                Err(error) => return Err(document_error(error)),
                _ => return Err(DocumentIoErrorCode::ReadFailed),
            };
            let revision = decode_revision(&document.revision)
                .map_err(|_| DocumentIoErrorCode::MetadataFailed)?;
            if document.content.len() > files::FILE_BYTES_MAX {
                return Err(DocumentIoErrorCode::ContentTooLarge);
            }
            if document.content.len() as u64 != revision.length
                || digest(&document.content) != revision.sha256
            {
                return Err(DocumentIoErrorCode::Changed);
            }
            let byte_len = document.content.len() as u64;
            let revision = DocumentRevision::Remote {
                scope: self.document_scope(&relative),
                revision,
            };
            Ok(match String::from_utf8(document.content) {
                Ok(source) => DocumentLoadOutcome::Loaded { source, revision },
                Err(_) => DocumentLoadOutcome::Binary { byte_len },
            })
        })();
        result.unwrap_or_else(|code| DocumentLoadOutcome::Failed { code })
    }
    pub fn save_response(&self, path: &Path, reply: Reply) -> DocumentSaveOutcome {
        let result = (|| {
            let relative = self
                .relative_path(path)
                .map_err(|_| DocumentIoErrorCode::TargetMismatch)?;
            validate_relative(&relative, false).map_err(|_| DocumentIoErrorCode::TargetMismatch)?;
            match reply {
                Ok(Response::Written(revision)) => Ok(DocumentSaveOutcome::Saved {
                    revision: DocumentRevision::Remote {
                        scope: self.document_scope(&relative),
                        revision: decode_revision(&revision)
                            .map_err(|_| DocumentIoErrorCode::MetadataFailed)?,
                    },
                }),
                Err(services::ServiceError::Files(
                    files::FileError::Changed | files::FileError::NotFound,
                )) => Ok(DocumentSaveOutcome::Conflict),
                Err(error) => Err(document_error(error)),
                _ => Err(DocumentIoErrorCode::ReplaceFailed),
            }
        })();
        result.unwrap_or_else(|code| DocumentSaveOutcome::Failed { code })
    }
    pub fn directory(
        &self,
        path: &Path,
        max_items: usize,
        max_bytes: usize,
    ) -> Result<RemoteDirectoryListing, RemoteProviderError> {
        if max_items == 0
            || max_bytes == 0
            || max_items > FILE_TREE_LISTING_MAX_ITEMS
            || max_bytes > FILE_TREE_LISTING_MAX_BYTES
        {
            return Err(RemoteProviderError::TooLarge);
        }
        Ok(RemoteDirectoryListing {
            path: self.relative_path(path)?,
            cursor: None,
            items: Vec::new(),
            names: HashSet::new(),
            bytes: 0,
            max_items,
            max_bytes,
            done: false,
        })
    }
    pub fn reply_fence(
        &self,
        runtime_instance: String,
        generation: u64,
        request_id: u64,
    ) -> RemoteReplyFence {
        RemoteReplyFence {
            identity: runtime::SshTargetIdentity {
                remote_workspace_id: self.target.remote_workspace_id.clone(),
                account: self.target.account.clone(),
                canonical_root: self.target.canonical_root.as_str().into(),
                host_key_sha256: self.target.host_key_sha256.clone(),
            },
            runtime_instance,
            generation,
            request_id,
        }
    }
    fn document_scope(&self, path: &[u8]) -> [u8; 32] {
        let mut hash = Sha256::new();
        for field in [
            self.target.host.as_bytes(),
            &self.target.port.to_be_bytes(),
            self.target.account.as_bytes(),
            self.target.host_key_sha256.as_bytes(),
            self.target.remote_workspace_id.as_bytes(),
            self.target.canonical_root.as_str().as_bytes(),
            path,
        ] {
            hash.update((field.len() as u64).to_be_bytes());
            hash.update(field);
        }
        let mut bytes = [0; 32];
        bytes.copy_from_slice(&hash.finalize());
        bytes
    }
    pub fn git_request(&self, request: &GitPanelIoRequest) -> Result<Request, RemoteProviderError> {
        match request {
            GitPanelIoRequest::Snapshot => Ok(Request::GitStatus),
            GitPanelIoRequest::FileDiff {
                rel_path,
                mode: diff_viewer::DiffMode::Working,
            } => {
                validate_relative(rel_path.as_bytes(), false)?;
                Ok(Request::GitDiff {
                    path: Some(rel_path.as_bytes().to_vec()),
                    staged: false,
                })
            }
            _ => Err(RemoteProviderError::Unsupported),
        }
    }
    pub fn git_response(&self, request: &GitPanelIoRequest, reply: Reply) -> GitPanelIoResult {
        let bytes = match reply {
            Ok(Response::Git(bytes)) if bytes.len() <= deppy_remote_host::git::GIT_BYTES_MAX => {
                Ok(bytes)
            }
            Err(services::ServiceError::Git(deppy_remote_host::git::GitError::NotRepository)) => {
                Err(GitPanelErrorCode::NoRepo)
            }
            _ => Err(GitPanelErrorCode::CollectionFailed),
        };
        match request {
            GitPanelIoRequest::Snapshot => GitPanelIoResult::Snapshot(bytes.and_then(|bytes| {
                let (changes, changes_truncated) =
                    parse_status(&bytes).map_err(|_| GitPanelErrorCode::CollectionFailed)?;
                Ok(GitPanelSnapshot {
                    repo_root: PathBuf::from(self.target.canonical_root.as_str()),
                    changes,
                    changes_truncated,
                    ..Default::default()
                })
            })),
            GitPanelIoRequest::FileDiff {
                mode: diff_viewer::DiffMode::Working,
                ..
            } => GitPanelIoResult::FileDiff(bytes.and_then(|bytes| {
                let text =
                    std::str::from_utf8(&bytes).map_err(|_| GitPanelErrorCode::CollectionFailed)?;
                Ok(diff_viewer::parse_unified(text, false))
            })),
            GitPanelIoRequest::FileDiff { .. } => {
                GitPanelIoResult::FileDiff(Err(GitPanelErrorCode::CollectionFailed))
            }
            GitPanelIoRequest::CommitFiles { sha } => GitPanelIoResult::CommitFiles {
                sha: *sha,
                result: Err(GitPanelErrorCode::CollectionFailed),
            },
        }
    }
}
impl RemoteReplyFence {
    pub fn decode(&self, reply: runtime::SshRpcReply) -> Result<Reply, RemoteProviderError> {
        if self.generation == 0
            || self.request_id == 0
            || self.runtime_instance.is_empty()
            || self.runtime_instance.len() > 256
            || self.identity != reply.context.identity
            || self.runtime_instance != reply.context.runtime_instance
            || self.generation != reply.context.connection_generation
            || self.request_id != reply.context.request_id
        {
            return Err(RemoteProviderError::Stale);
        }
        let bytes = reply
            .result
            .map_err(|_| RemoteProviderError::Disconnected)?;
        if bytes.len() > runtime::SSH_RPC_BYTES_MAX {
            return Err(RemoteProviderError::TooLarge);
        }
        serde_json::from_slice(&bytes).map_err(|_| RemoteProviderError::InvalidResponse)
    }
}
impl RemoteDirectoryListing {
    pub fn request(&self) -> Request {
        Request::ListDirectory {
            path: self.path.clone(),
            cursor: self.cursor.clone(),
            limit: (self
                .max_items
                .saturating_sub(self.items.len())
                .min(files::DIRECTORY_PAGE_MAX)) as u16,
        }
    }
    pub fn accept(&mut self, reply: Reply) -> Result<RemoteListingProgress, RemoteProviderError> {
        let result = self.accept_inner(reply);
        if result.is_err() {
            self.done = true;
        }
        result
    }
    fn accept_inner(&mut self, reply: Reply) -> Result<RemoteListingProgress, RemoteProviderError> {
        if self.done {
            return Err(RemoteProviderError::Stale);
        }
        let page = match reply {
            Ok(Response::Directory(page)) => page,
            Err(services::ServiceError::Files(files::FileError::PermissionDenied)) => {
                return Err(RemoteProviderError::PermissionDenied);
            }
            Err(
                services::ServiceError::TooLarge
                | services::ServiceError::Files(files::FileError::TooLarge),
            ) => return Err(RemoteProviderError::TooLarge),
            _ => return Err(RemoteProviderError::InvalidResponse),
        };
        if page.entries.len() > files::DIRECTORY_PAGE_MAX
            || page.entries.len() > self.max_items.saturating_sub(self.items.len())
        {
            return Err(RemoteProviderError::TooLarge);
        }
        let total = self.items.len() + page.entries.len();
        if let Some(next) = &page.next
            && (page.entries.is_empty()
                || next.offset as usize != total
                || total >= self.max_items
                || decode_hex(&next.revision).is_err()
                || self
                    .cursor
                    .as_ref()
                    .is_some_and(|previous| previous.revision != next.revision))
        {
            return Err(RemoteProviderError::InvalidResponse);
        }
        for entry in page.entries {
            if entry.name.is_empty()
                || entry.name.contains(&b'/')
                || entry.name == b"."
                || entry.name == b".."
            {
                return Err(RemoteProviderError::InvalidPath);
            }
            self.bytes = self
                .bytes
                .checked_add(entry.name.len())
                .filter(|bytes| *bytes <= self.max_bytes)
                .ok_or(RemoteProviderError::TooLarge)?;
            if !self.names.insert(entry.name.clone()) {
                return Err(RemoteProviderError::InvalidResponse);
            }
            self.items.push(
                FileTreeListingItem::try_new(
                    OsString::from_vec(entry.name),
                    entry.is_directory && !entry.is_symlink,
                )
                .map_err(|_| RemoteProviderError::InvalidResponse)?,
            );
        }
        self.cursor = page.next;
        if self.cursor.is_some() {
            Ok(RemoteListingProgress::Next(self.request()))
        } else {
            self.done = true;
            let snapshot = FileTreeListingSnapshot::try_new(std::mem::take(&mut self.items))
                .map_err(|_| RemoteProviderError::TooLarge)?;
            self.names.clear();
            Ok(RemoteListingProgress::Complete(snapshot))
        }
    }
}
pub fn maintenance_error(error: RemoteProviderError) -> FileTreeMaintenanceErrorCode {
    match error {
        RemoteProviderError::PermissionDenied => FileTreeMaintenanceErrorCode::PermissionDenied,
        RemoteProviderError::TooLarge => FileTreeMaintenanceErrorCode::ListingTooLarge,
        RemoteProviderError::InvalidPath
        | RemoteProviderError::OutsideRoot
        | RemoteProviderError::InvalidResponse
        | RemoteProviderError::TargetMismatch => FileTreeMaintenanceErrorCode::InvalidSnapshot,
        _ => FileTreeMaintenanceErrorCode::NativeFailure,
    }
}
fn validate_relative(path: &[u8], empty: bool) -> Result<(), RemoteProviderError> {
    if path.is_empty() {
        return if empty {
            Ok(())
        } else {
            Err(RemoteProviderError::InvalidPath)
        };
    }
    if path.len() > 4096
        || path.contains(&0)
        || path
            .split(|byte| *byte == b'/')
            .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return Err(RemoteProviderError::InvalidPath);
    }
    Ok(())
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut digest = [0; 32];
    digest.copy_from_slice(&Sha256::digest(bytes));
    digest
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn decode_hex(value: &str) -> Result<[u8; 32], RemoteProviderError> {
    let bytes = value.as_bytes();
    if bytes.len() != 64
        || !bytes
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
    {
        return Err(RemoteProviderError::InvalidResponse);
    }
    let nibble = |byte: u8| {
        if byte <= b'9' {
            byte - b'0'
        } else {
            byte - b'a' + 10
        }
    };
    let mut output = [0; 32];
    for (out, pair) in output.iter_mut().zip(bytes.chunks_exact(2)) {
        *out = nibble(pair[0]) * 16 + nibble(pair[1]);
    }
    Ok(output)
}
fn decode_revision(
    revision: &files::FileRevision,
) -> Result<RemoteDocumentRevision, RemoteProviderError> {
    if revision.length > files::FILE_BYTES_MAX as u64
        || !(0..1_000_000_000).contains(&revision.modified_nanoseconds)
    {
        return Err(RemoteProviderError::InvalidResponse);
    }
    Ok(RemoteDocumentRevision {
        device: revision.device,
        inode: revision.inode,
        length: revision.length,
        modified_seconds: revision.modified_seconds,
        modified_nanoseconds: revision.modified_nanoseconds,
        sha256: decode_hex(&revision.sha256)?,
    })
}
fn document_error(error: services::ServiceError) -> DocumentIoErrorCode {
    match error {
        services::ServiceError::TargetMismatch
        | services::ServiceError::Files(
            files::FileError::OutsideRoot | files::FileError::InvalidPath,
        ) => DocumentIoErrorCode::TargetMismatch,
        services::ServiceError::Files(files::FileError::NotFound) => DocumentIoErrorCode::NotFound,
        services::ServiceError::Files(
            files::FileError::NotDirectory | files::FileError::NotRegularFile,
        ) => DocumentIoErrorCode::FileTypeInvalid,
        services::ServiceError::TooLarge
        | services::ServiceError::Files(files::FileError::TooLarge) => {
            DocumentIoErrorCode::ContentTooLarge
        }
        services::ServiceError::Files(files::FileError::Changed) => DocumentIoErrorCode::Changed,
        _ => DocumentIoErrorCode::ReadFailed,
    }
}
fn parse_status(bytes: &[u8]) -> Result<(Vec<GitFileRow>, bool), RemoteProviderError> {
    if !bytes.is_empty() && !bytes.ends_with(&[0]) {
        return Err(RemoteProviderError::InvalidResponse);
    }
    let mut fields = bytes.split(|byte| *byte == 0).peekable();
    let mut rows = Vec::new();
    let mut paths = HashSet::new();
    let mut truncated = false;
    while let Some(field) = fields.next() {
        if field.is_empty() {
            if fields.peek().is_none() {
                break;
            }
            return Err(RemoteProviderError::InvalidResponse);
        }
        if field.len() < 4
            || field[2] != b' '
            || !field[..2].iter().all(|byte| b" MTADRCU?!".contains(byte))
        {
            return Err(RemoteProviderError::InvalidResponse);
        }
        let (x, y) = (field[0], field[1]);
        let path = &field[3..];
        validate_relative(path, false)?;
        if [x, y].iter().any(|byte| matches!(*byte, b'R' | b'C')) {
            validate_relative(
                fields.next().ok_or(RemoteProviderError::InvalidResponse)?,
                false,
            )?;
        }
        let rel_path = std::str::from_utf8(path).map_err(|_| RemoteProviderError::Unsupported)?;
        if !paths.insert(path.to_vec()) {
            return Err(RemoteProviderError::InvalidResponse);
        }
        if rows.len() >= MAX_PANEL_FILES {
            truncated = true;
            continue;
        }
        let status = if matches!(
            (x, y),
            (b'D', b'D')
                | (b'A', b'U')
                | (b'U', b'D')
                | (b'U', b'A')
                | (b'D', b'U')
                | (b'A', b'A')
                | (b'U', b'U')
        ) {
            b'U'
        } else if x == b'?' {
            b'?'
        } else if y != b' ' {
            y
        } else {
            x
        };
        rows.push(GitFileRow {
            rel_path: rel_path.into(),
            status: status as char,
            adds: None,
            dels: None,
        });
    }
    Ok((rows, truncated))
}
#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{
        os::unix::ffi::{OsStrExt, OsStringExt},
        path::PathBuf,
    };
    fn provider() -> RemoteWorkspaceFiles {
        RemoteWorkspaceFiles::new(
            SshWorkspaceTarget::new(
                "connection".into(),
                "example.test".into(),
                22,
                "deploy".into(),
                format!("SHA256:{}", "a".repeat(43)),
                "workspace".into(),
                deppy_core::workspace_target::RemotePath::new("/remote/project").unwrap(),
            )
            .unwrap(),
        )
        .unwrap()
    }
    fn revision(content: &[u8]) -> files::FileRevision {
        files::FileRevision {
            device: 1,
            inode: 2,
            length: content.len() as u64,
            modified_seconds: 3,
            modified_nanoseconds: 4,
            sha256: Sha256::digest(content)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        }
    }
    #[test]
    fn remote_paths_are_lexical_bounded_and_preserve_raw_names() {
        let provider = provider();
        let raw = PathBuf::from(std::ffi::OsString::from_vec(
            b"/remote/project/odd-\xff.txt".to_vec(),
        ));
        assert_eq!(provider.relative_path(&raw), Ok(b"odd-\xff.txt".to_vec()));
        assert_eq!(
            provider.relative_path(Path::new("/remote/project")),
            Ok(vec![])
        );
        for bad in [
            "/remote/project-other/file",
            "/etc/passwd",
            "/remote/project/../escape",
            "relative",
            "/remote/project/a//b",
            "/remote/project/./a",
        ] {
            assert!(provider.relative_path(Path::new(bad)).is_err());
        }
    }
    #[test]
    fn remote_directory_pages_preserve_raw_identity_and_enforce_cursors_and_bounds() {
        let provider = provider();
        let mut listing = provider
            .directory(Path::new("/remote/project"), 4, 100)
            .unwrap();
        let next = files::DirectoryCursor {
            offset: 1,
            revision: "a".repeat(64),
        };
        assert!(matches!(
            listing
                .accept(Ok(Response::Directory(files::DirectoryPage {
                    entries: vec![files::DirectoryEntry {
                        name: b"odd-\xff".to_vec(),
                        is_directory: false,
                        is_symlink: false
                    }],
                    next: Some(next.clone())
                })))
                .unwrap(),
            RemoteListingProgress::Next(Request::ListDirectory {
                cursor: Some(_),
                ..
            })
        ));
        let RemoteListingProgress::Complete(snapshot) = listing
            .accept(Ok(Response::Directory(files::DirectoryPage {
                entries: vec![files::DirectoryEntry {
                    name: b"dir".to_vec(),
                    is_directory: true,
                    is_symlink: false,
                }],
                next: None,
            })))
            .unwrap()
        else {
            panic!("not complete")
        };
        assert!(snapshot.items()[0].is_dir());
        assert_eq!(snapshot.items()[1].name().as_bytes(), b"odd-\xff");
        let mut listing = provider
            .directory(Path::new("/remote/project"), 1, 100)
            .unwrap();
        assert!(
            listing
                .accept(Ok(Response::Directory(files::DirectoryPage {
                    entries: vec![files::DirectoryEntry {
                        name: b"../escape".to_vec(),
                        is_directory: true,
                        is_symlink: false
                    }],
                    next: None
                })))
                .is_err()
        );
        let mut listing = provider
            .directory(Path::new("/remote/project"), 1, 100)
            .unwrap();
        assert!(
            listing
                .accept(Ok(Response::Directory(files::DirectoryPage {
                    entries: vec![files::DirectoryEntry {
                        name: b"one".to_vec(),
                        is_directory: false,
                        is_symlink: false
                    }],
                    next: Some(files::DirectoryCursor {
                        offset: 0,
                        revision: "a".repeat(64)
                    })
                })))
                .is_err()
        );
    }
    #[test]
    fn remote_documents_roundtrip_only_their_bound_revision_and_verified_content() {
        let provider = provider();
        let path = Path::new("/remote/project/document.md");
        let content = b"remote document";
        let expected = revision(content);
        let DocumentLoadOutcome::Loaded { source, revision } = provider.load_response(
            path,
            Ok(Response::File(files::FileDocument {
                content: content.to_vec(),
                revision: expected.clone(),
            })),
        ) else {
            panic!("remote load")
        };
        assert_eq!(source, "remote document");
        assert!(matches!(revision, DocumentRevision::Remote { .. }));
        let request = DocumentSaveRequest {
            path: path.into(),
            contents: "changed".into(),
            expected_revision: revision,
        };
        let Request::WriteFileIfVersion {
            path,
            expected: actual,
            content,
        } = provider.write_request(&request).unwrap()
        else {
            panic!("write request")
        };
        assert_eq!(path, b"document.md");
        assert_eq!(actual, expected);
        assert_eq!(content, b"changed");
        let mut moved = request.clone();
        moved.path = PathBuf::from("/remote/project/other.md");
        assert!(provider.write_request(&moved).is_err());
        assert!(matches!(
            provider.load_response(
                &request.path,
                Ok(Response::File(files::FileDocument {
                    content: b"tampered".to_vec(),
                    revision: expected
                }))
            ),
            DocumentLoadOutcome::Failed { .. }
        ));
        assert!(matches!(
            provider.save_response(
                &request.path,
                Err(services::ServiceError::Files(files::FileError::Changed))
            ),
            DocumentSaveOutcome::Conflict
        ));
    }
    #[test]
    fn remote_rpc_fence_rejects_old_generations_instances_targets_and_requests() {
        let provider = provider();
        let fence = provider.reply_fence("runtime".into(), 4, 8);
        let context = runtime::SshRequestContext {
            identity: runtime::SshTargetIdentity {
                remote_workspace_id: "workspace".into(),
                account: "deploy".into(),
                canonical_root: "/remote/project".into(),
                host_key_sha256: format!("SHA256:{}", "a".repeat(43)),
            },
            runtime_instance: "runtime".into(),
            connection_generation: 4,
            request_id: 8,
        };
        let bytes = serde_json::to_vec(&Reply::Ok(Response::Git(vec![]))).unwrap();
        assert!(
            fence
                .decode(runtime::SshRpcReply {
                    context: context.clone(),
                    result: Ok(bytes.clone())
                })
                .is_ok()
        );
        let mut bad = context.clone();
        bad.connection_generation = 3;
        assert_eq!(
            fence.decode(runtime::SshRpcReply {
                context: bad,
                result: Ok(bytes.clone())
            }),
            Err(RemoteProviderError::Stale)
        );
        let mut bad = context.clone();
        bad.identity.canonical_root = "/other".into();
        assert_eq!(
            fence.decode(runtime::SshRpcReply {
                context: bad,
                result: Ok(bytes.clone())
            }),
            Err(RemoteProviderError::Stale)
        );
        let mut bad = context.clone();
        bad.runtime_instance = "other".into();
        assert!(
            fence
                .decode(runtime::SshRpcReply {
                    context: bad,
                    result: Ok(bytes.clone())
                })
                .is_err()
        );
        let mut bad = context;
        bad.request_id = 7;
        assert!(
            fence
                .decode(runtime::SshRpcReply {
                    context: bad,
                    result: Ok(bytes)
                })
                .is_err()
        );
    }
    #[test]
    fn remote_git_status_and_diff_project_without_running_local_git() {
        let provider = provider();
        let request = GitPanelIoRequest::Snapshot;
        assert!(matches!(
            provider.git_request(&request),
            Ok(Request::GitStatus)
        ));
        let GitPanelIoResult::Snapshot(Ok(snapshot)) = provider.git_response(
            &request,
            Ok(Response::Git(
                b" M --output=escape;file\0R  new.txt\0old.txt\0?? untracked.txt\0".to_vec(),
            )),
        ) else {
            panic!("git snapshot")
        };
        assert_eq!(snapshot.repo_root, Path::new("/remote/project"));
        assert_eq!(snapshot.changes.len(), 3);
        assert_eq!(snapshot.changes[0].rel_path, "--output=escape;file");
        assert_eq!(snapshot.changes[1].rel_path, "new.txt");
        let request = GitPanelIoRequest::FileDiff {
            rel_path: "--output=escape;file".into(),
            mode: diff_viewer::DiffMode::Working,
        };
        assert!(matches!(
            provider.git_request(&request),
            Ok(Request::GitDiff {
                path: Some(_),
                staged: false
            })
        ));
        let GitPanelIoResult::FileDiff(Ok(diff)) = provider.git_response(
            &request,
            Ok(Response::Git(b"@@ -1 +1 @@\n-old\n+new\n".to_vec())),
        ) else {
            panic!("git diff")
        };
        assert_eq!(diff.hunks.len(), 1);
        let request = GitPanelIoRequest::FileDiff {
            rel_path: "file".into(),
            mode: diff_viewer::DiffMode::Branch,
        };
        assert_eq!(
            provider.git_request(&request).unwrap_err(),
            RemoteProviderError::Unsupported
        );
    }
    #[test]
    fn remote_pages_refuse_replayed_cursors_duplicate_names_and_excess_bytes() {
        let provider = provider();
        let entry = |name: &[u8], symlink| files::DirectoryEntry {
            name: name.to_vec(),
            is_directory: true,
            is_symlink: symlink,
        };
        let mut listing = provider
            .directory(Path::new("/remote/project"), 4, 100)
            .unwrap();
        let RemoteListingProgress::Complete(snapshot) = listing
            .accept(Ok(Response::Directory(files::DirectoryPage {
                entries: vec![entry(b"link", true)],
                next: None,
            })))
            .unwrap()
        else {
            panic!("listing")
        };
        assert!(
            !snapshot.items()[0].is_dir(),
            "symlink cannot become a walkable directory"
        );
        assert!(
            listing
                .accept(Ok(Response::Directory(files::DirectoryPage {
                    entries: vec![],
                    next: None
                })))
                .is_err()
        );
        for (second, revision) in [(b"one".as_slice(), "a"), (b"two".as_slice(), "b")] {
            let mut listing = provider
                .directory(Path::new("/remote/project"), 4, 100)
                .unwrap();
            assert!(
                listing
                    .accept(Ok(Response::Directory(files::DirectoryPage {
                        entries: vec![entry(b"one", false)],
                        next: Some(files::DirectoryCursor {
                            offset: 1,
                            revision: "a".repeat(64)
                        })
                    })))
                    .is_ok()
            );
            assert!(
                listing
                    .accept(Ok(Response::Directory(files::DirectoryPage {
                        entries: vec![entry(second, false)],
                        next: Some(files::DirectoryCursor {
                            offset: 2,
                            revision: revision.repeat(64)
                        })
                    })))
                    .is_err()
            );
        }
        let mut listing = provider
            .directory(Path::new("/remote/project"), 4, 3)
            .unwrap();
        assert!(matches!(
            listing.accept(Ok(Response::Directory(files::DirectoryPage {
                entries: vec![entry(b"long", false)],
                next: None
            }))),
            Err(RemoteProviderError::TooLarge)
        ));
        let mut listing = provider
            .directory(Path::new("/remote/project"), 4, 100)
            .unwrap();
        let error = match listing.accept(Err(services::ServiceError::Files(
            files::FileError::PermissionDenied,
        ))) {
            Err(error) => error,
            Ok(_) => panic!("permission denied"),
        };
        assert_eq!(
            maintenance_error(error),
            FileTreeMaintenanceErrorCode::PermissionDenied
        );
    }
    #[test]
    fn remote_revisions_cannot_be_reused_for_another_server_and_saves_advance_revision() {
        let provider = provider();
        let path = Path::new("/remote/project/file.md");
        let DocumentLoadOutcome::Loaded {
            revision: loaded, ..
        } = provider.load_response(
            path,
            Ok(Response::File(files::FileDocument {
                content: b"original".to_vec(),
                revision: revision(b"original"),
            })),
        )
        else {
            panic!("load")
        };
        for host in ["other.example.test", "example.test"] {
            let mut other = provider.target().clone();
            other.host = host.into();
            if host == "example.test" {
                other.port = 2222;
            }
            let other = RemoteWorkspaceFiles::new(other).unwrap();
            assert_eq!(
                other
                    .write_request(&DocumentSaveRequest {
                        path: path.into(),
                        contents: "replacement".into(),
                        expected_revision: loaded
                    })
                    .unwrap_err(),
                RemoteProviderError::TargetMismatch
            );
        }
        let expected = revision(b"replacement");
        let DocumentSaveOutcome::Saved { revision: saved } =
            provider.save_response(path, Ok(Response::Written(expected.clone())))
        else {
            panic!("save")
        };
        let Request::WriteFileIfVersion {
            expected: actual, ..
        } = provider
            .write_request(&DocumentSaveRequest {
                path: path.into(),
                contents: "next".into(),
                expected_revision: saved,
            })
            .unwrap()
        else {
            panic!("request")
        };
        assert_eq!(actual, expected);
        assert!(matches!(
            provider.load_response(
                path,
                Ok(Response::File(files::FileDocument {
                    content: vec![0xff],
                    revision: revision(&[0xff])
                }))
            ),
            DocumentLoadOutcome::Binary { byte_len: 1 }
        ));
    }
    #[test]
    fn remote_git_rejects_malformed_and_lossy_action_paths_and_bounds_rows() {
        let provider = provider();
        for bytes in [
            b" M unterminated".as_slice(),
            b"R  new\0",
            b"!! ../escape\0",
            b" M bad-\xff\0",
            b"? file\0",
            b" M repeated\0 M repeated\0",
        ] {
            assert!(
                matches!(
                    provider.git_response(
                        &GitPanelIoRequest::Snapshot,
                        Ok(Response::Git(bytes.to_vec()))
                    ),
                    GitPanelIoResult::Snapshot(Err(_))
                ),
                "malformed status must fail"
            );
        }
        let bytes = (0..=MAX_PANEL_FILES)
            .flat_map(|index| format!("?? file-{index}\0").into_bytes())
            .collect();
        let GitPanelIoResult::Snapshot(Ok(snapshot)) =
            provider.git_response(&GitPanelIoRequest::Snapshot, Ok(Response::Git(bytes)))
        else {
            panic!("bounded snapshot")
        };
        assert_eq!(snapshot.changes.len(), MAX_PANEL_FILES);
        assert!(snapshot.changes_truncated);
    }
}
