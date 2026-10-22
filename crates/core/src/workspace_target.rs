//! Typed workspace execution targets. Remote roots are POSIX paths, never local filesystem paths.
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceTargetError(pub &'static str);
impl std::fmt::Display for WorkspaceTargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for WorkspaceTargetError {}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RemotePath(String);
impl RemotePath {
    pub fn new(value: impl Into<String>) -> Result<Self, WorkspaceTargetError> {
        let value = value.into();
        if value.len() > 4096
            || !value.starts_with('/')
            || value.chars().any(char::is_control)
            || (value != "/"
                && value
                    .split('/')
                    .skip(1)
                    .any(|part| part.is_empty() || part == "." || part == ".."))
        {
            return Err(WorkspaceTargetError("workspace_target_remote_root_invalid"));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for RemotePath {
    type Error = WorkspaceTargetError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<RemotePath> for String {
    fn from(value: RemotePath) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "SshWorkspaceTargetFields")]
pub struct SshWorkspaceTarget {
    pub connection_id: String,
    pub host: String,
    pub port: u16,
    pub account: String,
    pub host_key_sha256: String,
    pub remote_workspace_id: String,
    pub canonical_root: RemotePath,
}
#[derive(serde::Deserialize)]
struct SshWorkspaceTargetFields {
    connection_id: String,
    host: String,
    port: u16,
    account: String,
    host_key_sha256: String,
    remote_workspace_id: String,
    canonical_root: RemotePath,
}
impl TryFrom<SshWorkspaceTargetFields> for SshWorkspaceTarget {
    type Error = WorkspaceTargetError;
    fn try_from(value: SshWorkspaceTargetFields) -> Result<Self, Self::Error> {
        Self::new(
            value.connection_id,
            value.host,
            value.port,
            value.account,
            value.host_key_sha256,
            value.remote_workspace_id,
            value.canonical_root,
        )
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct SshWorkspaceIdentity {
    pub host: String,
    pub port: u16,
    pub account: String,
    pub host_key_sha256: String,
    pub canonical_root: RemotePath,
}
impl SshWorkspaceTarget {
    pub fn new(
        connection_id: String,
        host: String,
        port: u16,
        account: String,
        host_key_sha256: String,
        remote_workspace_id: String,
        canonical_root: RemotePath,
    ) -> Result<Self, WorkspaceTargetError> {
        let value = Self {
            connection_id,
            host,
            port,
            account,
            host_key_sha256,
            remote_workspace_id,
            canonical_root,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<(), WorkspaceTargetError> {
        let bounded = |value: &str, max: usize| {
            !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
        };
        let host_valid = bounded(&self.host, 253)
            && !self.host.starts_with('-')
            && self
                .host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-_:[]%".contains(&byte));
        let account_valid = bounded(&self.account, 256)
            && !self.account.starts_with('-')
            && self
                .account
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte));
        let fingerprint_valid = self
            .host_key_sha256
            .strip_prefix("SHA256:")
            .is_some_and(|value| {
                value.len() == 43
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"+/".contains(&byte))
            });
        if !bounded(&self.connection_id, 256)
            || !host_valid
            || self.port == 0
            || !account_valid
            || !fingerprint_valid
            || !bounded(&self.remote_workspace_id, 256)
        {
            return Err(WorkspaceTargetError(
                "workspace_target_ssh_identity_invalid",
            ));
        }
        RemotePath::new(self.canonical_root.as_str())?;
        Ok(())
    }
    pub fn identity(&self) -> SshWorkspaceIdentity {
        SshWorkspaceIdentity {
            host: self.host.to_ascii_lowercase(),
            port: self.port,
            account: self.account.clone(),
            host_key_sha256: self.host_key_sha256.clone(),
            canonical_root: self.canonical_root.clone(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WorkspaceTarget {
    Local { root: String },
    Ssh(SshWorkspaceTarget),
}
impl WorkspaceTarget {
    /// The only adapter for callers that need a local filesystem path. SSH roots never cross it.
    pub fn local_root(&self) -> Option<&Path> {
        match self {
            Self::Local { root } => Some(Path::new(root)),
            Self::Ssh(_) => None,
        }
    }
}

/// Nonsecret OpenSSH invocation settings; resolved host/account live in the verified target.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "SshConnectionProfileFields")]
pub struct SshConnectionProfile {
    pub id: String,
    pub destination: String,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
}
#[derive(serde::Deserialize)]
struct SshConnectionProfileFields {
    id: String,
    destination: String,
    port: Option<u16>,
    identity_file: Option<String>,
}
impl TryFrom<SshConnectionProfileFields> for SshConnectionProfile {
    type Error = WorkspaceTargetError;
    fn try_from(value: SshConnectionProfileFields) -> Result<Self, Self::Error> {
        let value = Self {
            id: value.id,
            destination: value.destination,
            port: value.port,
            identity_file: value.identity_file,
        };
        value.validate()?;
        Ok(value)
    }
}
impl SshConnectionProfile {
    pub fn validate(&self) -> Result<(), WorkspaceTargetError> {
        let valid_text = |value: &str, max: usize| {
            !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
        };
        if !valid_text(&self.id, 256)
            || !valid_text(&self.destination, 1024)
            || self.port == Some(0)
            || self
                .identity_file
                .as_ref()
                .is_some_and(|path| path.is_empty() || path.len() > 4096 || path.contains('\0'))
        {
            return Err(WorkspaceTargetError("ssh_connection_profile_invalid"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> SshWorkspaceTarget {
        SshWorkspaceTarget::new(
            "alias".into(),
            "server.example".into(),
            22,
            "ubuntu".into(),
            "SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG".into(),
            "server-workspace".into(),
            RemotePath::new("/home/ubuntu/project").unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn workspace_target_remote_path_rejects_noncanonical_or_unsafe_roots() {
        for root in [
            "",
            "home/ubuntu",
            "~/project",
            "/home/../etc",
            "/home/./p",
            "/home//p",
            "/home/p/",
            "/home/\0p",
            "/home/\np",
        ] {
            assert!(RemotePath::new(root).is_err(), "accepted {root:?}");
        }
        assert!(RemotePath::new(format!("/{}", "a".repeat(4096))).is_err());
    }
    #[test]
    fn workspace_target_remote_path_preserves_bytes_and_is_not_local() {
        let root = "/home/한글/e\u{301} 'quoted' \\project";
        let mut remote = target();
        remote.canonical_root = RemotePath::new(root).unwrap();
        assert_eq!(remote.canonical_root.as_str(), root);
        assert!(WorkspaceTarget::Ssh(remote).local_root().is_none());
        assert_eq!(
            WorkspaceTarget::Local { root: root.into() }.local_root(),
            Some(Path::new(root))
        );
        assert_eq!(RemotePath::new("/").unwrap().as_str(), "/");
    }
    #[test]
    fn workspace_target_identity_scopes_server_port_account_and_root() {
        let original = target();
        let mut alias = original.clone();
        alias.connection_id = "other-alias".into();
        alias.host = "SERVER.EXAMPLE".into();
        assert_eq!(original.identity(), alias.identity());
        for changed in 0..5 {
            let mut other = original.clone();
            match changed {
                0 => other.host = "other.example".into(),
                1 => other.account = "root".into(),
                2 => other.port = 2222,
                3 => other.canonical_root = RemotePath::new("/other").unwrap(),
                _ => {
                    other.host_key_sha256 =
                        "SHA256:ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefg".into()
                }
            }
            assert_ne!(original.identity(), other.identity());
        }
    }
    #[test]
    fn workspace_target_rejects_unverified_or_invalid_ssh_identity() {
        for field in 0..7 {
            let mut value = target();
            match field {
                0 => value.connection_id.clear(),
                1 => value.host = "-option".into(),
                2 => value.port = 0,
                3 => value.account = "user\ncommand".into(),
                4 => value.host_key_sha256 = "unverified".into(),
                5 => value.remote_workspace_id.clear(),
                _ => value.host = "host;command".into(),
            }
            assert!(value.validate().is_err(), "invalid field {field}");
        }
    }
    #[test]
    fn workspace_target_ssh_profile_rejects_invalid_and_secret_prompt_inputs() {
        let profile = SshConnectionProfile {
            id: "profile".into(),
            destination: "ubuntu@dev-alias".into(),
            port: Some(2222),
            identity_file: Some("~/.ssh/id_ed25519".into()),
        };
        assert!(profile.validate().is_ok());
        for change in 0..7 {
            let mut invalid = profile.clone();
            match change {
                0 => invalid.id.clear(),
                1 => invalid.destination = "host\ncommand".into(),
                2 => invalid.destination = "a".repeat(1025),
                3 => invalid.port = Some(0),
                4 => invalid.identity_file = Some("a".repeat(4097)),
                5 => invalid.identity_file = Some("key\0path".into()),
                _ => invalid.destination.clear(),
            }
            assert!(invalid.validate().is_err(), "change {change}");
        }
    }
}
