use crate::{AuthKind, ConnectionInfo, ConnectionState, LEGACY_CONNECTION_ID, Principal};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{BufReader, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use web_remote::http::{self, Response};
use zeroize::Zeroizing;

pub const TOKEN_TTL: u64 = 24 * 60 * 60;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024;
const MAX_UNREAD_REQUEST_BYTES: usize = http::MAX_HEAD_BYTES + MAX_REQUEST_BODY_BYTES;
const REJECT_CLOSE_TIMEOUT: Duration = Duration::from_millis(100);
const BUSY_CLOSE_TIMEOUT: Duration = Duration::from_millis(10);
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
struct Credential {
    token: Zeroizing<String>,
    epoch: u64,
    expires: u64,
    oauth: crate::oauth::OAuth,
    manual: HashMap<String, ManualAccess>,
    connections: HashMap<String, ConnectionInfo>,
    budgets: HashMap<String, CallBudget>,
    redaction: secret::RedactionService,
}
struct ManualAccess {
    principal: Principal,
    _lease: secret::RedactionLease,
}
#[derive(Default)]
struct CallBudget {
    epoch: u64,
    window: u64,
    polls: u8,
    active: u8,
    active_polls: u8,
}
pub struct Auth(Mutex<Credential>);
impl Auth {
    pub fn new_at(time: u64) -> Self {
        Self::with_redaction(time, secret::RedactionService::new())
    }
    fn with_redaction(time: u64, redaction: secret::RedactionService) -> Self {
        let principal = legacy_principal(time);
        Self(Mutex::new(Credential {
            token: make_token(),
            epoch: 1,
            expires: time + TOKEN_TTL,
            oauth: crate::oauth::OAuth::new(redaction.clone()),
            manual: HashMap::new(),
            connections: HashMap::from([(principal.id.clone(), ConnectionInfo::new(principal))]),
            budgets: HashMap::new(),
            redaction,
        }))
    }
    pub fn token_for_user(&self) -> Zeroizing<String> {
        self.0.lock().unwrap().token.clone()
    }
    pub fn expires(&self) -> u64 {
        self.0.lock().unwrap().expires
    }
    pub fn rotate_at(&self, time: u64) {
        let mut c = self.0.lock().unwrap();
        c.oauth.clear();
        c.manual.clear();
        c.connections.clear();
        c.budgets.clear();
        let principal = legacy_principal(time);
        c.connections
            .insert(principal.id.clone(), ConnectionInfo::new(principal));
        c.token = make_token();
        c.epoch += 1;
        c.expires = time + TOKEN_TTL;
    }
    pub fn revoke(&self) {
        let mut c = self.0.lock().unwrap();
        c.epoch += 1;
        c.expires = 0;
        c.oauth.clear();
        c.manual.clear();
        for info in c.connections.values_mut() {
            info.state = ConnectionState::Revoked;
        }
        c.token = Zeroizing::new(String::new());
    }
    pub fn authenticate(&self, header: &str, time: u64) -> Option<u64> {
        self.authenticate_scoped(header, time)
            .map(|(epoch, _, _)| epoch)
    }
    pub fn authenticate_principal(&self, header: &str, time: u64) -> Option<Principal> {
        self.authenticate_scoped(header, time)
            .map(|(_, principal, _)| principal)
    }
    fn authenticate_scoped(
        &self,
        header: &str,
        time: u64,
    ) -> Option<(u64, Principal, Option<String>)> {
        let supplied = header.strip_prefix("Bearer ")?;
        let mut c = self.0.lock().ok()?;
        if time >= c.expires || supplied.is_empty() {
            return None;
        }
        let (mut principal, key) = if bool::from(c.token.as_bytes().ct_eq(supplied.as_bytes())) {
            (
                c.connections.get(LEGACY_CONNECTION_ID)?.principal.clone(),
                None,
            )
        } else {
            let key = crate::oauth::hash(supplied);
            let principal = c
                .manual
                .get(&key)
                .filter(|a| time < a.principal.expires_at)
                .map(|a| a.principal.clone())
                .or_else(|| c.oauth.principal_key(&key, time))?;
            (principal, Some(key))
        };
        if c.connections
            .get(&principal.id)
            .is_some_and(|info| info.state == ConnectionState::Revoked)
        {
            return None;
        }
        principal.authenticated_at = time;
        let connection_expiry = c
            .oauth
            .connection_expiry(&principal.id)
            .unwrap_or(principal.expires_at);
        if !c.connections.contains_key(&principal.id) && !make_registry_room(&mut c, time) {
            return None;
        }
        let info = c
            .connections
            .entry(principal.id.clone())
            .or_insert_with(|| ConnectionInfo::new(principal.clone()));
        principal.name.clone_from(&info.principal.name);
        let last_authenticated_at = info.last_authenticated_at.unwrap_or(0).max(time);
        info.principal = principal.clone();
        info.principal.authenticated_at = last_authenticated_at;
        info.principal.expires_at = connection_expiry;
        info.last_authenticated_at = Some(last_authenticated_at);
        info.state = ConnectionState::Active;
        Some((c.epoch, principal, key))
    }
    pub fn connections(&self) -> Vec<ConnectionInfo> {
        self.connections_at(now())
    }
    pub fn connections_at(&self, time: u64) -> Vec<ConnectionInfo> {
        let c = self.0.lock().unwrap();
        let mut connections: Vec<_> = c
            .connections
            .values()
            .cloned()
            .map(|mut info| {
                if info.state != ConnectionState::Revoked && time >= info.principal.expires_at {
                    info.state = ConnectionState::Expired;
                }
                info
            })
            .collect();
        connections.sort_by(|a, b| a.principal.id.cmp(&b.principal.id));
        connections
    }
    pub fn rename_connection(&self, id: &str, name: &str) -> bool {
        if !crate::principal::valid_connection_name(name) {
            return false;
        }
        let mut c = self.0.lock().unwrap();
        let Some(info) = c.connections.get_mut(id) else {
            return false;
        };
        info.principal.name = name.trim().to_owned();
        true
    }
    /// Revoke only this credential-issued connection. Serialized with queue admission.
    pub fn revoke_connection(&self, id: &str) -> bool {
        let mut c = self.0.lock().unwrap();
        let Some(info) = c.connections.get_mut(id) else {
            return false;
        };
        info.state = ConnectionState::Revoked;
        c.manual.retain(|_, access| access.principal.id != id);
        c.oauth.revoke_connection(id);
        true
    }
    /// Local-owner API: the returned credential is shown once and never enters a snapshot.
    pub fn mint_manual_connection(
        &self,
        name: &str,
        input_scope: bool,
        time: u64,
    ) -> Option<(Principal, Zeroizing<String>)> {
        if !crate::principal::valid_connection_name(name) {
            return None;
        }
        let mut c = self.0.lock().ok()?;
        if time >= c.expires {
            return None;
        }
        c.manual
            .retain(|_, access| time < access.principal.expires_at);
        if c.manual.len() >= 32 || !make_registry_room(&mut c, time) {
            return None;
        }
        let token = make_token();
        let secret = secret::SecretString::new(token.to_string());
        let lease = c.redaction.acquire_execution_lease(&[&secret]).ok()?;
        let principal = Principal {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.trim().into(),
            auth_kind: AuthKind::ManualBearer,
            input_scope,
            authenticated_at: time,
            expires_at: c.expires,
        };
        c.connections
            .insert(principal.id.clone(), ConnectionInfo::new(principal.clone()));
        c.manual.insert(
            crate::oauth::hash(&token),
            ManualAccess {
                principal: principal.clone(),
                _lease: lease,
            },
        );
        Some((principal, token))
    }
    fn observe_initialize(&self, id: &str) {
        if let Some(info) = self.0.lock().unwrap().connections.get_mut(id) {
            info.last_initialize_at = Some(now());
        }
    }
    fn observe_tool_success(&self, id: &str, epoch: u64) {
        let mut c = self.0.lock().unwrap();
        if c.epoch != epoch {
            return;
        }
        if let Some(info) = c
            .connections
            .get_mut(id)
            .filter(|info| info.state != ConnectionState::Revoked)
        {
            info.last_tool_success_at = Some(now());
        }
    }
    fn begin_call(&self, id: &str, poll: bool, epoch: u64) -> bool {
        let mut c = self.0.lock().unwrap();
        if c.epoch != epoch
            || c.connections
                .get(id)
                .is_none_or(|info| info.state == ConnectionState::Revoked)
        {
            return false;
        }
        let budget = c.budgets.entry(id.into()).or_default();
        if budget.epoch != epoch {
            *budget = CallBudget {
                epoch,
                ..Default::default()
            };
        }
        let time = now();
        if budget.window != time {
            budget.window = time;
            budget.polls = 0;
        }
        if budget.active >= 2 || (poll && (budget.active_polls >= 1 || budget.polls >= 8)) {
            return false;
        }
        budget.active += 1;
        if poll {
            budget.active_polls += 1;
            budget.polls += 1;
        }
        true
    }
    fn end_call(&self, id: &str, poll: bool, epoch: u64) {
        if let Some(budget) = self
            .0
            .lock()
            .unwrap()
            .budgets
            .get_mut(id)
            .filter(|budget| budget.epoch == epoch)
        {
            budget.active = budget.active.saturating_sub(1);
            if poll {
                budget.active_polls = budget.active_polls.saturating_sub(1);
            }
        }
    }
    pub fn approvals(&self) -> Vec<crate::Approval> {
        let mut c = self.0.lock().unwrap();
        if now() >= c.expires {
            return vec![];
        }
        c.oauth.approvals(now())
    }
    pub fn approve(&self, id: &str, allow: bool) -> bool {
        let mut c = self.0.lock().unwrap();
        now() < c.expires && c.oauth.approve(id, allow, now())
    }
    fn oauth_route(
        &self,
        h: &http::RequestHead,
        body: &[u8],
        base: &str,
        headers: &mut Vec<(String, String)>,
        wake: &dyn Fn(),
    ) -> Response {
        // Route mutation and approval publication are serialized by the credential
        // mutex. Notify only after releasing it: the supplied callback may read Auth.
        let notification_requested = std::cell::Cell::new(false);
        let response = {
            let mut c = self.0.lock().unwrap();
            let expiry = c.expires;
            let time = now();
            let response = c.oauth.route(h, body, base, time, expiry, headers, &|| {
                notification_requested.set(true);
            });
            if h.path == "/oauth/token" && response.status == 200 {
                sync_oauth_connections(&mut c, time);
            }
            response
        };
        if notification_requested.get() {
            wake();
        }
        response
    }
    pub fn current(&self, epoch: u64, time: u64) -> bool {
        let c = self.0.lock().unwrap();
        c.epoch == epoch && time < c.expires
    }
    pub fn current_access(&self, epoch: u64, access_key: Option<&str>, time: u64) -> bool {
        let c = self.0.lock().unwrap();
        credential_is_current(&c, epoch, access_key, time)
    }
    /// Serialize credential replacement/revocation with a short queue admission.
    /// The callback must not invoke events, user code or authentication again.
    pub fn admit_current_access(
        &self,
        epoch: u64,
        access_key: Option<&str>,
        write: &mut dyn FnMut(),
    ) {
        let c = self.0.lock().unwrap();
        if credential_is_current(&c, epoch, access_key, now()) {
            write();
        }
    }
}
fn credential_is_current(c: &Credential, epoch: u64, access_key: Option<&str>, time: u64) -> bool {
    c.epoch == epoch
        && time < c.expires
        && match access_key {
            None => c
                .connections
                .get(LEGACY_CONNECTION_ID)
                .is_some_and(|info| info.state != ConnectionState::Revoked),
            Some(key) => {
                c.manual
                    .get(key)
                    .is_some_and(|access| time < access.principal.expires_at)
                    || c.oauth.authenticate_key(key, time).is_some()
            }
        }
}
fn legacy_principal(time: u64) -> Principal {
    Principal {
        id: LEGACY_CONNECTION_ID.into(),
        name: "Shared manual bearer".into(),
        auth_kind: AuthKind::ManualBearer,
        input_scope: true,
        authenticated_at: time,
        expires_at: time + TOKEN_TTL,
    }
}
fn sync_oauth_connections(c: &mut Credential, time: u64) {
    let principals = c.oauth.live_principals(time);
    for mut principal in principals {
        if !c.connections.contains_key(&principal.id) && !make_registry_room(c, time) {
            continue;
        }
        principal.expires_at = c
            .oauth
            .connection_expiry(&principal.id)
            .unwrap_or(principal.expires_at);
        let info = c
            .connections
            .entry(principal.id.clone())
            .or_insert_with(|| ConnectionInfo::new(principal.clone()));
        principal.name.clone_from(&info.principal.name);
        let updated = info.last_authenticated_at.is_none()
            || principal.authenticated_at > info.principal.authenticated_at;
        principal.authenticated_at = principal
            .authenticated_at
            .max(info.principal.authenticated_at);
        info.principal = principal;
        if updated {
            info.last_authenticated_at = Some(info.last_authenticated_at.unwrap_or(0).max(time));
        }
    }
}
fn make_registry_room(c: &mut Credential, time: u64) -> bool {
    if c.connections.len() < 65 {
        return true;
    }
    let retired = c
        .connections
        .iter()
        .filter(|(id, info)| {
            id.as_str() != LEGACY_CONNECTION_ID
                && (info.state == ConnectionState::Revoked || time >= info.principal.expires_at)
        })
        .min_by_key(|(_, info)| info.principal.authenticated_at)
        .map(|(id, _)| id.clone());
    if let Some(id) = retired {
        c.connections.remove(&id);
        c.budgets.remove(&id);
        true
    } else {
        false
    }
}
fn make_token() -> Zeroizing<String> {
    Zeroizing::new(format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    ))
}

