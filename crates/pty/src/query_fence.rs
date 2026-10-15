//! Ordered ownership boundary between a wrapped agent and its fallback shell.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::time::SystemTime;

/// Private OSC: terminals ignore it visually; the wrapper emits its own PID after agent exit.
pub const AGENT_QUERY_FENCE_PREFIX: &str = "\x1b]777;deppy-agent-exit;";
/// Shared with the wrapper and status detector; this existing file also survives reader pressure.
pub const AGENT_EXIT_SENTINEL_PREFIX: &str = "deppy-agent-exit-";

struct ExitSentinel {
    path: PathBuf,
    #[cfg(unix)]
    spawned_after: SystemTime,
}

#[derive(Clone)]
pub struct QueryFence {
    marker: Arc<Vec<u8>>,
    observed: Arc<AtomicBool>,
    exit_sentinel: Option<Arc<ExitSentinel>>,
}

impl QueryFence {
    #[cfg(test)]
    pub(crate) fn new(pid: u32) -> Self {
        Self {
            marker: Arc::new(format!("{AGENT_QUERY_FENCE_PREFIX}{pid}\x07").into_bytes()),
            observed: Arc::new(AtomicBool::new(false)),
            exit_sentinel: None,
        }
    }

    #[cfg(unix)]
    pub(crate) fn with_exit_sentinel(
        pid: u32,
        directory: PathBuf,
        spawned_after: SystemTime,
    ) -> Self {
        Self {
            marker: Arc::new(format!("{AGENT_QUERY_FENCE_PREFIX}{pid}\x07").into_bytes()),
            observed: Arc::new(AtomicBool::new(false)),
            exit_sentinel: Some(Arc::new(ExitSentinel {
                path: directory.join(format!("{AGENT_EXIT_SENTINEL_PREFIX}{pid}")),
                spawned_after,
            })),
        }
    }

    pub fn retire(&self) {
        self.observed.store(true, Ordering::Release);
    }

    /// Called only at runtime-worker query admission or on the PTY writer, never in a UI frame.
    /// The wrapper writes this before exec, independently of a full reader output queue. Open a
    /// pinned regular file without following links/blocking on a FIFO, and reject a reused PID's
    /// older sentinel. The status worker must retire this same flag before deleting a valid file.
    pub fn refresh_exit(&self) -> bool {
        if !self.observed.load(Ordering::Acquire) && self.read_exit_code().is_some() {
            self.retire();
        }
        self.observed.load(Ordering::Acquire)
    }

    /// The status worker uses the same bounded, pinned proof as the protocol response writer.
    pub fn read_exit_code(&self) -> Option<u32> {
        #[cfg(unix)]
        {
            use std::io::Read as _;
            use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
            let sentinel = self.exit_sentinel.as_ref()?;
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&sentinel.path)
                .ok()?;
            let metadata = file.metadata().ok()?;
            if !metadata.is_file()
                || metadata.len() > 16
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.modified().ok()? < sentinel.spawned_after
            {
                return None;
            }
            let mut bytes = [0u8; 17];
            let mut length = 0;
            loop {
                let count = file.read(&mut bytes[length..]).ok()?;
                if count == 0 {
                    break;
                }
                length += count;
                if length > 16 {
                    return None;
                }
            }
            std::str::from_utf8(&bytes[..length])
                .ok()?
                .trim()
                .parse::<u32>()
                .ok()
        }
        #[cfg(not(unix))]
        None
    }

    pub fn sentinel_watch(&self) -> Option<(PathBuf, bool)> {
        self.exit_sentinel
            .as_ref()
            .map(|sentinel| (sentinel.path.clone(), self.refresh_exit()))
    }

    pub fn scanner(&self) -> QueryFenceScanner {
        QueryFenceScanner {
            marker: Arc::clone(&self.marker),
            pending: Vec::new(),
            crossed: false,
        }
    }

    pub fn permits_response(&self, after_boundary: bool) -> bool {
        after_boundary || !self.refresh_exit()
    }

    pub(crate) fn observer(&self) -> QueryFenceObserver {
        QueryFenceObserver {
            marker: Arc::clone(&self.marker),
            matched: 0,
        }
    }

    pub(crate) fn observe(&self, scanner: &mut QueryFenceObserver, bytes: &[u8]) {
        if self.observed.load(Ordering::Acquire) {
            return;
        }
        for byte in bytes {
            if scanner.marker[scanner.matched] == *byte {
                scanner.matched += 1;
                if scanner.matched == scanner.marker.len() {
                    self.retire();
                    return;
                }
            } else {
                // ESC occurs only at the beginning of our marker, so it is the sole overlap.
                scanner.matched = usize::from(*byte == scanner.marker[0]);
            }
        }
    }
}

