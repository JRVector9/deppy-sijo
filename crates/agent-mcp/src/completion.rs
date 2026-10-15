//! Bounded original-request completion transport. This module never opens a database.
use crate::{Auth, management::validate_identity};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
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
struct Stream {
    identity: CompletionIdentity,
    owner: TransportOwner,
    permit: CompletionPermit,
    reply: Option<Receiver<Result<Value, String>>>,
    result: Option<Result<Value, String>>,
    socket: Option<TcpStream>,
    reader: bool,
    primed: bool,
    started: Instant,
    disconnected: Option<Instant>,
    heartbeat: Instant,
    expires: u64,
    closed: bool,
    state: CompletionDeliveryState,
    dirty: bool,
}
#[derive(Default)]
struct Registry {
    sessions: HashMap<String, Session>,
    streams: HashMap<String, Stream>,
}
pub(crate) struct CompletionLane {
    registry: Mutex<Registry>,
    auth: Weak<Auth>,
    stop: Arc<AtomicBool>,
    redaction: secret::RedactionService,
    wake: SyncSender<()>,
    events: SyncSender<CompletionDeliveryEvent>,
}

/// Server-issued capability for this original POST, never supplied in tool arguments.
#[derive(Clone)]
pub struct DeferredTransport {
    pub(crate) lane: Arc<CompletionLane>,
    pub(crate) owner: TransportOwner,
    pub(crate) session: Option<String>,
    pub(crate) request_id: Value,
    pub(crate) handoff: SyncSender<String>,
    pub(crate) reserved: Arc<AtomicBool>,
}
impl DeferredTransport {
    pub fn reserve(
        &self,
        scope: CompletionScope,
        permit: CompletionPermit,
    ) -> Result<CompletionIdentity, String> {
        let session = self
            .session
            .as_ref()
            .ok_or("completion_transport_required_no_effect")?;
        let identity = CompletionIdentity {
            connection_id: self.owner.connection_id.clone(),
            transport_session_id: session.clone(),
            stream_id: uuid::Uuid::new_v4().to_string(),
            request_id: self.request_id.clone(),
            auth_epoch: self.owner.epoch,
            scope,
        };
        identity
            .validate()
            .map_err(|_| "invalid_completion_identity_no_effect")?;
        let at = crate::now();
        // Reserve the final framed RPC, including both nested JSON encodings and
        // the original request ID. App outcomes use fixed codes of at most 64
        // ASCII bytes; the all-fields envelopes also cover optional timestamps.
        let code = "x".repeat(64);
        let admissions = [
            json!({"status":"queued","admission":"pty_queue","bytes":u64::MAX,"submit":false,"retry":false,"completion":"not_confirmed"}),
            json!({"status":"unknown","error":code,"retry":false}),
            json!({"status":"rejected","error":code,"retry":false}),
        ];
        let outcomes = [
            json!({"status":"completed","local_turn_completed":true,"source":"verified_native_turn"}),
            json!({"status":"unknown","local_turn_completed":false,"source":"verified_native_turn","reason":code,"observed_at":u64::MAX}),
            json!({"status":"rejected","local_turn_completed":false,"source":"verified_native_turn","reason":code,"observed_at":u64::MAX}),
        ];
        for admission in &admissions {
            for outcome in &outcomes {
                CompletionReceipt {
                    identity: identity.clone(),
                    event_id: identity.event_id(),
                    admission: admission.clone(),
                    outcome: outcome.clone(),
                    created_at: at,
                    completed_at: None,
                    expires_at: at + COMPLETION_RECEIPT_TTL,
                }
                .validate()
                .map_err(|_| "completion_result_too_large_no_effect")?;
            }
        }
        if !permit.is_live() || !self.lane.authorized(&self.owner) {
            return Err("completion_revoked_no_effect".into());
        }
        let mut registry = self
            .lane
            .registry
            .try_lock()
            .map_err(|_| "completion_busy_no_effect")?;
        if registry
            .sessions
            .get(session)
            .is_none_or(|s| s.owner != self.owner || at >= s.expires)
        {
            return Err("completion_transport_required_no_effect".into());
        }
        if registry.streams.len() >= MAX_COMPLETION_RECEIPTS
            || registry
                .streams
                .values()
                .filter(|s| !s.closed && s.result.is_none())
                .count()
                >= MAX_COMPLETION_WATCHES
            || registry.streams.values().filter(|s| s.reader).count() >= MAX_READERS
        {
            return Err("completion_capacity_no_effect".into());
        }
        if registry.streams.values().any(|s| {
            !s.closed
                && s.result.is_none()
                && s.identity.scope.workspace == identity.scope.workspace
                && s.identity.scope.session_id == identity.scope.session_id
                && s.identity.scope.generation == identity.scope.generation
        }) {
            return Err("local_task_busy_no_effect".into());
        }
        if self
            .reserved
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("completion_transport_already_reserved_no_effect".into());
        }
        let _ = permit.0.wake.set(self.lane.wake.clone());
        let started = Instant::now();
        registry.streams.insert(
            identity.stream_id.clone(),
            Stream {
                identity: identity.clone(),
                owner: self.owner.clone(),
                permit,
                reply: None,
                result: None,
                socket: None,
                reader: true,
                primed: false,
                started,
                disconnected: None,
                heartbeat: started,
                expires: at + COMPLETION_RECEIPT_TTL,
                closed: false,
                state: CompletionDeliveryState::Waiting,
                dirty: true,
            },
        );
        if self.handoff.try_send(identity.stream_id.clone()).is_err() {
            registry.streams.remove(&identity.stream_id);
            return Err("completion_transport_closed_no_effect".into());
        }
        let _ = self.lane.wake.try_send(());
        Ok(identity)
    }
}