// No Debug: args may contain secrets and private answers.
pub struct Request {
    pub epoch: u64,
    pub principal: Principal,
    /// Non-secret credential fingerprint; the legacy bearer is bound by epoch and its connection state.
    pub access_key: Option<String>,
    pub input_scope: bool,
    pub deadline: Instant,
    pub tool: String,
    pub args: Value,
    pub reply: SyncSender<Result<Value, String>>,
    pub deferred: Option<crate::DeferredTransport>,
}
impl Request {
    pub fn live(&self, auth: &Auth) -> bool {
        Instant::now() < self.deadline
            && auth.current_access(self.epoch, self.access_key.as_deref(), now())
    }
}
pub struct Server {
    pub addr: SocketAddr,
    pub auth: Arc<Auth>,
    pub requests: Receiver<Request>,
    pub delivery_events: Receiver<crate::CompletionDeliveryEvent>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    public_host: Arc<std::sync::RwLock<String>>,
}
impl Server {
    pub fn start(
        port: u16,
        public_host: &str,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        Self::start_with_redaction(port, public_host, secret::RedactionService::new(), wake)
    }
    pub fn start_with_redaction(
        port: u16,
        public_host: &str,
        redaction: secret::RedactionService,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(valid_public_host(public_host), "invalid_public_hostname");
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let auth = Arc::new(Auth::with_redaction(now(), redaction));
        let stop = Arc::new(AtomicBool::new(false));
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let (completion, delivery_events) = crate::completion::CompletionLane::new(
            &auth,
            stop.clone(),
            auth.0.lock().unwrap().redaction.clone(),
            wake.clone(),
        );
        let (tx, requests) = mpsc::sync_channel(16);
        let (a, s, w) = (auth.clone(), stop.clone(), wake);
        let host = Arc::new(std::sync::RwLock::new(public_host.to_owned()));
        let public_host = host.clone();
        let active = Arc::new(AtomicUsize::new(0));
        let worker = thread::Builder::new()
            .name("agent-mcp".into())
            .spawn(move || {
                while !s.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            // macOS accepted sockets inherit listener O_NONBLOCK.
                            // DeadlineReader needs blocking reads with bounded timeouts.
                            if stream.set_nonblocking(false).is_err() {
                                continue;
                            }
                            let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
                            if active.load(Ordering::Acquire) >= 4 {
                                let deadline = Instant::now() + BUSY_CLOSE_TIMEOUT;
                                if http::write_response_with_headers(
                                    &mut DeadlineWriter {
                                        stream: &mut stream,
                                        deadline,
                                    },
                                    &busy_response(),
                                    &[("Retry-After".into(), "1".into())],
                                )
                                .is_ok()
                                {
                                    close_response(&mut stream, MAX_UNREAD_REQUEST_BYTES, deadline);
                                }
                                continue;
                            }
                            active.fetch_add(1, Ordering::AcqRel);
                            let (a, tx, w, active, host, completion) = (
                                a.clone(),
                                tx.clone(),
                                w.clone(),
                                active.clone(),
                                host.clone(),
                                completion.clone(),
                            );
                            thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
                                let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
                                let public = host.read().unwrap().clone();
                                handle(&mut stream, addr, &public, &a, &tx, &*w, completion);
                                active.fetch_sub(1, Ordering::AcqRel);
                            });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10))
                        }
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Self {
            addr,
            auth,
            requests,
            delivery_events,
            stop,
            worker: Some(worker),
            public_host,
        })
    }

    /// Publish the generated endpoint once, before exposing URL/token controls.
    /// Replacing an active OAuth issuer requires stopping and restarting the server.
    pub fn set_public_host(&self, host: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            !host.is_empty() && valid_public_host(host),
            "invalid_public_hostname"
        );
        let mut current = self.public_host.write().unwrap();
        anyhow::ensure!(
            current.is_empty() || current.as_str() == host,
            "public_host_already_set"
        );
        *current = host.to_owned();
        Ok(())
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.auth.revoke();
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
pub fn valid_public_host(host: &str) -> bool {
    if host.is_empty() {
        return true;
    }
    let domain = if let Some((domain, port)) = host.rsplit_once(':') {
        if !port.parse::<u16>().is_ok_and(|p| p > 0) {
            return false;
        }
        domain
    } else {
        host
    };
    domain.len() <= 253
        && domain.contains('.')
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
}