#[cfg(unix)]
pub(crate) fn sentinel_directory(command: &crate::CommandSpec) -> PathBuf {
    let directory = command
        .env
        .iter()
        .rev()
        .find(|(key, _)| key == "TMPDIR")
        .map(|(_, value)| std::ffi::OsString::from(value))
        .or_else(|| std::env::var_os("TMPDIR"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    if directory.is_absolute() {
        return directory;
    }
    let cwd = command.cwd.clone().unwrap_or_default();
    let base = if cwd.is_absolute() {
        cwd
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(cwd)
    };
    base.join(directory)
}

pub(crate) struct QueryFenceObserver {
    marker: Arc<Vec<u8>>,
    matched: usize,
}

pub enum QueryFencePart {
    Output {
        bytes: Vec<u8>,
        after_boundary: bool,
    },
    Boundary,
}

/// Retains at most a marker prefix. An old query and a new query in the same reader chunk
/// keep distinct owners, even when the marker itself crosses arbitrary chunk boundaries.
pub struct QueryFenceScanner {
    marker: Arc<Vec<u8>>,
    pending: Vec<u8>,
    crossed: bool,
}

impl QueryFenceScanner {
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<QueryFencePart> {
        if self.crossed {
            return vec![QueryFencePart::Output {
                bytes: bytes.to_vec(),
                after_boundary: true,
            }];
        }
        let mut parts = Vec::new();
        let mut output = Vec::new();
        for (offset, byte) in bytes.iter().enumerate() {
            self.pending.push(*byte);
            while !self.marker.starts_with(&self.pending) {
                output.push(self.pending.remove(0));
            }
            if self.pending.len() == self.marker.len() {
                if !output.is_empty() {
                    parts.push(QueryFencePart::Output {
                        bytes: std::mem::take(&mut output),
                        after_boundary: false,
                    });
                }
                self.pending.clear();
                self.crossed = true;
                parts.push(QueryFencePart::Boundary);
                if offset + 1 < bytes.len() {
                    parts.push(QueryFencePart::Output {
                        bytes: bytes[offset + 1..].to_vec(),
                        after_boundary: true,
                    });
                }
                return parts;
            }
        }
        if !output.is_empty() {
            parts.push(QueryFencePart::Output {
                bytes: output,
                after_boundary: false,
            });
        }
        parts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_marker_separates_old_and_new_query_owners() {
        let marker_len = QueryFence::new(42).marker.len();
        for split in 0..=marker_len {
            let fence = QueryFence::new(42);
            let mut reader = fence.observer();
            let mut parser = fence.scanner();
            let mut first = b"\x1b[6n".to_vec();
            first.extend_from_slice(&fence.marker[..split]);
            let mut second = fence.marker[split..].to_vec();
            second.extend_from_slice(b"\x1b[6n");
            fence.observe(&mut reader, &first);
            fence.observe(&mut reader, &second);
            let parts = parser.scan(&first).into_iter().chain(parser.scan(&second));
            let mut old = Vec::new();
            let mut new = Vec::new();
            for part in parts {
                if let QueryFencePart::Output {
                    bytes,
                    after_boundary,
                } = part
                {
                    if after_boundary {
                        new.extend(bytes);
                    } else {
                        old.extend(bytes);
                    }
                }
            }
            assert_eq!(old, b"\x1b[6n");
            assert_eq!(new, b"\x1b[6n");
            assert!(!fence.permits_response(false));
            assert!(fence.permits_response(true));
        }
    }

    #[cfg(unix)]
    fn sentinel_fixture() -> (PathBuf, QueryFence) {
        let directory = std::env::temp_dir().join(format!(
            "deppy-fence-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let fence = QueryFence::with_exit_sentinel(42, directory.clone(), SystemTime::now());
        (directory, fence)
    }

    #[cfg(unix)]
    #[test]
    fn exit_file_retires_old_owner_without_reader_and_survives_consumption() {
        let (directory, fence) = sentinel_fixture();
        let path = fence.sentinel_watch().unwrap().0;
        std::fs::write(&path, b"0").unwrap();
        assert_eq!(fence.read_exit_code(), Some(0));
        assert!(!fence.permits_response(false));
        std::fs::remove_file(&path).unwrap();
        assert!(!fence.permits_response(false));
        assert!(fence.permits_response(true));
        std::fs::remove_dir(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn previous_pid_execution_file_does_not_retire_a_new_live_owner() {
        let (directory, fence) = sentinel_fixture();
        let path = fence.sentinel_watch().unwrap().0;
        std::fs::write(&path, b"0").unwrap();
        let times = std::fs::FileTimes::new()
            .set_modified(SystemTime::now() - std::time::Duration::from_secs(10));
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(times)
            .unwrap();
        assert_eq!(fence.read_exit_code(), None);
        assert!(fence.permits_response(false));
        assert!(!fence.sentinel_watch().unwrap().1);
        std::fs::write(&path, b"0").unwrap();
        assert!(!fence.permits_response(false));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn exit_probe_does_not_follow_links_or_block_on_a_fifo() {
        use std::os::unix::ffi::OsStrExt as _;
        let (directory, fence) = sentinel_fixture();
        let path = fence.sentinel_watch().unwrap().0;
        let target = directory.join("other-file");
        std::fs::write(&target, b"0").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(fence.permits_response(false));
        std::fs::remove_file(&path).unwrap();
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert!(fence.permits_response(false));
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn sentinel_directory_matches_shell_empty_relative_and_latest_tmpdir() {
        let mut command = crate::CommandSpec {
            program: "/bin/sh".into(),
            args: Vec::new(),
            env: vec![
                ("TMPDIR".into(), "ignored".into()),
                ("TMPDIR".into(), "".into()),
            ],
            cwd: Some(PathBuf::from("/private/test-cwd")),
        };
        assert_eq!(sentinel_directory(&command), PathBuf::from("/tmp"));
        command.env.push(("TMPDIR".into(), "relative-tmp".into()));
        assert_eq!(
            sentinel_directory(&command),
            PathBuf::from("/private/test-cwd/relative-tmp")
        );
        command
            .env
            .push(("TMPDIR".into(), "/private/absolute-tmp".into()));
        assert_eq!(
            sentinel_directory(&command),
            PathBuf::from("/private/absolute-tmp")
        );
    }

    #[test]
    fn unrelated_marker_and_regular_shell_queries_are_preserved() {
        let fence = QueryFence::new(42);
        let mut scanner = fence.scanner();
        let bytes = format!("before{AGENT_QUERY_FENCE_PREFIX}41\x07\x1b[6nafter");
        let parts = scanner.scan(bytes.as_bytes());
        assert_eq!(parts.len(), 1);
        let QueryFencePart::Output {
            bytes: actual,
            after_boundary: false,
        } = &parts[0]
        else {
            panic!("wrong owner")
        };
        assert_eq!(actual, bytes.as_bytes());
        assert!(fence.permits_response(false));
    }
}
