//! Remote terminal path lookup uses only root-scoped service replies, never local I/O.
use crate::{
    ssh_workspace_files::{
        RemoteDirectoryListing, RemoteListingProgress, RemoteProviderError, RemoteWorkspaceFiles,
    },
    ui::{
        file_tree::classify_document_target,
        workspace::{WorkspacePathKind, WorkspacePathPayload, WorkspacePathResolution},
    },
};
use deppy_remote_host::{
    files::FileError,
    services::{Reply, Request, Response, ServiceError},
};
use std::path::{Path, PathBuf};

const MAX_CANDIDATES: usize = 6;
const MAX_LISTING_ITEMS: usize = 4096;
const MAX_LISTING_BYTES: usize = 1024 * 1024;

struct Candidate {
    path: PathBuf,
    parent: PathBuf,
    name: Option<Vec<u8>>,
    kind: Option<WorkspacePathKind>,
}

pub(crate) struct RemotePathLookup {
    provider: RemoteWorkspaceFiles,
    candidates: Vec<Candidate>,
    listing: RemoteDirectoryListing,
    parent: PathBuf,
    completed_parents: Vec<PathBuf>,
    items: usize,
    bytes: usize,
    done: bool,
}

pub(crate) enum RemotePathLookupProgress {
    Next(Request),
    Complete(Option<WorkspacePathResolution>),
}