fn supported(version: &str) -> bool {
    matches!(version, "2025-03-26" | "2025-06-18" | "2025-11-25")
}
fn handle(
    stream: &mut TcpStream,
    addr: SocketAddr,
    public: &str,
    auth: &Auth,
    tx: &SyncSender<Request>,
    wake: &dyn Fn(),
    completion: Arc<crate::completion::CompletionLane>,
) {
    let mut metadata = ResponseMetadata {
        headers: Vec::new(),
        unread_bytes: MAX_UNREAD_REQUEST_BYTES,
        completion: Some(completion.clone()),
        deferred: None,
    };
    let response = process(stream, addr, public, auth, tx, wake, &mut metadata);
    if let Some(id) = metadata.deferred {
        if !stream
            .try_clone()
            .is_ok_and(|socket| completion.attach(&id, socket))
        {
            completion.abandon_reader(&id);
        }
        return;
    }
    if http::write_response_with_headers(stream, &response, &metadata.headers).is_ok() {
        close_response(
            stream,
            metadata.unread_bytes,
            Instant::now() + REJECT_CLOSE_TIMEOUT,
        );
    }
}

struct ResponseMetadata {
    headers: Vec<(String, String)>,
    unread_bytes: usize,
    completion: Option<Arc<crate::completion::CompletionLane>>,
    deferred: Option<String>,
}

