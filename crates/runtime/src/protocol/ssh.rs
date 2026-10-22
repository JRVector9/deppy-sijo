//! SSH stdio has its own envelope version: the existing loopback wire is unchanged.
use super::PROTO_VERSION;

pub const SSH_FRAME_BYTES_MAX: usize = 16 * 1024 * 1024;
pub const SSH_RPC_BYTES_MAX: usize = 4 * 1024 * 1024;
pub const SSH_CAP_RUNTIME: u32 = 1;
pub const SSH_CAP_RPC: u32 = 2;
pub const SSH_CAP_REATTACH: u32 = 4;
pub const SSH_CAPABILITIES: u32 = SSH_CAP_RUNTIME | SSH_CAP_RPC | SSH_CAP_REATTACH;

/// Stable, payload-free hello rejection. A missing supervisor with a presented
/// lease is the only case that permits the app to discard an expired proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SshHandshakeError {
    ResumeExpired,
    ResumeRequired,
    LeaseMismatch,
    TargetMismatch,
    ProtocolMismatch,
    CapabilityMismatch,
    InvalidRequest,
    Unavailable,
}

impl std::fmt::Display for SshHandshakeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ResumeExpired => "ssh_resume_expired",
            Self::ResumeRequired => "ssh_resume_required",
            Self::LeaseMismatch => "ssh_lease_mismatch",
            Self::TargetMismatch => "ssh_target_mismatch",
            Self::ProtocolMismatch => "ssh_protocol_mismatch",
            Self::CapabilityMismatch => "ssh_capability_mismatch",
            Self::InvalidRequest => "ssh_handshake_invalid_request",
            Self::Unavailable => "ssh_helper_unavailable",
        })
    }
}

