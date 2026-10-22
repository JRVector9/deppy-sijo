//! UI-side generation gate. It owns no filesystem/socket I/O and never retains authentication values.
use ssh_transport::{
    AuthPrompt, FailureKind, PromptKind, PromptReply, ResolvedIdentity, SshEvent, SshLaunchOptions,
    SshStream, SshWorker,
};
use std::{
    io,
    path::{Path, PathBuf},
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Idle,
    Connecting,
    HostTrust,
    Authenticating,
    Ready,
    Failed(FailureKind),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectedPeer {
    pub host: String,
    pub account: String,
    pub port: u16,
    pub host_key_sha256: String,
}
pub struct SshConnectionController {
    generation: u64,
    worker: Option<SshWorker>,
    pub state: ConnectionState,
    pub resolved: Option<ResolvedIdentity>,
    pub observed_key: Option<String>,
    pub prompt: Option<AuthPrompt>,
}
impl Default for SshConnectionController {
    fn default() -> Self {
        Self {
            generation: 0,
            worker: None,
            state: ConnectionState::Idle,
            resolved: None,
            observed_key: None,
            prompt: None,
        }
    }
}
impl SshConnectionController {
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Address/authentication edits, cancel and sleep/wake all invalidate outstanding responses.
    pub fn invalidate(&mut self) -> u64 {
        self.worker.take(); // nonblocking Drop signals owned worker cleanup
        self.generation = self
            .generation
            .checked_add(1)
            .expect("SSH connection generation exhausted");
        self.state = ConnectionState::Idle;
        self.resolved = None;
        self.observed_key = None;
        self.prompt = None;
        self.generation
    }
    pub fn begin(&mut self, options: SshLaunchOptions) -> io::Result<(u64, SshStream)> {
        let generation = self.invalidate();
        match SshWorker::spawn(options) {
            Ok((worker, stream)) => {
                self.worker = Some(worker);
                self.state = ConnectionState::Connecting;
                Ok((generation, stream))
            }
            Err(error) => {
                self.state = ConnectionState::Failed(FailureKind::InvalidConfiguration);
                Err(error)
            }
        }
    }
    /// Bounded poll, suitable for an egui update. No socket or process operations run here.
    pub fn poll(&mut self) -> Vec<(u64, SshEvent)> {
        let mut events = Vec::new();
        for _ in 0..32 {
            let Some(event) = self.worker.as_ref().and_then(SshWorker::try_event) else {
                break;
            };
            if self.apply_event(self.generation, event.clone()) {
                events.push((self.generation, event));
            }
        }
        events
    }
    pub fn apply_event(&mut self, generation: u64, event: SshEvent) -> bool {
        if generation != self.generation || matches!(self.state, ConnectionState::Failed(_)) {
            return false;
        }
        match event {
            SshEvent::Resolved(peer) => self.resolved = Some(peer),
            SshEvent::HostKeyObserved(key) => {
                if !valid_fingerprint(&key) {
                    self.state = ConnectionState::Failed(FailureKind::HostVerification);
                    self.worker.take();
                } else {
                    self.observed_key = Some(key);
                }
            }
            SshEvent::Prompt(prompt) => {
                self.state = if prompt.kind == PromptKind::HostTrust {
                    ConnectionState::HostTrust
                } else {
                    ConnectionState::Authenticating
                };
                self.prompt = Some(prompt);
            }
            SshEvent::Ended(reason) => {
                self.prompt = None;
                self.state = ConnectionState::Failed(reason);
            }
        }
        true
    }
    pub fn reply(
        &mut self,
        generation: u64,
        request_id: u64,
        reply: PromptReply,
    ) -> io::Result<()> {
        if generation != self.generation || self.prompt.as_ref().is_none_or(|p| p.id != request_id)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stale SSH authentication request",
            ));
        }
        self.worker
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "SSH worker disconnected"))?
            .reply(request_id, reply)?;
        self.prompt = None;
        self.state = ConnectionState::Connecting;
        Ok(())
    }
    /// Call only after a successful authenticated helper hello; never persist an observed key earlier.
    pub fn handshake_ready(
        &mut self,
        generation: u64,
        remote_account: &str,
    ) -> io::Result<ConnectedPeer> {
        if generation != self.generation
            || matches!(
                self.state,
                ConnectionState::Idle | ConnectionState::Failed(_)
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "stale SSH handshake",
            ));
        }
        let peer = self
            .resolved
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "SSH account unresolved"))?;
        let key = self
            .observed_key
            .as_ref()
            .filter(|key| valid_fingerprint(key))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::PermissionDenied, "SSH host key unavailable")
            })?;
        if peer.account != remote_account {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SSH helper account mismatch",
            ));
        }
        let connected = ConnectedPeer {
            host: peer.host.clone(),
            account: peer.account.clone(),
            port: peer.port,
            host_key_sha256: key.clone(),
        };
        self.state = ConnectionState::Ready;
        self.prompt = None;
        Ok(connected)
    }
}
fn valid_fingerprint(key: &str) -> bool {
    key.strip_prefix("SHA256:").is_some_and(|key| {
        key.len() == 43
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/'))
    })
}

pub fn bundled_askpass_path(app_executable: &Path) -> PathBuf {
    app_executable.with_file_name("deppy-ssh-askpass")
}
#[cfg(test)]
mod tests {
    use super::*;
    fn resolved() -> ResolvedIdentity {
        ResolvedIdentity {
            host: "server.test".into(),
            account: "deploy".into(),
            port: 22,
        }
    }
    #[test]
    fn invalidation_discards_pending_authentication_and_all_previous_target_state() {
        let mut connection = SshConnectionController::default();
        connection.resolved = Some(resolved());
        connection.observed_key = Some("key".into());
        connection.prompt = Some(AuthPrompt {
            id: 1,
            kind: PromptKind::HostTrust,
            text: "fingerprint".into(),
        });
        let generation = connection.generation();
        assert!(connection.invalidate() > generation);
        assert!(connection.resolved.is_none());
        assert!(connection.observed_key.is_none());
        assert!(connection.prompt.is_none());
        assert_eq!(connection.state, ConnectionState::Idle);
    }
    #[test]
    fn stale_generation_response_never_mutates_ui() {
        let mut connection = SshConnectionController::default();
        let old = connection.generation();
        connection.invalidate();
        assert!(!connection.apply_event(old, SshEvent::Resolved(resolved())));
        assert!(connection.resolved.is_none());
    }
    #[test]
    fn identity_requires_actual_account_and_valid_verified_openssh_key() {
        let mut connection = SshConnectionController::default();
        connection.state = ConnectionState::Connecting;
        connection.resolved = Some(resolved());
        let generation = connection.generation();
        assert!(connection.handshake_ready(generation, "deploy").is_err());
        connection.observed_key = Some(format!("SHA256:{}", "a".repeat(43)));
        assert!(
            connection
                .handshake_ready(generation, "other-account")
                .is_err()
        );
        let peer = connection.handshake_ready(generation, "deploy").unwrap();
        assert_eq!(peer.host, "server.test");
        assert_eq!(peer.account, "deploy");
        assert_eq!(connection.state, ConnectionState::Ready);
    }
}
