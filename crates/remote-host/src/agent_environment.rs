//! SSH exec sessions do not load a login PATH. Keep discovery and PTY inheritance
//! consistent without running shell startup files or mutating process-global env.
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

const PATH_ITEMS_MAX: usize = 96;
const PATH_BYTES_MAX: usize = 32 * 1024;

pub(crate) fn current_path() -> OsString {
    search_path(
        &std::env::var_os("PATH").unwrap_or_default(),
        std::env::var_os("HOME").as_deref().map(Path::new),
    )
}

pub(crate) fn configure_child(command: &mut std::process::Command) {
    command.env("PATH", current_path());
}

fn search_path(inherited: &OsStr, home: Option<&Path>) -> OsString {
    let mut paths = Vec::new();
    let mut bytes = 0usize;
    let mut push = |path: PathBuf| {
        let length = path.as_os_str().as_encoded_bytes().len();
        if path.is_absolute()
            && length <= 4096
            && !path
                .as_os_str()
                .as_encoded_bytes()
                .iter()
                .any(|b| *b == b':' || *b == 0)
            && bytes + length < PATH_BYTES_MAX
            && paths.len() < PATH_ITEMS_MAX
            && !paths.contains(&path)
            && path.is_dir()
        {
            bytes += length + 1;
            paths.push(path);
        }
    };
    // Preserve the account's configured command precedence.
    for path in std::env::split_paths(inherited).take(64) {
        push(path);
    }
    if let Some(home) = home.filter(|home| home.is_absolute()) {
        for suffix in [
            ".local/bin",
            ".opencode/bin",
            ".cargo/bin",
            ".kimi-code/bin",
            ".cursor/bin",
            ".grok/bin",
            ".asdf/shims",
            ".bun/bin",
            ".npm-global/bin",
            ".volta/bin",
            ".local/share/pnpm",
            ".local/share/mise/shims",
            "bin",
        ] {
            push(home.join(suffix));
        }
        for (root, suffix) in [
            (".nvm/versions/node", "bin"),
            (".local/share/fnm/node-versions", "installation/bin"),
            (".asdf/installs/nodejs", "bin"),
            (".local/share/mise/installs/node", "bin"),
        ] {
            if let Ok(entries) = std::fs::read_dir(home.join(root)) {
                let mut versions: Vec<_> = entries
                    .take(64)
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .collect();
                versions.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
                for version in versions {
                    push(version.join(suffix));
                }
            }
        }
    }
    for path in ["/usr/local/bin", "/usr/bin", "/bin"] {
        push(PathBuf::from(path));
    }
    // Every retained item is an absolute filesystem path with no PATH separator.
    std::env::join_paths(paths).unwrap_or_else(|_| OsString::from("/usr/local/bin:/usr/bin:/bin"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "deppy-agent-path-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn directory(&self, suffix: &str) -> PathBuf {
            let path = self.0.join(suffix);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn home_search_includes_opencode_native_installer_directory() {
        let fixture = Fixture::new();
        let bin = fixture.directory(".opencode/bin");
        let actual = search_path(OsStr::new("/usr/bin:/bin"), Some(&fixture.0));
        assert!(
            std::env::split_paths(&actual).any(|path| path == bin),
            "OpenCode's native installer directory must be discoverable without shell init"
        );
    }

    #[test]
    fn home_search_preserves_inherited_precedence_and_rejects_cwd_entries() {
        let fixture = Fixture::new();
        let preferred = fixture.directory("preferred");
        let second = fixture.directory("second");
        let home_bin = fixture.directory(".local/bin");
        let inherited = std::env::join_paths([
            preferred.clone(),
            PathBuf::new(),
            PathBuf::from("relative/bin"),
            second.clone(),
            preferred.clone(),
            fixture.0.join("missing"),
        ])
        .unwrap();
        let actual = search_path(&inherited, Some(&fixture.0));
        let paths: Vec<_> = std::env::split_paths(&actual).collect();
        assert_eq!(&paths[..3], &[preferred, second, home_bin]);
        assert!(paths.iter().all(|path| path.is_absolute() && path.is_dir()));
        let unique: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(unique.len(), paths.len());
    }

    #[test]
    fn home_search_bounds_version_scan_and_combined_path() {
        let fixture = Fixture::new();
        let inherited: Vec<_> = (0..64)
            .map(|index| fixture.directory(&format!("inherited/{index}")))
            .collect();
        for index in 0..80 {
            fixture.directory(&format!(".nvm/versions/node/v{index}.0.0/bin"));
        }
        let actual = search_path(
            &std::env::join_paths(inherited.clone()).unwrap(),
            Some(&fixture.0),
        );
        let paths: Vec<_> = std::env::split_paths(&actual).collect();
        assert_eq!(&paths[..inherited.len()], inherited.as_slice());
        assert!(paths.len() <= PATH_ITEMS_MAX);
        assert!(actual.as_encoded_bytes().len() <= PATH_BYTES_MAX);
    }

    #[test]
    fn home_search_bounds_total_bytes_even_with_long_existing_entries() {
        let fixture = Fixture::new();
        let segments = [
            "a".repeat(128),
            "b".repeat(128),
            "c".repeat(128),
            "d".repeat(128),
        ]
        .join("/");
        let inherited: Vec<_> = (0..64)
            .map(|index| fixture.directory(&format!("{segments}/{index}")))
            .collect();
        // Exercise the byte budget independently of the item budget.
        assert!(std::env::join_paths(&inherited).unwrap().len() > PATH_BYTES_MAX);
        let actual = search_path(&std::env::join_paths(inherited).unwrap(), None);
        assert!(actual.as_encoded_bytes().len() <= PATH_BYTES_MAX);
        assert!(std::env::split_paths(&actual).count() <= PATH_ITEMS_MAX);
    }
}
