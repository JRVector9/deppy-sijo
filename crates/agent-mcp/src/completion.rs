//! Bounded original-request completion transport. This module never opens a database.
use crate::{Auth, management::validate_identity};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

pub const MAX_COMPLETION_EVENT_BYTES: usize = 1024;
pub const MAX_COMPLETION_WATCHES: usize = 64;
pub const MAX_COMPLETION_RECEIPTS: usize = 1000;
pub const COMPLETION_RECEIPT_TTL: u64 = 24 * 60 * 60;
const MAX_READERS: usize = 16;
const MAX_SESSIONS: usize = 256;
const WATCH_TTL: Duration = Duration::from_secs(60 * 60);
const RECONNECT_TTL: Duration = Duration::from_secs(10 * 60);
const HEARTBEAT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_millis(50);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionScope {
    pub workspace: String,
    pub session_id: String,
    pub generation: String,
    pub operation_id: String,
    pub run_id: Option<String>,
}
impl CompletionScope {
    pub fn validate(&self) -> anyhow::Result<()> {
        for value in [&self.workspace, &self.session_id, &self.generation] {
            validate_identity(value)?;
        }
        crate::management::validate_operation_id(&self.operation_id)?;
        if let Some(run) = &self.run_id {
            validate_identity(run)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionIdentity {
    pub connection_id: String,
    pub transport_session_id: String,
    pub stream_id: String,
    pub request_id: Value,
    pub auth_epoch: u64,
    pub scope: CompletionScope,
}
impl CompletionIdentity {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.scope.validate()?;
        validate_identity(&self.connection_id)?;
        for id in [&self.transport_session_id, &self.stream_id] {
            anyhow::ensure!(
                uuid::Uuid::parse_str(id).is_ok(),
                "invalid_completion_identity"
            );
        }
        anyhow::ensure!(
            (self.request_id.is_string() || self.request_id.is_number())
                && self.request_id.to_string().len() <= 128
                && self.auth_epoch > 0,
            "invalid_completion_identity"
        );
        anyhow::ensure!(self.encoded_len()? <= 2048, "completion_identity_too_large");
        Ok(())
    }
    pub fn encoded_len(&self) -> anyhow::Result<usize> {
        Ok(serde_json::to_vec(self)?.len())
    }
    pub fn event_id(&self) -> String {
        format!("{}:1", self.stream_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionReceipt {
    pub identity: CompletionIdentity,
    pub event_id: String,
    pub admission: Value,
    pub outcome: Value,
    pub created_at: u64,
    pub completed_at: Option<u64>,
    pub expires_at: u64,
}
impl CompletionReceipt {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.identity.validate()?;
        anyhow::ensure!(
            self.event_id == self.identity.event_id()
                && self.admission.is_object()
                && self.outcome.is_object()
                && self.expires_at >= self.created_at
                && self.expires_at.saturating_sub(self.created_at) <= COMPLETION_RECEIPT_TTL + 3600
                && self
                    .completed_at
                    .is_none_or(|at| at >= self.created_at && at <= self.expires_at),
            "invalid_completion_receipt"
        );
        anyhow::ensure!(
            completion_frame(&self.identity, &Ok(self.tool_result())).len()
                <= MAX_COMPLETION_EVENT_BYTES,
            "completion_result_too_large"
        );
        Ok(())
    }
    /// Routing identifiers are opaque, bounded values; terminal output and prompts are excluded.
    pub fn tool_result(&self) -> Value {
        let mut value = json!({
            "event_id": self.event_id,
            "operation_id": self.identity.scope.operation_id,
            "session_id": self.identity.scope.session_id,
            "generation": self.identity.scope.generation,
            "admission": self.admission,
            "local_completion": self.outcome,
            "retry": false,
        });
        if let Some(run) = &self.identity.scope.run_id {
            value["run_id"] = json!(run);
        }
        if let Some(status) = self.outcome.get("status") {
            value["status"] = status.clone();
        }
        value
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionDeliveryState {
    Waiting,
    Ready,
    ReturnedUnacknowledged,
    Disconnected,
    Closed,
}
#[derive(Clone, Debug)]
pub struct CompletionDeliveryEvent {
    pub identity: CompletionIdentity,
    pub state: CompletionDeliveryState,
}

struct PermitState {
    live: AtomicBool,
    wake: OnceLock<SyncSender<()>>,
}
/// Revocation is an atomic admission fence, and never waits for socket I/O.
/// A write admitted before revocation has an absolute 50ms lifetime.
#[derive(Clone)]
pub struct CompletionPermit(Arc<PermitState>);
impl Default for CompletionPermit {
    fn default() -> Self {
        Self::new()
    }
}
impl CompletionPermit {
    pub fn new() -> Self {
        Self(Arc::new(PermitState {
            live: AtomicBool::new(true),
            wake: OnceLock::new(),
        }))
    }
    pub fn revoke(&self) {
        self.0.live.store(false, Ordering::Release);
        if let Some(wake) = self.0.wake.get() {
            let _ = wake.try_send(());
        }
    }
    pub fn is_live(&self) -> bool {
        self.0.live.load(Ordering::Acquire)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TransportOwner {
    pub connection_id: String,
    pub epoch: u64,
    pub access_key: Option<String>,
}
struct Session {
    owner: TransportOwner,
    expires: u64,
}
#[derive(Default)]
struct Registry {
    sessions: HashMap<String, Session>,
}
pub(crate) struct CompletionLane {
    registry: Mutex<Registry>,
}

/// Server-issued capability for this original POST, never supplied in tool arguments.
#[derive(Clone)]
pub struct DeferredTransport {
    pub(crate) lane: Arc<CompletionLane>,
    pub(crate) owner: TransportOwner,
    pub(crate) session: Option<String>,
    pub(crate) request_id: Value,
    pub(crate) handoff: SyncSender<String>,
}
impl DeferredTransport {
    pub fn reserve(
        &self,
        _scope: CompletionScope,
        _permit: CompletionPermit,
    ) -> Result<CompletionIdentity, String> {
        Err("completion_unavailable_no_effect".into())
    }
}

impl CompletionLane {
    pub(crate) fn new(
        _auth: &Arc<Auth>,
        _stop: Arc<AtomicBool>,
        _redaction: secret::RedactionService,
    ) -> (Arc<Self>, Receiver<CompletionDeliveryEvent>) {
        let (_events, event_rx) = mpsc::sync_channel(128);
        (
            Arc::new(Self {
                registry: Mutex::new(Registry::default()),
            }),
            event_rx,
        )
    }
    pub(crate) fn initialize(&self, owner: TransportOwner, expires: u64) -> Option<String> {
        let mut registry = self.registry.lock().ok()?;
        registry.sessions.retain(|_, s| crate::now() < s.expires);
        if registry.sessions.len() >= MAX_SESSIONS {
            return None;
        }
        let id = uuid::Uuid::new_v4().to_string();
        registry
            .sessions
            .insert(id.clone(), Session { owner, expires });
        Some(id)
    }
    pub(crate) fn owns_session(&self, owner: &TransportOwner, session: &str) -> bool {
        self.registry.lock().is_ok_and(|registry| {
            registry
                .sessions
                .get(session)
                .is_some_and(|s| s.owner == *owner && crate::now() < s.expires)
        })
    }
    pub(crate) fn begin(&self, _stream: &str, _reply: Receiver<Result<Value, String>>) -> bool {
        false
    }
    pub(crate) fn attach(&self, _id: &str, _socket: TcpStream) -> bool {
        false
    }
    pub(crate) fn resume(
        &self,
        _owner: &TransportOwner,
        _session: &str,
        _cursor: &str,
    ) -> Result<String, u16> {
        Err(404)
    }
    pub(crate) fn abandon_reader(&self, _id: &str) {}
}

pub(crate) fn completion_frame(
    identity: &CompletionIdentity,
    result: &Result<Value, String>,
) -> String {
    format!(
        "id: {}\nevent: message\ndata: {}\n\n",
        identity.event_id(),
        crate::server::tool_rpc_value(identity.request_id.clone(), result.clone())
    )
}