/// Closing a socket with an unread request body can reset it and discard the
/// error response. Send FIN first, then discard only a bounded remainder. Never
/// wait for the peer's EOF or trust an unbounded Content-Length during cleanup.
fn close_response(stream: &mut TcpStream, mut remaining: usize, deadline: Instant) {
    if stream.shutdown(Shutdown::Write).is_err() {
        return;
    }
    remaining = remaining.min(MAX_UNREAD_REQUEST_BYTES);
    let mut bytes = [0; 8192];
    while remaining > 0 {
        let Some(left) = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
        else {
            break;
        };
        if stream.set_read_timeout(Some(left)).is_err() {
            break;
        }
        let len = remaining.min(bytes.len());
        match stream.read(&mut bytes[..len]) {
            Ok(0) => break,
            Ok(read) => remaining -= read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

struct DeadlineWriter<'a> {
    stream: &'a mut TcpStream,
    deadline: Instant,
}
impl Write for DeadlineWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "response_deadline")
            })?;
        self.stream.set_write_timeout(Some(remaining))?;
        self.stream.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

struct DeadlineReader<'a> {
    stream: &'a mut TcpStream,
    deadline: Instant,
    bytes_read: usize,
}
impl Read for DeadlineReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "request_deadline"))?;
        self.stream.set_read_timeout(Some(remaining))?;
        let read = self.stream.read(bytes)?;
        self.bytes_read = self.bytes_read.saturating_add(read);
        Ok(read)
    }
}

fn read_body(
    reader: &mut BufReader<DeadlineReader<'_>>,
    len: usize,
    metadata: &mut ResponseMetadata,
) -> Result<Vec<u8>, http::HeadError> {
    let before = reader.get_ref().bytes_read;
    let body = http::read_body(reader, len);
    metadata.unread_bytes = metadata
        .unread_bytes
        .saturating_sub(reader.get_ref().bytes_read.saturating_sub(before));
    if body.is_ok() {
        metadata.unread_bytes = 0;
    }
    body
}

