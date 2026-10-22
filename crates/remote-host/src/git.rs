//! Read-only Git operations run at the helper's verified root.
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::{ffi::OsStrExt, process::CommandExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const GIT_BYTES_MAX: usize = 512 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GitError {
    Missing,
    NotRepository,
    InvalidPath,
    TooLarge,
    TimedOut,
    Failed,
}

pub fn status(root: &Path) -> Result<Vec<u8>, GitError> {
    // Porcelain v1 always reports repository-relative names, even under -C.
    // Keep the Git query scoped and translate its action paths to our root.
    let mut prefix = run(root, &["rev-parse".as_ref(), "--show-prefix".as_ref()])?;
    if prefix.pop() != Some(b'\n') || prefix.len() > 4096 {
        return Err(GitError::Failed);
    }
    let output = run(
        root,
        &[
            "status".as_ref(),
            "--porcelain=v1".as_ref(),
            "-z".as_ref(),
            "--untracked-files=all".as_ref(),
            "--".as_ref(),
            ".".as_ref(),
        ],
    )?;
    scoped_status(&output, &prefix)
}

fn scoped_status(output: &[u8], prefix: &[u8]) -> Result<Vec<u8>, GitError> {
    let relative = |path: &[u8]| -> Result<Option<Vec<u8>>, GitError> {
        let Some(path) = path.strip_prefix(prefix) else {
            return Ok(None);
        };
        if path.is_empty() || crate::files::components(path).is_err() {
            return Err(GitError::Failed);
        }
        Ok(Some(path.to_vec()))
    };
    let mut entries = Vec::new();
    let mut records = output.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if record.len() < 4 || record[2] != b' ' {
            return Err(GitError::Failed);
        }
        let destination = relative(&record[3..])?;
        let rename = record[..2].contains(&b'R');
        let copy = record[..2].contains(&b'C');
        let source = if rename || copy {
            relative(
                records
                    .next()
                    .filter(|path| !path.is_empty())
                    .ok_or(GitError::Failed)?,
            )?
        } else {
            None
        };
        let mut status = [record[0], record[1], b' '];
        let (path, old_path) = match (destination, source) {
            (Some(path), Some(old_path)) => (path, Some(old_path)),
            (Some(path), None) => {
                for byte in &mut status[..2] {
                    if matches!(*byte, b'R' | b'C') {
                        *byte = b'A';
                    }
                }
                (path, None)
            }
            (None, Some(path)) if rename => {
                status = if record[0] == b'R' { *b"D  " } else { *b" D " };
                (path, None)
            }
            _ => continue,
        };
        let mut encoded = status.to_vec();
        encoded.extend_from_slice(&path);
        encoded.push(0);
        if let Some(old_path) = old_path {
            encoded.extend_from_slice(&old_path);
            encoded.push(0);
        }
        entries.push((path, encoded));
    }
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok(entries
        .into_iter()
        .flat_map(|(_, encoded)| encoded)
        .collect())
}
pub fn diff(root: &Path, path: Option<&[u8]>, staged: bool) -> Result<Vec<u8>, GitError> {
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "diff".as_ref(),
        "--no-ext-diff".as_ref(),
        "--no-textconv".as_ref(),
        "--no-color".as_ref(),
        "--relative".as_ref(),
        // A cross-root rename must not expose the outside endpoint in a patch.
        "--no-renames".as_ref(),
    ];
    if staged {
        args.push("--cached".as_ref());
    }
    args.push("--".as_ref());
    if let Some(path) = path {
        if path.is_empty() || crate::files::components(path).is_err() {
            return Err(GitError::InvalidPath);
        }
        args.push(std::ffi::OsStr::from_bytes(path));
    } else {
        args.push(".".as_ref());
    }
    run(root, &args)
}

