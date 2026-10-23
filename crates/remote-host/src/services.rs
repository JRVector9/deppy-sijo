//! The only file/Git RPC surface. No request accepts a shell command.
use crate::{files, git};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Request {
    Metadata,
    StatRoot {
        path: String,
    },
    ListDirectory {
        path: Vec<u8>,
        cursor: Option<files::DirectoryCursor>,
        limit: u16,
    },
    ReadFile {
        path: Vec<u8>,
    },
    WriteFileIfVersion {
        path: Vec<u8>,
        expected: files::FileRevision,
        content: Vec<u8>,
    },
    GitStatus,
    GitDiff {
        path: Option<Vec<u8>>,
        staged: bool,
    },
    InspectAgents,
    StageClipboardImage(crate::clipboard::ImageChunk),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub kind: String,
    pub executable: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ServerMetadata {
    pub account: String,
    pub home: String,
    pub os: String,
    pub architecture: String,
    pub helper_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerifiedRoot {
    pub canonical_root: String,
    pub remote_workspace_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Metadata(ServerMetadata),
    Root(VerifiedRoot),
    Directory(files::DirectoryPage),
    File(files::FileDocument),
    Written(files::FileRevision),
    Git(Vec<u8>),
    Agents(Vec<Agent>),
    ClipboardImage(crate::clipboard::Receipt),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ServiceError {
    InvalidRequest,
    Files(files::FileError),
    Git(git::GitError),
    TooLarge,
    TargetMismatch,
}

pub type Reply = Result<Response, ServiceError>;

pub fn handle_rpc(context: &runtime::SshRequestContext, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let reply = if payload.len() > runtime::SSH_RPC_BYTES_MAX {
        Err(ServiceError::TooLarge)
    } else if context.identity.validate().is_err()
        || context.connection_generation == 0
        || context.request_id == 0
    {
        Err(ServiceError::TargetMismatch)
    } else {
        match serde_json::from_slice::<Request>(payload) {
            Ok(request) => dispatch(context, request),
            Err(_) => Err(ServiceError::InvalidRequest),
        }
    };
    let encoded = serde_json::to_vec(&reply)?;
    if encoded.len() > runtime::SSH_RPC_BYTES_MAX {
        return Ok(serde_json::to_vec(&Reply::Err(ServiceError::TooLarge))?);
    }
    Ok(encoded)
}

fn dispatch(context: &runtime::SshRequestContext, request: Request) -> Reply {
    let root = files::FileRoot::open(std::path::Path::new(&context.identity.canonical_root))
        .map_err(ServiceError::Files)?;
    if root.canonical() != std::path::Path::new(&context.identity.canonical_root) {
        return Err(ServiceError::TargetMismatch);
    }
    match request {
        Request::Metadata => Ok(Response::Metadata(ServerMetadata {
            account: context.identity.account.clone(),
            home: std::env::var("HOME")
                .ok()
                .filter(|path| path.starts_with('/') && path.len() <= 4096)
                .unwrap_or_else(|| "/".into()),
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
            helper_version: env!("CARGO_PKG_VERSION").into(),
        })),
        Request::StatRoot { path } => {
            if path.len() > 4096 || !path.starts_with('/') || path.chars().any(char::is_control) {
                return Err(ServiceError::InvalidRequest);
            }
            let selected =
                files::FileRoot::open(std::path::Path::new(&path)).map_err(ServiceError::Files)?;
            if root.canonical() != std::path::Path::new("/")
                && selected.canonical() != root.canonical()
            {
                return Err(ServiceError::TargetMismatch);
            }
            let canonical_root = selected
                .canonical()
                .to_str()
                .ok_or(ServiceError::InvalidRequest)?
                .to_owned();
            Ok(Response::Root(VerifiedRoot {
                remote_workspace_id: workspace_id(&context.identity.account, &canonical_root),
                canonical_root,
            }))
        }
        Request::ListDirectory {
            path,
            cursor,
            limit,
        } => root
            .list(&path, cursor.as_ref(), limit as usize)
            .map(Response::Directory)
            .map_err(ServiceError::Files),
        Request::ReadFile { path } => root
            .read(&path)
            .map(Response::File)
            .map_err(ServiceError::Files),
        Request::WriteFileIfVersion {
            path,
            expected,
            content,
        } => root
            .write_if_version(&path, &expected, &content)
            .map(Response::Written)
            .map_err(ServiceError::Files),
        Request::GitStatus => git::status(root.canonical())
            .map(Response::Git)
            .map_err(ServiceError::Git),
        Request::GitDiff { path, staged } => git::diff(root.canonical(), path.as_deref(), staged)
            .map(Response::Git)
            .map_err(ServiceError::Git),
        Request::InspectAgents => Ok(Response::Agents(inspect_agents())),
        Request::StageClipboardImage(chunk) => crate::clipboard::stage(
            &workspace_id(&context.identity.account, &context.identity.canonical_root),
            chunk,
        )
        .map(Response::ClipboardImage)
        .map_err(ServiceError::Files),
    }
}

pub fn workspace_id(account: &str, root: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(account.as_bytes());
    digest.update([0]);
    digest.update(root.as_bytes());
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn inspect_agents() -> Vec<Agent> {
    use std::os::unix::fs::PermissionsExt;
    let path = crate::agent_environment::current_path();
    let directories: Vec<_> = std::env::split_paths(&path)
        .filter(|path| path.is_absolute())
        .take(96)
        .collect();
    [
        ("claude", "claude"),
        ("codex", "codex"),
        ("opencode", "opencode"),
        ("gemini", "gemini"),
        ("aider", "aider"),
        ("goose", "goose"),
        ("amp", "amp"),
        ("kimi", "kimi"),
        ("qwen-code", "qwen"),
        ("grok", "grok"),
        ("cursor", "cursor-agent"),
        ("copilot", "copilot"),
    ]
    .into_iter()
    .filter_map(|(kind, executable)| {
        directories
            .iter()
            .map(|directory| directory.join(executable))
            .find(|path| {
                path.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
            .and_then(|path| {
                path.to_str()
                    .filter(|path| path.len() <= 4096)
                    .map(|path| Agent {
                        kind: kind.into(),
                        executable: path.into(),
                    })
            })
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    fn context(root: &std::path::Path) -> runtime::SshRequestContext {
        runtime::SshRequestContext {
            identity: runtime::SshTargetIdentity {
                remote_workspace_id: "test".into(),
                account: "fixture".into(),
                canonical_root: root.canonicalize().unwrap().to_str().unwrap().into(),
                host_key_sha256: "SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG".into(),
            },
            runtime_instance: "runtime".into(),
            connection_generation: 1,
            request_id: 1,
        }
    }
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("deppy-rpc-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn call(context: &runtime::SshRequestContext, request: &Request) -> Reply {
        serde_json::from_slice(&handle_rpc(context, &serde_json::to_vec(request).unwrap()).unwrap())
            .unwrap()
    }
    #[test]
    fn ssh_agent_discovery_finds_home_installations_without_login_path() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let bin = fixture.0.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        for executable in [
            "claude",
            "codex",
            "opencode",
            "gemini",
            "aider",
            "goose",
            "amp",
            "kimi",
            "qwen",
            "grok",
            "cursor-agent",
            "copilot",
        ] {
            let path = bin.join(executable);
            std::fs::write(
                &path,
                if executable == "codex" {
                    "#!/usr/bin/env deppy-node-fixture\n"
                } else {
                    "#!/bin/sh\nexit 0\n"
                },
            )
            .unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let node_bin = fixture.0.join(".nvm/versions/node/v99.0.0/bin");
        std::fs::create_dir_all(&node_bin).unwrap();
        let interpreter = node_bin.join("deppy-node-fixture");
        std::fs::write(&interpreter, "#!/bin/sh\nprintf interpreter-ok\n").unwrap();
        std::fs::set_permissions(interpreter, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "services::tests::ssh_agent_discovery_child",
                "--ignored",
                "--nocapture",
            ])
            .env("HOME", &fixture.0)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "isolated HOME/PATH subprocess, invoked by ssh_agent_discovery_finds_home_installations_without_login_path"]
    fn ssh_agent_discovery_child() {
        let agents = inspect_agents();
        let mut command = std::process::Command::new(
            &agents
                .iter()
                .find(|agent| agent.kind == "codex")
                .expect("Codex in home bin")
                .executable,
        );
        crate::agent_environment::configure_child(&mut command);
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"interpreter-ok");
        let actual: Vec<_> = agents.into_iter().map(|agent| agent.kind).collect();
        for kind in [
            "claude",
            "codex",
            "opencode",
            "gemini",
            "aider",
            "goose",
            "amp",
            "kimi",
            "qwen-code",
            "grok",
            "cursor",
            "copilot",
        ] {
            assert!(
                actual.iter().any(|value| value == kind),
                "missing {kind}: {actual:?}"
            );
        }
    }

    #[test]
    fn ssh_image_clipboard_is_staged_on_the_server_before_a_path_is_returned() {
        let fixture = Fixture::new();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "services::tests::ssh_image_clipboard_child",
                "--ignored",
                "--nocapture",
            ])
            .env("HOME", &fixture.0)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "isolated HOME subprocess, invoked by ssh_image_clipboard_is_staged_on_the_server_before_a_path_is_returned"]
    fn ssh_image_clipboard_child() {
        use sha2::{Digest, Sha256};
        let fixture = Fixture::new();
        let data = b"\x89PNG\r\n\x1a\nimage-clipboard-fixture";
        let request = serde_json::json!({"StageClipboardImage": {
            "upload_id": uuid::Uuid::new_v4().to_string(), "offset": 0,
            "total": data.len(), "sha256": Sha256::digest(data).iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
            "extension": "png", "data": data,
        }});
        let reply: serde_json::Value = serde_json::from_slice(
            &handle_rpc(&context(&fixture.0), &serde_json::to_vec(&request).unwrap()).unwrap(),
        )
        .unwrap();
        let path = reply["Ok"]["ClipboardImage"]["path"]
            .as_str()
            .expect("server-staged path, not unsupported RPC");
        assert!(std::path::Path::new(path).starts_with(std::env::var_os("HOME").unwrap()));
        assert_eq!(std::fs::read(path).unwrap(), data);
    }
    #[test]
    fn services_files_same_paths_are_isolated_by_context_root() {
        let a = Fixture::new();
        let b = Fixture::new();
        std::fs::write(a.0.join("same"), b"first").unwrap();
        std::fs::write(b.0.join("same"), b"second").unwrap();
        let path = b"same".to_vec();
        let Response::File(first) =
            call(&context(&a.0), &Request::ReadFile { path: path.clone() }).unwrap()
        else {
            panic!("not file")
        };
        let Response::File(second) = call(&context(&b.0), &Request::ReadFile { path }).unwrap()
        else {
            panic!("not file")
        };
        assert_eq!(first.content, b"first");
        assert_eq!(second.content, b"second");
    }
    #[test]
    fn services_stat_root_cannot_change_a_bound_workspace() {
        let fixture = Fixture::new();
        assert_eq!(
            call(
                &context(&fixture.0),
                &Request::StatRoot { path: "/".into() }
            ),
            Err(ServiceError::TargetMismatch)
        );
    }
    #[test]
    fn services_bound_root_replacement_cannot_rebind_requests_outside_the_target() {
        let fixture = Fixture::new();
        let selected = fixture.0.join("project");
        let outside = fixture.0.join("outside");
        std::fs::create_dir(&selected).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"outside").unwrap();
        let bound = context(&selected.canonicalize().unwrap());
        std::fs::remove_dir(&selected).unwrap();
        std::os::unix::fs::symlink(&outside, &selected).unwrap();
        assert_eq!(
            call(
                &bound,
                &Request::ReadFile {
                    path: b"secret".to_vec()
                }
            ),
            Err(ServiceError::TargetMismatch)
        );
    }
    #[test]
    fn services_reject_unknown_commands_and_oversize_payloads() {
        let fixture = Fixture::new();
        let context = context(&fixture.0);
        let reply: Reply = serde_json::from_slice(
            &handle_rpc(&context, br#"{"RunShell":"rm anything"}"#).unwrap(),
        )
        .unwrap();
        assert_eq!(reply, Err(ServiceError::InvalidRequest));
        let reply: Reply = serde_json::from_slice(
            &handle_rpc(&context, &vec![b' '; 4 * 1024 * 1024 + 1]).unwrap(),
        )
        .unwrap();
        assert_eq!(reply, Err(ServiceError::TooLarge));
    }
}