fn process(
    stream: &mut TcpStream,
    addr: SocketAddr,
    public: &str,
    auth: &Auth,
    tx: &SyncSender<Request>,
    wake: &dyn Fn(),
    metadata: &mut ResponseMetadata,
) -> Response {
    let mut reader = BufReader::new(DeadlineReader {
        stream,
        deadline: Instant::now() + REQUEST_TIMEOUT,
        bytes_read: 0,
    });
    let Ok(head) = http::read_request_head(&mut reader) else {
        return Response::plain(400, "invalid_request");
    };
    metadata.unread_bytes =
        if head.header("transfer-encoding").is_none() && head.header_count("content-length") == 1 {
            head.content_length().map_or(MAX_REQUEST_BODY_BYTES, |len| {
                len.saturating_sub(reader.buffer().len())
                    .min(MAX_REQUEST_BODY_BYTES)
            })
        } else if head.header("transfer-encoding").is_none()
            && head.header_count("content-length") == 0
            && head.method != "POST"
        {
            0
        } else {
            MAX_REQUEST_BODY_BYTES
        };
    let local = addr.to_string();
    let localhost = format!("localhost:{}", addr.port());
    if head.header_count("host") != 1
        || !head
            .header("host")
            .is_some_and(|v| v == local || v == localhost || (!public.is_empty() && v == public))
    {
        return Response::plain(403, "invalid_host");
    }
    if head.header_count("origin") > 1
        || head.header("origin").is_some_and(|v| {
            v != format!("http://{local}")
                && v != format!("http://{localhost}")
                && (public.is_empty() || v != format!("https://{public}"))
        })
    {
        return Response::plain(403, "invalid_origin");
    }
    let base = if public.is_empty() {
        format!("http://{addr}")
    } else {
        format!("https://{public}")
    };
    if head.path.starts_with("/.well-known/") || head.path.starts_with("/oauth/") {
        let body = if head.method == "POST" {
            if head.header("transfer-encoding").is_some()
                || head.header_count("content-length") != 1
                || head.header_count("content-type") != 1
            {
                return Response::plain(400, "invalid_request");
            }
            let Some(len) = head.content_length().filter(|l| *l <= 8192) else {
                return Response::plain(413, "body_size");
            };
            let Ok(body) = read_body(&mut reader, len, metadata) else {
                return Response::plain(400, "truncated_body");
            };
            body
        } else {
            vec![]
        };
        return auth.oauth_route(&head, &body, &base, &mut metadata.headers, wake);
    }
    if head.path != "/mcp" || !head.query.is_empty() {
        return Response::plain(404, "not_found");
    }
    metadata.headers.push((
        "WWW-Authenticate".into(),
        format!("Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\""),
    ));
    if head.header_count("authorization") != 1 {
        return Response::plain(401, "bearer_token_required");
    }
    let Some((epoch, principal, access_key)) =
        auth.authenticate_scoped(head.header("authorization").unwrap_or(""), now())
    else {
        return Response::plain(401, "invalid_or_expired_token");
    };
    if head.header_count("mcp-protocol-version") > 1
        || head
            .header("mcp-protocol-version")
            .is_some_and(|v| !supported(v))
    {
        return Response::plain(400, "unsupported_protocol");
    }
    let owner = crate::completion::TransportOwner {
        connection_id: principal.id.clone(),
        epoch,
        access_key: access_key.clone(),
    };
    if head.header_count("mcp-session-id") > 1 || head.header_count("last-event-id") > 1 {
        return Response::plain(400, "invalid_transport_headers");
    }
    let transport_session = head.header("mcp-session-id").map(str::to_owned);
    if let Some(session) = &transport_session
        && !metadata
            .completion
            .as_ref()
            .is_some_and(|lane| lane.owns_session(&owner, session))
    {
        return Response::plain(404, "transport_session_not_found");
    }
    if head.method == "GET" {
        let Some(cursor) = head.header("last-event-id") else {
            return Response::plain(405, "POST_required; resume_cursor_required");
        };
        if !head
            .header("accept")
            .is_some_and(|v| v.contains("text/event-stream"))
        {
            return Response::plain(400, "accept_event_stream_required");
        }
        let (Some(session), Some(lane)) = (&transport_session, &metadata.completion) else {
            return Response::plain(404, "transport_stream_not_found");
        };
        return match lane.resume(&owner, session, cursor) {
            Ok(id) => {
                metadata.deferred = Some(id);
                Response::plain(202, "")
            }
            Err(status) => Response::plain(status, "transport_stream_not_found"),
        };
    }
    if head.method != "POST" {
        return Response::plain(405, "POST_required");
    }
    if head.header_count("content-type") != 1
        || head
            .header("content-type")
            .is_none_or(|v| v.split(';').next() != Some("application/json"))
    {
        return Response::plain(415, "application/json_required");
    }
    if !head
        .header("accept")
        .is_some_and(|v| v.contains("application/json") && v.contains("text/event-stream"))
    {
        return Response::plain(400, "accept_json_and_event_stream_required");
    }
    if head.header("transfer-encoding").is_some() || head.header_count("content-length") != 1 {
        return Response::plain(400, "content_length_required");
    }
    let Some(len) = head
        .content_length()
        .filter(|len| *len > 0 && *len <= MAX_REQUEST_BODY_BYTES)
    else {
        return Response::plain(413, "body_size");
    };
    let Ok(body) = read_body(&mut reader, len, metadata) else {
        return Response::plain(400, "truncated_body");
    };
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return rpc_error(Value::Null, -32700, "parse_error");
    };
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    if !v.is_object()
        || v["jsonrpc"] != "2.0"
        || !(id.is_null() || id.is_string() || id.is_number())
    {
        return rpc_error(Value::Null, -32600, "invalid_request");
    }
    let Some(method) = v["method"].as_str() else {
        if v.get("id").is_some() && (v.get("result").is_some() ^ v.get("error").is_some()) {
            return Response::plain(202, "");
        }
        return rpc_error(id, -32600, "invalid_request");
    };
    if v.get("id").is_none() {
        return Response::plain(202, "");
    }
    let result = match method {
        "initialize" => {
            auth.observe_initialize(&principal.id);
            if transport_session.is_some() {
                return Response::plain(400, "initialize_requires_new_transport_session");
            }
            if let Some(lane) = &metadata.completion {
                let Some(session) = lane.initialize(owner.clone(), principal.expires_at) else {
                    return Response::plain(503, "transport_session_capacity");
                };
                metadata.headers.push(("MCP-Session-Id".into(), session));
            }
            let version = v["params"]["protocolVersion"].as_str().unwrap_or("");
            let version = if supported(version) {
                version
            } else {
                "2025-11-25"
            };
            json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"Deppy","version":env!("CARGO_PKG_VERSION")},"instructions":"Operate only explicitly shared sessions. Always deliver your own final answer with notify. Never automatically retry an unknown input outcome with a different operation_id. With owner-enabled local completion, supported submit:true calls return their final result on the original SSE request. Configure an adequate long request timeout; heartbeat comments do not reset client deadlines. Resume only that original stream with MCP-Session-Id and Last-Event-ID. A socket write is not confirmation that an agent read the result."})
        }
        "ping" => json!({}),
        "tools/list" => crate::tools(),
        "tools/call" => {
            let Some(tool) = v["params"]["name"].as_str().filter(|s| {
                matches!(
                    *s,
                    "list_sessions"
                        | "read_output"
                        | "send_text"
                        | "paste_text"
                        | "send_ctrl_c"
                        | "notify"
                        | "start_run"
                        | "update_run"
                        | "get_run"
                        | "list_runs"
                        | "read_history"
                        | "get_operation"
                        | "acquire_control"
                        | "release_control"
                )
            }) else {
                return rpc_error(id, -32602, "unknown_tool");
            };
            let args = v["params"].get("arguments").cloned().unwrap_or(json!({}));
            if !args.is_object() {
                return rpc_error(id, -32602, "invalid_arguments");
            }
            let poll = matches!(
                tool,
                "list_sessions"
                    | "read_output"
                    | "read_history"
                    | "get_run"
                    | "list_runs"
                    | "get_operation"
            );
            if !auth.begin_call(&principal.id, poll, epoch) {
                metadata.headers.push(("Retry-After".into(), "1".into()));
                return tool_response(id, Err("poll_budget_no_effect".into()));
            }
            let connection_id = principal.id.clone();
            let (reply, rx) = mpsc::sync_channel(1);
            let (handoff, deferred_rx) = mpsc::sync_channel(1);
            let deferred = metadata
                .completion
                .as_ref()
                .map(|lane| crate::DeferredTransport {
                    lane: lane.clone(),
                    owner,
                    session: transport_session,
                    request_id: id.clone(),
                    handoff,
                    reserved: Arc::new(AtomicBool::new(false)),
                });
            let req = Request {
                epoch,
                access_key,
                input_scope: principal.input_scope,
                principal,
                deadline: Instant::now() + Duration::from_secs(3),
                tool: tool.into(),
                args,
                reply,
                deferred,
            };
            if tx.try_send(req).is_err() {
                auth.end_call(&connection_id, poll, epoch);
                metadata.headers.push(("Retry-After".into(), "1".into()));
                return tool_response(id, Err("busy_no_effect".into()));
            }
            wake();
            let deadline = Instant::now() + Duration::from_secs(3);
            let result = loop {
                if let Ok(stream) = deferred_rx.try_recv()
                    && let Some(lane) = &metadata.completion
                {
                    auth.end_call(&connection_id, poll, epoch);
                    if lane.begin(&stream, rx) {
                        metadata.deferred = Some(stream);
                        return Response::plain(202, "");
                    }
                    return tool_response(id, Err("outcome_unknown_do_not_retry_input".into()));
                }
                match rx.recv_timeout(Duration::from_millis(5)) {
                    Ok(result) => {
                        if let Ok(stream) = deferred_rx.try_recv()
                            && let Some(lane) = &metadata.completion
                        {
                            let (ready, ready_rx) = mpsc::sync_channel(1);
                            let _ = ready.try_send(result);
                            auth.end_call(&connection_id, poll, epoch);
                            if lane.begin(&stream, ready_rx) {
                                metadata.deferred = Some(stream);
                                return Response::plain(202, "");
                            }
                            return tool_response(
                                id,
                                Err("outcome_unknown_do_not_retry_input".into()),
                            );
                        }
                        break result;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        // Reserve sends its handoff before the App may drop/send its reply.
                        if let Ok(stream) = deferred_rx.try_recv()
                            && let Some(lane) = &metadata.completion
                        {
                            auth.end_call(&connection_id, poll, epoch);
                            if lane.begin(&stream, rx) {
                                metadata.deferred = Some(stream);
                                return Response::plain(202, "");
                            }
                            return tool_response(
                                id,
                                Err("outcome_unknown_do_not_retry_input".into()),
                            );
                        }
                        break Err("outcome_unknown_do_not_retry_input".into());
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                    Err(_) => {
                        break Err(if poll {
                            "timeout_no_effect"
                        } else {
                            "outcome_unknown_do_not_retry_input"
                        }
                        .into());
                    }
                }
            };
            auth.end_call(&connection_id, poll, epoch);
            if result
                .as_ref()
                .is_ok_and(|value| !tool_payload_is_error(value))
            {
                auth.observe_tool_success(&connection_id, epoch);
            }
            return tool_response(id, result);
        }
        _ => return rpc_error(id, -32601, "method_not_found"),
    };
    json_response(json!({"jsonrpc":"2.0","id":id,"result":result}))
}
fn tool_response(id: Value, result: Result<Value, String>) -> Response {
    json_response(tool_rpc_value(id, result))
}
pub(crate) fn tool_rpc_value(id: Value, result: Result<Value, String>) -> Value {
    let error = result.is_err() || result.as_ref().is_ok_and(tool_payload_is_error);
    let payload = crate::protocol::normalize_tool_payload(
        result.unwrap_or_else(|error| crate::protocol::error_payload(&error)),
    );
    json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":payload.to_string()}],"isError":error}})
}
fn tool_payload_is_error(value: &Value) -> bool {
    value.get("error").is_some() || matches!(value["status"].as_str(), Some("rejected" | "unknown"))
}
fn busy_response() -> Response {
    let mut response = json_response(crate::protocol::error_payload("busy_no_effect"));
    response.status = 503;
    response
}
fn rpc_error(id: Value, code: i32, message: &str) -> Response {
    json_response(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
}
fn json_response(v: Value) -> Response {
    Response {
        status: 200,
        content_type: "application/json",
        body: std::borrow::Cow::Owned(v.to_string().into_bytes()),
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn completed_bodies_and_bodyless_discovery_skip_cleanup_wait() {
        let auth = Auth::new_at(now());
        let token = auth.token_for_user();
        let token = token.as_str();
        for (method, path, body, status) in [
            (
                "POST",
                "/mcp",
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
                200,
            ),
            ("POST", "/mcp", "{invalid-json", 200),
            ("POST", "/oauth/token", "invalid=form", 415),
            ("GET", "/.well-known/oauth-protected-resource/mcp", "", 200),
            ("GET", "/mcp", "", 405),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let mut client = TcpStream::connect(addr).unwrap();
            let length = if method == "POST" {
                format!("Content-Length: {}\r\n", body.len())
            } else {
                String::new()
            };
            write!(client, "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\n{length}\r\n{body}").unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let (tx, _rx) = mpsc::sync_channel(1);
            let mut metadata = ResponseMetadata {
                headers: Vec::new(),
                unread_bytes: MAX_UNREAD_REQUEST_BYTES,
                completion: None,
                deferred: None,
            };
            assert_eq!(
                process(&mut stream, addr, "", &auth, &tx, &|| {}, &mut metadata).status,
                status,
                "{method} {path}"
            );
            assert_eq!(metadata.unread_bytes, 0, "{method} {path}");
        }
    }

    #[test]
    fn dripping_body_does_not_extend_cleanup_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let writer = thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            for _ in 0..100 {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        let (mut stream, _) = listener.accept().unwrap();
        let start = Instant::now();
        close_response(&mut stream, usize::MAX, start + Duration::from_millis(50));
        assert!(start.elapsed() < Duration::from_millis(350));
        drop(stream);
        writer.join().unwrap();
    }

    #[test]
    fn untrusted_cleanup_length_cannot_consume_beyond_byte_cap() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut body = vec![b'x'; MAX_UNREAD_REQUEST_BYTES];
        body.push(b'z');
        client.write_all(&body).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        close_response(
            &mut stream,
            usize::MAX,
            Instant::now() + Duration::from_secs(1),
        );
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut remaining = [0];
        stream.read_exact(&mut remaining).unwrap();
        assert_eq!(remaining, [b'z']);
    }

    #[test]
    fn dripping_bytes_do_not_extend_total_read_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let writer = thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            for _ in 0..100 {
                if s.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        let (mut stream, _) = listener.accept().unwrap();
        let start = Instant::now();
        let mut reader = DeadlineReader {
            stream: &mut stream,
            deadline: start + Duration::from_millis(50),
            bytes_read: 0,
        };
        let mut bytes = [0; 100];
        assert!(reader.read_exact(&mut bytes).is_err());
        assert!(start.elapsed() < Duration::from_millis(350));
        drop(stream);
        writer.join().unwrap();
    }
}

#[cfg(test)]
mod hostname_tests {
    use super::*;
    #[test]
    fn generated_hostname_updates_real_host_checks_and_oauth_resource_once() {
        use std::io::{Read, Write};
        fn get(server: &Server, host: &str) -> String {
            let mut stream = TcpStream::connect(server.addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            write!(
                stream,
                "GET /.well-known/oauth-protected-resource/mcp HTTP/1.1\r\nHost: {host}\r\n\r\n"
            )
            .unwrap();
            let mut output = String::new();
            stream.read_to_string(&mut output).unwrap();
            output
        }
        let server = Server::start(0, "", || {}).unwrap();
        let token = server.auth.token_for_user();
        assert!(get(&server, "abc.trycloudflare.com").starts_with("HTTP/1.1 403"));
        server.set_public_host("abc.trycloudflare.com").unwrap();
        let response = get(&server, "abc.trycloudflare.com");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("https://abc.trycloudflare.com/mcp"));
        assert_eq!(server.auth.token_for_user().as_str(), token.as_str());
        assert!(server.set_public_host("https://bad.example").is_err());
        assert!(
            server
                .set_public_host("different.trycloudflare.com")
                .is_err()
        );
        assert!(get(&server, "abc.trycloudflare.com").starts_with("HTTP/1.1 200"));
    }
    #[test]
    fn dedicated_funnel_port_is_allowed_but_urls_and_bad_domains_are_rejected() {
        assert!(valid_public_host("mac.example.ts.net:8443"));
        for host in [
            "https://mac.example.ts.net",
            "a.example:0",
            "a.example:65536",
            "a..example",
            "-a.example",
            "a.example/mcp",
        ] {
            assert!(!valid_public_host(host));
        }
    }
}

#[cfg(test)]
mod oauth_lock_tests {
    use super::*;

    #[test]
    fn oauth_wake_can_read_auth_without_deadlock() {
        const CHILD_ENV: &str = "DEPPY_OAUTH_WAKE_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            let auth = Auth::new_at(now());
            let head = |method: &str, target: &str, content_type: &str| {
                let wire = format!(
                    "{method} {target} HTTP/1.1\r\nHost: localhost\r\nContent-Type: {content_type}\r\n\r\n"
                );
                http::read_request_head(&mut BufReader::new(wire.as_bytes())).unwrap()
            };
            let base = "http://localhost:4321";
            let mut headers = Vec::new();
            let response = auth.oauth_route(
                &head("POST", "/oauth/register", "application/json"),
                br#"{"redirect_uris":["https://client.example/callback"]}"#,
                base,
                &mut headers,
                &|| panic!("registration must not request an approval wake"),
            );
            assert_eq!(response.status, 201);
            let registration: Value = serde_json::from_slice(&response.body).unwrap();
            let resource = format!("{base}/mcp");
            let challenge = "A".repeat(43);
            let query = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs([
                    ("client_id", registration["client_id"].as_str().unwrap()),
                    ("redirect_uri", "https://client.example/callback"),
                    ("resource", resource.as_str()),
                    ("response_type", "code"),
                    ("code_challenge_method", "S256"),
                    ("code_challenge", challenge.as_str()),
                ])
                .finish();
            let wakes = std::cell::Cell::new(0);
            let response = auth.oauth_route(
                &head("GET", &format!("/oauth/authorize?{query}"), ""),
                &[],
                base,
                &mut headers,
                &|| {
                    // Real Auth reads take its credential mutex again. The notification
                    // must run after publication and after that mutex is released.
                    assert_eq!(auth.approvals().len(), 1);
                    assert!(auth.expires() > now());
                    wakes.set(wakes.get() + 1);
                },
            );
            assert_eq!(response.status, 200);
            assert_eq!(wakes.get(), 1);
            let response = auth.oauth_route(
                &head("GET", "/oauth/authorize?invalid=1", ""),
                &[],
                base,
                &mut headers,
                &|| panic!("failed authorization must not notify"),
            );
            assert_eq!(response.status, 400);
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "server::oauth_lock_tests::oauth_wake_can_read_auth_without_deadlock",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "OAuth wake regression child failed: {status}"
                );
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("OAuth approval wake deadlocked while reading Auth");
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest, Sha256};

    #[test]
    fn cloud_management_oauth_sync_preserves_last_verified_authentication() {
        let auth = Auth::new_at(100);
        let mut credential = auth.0.lock().unwrap();
        let headers = std::cell::RefCell::new(vec![]);
        let base = "https://deppy.example";
        let resource = format!("{base}/mcp");
        let route =
            |oauth: &mut crate::oauth::OAuth, method: &str, target: &str, body: &str, time: u64| {
                let wire = format!(
                    "{method} {target} HTTP/1.1\r\nHost: deppy.example\r\nContent-Type: {}\r\n\r\n",
                    if target == "/oauth/register" {
                        "application/json"
                    } else {
                        "application/x-www-form-urlencoded"
                    }
                );
                let head = http::read_request_head(&mut BufReader::new(wire.as_bytes())).unwrap();
                let mut headers = headers.borrow_mut();
                headers.clear();
                oauth.route(
                    &head,
                    body.as_bytes(),
                    base,
                    time,
                    100 + TOKEN_TTL,
                    &mut headers,
                    &|| {},
                )
            };
        let registration = route(
            &mut credential.oauth,
            "POST",
            "/oauth/register",
            r#"{"redirect_uris":["https://client.example/callback"]}"#,
            100,
        );
        let registration: Value = serde_json::from_slice(&registration.body).unwrap();
        let client = registration["client_id"].as_str().unwrap();
        let verifier = "v".repeat(43);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier));
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("client_id", client),
                ("redirect_uri", "https://client.example/callback"),
                ("resource", &resource),
                ("response_type", "code"),
                ("code_challenge_method", "S256"),
                ("code_challenge", &challenge),
            ])
            .finish();
        assert_eq!(
            route(
                &mut credential.oauth,
                "GET",
                &format!("/oauth/authorize?{query}"),
                "",
                101
            )
            .status,
            200
        );
        let approval = credential.oauth.approvals(101).pop().unwrap();
        assert!(credential.oauth.approve(&approval.id, true, 102));
        assert_eq!(
            route(
                &mut credential.oauth,
                "GET",
                &format!("/oauth/authorize?request={}", approval.id),
                "",
                103
            )
            .status,
            303
        );
        let location = headers
            .borrow()
            .iter()
            .find(|(name, _)| name == "Location")
            .unwrap()
            .1
            .clone();
        let code = url::Url::parse(&location)
            .unwrap()
            .query_pairs()
            .find(|(name, _)| name == "code")
            .unwrap()
            .1
            .into_owned();
        let form = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("grant_type", "authorization_code"),
                ("client_id", client),
                ("resource", &resource),
                ("redirect_uri", "https://client.example/callback"),
                ("code", &code),
                ("code_verifier", &verifier),
            ])
            .finish();
        let response = route(&mut credential.oauth, "POST", "/oauth/token", &form, 104);
        let tokens: Value = serde_json::from_slice(&response.body).unwrap();
        sync_oauth_connections(&mut credential, 104);
        drop(credential);
        let principal = auth
            .authenticate_principal(
                &format!("Bearer {}", tokens["access_token"].as_str().unwrap()),
                110,
            )
            .unwrap();
        let mut credential = auth.0.lock().unwrap();
        sync_oauth_connections(&mut credential, 111);
        let info = credential.connections.get(&principal.id).unwrap();
        assert_eq!(info.principal.authenticated_at, 110);
        assert_eq!(info.last_authenticated_at, Some(110));
    }
}