impl CompletionLane {
    pub(crate) fn new(
        auth: &Arc<Auth>,
        stop: Arc<AtomicBool>,
        redaction: secret::RedactionService,
    ) -> (Arc<Self>, Receiver<CompletionDeliveryEvent>) {
        let (events, event_rx) = mpsc::sync_channel(128);
        let (wake, wake_rx) = mpsc::sync_channel(1);
        let lane = Arc::new(Self {
            registry: Mutex::new(Registry::default()),
            auth: Arc::downgrade(auth),
            stop,
            redaction,
            wake,
            events,
        });
        let weak = Arc::downgrade(&lane);
        thread::Builder::new()
            .name("mcp-completion".into())
            .spawn(move || {
                while let Some(lane) = weak.upgrade() {
                    lane.maintain();
                    if lane.stop.load(Ordering::Acquire) {
                        break;
                    }
                    drop(lane);
                    let _ = wake_rx.recv_timeout(Duration::from_millis(20));
                }
            })
            .expect("completion maintenance thread");
        (lane, event_rx)
    }
    fn authorized(&self, owner: &TransportOwner) -> bool {
        !self.stop.load(Ordering::Acquire)
            && self.auth.upgrade().is_some_and(|auth| {
                auth.current_access(owner.epoch, owner.access_key.as_deref(), crate::now())
            })
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
    pub(crate) fn begin(&self, id: &str, reply: Receiver<Result<Value, String>>) -> bool {
        let Ok(mut registry) = self.registry.lock() else {
            return false;
        };
        let Some(stream) = registry.streams.get_mut(id).filter(|s| !s.closed) else {
            return false;
        };
        stream.reply = Some(reply);
        let _ = self.wake.try_send(());
        true
    }
    pub(crate) fn attach(&self, id: &str, socket: TcpStream) -> bool {
        let Ok(mut registry) = self.registry.lock() else {
            return false;
        };
        let Some(stream) = registry
            .streams
            .get_mut(id)
            .filter(|s| !s.closed && s.reader && s.socket.is_none())
        else {
            return false;
        };
        stream.socket = Some(socket);
        stream.primed = false;
        stream.disconnected = None;
        stream.heartbeat = Instant::now();
        let _ = self.wake.try_send(());
        true
    }
    pub(crate) fn resume(
        &self,
        owner: &TransportOwner,
        session: &str,
        cursor: &str,
    ) -> Result<String, u16> {
        let (id, sequence) = cursor.rsplit_once(':').ok_or(404u16)?;
        if !matches!(sequence, "0" | "1") {
            return Err(404);
        }
        let mut registry = self.registry.lock().map_err(|_| 503u16)?;
        let readers = registry.streams.values().filter(|s| s.reader).count();
        let stream = registry.streams.get_mut(id).ok_or(404u16)?;
        if stream.closed
            || stream.owner != *owner
            || stream.identity.transport_session_id != session
            || !stream.permit.is_live()
            || !self.authorized(owner)
            || stream
                .disconnected
                .is_some_and(|at| at.elapsed() >= RECONNECT_TTL)
            || crate::now() >= stream.expires
            || (sequence == "1" && stream.result.is_none())
        {
            return Err(404);
        }
        if stream.reader {
            return Err(409);
        }
        if sequence == "1" {
            return Err(204);
        }
        if readers >= MAX_READERS {
            return Err(503);
        }
        stream.reader = true;
        Ok(id.into())
    }
    pub(crate) fn abandon_reader(&self, id: &str) {
        if let Ok(mut registry) = self.registry.lock()
            && let Some(stream) = registry.streams.get_mut(id)
        {
            stream.disconnect();
        }
    }
    fn maintain(&self) {
        let mut writes = Vec::new();
        {
            let Ok(mut registry) = self.registry.lock() else {
                return;
            };
            registry.sessions.retain(|_, s| crate::now() < s.expires);
            registry.streams.retain(|_, s| crate::now() < s.expires);
            for (id, stream) in &mut registry.streams {
                if !stream.closed
                    && (!stream.permit.is_live()
                        || !self.authorized(&stream.owner)
                        || (stream.result.is_none() && stream.started.elapsed() >= WATCH_TTL)
                        || stream
                            .disconnected
                            .is_some_and(|at| at.elapsed() >= RECONNECT_TTL))
                {
                    stream.closed = true;
                    stream.reader = false;
                    if let Some(socket) = stream.socket.take() {
                        let _ = socket.shutdown(Shutdown::Both);
                    }
                    stream.reply.take();
                    stream.state = CompletionDeliveryState::Closed;
                    stream.dirty = true;
                }
                if !stream.closed
                    && stream.result.is_none()
                    && let Some(reply) = &stream.reply
                {
                    let result = match reply.try_recv() {
                        Ok(result) => Some(result),
                        Err(TryRecvError::Disconnected) => {
                            Some(Err("outcome_unknown_do_not_retry_input".into()))
                        }
                        Err(TryRecvError::Empty) => None,
                    };
                    if let Some(result) = result {
                        stream.reply = None;
                        stream.result = Some(result);
                        stream.expires = crate::now() + COMPLETION_RECEIPT_TTL;
                        stream.state = CompletionDeliveryState::Ready;
                        stream.dirty = true;
                    }
                }
                if stream.dirty
                    && self
                        .events
                        .try_send(CompletionDeliveryEvent {
                            identity: stream.identity.clone(),
                            state: stream.state,
                        })
                        .is_ok()
                {
                    stream.dirty = false;
                }
                if !stream.closed
                    && let Some(socket) = stream.socket.take()
                {
                    writes.push(SocketWrite {
                        id: id.clone(),
                        socket,
                        identity: stream.identity.clone(),
                        owner: stream.owner.clone(),
                        permit: stream.permit.clone(),
                        primed: stream.primed,
                        heartbeat_due: Instant::now() >= stream.heartbeat,
                        result: stream.result.clone(),
                    });
                }
            }
        }
        for mut action in writes {
            let result = self.write_socket(&mut action);
            let Ok(mut registry) = self.registry.lock() else {
                continue;
            };
            let Some(stream) = registry.streams.get_mut(&action.id) else {
                continue;
            };
            if stream.closed || !stream.permit.is_live() {
                let _ = action.socket.shutdown(Shutdown::Both);
                stream.reader = false;
            } else if result.is_err() {
                let _ = action.socket.shutdown(Shutdown::Both);
                stream.disconnect();
            } else if action.result.is_some() {
                let _ = action.socket.shutdown(Shutdown::Write);
                stream.reader = false;
                stream.state = CompletionDeliveryState::ReturnedUnacknowledged;
                stream.dirty = true;
            } else {
                stream.primed = true;
                if !action.primed || action.heartbeat_due {
                    stream.heartbeat = Instant::now() + HEARTBEAT;
                }
                stream.socket = Some(action.socket);
            }
        }
    }
    fn write_socket(&self, action: &mut SocketWrite) -> std::io::Result<()> {
        action.socket.set_nonblocking(true)?;
        let peer_closed = matches!(action.socket.peek(&mut [0]), Ok(0));
        action.socket.set_nonblocking(false)?;
        if peer_closed {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        let mut bytes = Vec::new();
        if !action.primed {
            bytes.extend_from_slice(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\nMCP-Session-Id: {}\r\n\r\nid: {}:0\nretry: 1000\ndata:\n\n", action.identity.transport_session_id, action.identity.stream_id).as_bytes());
        }
        if let Some(result) = &action.result {
            let mut result = result.clone();
            if let Ok(value) = &mut result {
                redact_display(value, &self.redaction.snapshot(), true);
            }
            let mut frame = completion_frame(&action.identity, &result);
            if frame.len() > MAX_COMPLETION_EVENT_BYTES {
                frame = completion_frame(
                    &action.identity,
                    &Err("completion_result_too_large_unknown".into()),
                );
            }
            bytes.extend_from_slice(frame.as_bytes());
        } else if action.primed && action.heartbeat_due {
            bytes.extend_from_slice(b": keepalive\n\n");
        }
        let deadline = Instant::now() + WRITE_TIMEOUT;
        let mut offset = 0;
        while offset < bytes.len() {
            // Each syscall is a fresh admission. No credential/registry mutex crosses I/O.
            if !action.permit.is_live() || !self.authorized(&action.owner) {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::TimedOut))?;
            action.socket.set_write_timeout(Some(remaining))?;
            let written = action.socket.write(&bytes[offset..])?;
            if written == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            offset += written;
        }
        action.socket.flush()
    }
}

impl Stream {
    fn disconnect(&mut self) {
        self.reader = false;
        self.disconnected.get_or_insert_with(Instant::now);
        self.state = CompletionDeliveryState::Disconnected;
        self.dirty = true;
    }
}
struct SocketWrite {
    id: String,
    socket: TcpStream,
    identity: CompletionIdentity,
    owner: TransportOwner,
    permit: CompletionPermit,
    primed: bool,
    heartbeat_due: bool,
    result: Option<Result<Value, String>>,
}
fn redact_display(
    value: &mut Value,
    redaction: &secret::RedactionSnapshot<'_>,
    preserve_routing: bool,
) {
    match value {
        Value::String(text) => *text = redaction.redact_wrapped_text(text),
        Value::Array(values) => {
            for value in values {
                redact_display(value, redaction, false);
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                if !preserve_routing
                    || !matches!(
                        key.as_str(),
                        "event_id"
                            | "operation_id"
                            | "run_id"
                            | "session_id"
                            | "generation"
                            | "workspace"
                    )
                {
                    redact_display(value, redaction, false);
                }
            }
        }
        _ => {}
    }
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
