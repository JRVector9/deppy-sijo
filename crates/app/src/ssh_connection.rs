//! Bundled authentication helper location. Connection lifecycle belongs to SshWorkspaceHost.
use std::path::{Path, PathBuf};

pub fn bundled_askpass_path(app_executable: &Path) -> PathBuf {
    app_executable.with_file_name("deppy-ssh-askpass")
}