impl std::error::Error for SshHandshakeError {}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SshTargetIdentity {
    pub remote_workspace_id: String,
    pub account: String,
    pub canonical_root: String,
    pub host_key_sha256: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SshResume {
    pub runtime_instance: String,
    pub lease: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SshClientHello {
    pub magic: [u8; 4],
    pub envelope_version: u16,
    pub runtime_protocol: u16,
    pub capabilities: u32,
    pub max_frame_bytes: u32,
    pub identity: SshTargetIdentity,
    pub connection_generation: u64,
    pub resume: Option<SshResume>,
}

impl SshClientHello {
    pub fn new(identity: SshTargetIdentity, connection_generation: u64) -> Self {
        Self {
            magic: *b"DPSH",
            envelope_version: 4,
            runtime_protocol: PROTO_VERSION,
            capabilities: SSH_CAPABILITIES,
            max_frame_bytes: SSH_FRAME_BYTES_MAX as u32,
            identity,
            connection_generation,
            resume: None,
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SshServerHello {
    pub envelope_version: u16,
    pub runtime_protocol: u16,
    pub capabilities: u32,
    pub max_frame_bytes: u32,
    pub identity: SshTargetIdentity,
    pub runtime_instance: String,
    pub lease: String,
    pub lease_ttl_secs: u32,
    pub connection_generation: u64,
}

#[derive(Clone)]
pub struct SshServerConfig {
    pub identity: SshTargetIdentity,
    pub runtime_instance: String,
    pub lease: String,
    pub lease_ttl_secs: u32,
    pub require_resume: bool,
}

impl SshServerConfig {
    pub fn new(identity: SshTargetIdentity, runtime_instance: String) -> Self {
        Self {
            identity,
            runtime_instance,
            lease: uuid::Uuid::new_v4().to_string(),
            lease_ttl_secs: 300,
            require_resume: false,
        }
    }

    pub fn negotiate(&self, hello: &SshClientHello) -> anyhow::Result<SshServerHello> {
        anyhow::ensure!(
            hello.magic == *b"DPSH"
                && hello.envelope_version == 4
                && hello.runtime_protocol == PROTO_VERSION,
            SshHandshakeError::ProtocolMismatch
        );
        anyhow::ensure!(
            hello.identity == self.identity,
            SshHandshakeError::TargetMismatch
        );
        hello
            .identity
            .validate()
            .map_err(|_| SshHandshakeError::InvalidRequest)?;
        anyhow::ensure!(
            hello.connection_generation > 0,
            SshHandshakeError::InvalidRequest
        );
        anyhow::ensure!(
            hello.capabilities & SSH_CAPABILITIES == SSH_CAPABILITIES,
            SshHandshakeError::CapabilityMismatch
        );
        anyhow::ensure!(
            (1024..=SSH_FRAME_BYTES_MAX as u32).contains(&hello.max_frame_bytes),
            SshHandshakeError::InvalidRequest
        );
        anyhow::ensure!(
            !self.require_resume || hello.resume.is_some(),
            SshHandshakeError::ResumeRequired
        );
        if let Some(resume) = &hello.resume {
            anyhow::ensure!(
                resume.runtime_instance == self.runtime_instance && resume.lease == self.lease,
                SshHandshakeError::LeaseMismatch
            );
        }
        Ok(SshServerHello {
            envelope_version: 4,
            runtime_protocol: PROTO_VERSION,
            capabilities: SSH_CAPABILITIES,
            max_frame_bytes: hello.max_frame_bytes,
            identity: self.identity.clone(),
            runtime_instance: self.runtime_instance.clone(),
            lease: self.lease.clone(),
            lease_ttl_secs: self.lease_ttl_secs,
            connection_generation: hello.connection_generation,
        })
    }
}

impl SshTargetIdentity {
    pub fn validate(&self) -> anyhow::Result<()> {
        for value in [
            &self.remote_workspace_id,
            &self.account,
            &self.canonical_root,
            &self.host_key_sha256,
        ] {
            anyhow::ensure!(
                !value.is_empty()
                    && value.len() <= 4096
                    && !value
                        .bytes()
                        .any(|byte| byte == 0 || byte == b'\n' || byte == b'\r'),
                "ssh_identity_invalid"
            );
        }
        anyhow::ensure!(
            self.canonical_root.starts_with('/') && self.host_key_sha256.starts_with("SHA256:"),
            "ssh_identity_invalid"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SshRequestContext {
    pub identity: SshTargetIdentity,
    pub runtime_instance: String,
    pub connection_generation: u64,
    pub request_id: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SshRpcReply {
    pub context: SshRequestContext,
    pub result: Result<Vec<u8>, String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct SshRequestFrame {
    pub sequence: u64,
    pub context: SshRequestContext,
    pub body: SshRequestBody,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) enum SshRequestBody {
    Command(Box<crate::RuntimeCommand>),
    Rpc(Vec<u8>),
    Detach,
    GuardedInput(SshGuardedInput),
    Ping,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct SshGuardedInput {
    pub session: crate::SessionId,
    pub operation_id: String,
    pub parts: Vec<Vec<u8>>,
    pub guard: Option<SshAgentGuard>,
    pub require_bracketed_paste: bool,
    pub expires_at_unix_ms: u64,
}

/// Explicit wire evidence; server checks its own live foreground/editor again.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct SshAgentGuard {
    pub foreground_process_group: u32,
    pub provider: u8,
    pub append: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) enum SshResponseFrame {
    Event {
        context: SshRequestContext,
        event: Box<crate::RuntimeEvent>,
    },
    Rpc(SshRpcReply),
    Pong {
        context: SshRequestContext,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> SshTargetIdentity {
        SshTargetIdentity {
            remote_workspace_id: "workspace-a".into(),
            account: "user".into(),
            canonical_root: "/home/user/project".into(),
            host_key_sha256: "SHA256:verified-host".into(),
        }
    }

    #[test]
    fn ssh_stream_handshake_rejects_wrong_protocol_target_generation_and_capabilities() {
        let config = SshServerConfig::new(identity(), "runtime-a".into());
        let mut wrong = SshClientHello::new(identity(), 1);
        wrong.runtime_protocol -= 1;
        assert!(
            config.negotiate(&wrong).is_err(),
            "old peers must fail at hello"
        );
        wrong = SshClientHello::new(identity(), 1);
        wrong.identity.account = "other".into();
        assert!(
            config.negotiate(&wrong).is_err(),
            "account owns the runtime"
        );
        wrong = SshClientHello::new(identity(), 0);
        assert!(config.negotiate(&wrong).is_err());
        wrong = SshClientHello::new(identity(), 1);
        wrong.capabilities = SSH_CAP_RUNTIME;
        assert!(config.negotiate(&wrong).is_err());
    }

    #[test]
    fn ssh_stream_resume_requires_same_runtime_and_lease_and_never_creates_replacement() {
        let mut config = SshServerConfig::new(identity(), "runtime-a".into());
        config.require_resume = true;
        let mut hello = SshClientHello::new(identity(), 2);
        assert!(
            config.negotiate(&hello).is_err(),
            "missing lease cannot steal sessions"
        );
        hello.resume = Some(SshResume {
            runtime_instance: "runtime-b".into(),
            lease: config.lease.clone(),
        });
        assert!(config.negotiate(&hello).is_err());
        hello.resume = Some(SshResume {
            runtime_instance: config.runtime_instance.clone(),
            lease: "wrong".into(),
        });
        assert!(config.negotiate(&hello).is_err());
        hello.resume.as_mut().unwrap().lease = config.lease.clone();
        assert_eq!(
            config.negotiate(&hello).unwrap().runtime_instance,
            "runtime-a"
        );
    }
}
