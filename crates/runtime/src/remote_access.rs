//! Grant-bound TLS access. The legacy administrator token remains a separate authority.
use crate::remote_worker::{RemoteWorkIdentity, RemoteWorkerConfig, RemoteWorkerSlot};
use crate::{RuntimeCommand, RuntimeEvent};
use deppy_core::WorkspaceId;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};

pub(crate) struct ScopedRemoteAccess {
    token: secret::SecretString,
    device_id: String,
    workspace_id: WorkspaceId,
    grant_epoch: u64,
    current: Arc<AtomicBool>,
}
impl ScopedRemoteAccess {
    pub(crate) fn new(config: &RemoteWorkerConfig) -> Self {
        Self {
            token: secret::SecretString::new(secret::token::random_hex_token()),
            device_id: config.device_id.clone(),
            workspace_id: config.workspace_id.clone(),
            grant_epoch: config.grant_epoch,
            current: Arc::new(AtomicBool::new(true)),
        }
    }
    pub(crate) fn credential(
        &self,
        device: &str,
        workspace: &WorkspaceId,
        epoch: u64,
    ) -> anyhow::Result<secret::SecretString> {
        anyhow::ensure!(
            self.current.load(Ordering::Acquire)
                && self.device_id == device
                && self.workspace_id == *workspace
                && self.grant_epoch == epoch,
            "remote_worker_grant_changed"
        );
        Ok(secret::SecretString::new(self.token.expose().to_owned()))
    }
    pub(crate) fn authenticate(
        &self,
        provided: &[u8],
        slot: Weak<Mutex<RemoteWorkerSlot>>,
    ) -> Option<RemoteConnectionScope> {
        (self.current.load(Ordering::Acquire)
            && crate::remote::token_matches(self.token.expose(), provided))
        .then(|| RemoteConnectionScope {
            device_id: self.device_id.clone(),
            workspace_id: self.workspace_id.clone(),
            grant_epoch: self.grant_epoch,
            current: Arc::clone(&self.current),
            slot,
        })
    }
    pub(crate) fn revoke(&self) {
        self.current.store(false, Ordering::Release);
    }
}
impl Drop for ScopedRemoteAccess {
    fn drop(&mut self) {
        self.revoke();
    }
}

pub(crate) struct RemoteConnectionScope {
    device_id: String,
    workspace_id: WorkspaceId,
    grant_epoch: u64,
    current: Arc<AtomicBool>,
    slot: Weak<Mutex<RemoteWorkerSlot>>,
}
impl RemoteConnectionScope {
    pub(crate) fn is_current(&self) -> bool {
        self.current.load(Ordering::Acquire)
    }
    fn matches(&self, id: &RemoteWorkIdentity) -> bool {
        self.device_id == id.device_id
            && self.workspace_id == id.workspace_id
            && self.grant_epoch == id.grant_epoch
    }
    pub(crate) fn allows_command(&self, command: &RuntimeCommand) -> bool {
        self.is_current()
            && match command {
                RuntimeCommand::InspectRemoteWorker { .. } => true,
                RuntimeCommand::SpawnRemoteWork { request } => self.matches(&request.identity),
                RuntimeCommand::RemoteTaskInput { request } => {
                    self.matches(&request.target().identity)
                }
                _ => false,
            }
    }
    pub(crate) fn allows_event(&self, event: &RuntimeEvent) -> bool {
        if !self.is_current() {
            return false;
        }
        match event {
            RuntimeEvent::RemoteWorkerObserved {
                capabilities: Some(c),
                ..
            } => {
                c.device_id == self.device_id
                    && c.workspace_id == self.workspace_id
                    && c.grant_epoch == self.grant_epoch
            }
            RuntimeEvent::RemoteWorkResolved { identity, .. } => self.matches(identity),
            RuntimeEvent::RemoteTaskInput { target, .. } => self.matches(&target.identity),
            RuntimeEvent::SessionInputAccepted {
                session,
                operation_id: Some(op),
                ..
            }
            | RuntimeEvent::InputAdmitted {
                session,
                operation_id: op,
                ..
            } => self.managed_session(*session, Some(op)),
            RuntimeEvent::SessionStatusChanged { session, .. }
            | RuntimeEvent::SessionExited { session, .. } => self.managed_session(*session, None),
            // Never export mux/layout, raw viewport/history, admin launch/command results,
            // credentials, or activity from another project. Scoped terminal access is a
            // bounded exact-target RemoteTaskInput::ObserveSession response only.
            _ => false,
        }
    }
    fn managed_session(&self, session: crate::SessionId, operation: Option<&str>) -> bool {
        let Some(slot) = self.slot.upgrade() else {
            return false;
        };
        let Ok(slot) = slot.lock() else {
            return false;
        };
        let Some(host) = &slot.host else {
            return false;
        };
        self.is_current()
            && host.executions.iter().any(|record| {
                self.matches(&record.target.identity)
                    && record.target.session == session
                    && operation
                        .is_none_or(|op| record.inputs.iter().any(|(id, _, _)| id.as_str() == op))
            })
    }
}