impl RemotePathLookup {
    pub(crate) fn start(
        provider: &RemoteWorkspaceFiles,
        word: &str,
        cwd: Option<&Path>,
    ) -> Result<Option<Self>, RemoteProviderError> {
        let mut candidates: Vec<Candidate> = Vec::new();
        for candidate in crate::app::terminal_path_candidates(word)
            .into_iter()
            .take(MAX_CANDIDATES)
        {
            if candidate.len() > 4096 || candidate.starts_with("~/") {
                continue;
            }
            let path = if candidate.starts_with('/') {
                PathBuf::from(candidate)
            } else if let Some(cwd) = cwd {
                cwd.join(candidate.strip_prefix("./").unwrap_or(candidate))
            } else {
                continue;
            };
            let Ok(relative) = provider.relative_path(&path) else {
                continue;
            };
            if candidates.iter().any(|existing| existing.path == path) {
                continue;
            }
            let (parent, name) = if relative.is_empty() {
                (path.clone(), None)
            } else {
                let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                    continue;
                };
                (parent.to_owned(), Some(name.as_encoded_bytes().to_vec()))
            };
            candidates.push(Candidate {
                path,
                parent,
                name,
                kind: None,
            });
        }
        let Some(first) = candidates.first() else {
            return Ok(None);
        };
        let parent = first.parent.clone();
        let listing = provider.directory(&parent, MAX_LISTING_ITEMS, MAX_LISTING_BYTES)?;
        Ok(Some(Self {
            provider: provider.clone(),
            candidates,
            listing,
            parent,
            completed_parents: Vec::with_capacity(MAX_CANDIDATES),
            items: 0,
            bytes: 0,
            done: false,
        }))
    }

    pub(crate) fn request(&self) -> Request {
        self.listing.request()
    }

    pub(crate) fn accept(
        &mut self,
        reply: Reply,
    ) -> Result<RemotePathLookupProgress, RemoteProviderError> {
        if self.done {
            return Err(RemoteProviderError::Stale);
        }
        let result = self.accept_inner(reply);
        if result.is_err() || matches!(result, Ok(RemotePathLookupProgress::Complete(_))) {
            self.done = true;
        }
        result
    }

    fn accept_inner(
        &mut self,
        reply: Reply,
    ) -> Result<RemotePathLookupProgress, RemoteProviderError> {
        let missing_parent = matches!(
            &reply,
            Err(ServiceError::Files(
                FileError::NotFound | FileError::NotDirectory
            ))
        );
        let mut items = self.items;
        let mut bytes = self.bytes;
        let mut matches = Vec::with_capacity(MAX_CANDIDATES);
        if let Ok(Response::Directory(page)) = &reply {
            items = items
                .checked_add(page.entries.len())
                .filter(|count| *count <= MAX_LISTING_ITEMS)
                .ok_or(RemoteProviderError::TooLarge)?;
            for entry in &page.entries {
                bytes = bytes
                    .checked_add(entry.name.len())
                    .filter(|count| *count <= MAX_LISTING_BYTES)
                    .ok_or(RemoteProviderError::TooLarge)?;
            }
            // Preserve raw-name and symlink identity before the file-tree
            // projection drops the latter. A document candidate is revalidated
            // as a regular, non-symlink file by the existing remote loader.
            for (index, candidate) in self.candidates.iter().enumerate() {
                if candidate.parent != self.parent {
                    continue;
                }
                let kind = if let Some(name) = &candidate.name {
                    page.entries.iter().find_map(|entry| {
                        if entry.name != *name || entry.is_symlink {
                            None
                        } else if entry.is_directory {
                            Some(WorkspacePathKind::Directory)
                        } else {
                            classify_document_target(&candidate.path)
                                .map(|_| WorkspacePathKind::OpenableFile)
                        }
                    })
                } else {
                    Some(WorkspacePathKind::Directory)
                };
                if let Some(kind) = kind {
                    matches.push((index, kind));
                }
            }
        }
        let next = match self.listing.accept(reply) {
            Ok(RemoteListingProgress::Next(request)) => Some(request),
            Ok(RemoteListingProgress::Complete(_)) => None,
            Err(RemoteProviderError::InvalidResponse) if missing_parent => {
                // Literal wrapper paths may have a different parent from the
                // unwrapped path. A typed missing parent permits the next remote
                // candidate; permission, protocol and other failures stay closed.
                for candidate in &mut self.candidates {
                    if candidate.parent == self.parent {
                        candidate.kind = None;
                    }
                }
                None
            }
            Err(error) => return Err(error),
        };
        self.items = items;
        self.bytes = bytes;
        for (index, kind) in matches {
            self.candidates[index].kind = Some(kind);
        }
        if next.is_none() {
            self.completed_parents.push(self.parent.clone());
        }
        // A shorter :line candidate cannot beat an exact filename on a later
        // page. Every distinct parent is scanned once within the global budget.
        for candidate in &self.candidates {
            if let Some(kind) = candidate.kind {
                let path = WorkspacePathPayload::try_new(candidate.path.clone())
                    .map_err(|_| RemoteProviderError::InvalidPath)?;
                return Ok(RemotePathLookupProgress::Complete(Some(
                    WorkspacePathResolution { kind, path },
                )));
            }
            if !self.completed_parents.contains(&candidate.parent) {
                break;
            }
        }
        if let Some(request) = next {
            return Ok(RemotePathLookupProgress::Next(request));
        }
        let Some(candidate) = self
            .candidates
            .iter()
            .find(|candidate| !self.completed_parents.contains(&candidate.parent))
        else {
            return Ok(RemotePathLookupProgress::Complete(None));
        };
        self.parent = candidate.parent.clone();
        self.listing = self.provider.directory(
            &self.parent,
            MAX_LISTING_ITEMS - self.items,
            MAX_LISTING_BYTES - self.bytes,
        )?;
        Ok(RemotePathLookupProgress::Next(self.listing.request()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deppy_core::workspace_target::{RemotePath, SshWorkspaceTarget};
    use deppy_remote_host::files;

    fn provider() -> RemoteWorkspaceFiles {
        RemoteWorkspaceFiles::new(
            SshWorkspaceTarget::new(
                "terminal-path-fixture".into(),
                "example.test".into(),
                22,
                "deploy".into(),
                format!("SHA256:{}", "a".repeat(43)),
                "workspace".into(),
                RemotePath::new("/remote/project").unwrap(),
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn entry(name: &[u8], directory: bool, symlink: bool) -> files::DirectoryEntry {
        files::DirectoryEntry {
            name: name.to_vec(),
            is_directory: directory,
            is_symlink: symlink,
        }
    }

    fn page(entries: Vec<files::DirectoryEntry>, offset: Option<u32>) -> Reply {
        Ok(Response::Directory(files::DirectoryPage {
            entries,
            next: offset.map(|offset| files::DirectoryCursor {
                offset,
                revision: "a".repeat(64),
            }),
        }))
    }

    fn resolved(progress: RemotePathLookupProgress) -> Option<WorkspacePathResolution> {
        match progress {
            RemotePathLookupProgress::Complete(result) => result,
            RemotePathLookupProgress::Next(_) => panic!("lookup unexpectedly needs another page"),
        }
    }

    #[test]
    fn remote_terminal_path_resolves_document_extensions_and_directories_from_remote_entries() {
        for name in [
            "notes.md",
            "notes.markdown",
            "notes.txt",
            "server.log",
            "README",
        ] {
            let path = format!("/remote/project/{name}");
            let mut lookup = RemotePathLookup::start(&provider(), &path, None)
                .unwrap()
                .expect("a remote absolute document path should start a service lookup");
            assert!(
                matches!(lookup.request(), Request::ListDirectory { path, .. } if path.is_empty())
            );
            let resolution = resolved(
                lookup
                    .accept(page(vec![entry(name.as_bytes(), false, false)], None))
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(resolution.kind, WorkspacePathKind::OpenableFile);
            assert_eq!(resolution.path.as_path(), Path::new(&path));
        }
        let mut lookup = RemotePathLookup::start(&provider(), "/remote/project/docs", None)
            .unwrap()
            .unwrap();
        let resolution = resolved(
            lookup
                .accept(page(vec![entry(b"docs", true, false)], None))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(resolution.kind, WorkspacePathKind::Directory);
        assert_eq!(resolution.path.as_path(), Path::new("/remote/project/docs"));
    }

    #[test]
    fn remote_terminal_path_uses_shared_wrappers_suffixes_and_remote_cwd() {
        for (word, cwd) in [
            ("`/remote/project/notes.md:12:3`.", None),
            ("./notes.md:12", Some(Path::new("/remote/project"))),
            ("notes.md", Some(Path::new("/remote/project"))),
        ] {
            let mut lookup = RemotePathLookup::start(&provider(), word, cwd)
                .unwrap()
                .unwrap();
            let result = resolved(
                lookup
                    .accept(page(vec![entry(b"notes.md", false, false)], None))
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(result.path.as_path(), Path::new("/remote/project/notes.md"));
        }
    }

    #[test]
    fn remote_terminal_path_keeps_exact_filename_priority_across_pages() {
        let mut lookup = RemotePathLookup::start(&provider(), "/remote/project/README:12", None)
            .unwrap()
            .unwrap();
        let progress = lookup
            .accept(page(vec![entry(b"README", false, false)], Some(1)))
            .unwrap();
        assert!(matches!(
            progress,
            RemotePathLookupProgress::Next(Request::ListDirectory {
                cursor: Some(_),
                ..
            })
        ));
        let result = resolved(
            lookup
                .accept(page(vec![entry(b"README:12", false, false)], None))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            result.path.as_path(),
            Path::new("/remote/project/README:12")
        );
    }

    #[test]
    fn remote_terminal_path_missing_wrapped_parent_tries_the_unwrapped_remote_parent() {
        let mut lookup = RemotePathLookup::start(
            &provider(),
            "`/remote/project/notes.md:12`",
            Some(Path::new("/remote/project")),
        )
        .unwrap()
        .unwrap();
        assert!(
            matches!(lookup.request(), Request::ListDirectory { path, .. } if path == b"`/remote/project")
        );
        let progress = lookup
            .accept(Err(deppy_remote_host::services::ServiceError::Files(
                files::FileError::NotFound,
            )))
            .unwrap();
        assert!(
            matches!(progress, RemotePathLookupProgress::Next(Request::ListDirectory { path, .. }) if path.is_empty())
        );
        let result = resolved(
            lookup
                .accept(page(vec![entry(b"notes.md", false, false)], None))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result.path.as_path(), Path::new("/remote/project/notes.md"));
    }

    #[test]
    fn remote_terminal_path_rejects_local_fallback_traversal_home_and_oversized_paths() {
        for word in [
            "/etc/passwd",
            "/remote/project-other/README",
            "/remote/project/../escape.md",
            "../escape.md",
            "~/notes.md",
            "/remote/project/notes\0.md",
        ] {
            assert!(
                RemotePathLookup::start(&provider(), word, Some(Path::new("/remote/project")))
                    .unwrap()
                    .is_none(),
                "{word:?}"
            );
        }
        assert!(
            RemotePathLookup::start(&provider(), "notes.md", None)
                .unwrap()
                .is_none()
        );
        let oversized = format!("/remote/project/{}.md", "x".repeat(4096));
        assert!(
            RemotePathLookup::start(&provider(), &oversized, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn remote_terminal_path_rejects_symlinks_and_unsupported_file_types() {
        for (name, directory, symlink) in [
            ("notes.md", false, true),
            ("docs", true, true),
            ("image.png", false, false),
        ] {
            let path = format!("/remote/project/{name}");
            let mut lookup = RemotePathLookup::start(&provider(), &path, None)
                .unwrap()
                .unwrap();
            assert!(
                resolved(
                    lookup
                        .accept(page(vec![entry(name.as_bytes(), directory, symlink)], None))
                        .unwrap()
                )
                .is_none()
            );
        }
    }

    #[test]
    fn remote_terminal_path_enforces_provider_page_validation_and_terminal_state() {
        let mut lookup = RemotePathLookup::start(&provider(), "/remote/project/notes.md", None)
            .unwrap()
            .unwrap();
        assert!(
            lookup
                .accept(page(vec![entry(b"../notes.md", false, false)], None))
                .is_err()
        );
        assert!(matches!(
            lookup.accept(page(vec![], None)),
            Err(RemoteProviderError::Stale)
        ));

        let mut lookup = RemotePathLookup::start(&provider(), "/remote/project/notes.md", None)
            .unwrap()
            .unwrap();
        let too_many = (0..=files::DIRECTORY_PAGE_MAX)
            .map(|index| entry(format!("file-{index}").as_bytes(), false, false))
            .collect();
        assert!(matches!(
            lookup.accept(page(too_many, None)),
            Err(RemoteProviderError::TooLarge)
        ));
    }
}
