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
}
pub struct Auth(Mutex<Credential>);
impl Auth {
    pub fn new_at(time: u64) -> Self {
        Self(Mutex::new(Credential {
            token: make_token(),
            epoch: 1,
            expires: time + TOKEN_TTL,
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
        c.token = make_token();
        c.epoch += 1;
        c.expires = time + TOKEN_TTL;
    }
    pub fn revoke(&self) {
        let mut c = self.0.lock().unwrap();
        c.epoch += 1;
        c.expires = 0;
        c.token = Zeroizing::new(String::new());
    }
    pub fn authenticate(&self, header: &str, time: u64) -> Option<u64> {
        let supplied = header.strip_prefix("Bearer ")?;
        let c = self.0.lock().ok()?;
        (time < c.expires
            && !supplied.is_empty()
            && bool::from(c.token.as_bytes().ct_eq(supplied.as_bytes())))
        .then_some(c.epoch)
    }
    pub fn current(&self, epoch: u64, time: u64) -> bool {
        let c = self.0.lock().unwrap();
        c.epoch == epoch && time < c.expires
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
    pub deadline: Instant,
    pub tool: String,
    pub args: Value,
    pub reply: SyncSender<Result<Value, String>>,
}
impl Request {
    pub fn live(&self, auth: &Auth) -> bool {
        Instant::now() < self.deadline && auth.current(self.epoch, now())
    }
}
pub struct Server {
    pub addr: SocketAddr,
    pub auth: Arc<Auth>,
    pub requests: Receiver<Request>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    pub fn start(
        port: u16,
        public_host: &str,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(valid_public_host(public_host), "invalid_public_hostname");
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let auth = Arc::new(Auth::new_at(now()));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, requests) = mpsc::sync_channel(16);
        let (a, s, w) = (auth.clone(), stop.clone(), Arc::new(wake));
        let host = public_host.to_owned();
        let active = Arc::new(AtomicUsize::new(0));
        let worker = thread::Builder::new()
            .name("agent-mcp".into())
            .spawn(move || {
                while !s.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
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
                                handle(&mut stream, addr, &host, &a, &tx, &*w);
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
        })
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
    let response = process(stream, addr, public, auth, tx, wake);
    let _ = http::write_response(stream, &response);
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
) -> Response {
    let mut reader = BufReader::new(DeadlineReader {
        stream,
        deadline: Instant::now() + REQUEST_TIMEOUT,
    });
    let Ok(head) = http::read_request_head(&mut reader) else {
        return Response::plain(400, "invalid_request");
    };
    if head.path != "/mcp" || !head.query.is_empty() {
        return Response::plain(404, "not_found");
    }
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
    if head.header_count("authorization") != 1 {
        return Response::plain(401, "bearer_token_required");
    }
    let Some(epoch) = auth.authenticate(head.header("authorization").unwrap_or(""), now()) else {
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
        || !head
            .header("content-type")
            .is_some_and(|v| v.split(';').next() == Some("application/json"))
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
                    "list_sessions" | "read_output" | "send_text" | "send_ctrl_c" | "notify"
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
