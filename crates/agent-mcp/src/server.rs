use serde_json::{Value, json};
use std::{
    io::{BufReader, Read},
    net::{SocketAddr, TcpListener, TcpStream},
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
}
pub struct Auth(Mutex<Credential>);
impl Auth {
    pub fn new_at(time: u64) -> Self {
        Self::with_redaction(time, secret::RedactionService::new())
    }
    fn with_redaction(time: u64, redaction: secret::RedactionService) -> Self {
        Self(Mutex::new(Credential {
            token: make_token(),
            epoch: 1,
            expires: time + TOKEN_TTL,
            oauth: crate::oauth::OAuth::new(redaction),
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
        c.token = make_token();
        c.epoch += 1;
        c.expires = time + TOKEN_TTL;
    }
    pub fn revoke(&self) {
        let mut c = self.0.lock().unwrap();
        c.epoch += 1;
        c.expires = 0;
        c.oauth.clear();
        c.token = Zeroizing::new(String::new());
    }
    pub fn authenticate(&self, header: &str, time: u64) -> Option<u64> {
        self.authenticate_scoped(header, time)
            .map(|(epoch, _, _)| epoch)
    }
    fn authenticate_scoped(&self, header: &str, time: u64) -> Option<(u64, bool, Option<String>)> {
        let supplied = header.strip_prefix("Bearer ")?;
        let c = self.0.lock().ok()?;
        if time >= c.expires || supplied.is_empty() {
            return None;
        }
        if bool::from(c.token.as_bytes().ct_eq(supplied.as_bytes())) {
            return Some((c.epoch, true, None));
        }
        let key = crate::oauth::hash(supplied);
        c.oauth
            .authenticate_key(&key, time)
            .map(|input| (c.epoch, input, Some(key)))
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
            c.oauth.route(h, body, base, now(), expiry, headers, &|| {
                notification_requested.set(true);
            })
        };
        if notification_requested.get() {
            wake();
        }
        response
    }
    pub fn current(&self, epoch: u64, time: u64) -> bool {
        self.current_access(epoch, None, time)
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
        && access_key.is_none_or(|key| c.oauth.authenticate_key(key, time).is_some())
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
    /// OAuth token fingerprint; manual bearer requests are bound by epoch alone.
    pub access_key: Option<String>,
    pub input_scope: bool,
    pub deadline: Instant,
    pub tool: String,
    pub args: Value,
    pub reply: SyncSender<Result<Value, String>>,
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
        let (tx, requests) = mpsc::sync_channel(16);
        let (a, s, w) = (auth.clone(), stop.clone(), Arc::new(wake));
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
                                let _ = http::write_response(
                                    &mut stream,
                                    &Response::plain(503, "busy"),
                                );
                                continue;
                            }
                            active.fetch_add(1, Ordering::AcqRel);
                            let (a, tx, w, active, host) = (
                                a.clone(),
                                tx.clone(),
                                w.clone(),
                                active.clone(),
                                host.clone(),
                            );
                            thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
                                let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
                                let public = host.read().unwrap().clone();
                                handle(&mut stream, addr, &public, &a, &tx, &*w);
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
fn valid_public_host(host: &str) -> bool {
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
) {
    let mut headers = Vec::new();
    let response = process(stream, addr, public, auth, tx, wake, &mut headers);
    let _ = http::write_response_with_headers(stream, &response, &headers);
}
struct DeadlineReader<'a> {
    stream: &'a mut TcpStream,
    deadline: Instant,
}
impl Read for DeadlineReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "request_deadline"))?;
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(bytes)
    }
}

fn process(
    stream: &mut TcpStream,
    addr: SocketAddr,
    public: &str,
    auth: &Auth,
    tx: &SyncSender<Request>,
    wake: &dyn Fn(),
    headers: &mut Vec<(String, String)>,
) -> Response {
    let mut reader = BufReader::new(DeadlineReader {
        stream,
        deadline: Instant::now() + REQUEST_TIMEOUT,
    });
    let Ok(head) = http::read_request_head(&mut reader) else {
        return Response::plain(400, "invalid_request");
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
            let Ok(body) = http::read_body(&mut reader, len) else {
                return Response::plain(400, "truncated_body");
            };
            body
        } else {
            vec![]
        };
        return auth.oauth_route(&head, &body, &base, headers, wake);
    }
    if head.path != "/mcp" || !head.query.is_empty() {
        return Response::plain(404, "not_found");
    }
    headers.push((
        "WWW-Authenticate".into(),
        format!("Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\""),
    ));
    if head.header_count("authorization") != 1 {
        return Response::plain(401, "bearer_token_required");
    }
    let Some((epoch, input_scope, access_key)) =
        auth.authenticate_scoped(head.header("authorization").unwrap_or(""), now())
    else {
        return Response::plain(401, "invalid_or_expired_token");
    };
    if head.method != "POST" {
        return Response::plain(405, "POST_required; SSE_not_enabled");
    }
    if head.header_count("mcp-protocol-version") > 1
        || head
            .header("mcp-protocol-version")
            .is_some_and(|v| !supported(v))
    {
        return Response::plain(400, "unsupported_protocol");
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
        .filter(|len| *len > 0 && *len <= 64 * 1024)
    else {
        return Response::plain(413, "body_size");
    };
    let Ok(body) = http::read_body(&mut reader, len) else {
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
            let version = v["params"]["protocolVersion"].as_str().unwrap_or("");
            let version = if supported(version) {
                version
            } else {
                "2025-11-25"
            };
            json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"Deppy","version":env!("CARGO_PKG_VERSION")},"instructions":"Operate only explicitly shared sessions. Always deliver your own final answer with notify. Never automatically retry an unknown input outcome with a different operation_id."})
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
                )
            }) else {
                return rpc_error(id, -32602, "unknown_tool");
            };
            let args = v["params"].get("arguments").cloned().unwrap_or(json!({}));
            if !args.is_object() {
                return rpc_error(id, -32602, "invalid_arguments");
            }
            let (reply, rx) = mpsc::sync_channel(1);
            let req = Request {
                epoch,
                access_key,
                input_scope,
                deadline: Instant::now() + Duration::from_secs(3),
                tool: tool.into(),
                args,
                reply,
            };
            if tx.try_send(req).is_err() {
                return tool_response(id, Err("busy_no_effect".into()));
            }
            wake();
            let result = rx
                .recv_timeout(Duration::from_secs(3))
                .unwrap_or_else(|_| Err("outcome_unknown_do_not_retry_input".into()));
            return tool_response(id, result);
        }
        _ => return rpc_error(id, -32601, "method_not_found"),
    };
    json_response(json!({"jsonrpc":"2.0","id":id,"result":result}))
}
fn tool_response(id: Value, result: Result<Value, String>) -> Response {
    let error = result.is_err()
        || result
            .as_ref()
            .is_ok_and(|v| matches!(v["status"].as_str(), Some("rejected" | "unknown")));
    let payload = result.unwrap_or_else(|e| json!({"error":e}));
    json_response(
        json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":payload.to_string()}],"isError":error}}),
    )
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