fn run(root: &Path, args: &[&std::ffi::OsStr]) -> Result<Vec<u8>, GitError> {
    let root = crate::files::FileRoot::open(root).map_err(|_| GitError::NotRepository)?;
    let mut command = Command::new("/usr/bin/git");
    command
        .arg("--no-pager")
        .arg("--literal-pathspecs")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.quotePath=false",
        ])
        .arg("-C")
        .arg(root.canonical())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for (name, _) in std::env::vars_os() {
        if name.as_bytes().starts_with(b"GIT_") {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            GitError::Missing
        } else {
            GitError::Failed
        }
    })?;
    let mut reaped = false;
    let result = (|| {
        let mut stdout = child.stdout.take().ok_or(GitError::Failed)?;
        let mut stderr = child.stderr.take().ok_or(GitError::Failed)?;
        for descriptor in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
            if flags < 0
                || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(GitError::Failed);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        let mut errors = 0usize;
        loop {
            read_ready(&mut stdout, Some(&mut output), &mut 0usize, GIT_BYTES_MAX)?;
            read_ready(&mut stderr, None, &mut errors, 64 * 1024)?;
            // Observe without reaping: the owned process group cannot be recycled before cleanup.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } < 0
            {
                return Err(GitError::Failed);
            }
            if unsafe { info.si_pid() } != 0 {
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let status = child.wait().map_err(|_| GitError::Failed)?;
                reaped = true;
                read_ready(&mut stdout, Some(&mut output), &mut 0usize, GIT_BYTES_MAX)?;
                return if status.success() {
                    Ok(output)
                } else {
                    Err(GitError::NotRepository)
                };
            }
            if Instant::now() >= deadline {
                return Err(GitError::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    // On failure no reaping occurred yet; the group identity remains owned by this child.
    if result.is_err() && !reaped {
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.wait();
    }
    result
}

fn read_ready(
    reader: &mut impl Read,
    mut output: Option<&mut Vec<u8>>,
    discarded: &mut usize,
    limit: usize,
) -> Result<(), GitError> {
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                let length = output.as_ref().map_or(*discarded, |output| output.len());
                if length.saturating_add(count) > limit {
                    return Err(GitError::TooLarge);
                }
                if let Some(output) = &mut output {
                    output.extend_from_slice(&buffer[..count]);
                } else {
                    *discarded += count;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(GitError::Failed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("deppy-remote-git-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            assert!(
                Command::new("git")
                    .arg("init")
                    .arg("-q")
                    .arg(&root)
                    .status()
                    .unwrap()
                    .success()
            );
            Self(root)
        }
        fn git(&self, args: &[&str]) {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&self.0)
                    .args([
                        "-c",
                        "user.name=Fixture",
                        "-c",
                        "user.email=fixture@example.invalid"
                    ])
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn git_status_uses_exact_workspace_root() {
        let first = Fixture::new();
        let second = Fixture::new();
        std::fs::write(first.0.join("one.txt"), "one").unwrap();
        std::fs::write(second.0.join("two.txt"), "two").unwrap();
        let a = status(&first.0).unwrap();
        let b = status(&second.0).unwrap();
        assert!(a.windows(7).any(|w| w == b"one.txt"));
        assert!(!a.windows(7).any(|w| w == b"two.txt"));
        assert!(b.windows(7).any(|w| w == b"two.txt"));
    }
    #[test]
    fn git_diff_treats_option_like_file_as_path_and_never_shell() {
        let fixture = Fixture::new();
        let path = "--output=escaped ; 한글.txt";
        std::fs::write(fixture.0.join(path), "old\n").unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&fixture.0)
                .args(["add", "--", path])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(fixture.0.join(path), "new\n").unwrap();
        let output = diff(&fixture.0, Some(path.as_bytes()), false).unwrap();
        assert!(String::from_utf8_lossy(&output).contains("+new"));
        assert!(!fixture.0.join("escaped").exists());
        assert_eq!(
            diff(&fixture.0, Some(b"../outside"), false),
            Err(GitError::InvalidPath)
        );
    }

    #[test]
    fn git_selected_subfolder_status_and_diff_exclude_repository_neighbors() {
        let fixture = Fixture::new();
        let scope = fixture.0.join("selected");
        std::fs::create_dir_all(scope.join("new/deep")).unwrap();
        std::fs::write(scope.join("inside.txt"), "inside old\n").unwrap();
        std::fs::write(fixture.0.join("outside.txt"), "outside old\n").unwrap();
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-qm", "fixture"]);
        std::fs::write(scope.join("inside.txt"), "inside changed\n").unwrap();
        std::fs::write(fixture.0.join("outside.txt"), "OUTSIDE_NEIGHBOR_SECRET\n").unwrap();
        std::fs::write(scope.join("new/deep/untracked.txt"), "new\n").unwrap();
        let output = status(&scope).unwrap();
        let records: Vec<_> = output
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .collect();
        assert_eq!(
            records,
            vec![
                b" M inside.txt".as_slice(),
                b"?? new/deep/untracked.txt".as_slice()
            ]
        );
        let patch = String::from_utf8(diff(&scope, None, false).unwrap()).unwrap();
        assert!(patch.contains("+inside changed"));
        assert!(!patch.contains("outside.txt") && !patch.contains("OUTSIDE_NEIGHBOR_SECRET"));
        assert!(
            !patch.contains("a/selected/"),
            "action paths must be relative to selected root"
        );
        assert!(
            diff(&scope, Some(b":(top)outside.txt"), false)
                .unwrap()
                .is_empty(),
            "wire paths cannot activate Git pathspec magic"
        );
    }

    #[test]
    fn git_subfolder_cross_boundary_renames_expose_only_inside_action_paths() {
        let fixture = Fixture::new();
        let scope = fixture.0.join("selected");
        std::fs::create_dir(&scope).unwrap();
        std::fs::write(fixture.0.join("outside-origin.txt"), "incoming\n").unwrap();
        std::fs::write(scope.join("outgoing.txt"), "outgoing\n").unwrap();
        std::fs::write(scope.join("old.txt"), "internal\n").unwrap();
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-qm", "fixture"]);
        fixture.git(&["mv", "outside-origin.txt", "selected/incoming.txt"]);
        fixture.git(&["mv", "selected/outgoing.txt", "outside-destination.txt"]);
        fixture.git(&["mv", "selected/old.txt", "selected/new.txt"]);
        let output = status(&scope).unwrap();
        let records: Vec<_> = output
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .collect();
        assert_eq!(
            records,
            vec![
                b"A  incoming.txt".as_slice(),
                b"R  new.txt".as_slice(),
                b"old.txt".as_slice(),
                b"D  outgoing.txt".as_slice()
            ]
        );
        let patch = String::from_utf8(diff(&scope, None, true).unwrap()).unwrap();
        assert!(!patch.contains("outside-origin") && !patch.contains("outside-destination"));
        assert!(!patch.contains("a/selected/") && !patch.contains("b/selected/"));
    }

    #[test]
    fn git_diff_never_follows_a_replaced_directory_into_neighbor_content() {
        let fixture = Fixture::new();
        let scope = fixture.0.join("selected");
        std::fs::create_dir_all(scope.join("tracked")).unwrap();
        std::fs::create_dir(fixture.0.join("neighbor")).unwrap();
        std::fs::write(scope.join("tracked/document.txt"), "inside original\n").unwrap();
        std::fs::write(
            fixture.0.join("neighbor/document.txt"),
            "OUTSIDE_SYMLINK_SECRET\n",
        )
        .unwrap();
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-qm", "fixture"]);
        std::fs::remove_dir_all(scope.join("tracked")).unwrap();
        std::os::unix::fs::symlink("../neighbor", scope.join("tracked")).unwrap();
        for path in [None, Some(b"tracked/document.txt".as_slice())] {
            let patch = String::from_utf8(diff(&scope, path, false).unwrap()).unwrap();
            assert!(!patch.contains("OUTSIDE_SYMLINK_SECRET"));
            assert!(!patch.contains("neighbor/document.txt"));
        }
    }
}
