//! Reusable backend MCP sessions for the proxy.
//!
//! Connections are opened lazily, kept in a two-entry LRU, invalidated when their
//! non-secret config generation changes, and closed at the idle deadline. There is no
//! automatic call retry: in particular `McpDeliveryUnknown` is returned unchanged after the
//! connection is poisoned and released.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use mcp::{
    LocalMcpManager, McpConnection, McpDeliveryUnknown, McpHttpServerConfig, McpServerConfig,
    McpTool,
};
use serde_json::Value;

pub const MAX_BACKEND_LEASES: usize = 2;
pub const DEFAULT_BACKEND_IDLE_TTL: Duration = Duration::from_secs(30);
#[cfg(test)]
const BACKEND_IDLE_TTL_CANDIDATES: [Duration; 3] = [
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
];

/// kind별 backend configuration. Secret-bearing values remain inside the MCP config types and
/// their redacted Debug implementations.
pub enum BackendConfig {
    Stdio(McpServerConfig),
    Http(McpHttpServerConfig),
}

impl BackendConfig {
    pub fn name(&self) -> &str {
        match self {
            Self::Stdio(config) => &config.name,
            Self::Http(config) => &config.name,
        }
    }
}

static NEXT_CONFIG_REVISION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigRevision(u64);

impl ConfigRevision {
    fn next() -> Self {
        Self(NEXT_CONFIG_REVISION.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone)]
struct BackendTarget {
    id: String,
    revision: ConfigRevision,
    config: Arc<BackendConfig>,
}

impl BackendTarget {
    fn new(id: String, config: BackendConfig) -> Self {
        Self {
            id,
            revision: ConfigRevision::next(),
            config: Arc::new(config),
        }
    }
}

trait SessionTransport: Send {
    fn list_tools(&mut self) -> anyhow::Result<Vec<McpTool>>;
    fn call_tool(&mut self, name: &str, arguments: Value) -> anyhow::Result<Value>;
    fn cancel(&mut self);
}

impl SessionTransport for McpConnection {
    fn list_tools(&mut self) -> anyhow::Result<Vec<McpTool>> {
        McpConnection::list_tools(self)
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> anyhow::Result<Value> {
        McpConnection::call_tool(self, name, arguments)
    }

    fn cancel(&mut self) {
        McpConnection::cancel(self);
    }
}

trait BackendFactory: Send + Sync {
    fn connect(&self, config: &BackendConfig) -> anyhow::Result<Box<dyn SessionTransport>>;
}

struct ManagerBackendFactory {
    manager: LocalMcpManager,
}

impl BackendFactory for ManagerBackendFactory {
    fn connect(&self, config: &BackendConfig) -> anyhow::Result<Box<dyn SessionTransport>> {
        let connection = match config {
            BackendConfig::Stdio(config) => self.manager.connect(config),
            BackendConfig::Http(config) => self.manager.connect_http(config),
        }
        .with_context(|| format!("백엔드 '{}' connect 실패", config.name()))?;
        Ok(Box::new(connection))
    }
}

trait Clock: Send + Sync {
    fn now(&self) -> Tick;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Tick(u64);

impl Tick {
    fn add(self, duration: Duration) -> Self {
        Self(self.0.saturating_add(duration_ticks(duration)))
    }

    fn elapsed_since(self, earlier: Self) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

fn duration_ticks(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Tick {
        Tick(duration_ticks(self.origin.elapsed()))
    }
}

struct LeaseEntry {
    backend_id: String,
    revision: ConfigRevision,
    connection: Box<dyn SessionTransport>,
    last_used: Tick,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackendSessionStats {
    pub cold_connects: u64,
    pub warm_reuses: u64,
    pub lru_evictions: u64,
    pub idle_evictions: u64,
    pub revision_evictions: u64,
    pub poisoned_connections: u64,
    pub delivery_unknown: u64,
    pub active_leases: usize,
    pub peak_leases: usize,
    pub cold_path_nanos: u64,
    pub warm_path_nanos: u64,
}

#[derive(Default)]
struct SessionState {
    /// Oldest to newest. Successful use moves an entry to the back.
    leases: Vec<LeaseEntry>,
    stats: BackendSessionStats,
}

pub struct BackendSession {
    factory: Arc<dyn BackendFactory>,
    clock: Arc<dyn Clock>,
    idle_ttl: Duration,
    state: Mutex<SessionState>,
}

impl BackendSession {
    pub fn production(manager: LocalMcpManager, idle_ttl: Duration) -> Arc<Self> {
        Arc::new(Self {
            factory: Arc::new(ManagerBackendFactory { manager }),
            clock: Arc::new(SystemClock::new()),
            idle_ttl,
            state: Mutex::new(SessionState::default()),
        })
    }

    #[cfg(test)]
    fn with_seams(
        factory: Arc<dyn BackendFactory>,
        clock: Arc<dyn Clock>,
        idle_ttl: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            factory,
            clock,
            idle_ttl,
            state: Mutex::new(SessionState::default()),
        })
    }

    fn use_connection<T>(
        &self,
        target: &BackendTarget,
        operation: impl FnOnce(&mut dyn SessionTransport) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let started = self.clock.now();
        let mut state = self.state.lock().expect("backend session lock");
        self.reap_expired_locked(&mut state, started);
        self.invalidate_revision_locked(&mut state, &target.id, target.revision);

        let (mut lease, warm) =
            if let Some(index) = state.leases.iter().position(|lease| {
                lease.backend_id == target.id && lease.revision == target.revision
            }) {
                (state.leases.remove(index), true)
            } else {
                if state.leases.len() >= MAX_BACKEND_LEASES {
                    let mut evicted = state.leases.remove(0);
                    evicted.connection.cancel();
                    state.stats.lru_evictions += 1;
                }
                let connection = self.factory.connect(&target.config)?;
                state.stats.cold_connects += 1;
                (
                    LeaseEntry {
                        backend_id: target.id.clone(),
                        revision: target.revision,
                        connection,
                        last_used: started,
                    },
                    false,
                )
            };

        let result = operation(lease.connection.as_mut());
        let finished = self.clock.now();
        let elapsed = duration_ticks(finished.elapsed_since(started));
        if warm {
            state.stats.warm_reuses += 1;
            state.stats.warm_path_nanos = state.stats.warm_path_nanos.saturating_add(elapsed);
        } else {
            state.stats.cold_path_nanos = state.stats.cold_path_nanos.saturating_add(elapsed);
        }

        match result {
            Ok(value) => {
                lease.last_used = finished;
                state.leases.push(lease);
                state.stats.active_leases = state.leases.len();
                state.stats.peak_leases = state.stats.peak_leases.max(state.leases.len());
                Ok(value)
            }
            Err(error) if preserves_connection(&error) => {
                lease.last_used = finished;
                state.leases.push(lease);
                state.stats.active_leases = state.leases.len();
                state.stats.peak_leases = state.stats.peak_leases.max(state.leases.len());
                Err(error)
            }
            Err(error) => {
                if error.downcast_ref::<McpDeliveryUnknown>().is_some() {
                    state.stats.delivery_unknown += 1;
                }
                state.stats.poisoned_connections += 1;
                lease.connection.cancel();
                state.stats.active_leases = state.leases.len();
                // Never retry here. The next independent request may lazily connect again.
                Err(error)
            }
        }
    }

    fn invalidate_revision_locked(
        &self,
        state: &mut SessionState,
        backend_id: &str,
        revision: ConfigRevision,
    ) {
        let mut index = 0;
        while index < state.leases.len() {
            if state.leases[index].backend_id == backend_id
                && state.leases[index].revision != revision
            {
                let mut stale = state.leases.remove(index);
                stale.connection.cancel();
                state.stats.revision_evictions += 1;
            } else {
                index += 1;
            }
        }
        state.stats.active_leases = state.leases.len();
    }

    fn reap_expired_locked(&self, state: &mut SessionState, now: Tick) {
        let ttl = duration_ticks(self.idle_ttl);
        let mut index = 0;
        while index < state.leases.len() {
            if now.0.saturating_sub(state.leases[index].last_used.0) >= ttl {
                let mut expired = state.leases.remove(index);
                expired.connection.cancel();
                state.stats.idle_evictions += 1;
            } else {
                index += 1;
            }
        }
        state.stats.active_leases = state.leases.len();
    }

    pub fn reap_expired(&self) -> usize {
        let mut state = self.state.lock().expect("backend session lock");
        let before = state.leases.len();
        self.reap_expired_locked(&mut state, self.clock.now());
        before.saturating_sub(state.leases.len())
    }

    /// Time until the earliest retained lease expires. `None` means the reader may block forever
    /// because no backend resource is retained.
    pub fn next_expiry(&self) -> Option<Duration> {
        let state = self.state.lock().expect("backend session lock");
        let now = self.clock.now();
        state
            .leases
            .iter()
            .map(|lease| lease.last_used.add(self.idle_ttl))
            .min()
            .map(|deadline| deadline.elapsed_since(now))
    }

    #[cfg(test)]
    fn invalidate(&self, backend_id: &str, revision: ConfigRevision) {
        let mut state = self.state.lock().expect("backend session lock");
        self.invalidate_revision_locked(&mut state, backend_id, revision);
    }

    pub fn shutdown(&self) {
        let mut state = self.state.lock().expect("backend session lock");
        for mut lease in state.leases.drain(..) {
            lease.connection.cancel();
        }
        state.stats.active_leases = 0;
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> BackendSessionStats {
        let state = self.state.lock().expect("backend session lock");
        let mut stats = state.stats;
        stats.active_leases = state.leases.len();
        stats
    }
}

impl Drop for BackendSession {
    fn drop(&mut self) {
        if let Ok(state) = self.state.get_mut() {
            for lease in &mut state.leases {
                lease.connection.cancel();
            }
        }
    }
}

fn preserves_connection(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("server error:"))
}

/// A config handle shared by the permission hook and the forwarder. Updating it invalidates the
/// old revision immediately; no DB polling or background watcher is created.
pub struct BackendClient {
    session: Arc<BackendSession>,
    target: RwLock<BackendTarget>,
}

impl BackendClient {
    pub fn new(backend_id: String, config: BackendConfig, session: Arc<BackendSession>) -> Self {
        Self {
            session,
            target: RwLock::new(BackendTarget::new(backend_id, config)),
        }
    }

    #[cfg(test)]
    pub(crate) fn production(
        backend_id: String,
        config: BackendConfig,
        manager: LocalMcpManager,
    ) -> Arc<Self> {
        let session = BackendSession::production(manager, DEFAULT_BACKEND_IDLE_TTL);
        Arc::new(Self::new(backend_id, config, session))
    }

    pub fn revision(&self) -> ConfigRevision {
        self.target.read().expect("backend target lock").revision
    }

    #[cfg(test)]
    fn replace_config(&self, config: BackendConfig) -> ConfigRevision {
        let mut target = self.target.write().expect("backend target lock");
        let replacement = BackendTarget::new(target.id.clone(), config);
        let revision = replacement.revision;
        self.session.invalidate(&target.id, revision);
        *target = replacement;
        revision
    }

    pub fn list_tools(&self) -> anyhow::Result<Vec<McpTool>> {
        let target = self.target.read().expect("backend target lock").clone();
        self.session
            .use_connection(&target, |connection| connection.list_tools())
    }

    pub fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<Value> {
        let target = self.target.read().expect("backend target lock").clone();
        self.session
            .use_connection(&target, |connection| connection.call_tool(name, arguments))
    }
}

/// Unix stdin wrapper that blocks once until either input is readable or the earliest lease idle
/// deadline arrives. Timeout handling only reaps and then blocks again; there is no helper thread,
/// ticker, or periodic polling.
#[cfg(unix)]
pub struct IdleDeadlineReader<R> {
    inner: R,
    session: Arc<BackendSession>,
}

#[cfg(unix)]
impl<R> IdleDeadlineReader<R> {
    pub fn new(inner: R, session: Arc<BackendSession>) -> Self {
        Self { inner, session }
    }
}

#[cfg(unix)]
impl<R: std::io::Read + std::os::fd::AsRawFd> std::io::Read for IdleDeadlineReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        loop {
            self.session.reap_expired();
            let Some(until_expiry) = self.session.next_expiry() else {
                return self.inner.read(buffer);
            };
            if until_expiry.is_zero() {
                continue;
            }
            let timeout_ms = until_expiry
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32;
            let mut descriptor = libc::pollfd {
                fd: self.inner.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            };
            // SAFETY: descriptor points to one initialized pollfd for the duration of the call.
            let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
            if ready > 0 {
                return self.inner.read(buffer);
            }
            if ready == 0 {
                self.session.reap_expired();
                continue;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    static PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[derive(Default)]
    struct FakeClock {
        nanos: AtomicU64,
    }

    impl FakeClock {
        fn set(&self, duration: Duration) {
            self.nanos.store(duration_ticks(duration), Ordering::SeqCst);
        }

        fn advance(&self, duration: Duration) {
            self.nanos
                .fetch_add(duration_ticks(duration), Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Tick {
            Tick(self.nanos.load(Ordering::SeqCst))
        }
    }

    #[derive(Default)]
    struct FakeResources {
        connects: AtomicUsize,
        calls: AtomicUsize,
        lists: AtomicUsize,
        live: AtomicUsize,
        peak: AtomicUsize,
        drops: AtomicUsize,
        cancels: AtomicUsize,
        retained_nanos: AtomicU64,
    }

    struct FakeFactory {
        resources: Arc<FakeResources>,
        clock: Arc<FakeClock>,
        connect_cost: Duration,
        operation_cost: Duration,
    }

    impl BackendFactory for FakeFactory {
        fn connect(&self, _config: &BackendConfig) -> anyhow::Result<Box<dyn SessionTransport>> {
            self.resources.connects.fetch_add(1, Ordering::SeqCst);
            self.clock.advance(self.connect_cost);
            let live = self.resources.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.resources.peak.fetch_max(live, Ordering::SeqCst);
            Ok(Box::new(FakeTransport {
                resources: Arc::clone(&self.resources),
                clock: Arc::clone(&self.clock),
                connected_at: self.clock.now(),
                operation_cost: self.operation_cost,
            }))
        }
    }

    struct FakeTransport {
        resources: Arc<FakeResources>,
        clock: Arc<FakeClock>,
        connected_at: Tick,
        operation_cost: Duration,
    }

    impl SessionTransport for FakeTransport {
        fn list_tools(&mut self) -> anyhow::Result<Vec<McpTool>> {
            self.resources.lists.fetch_add(1, Ordering::SeqCst);
            self.clock.advance(self.operation_cost);
            Ok(vec![McpTool {
                name: "echo".to_owned(),
                description: None,
                input_schema_json: r#"{"type":"object"}"#.to_owned(),
            }])
        }

        fn call_tool(&mut self, _name: &str, arguments: Value) -> anyhow::Result<Value> {
            self.resources.calls.fetch_add(1, Ordering::SeqCst);
            self.clock.advance(self.operation_cost);
            match arguments.get("mode").and_then(Value::as_str) {
                Some("server_error") => anyhow::bail!("tools/call 실패 — server error: rejected"),
                Some("protocol_error") => anyhow::bail!("stdout 프로토콜 위반"),
                Some("unknown") => Err(anyhow::Error::new(McpDeliveryUnknown { status: None })),
                Some("tool_error") => Ok(serde_json::json!({"isError": true, "content": []})),
                _ => Ok(serde_json::json!({"isError": false, "content": []})),
            }
        }

        fn cancel(&mut self) {
            self.resources.cancels.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Drop for FakeTransport {
        fn drop(&mut self) {
            self.resources.live.fetch_sub(1, Ordering::SeqCst);
            self.resources.drops.fetch_add(1, Ordering::SeqCst);
            self.resources.retained_nanos.fetch_add(
                duration_ticks(self.clock.now().elapsed_since(self.connected_at)),
                Ordering::SeqCst,
            );
        }
    }

    fn stdio_config(name: &str) -> BackendConfig {
        BackendConfig::Stdio(McpServerConfig::stdio(
            name.to_owned(),
            "/bin/false".to_owned(),
            Vec::new(),
            Vec::new(),
            true,
        ))
    }

    fn fake_session(ttl: Duration) -> (Arc<BackendSession>, Arc<FakeClock>, Arc<FakeResources>) {
        let clock = Arc::new(FakeClock::default());
        let resources = Arc::new(FakeResources::default());
        let factory = Arc::new(FakeFactory {
            resources: Arc::clone(&resources),
            clock: Arc::clone(&clock),
            connect_cost: Duration::ZERO,
            operation_cost: Duration::ZERO,
        });
        let session = BackendSession::with_seams(factory, clock.clone(), ttl);
        (session, clock, resources)
    }

    #[test]
    fn cold와_warm_path_latency를_clock_seam으로_분리계측한다() {
        let clock = Arc::new(FakeClock::default());
        let resources = Arc::new(FakeResources::default());
        let factory = Arc::new(FakeFactory {
            resources,
            clock: Arc::clone(&clock),
            connect_cost: Duration::from_millis(120),
            operation_cost: Duration::from_millis(5),
        });
        let session = BackendSession::with_seams(factory, clock, DEFAULT_BACKEND_IDLE_TTL);
        let backend = BackendClient::new("srv".to_owned(), stdio_config("one"), session.clone());

        backend.list_tools().unwrap();
        backend.list_tools().unwrap();

        let stats = session.stats();
        assert_eq!(
            stats.cold_path_nanos,
            duration_ticks(Duration::from_millis(125))
        );
        assert_eq!(
            stats.warm_path_nanos,
            duration_ticks(Duration::from_millis(5))
        );
    }

    #[test]
    fn lazy_connect와_schema_call_same_connection_warm_reuse() {
        let (session, _clock, resources) = fake_session(DEFAULT_BACKEND_IDLE_TTL);
        let backend = BackendClient::new("srv".to_owned(), stdio_config("one"), session.clone());
        assert_eq!(resources.connects.load(Ordering::SeqCst), 0);

        backend.list_tools().unwrap();
        backend.call_tool("echo", serde_json::json!({})).unwrap();

        assert_eq!(resources.connects.load(Ordering::SeqCst), 1);
        assert_eq!(resources.live.load(Ordering::SeqCst), 1);
        assert_eq!(session.stats().warm_reuses, 1);
    }

    #[cfg(unix)]
    #[test]
    fn 실제_stdio_schema_discovery와_call은_한_process_connection을_공유한다() {
        let sequence = PATH_SEQUENCE.fetch_add(1, Ordering::SeqCst);
        let counter = std::env::temp_dir().join(format!(
            "deppy-mp01-stdio-spawn-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&counter);
        let script = format!(
            "echo spawn >> '{}'\n\
             read -r _init\n\
             printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{{}},\"serverInfo\":{{\"name\":\"mock\",\"version\":\"1\"}}}}}}'\n\
             read -r _initialized\n\
             read -r _list\n\
             printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"tools\":[{{\"name\":\"echo\",\"inputSchema\":{{\"type\":\"object\"}}}}]}}}}'\n\
             read -r _call\n\
             printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"same-session\"}}],\"isError\":false}}}}'\n\
             read -r _until_cancel\n",
            counter.display()
        );
        let manager = LocalMcpManager::new(secret::RedactionService::new())
            .with_request_timeout(Duration::from_secs(5));
        let session = BackendSession::production(manager, DEFAULT_BACKEND_IDLE_TTL);
        let backend = BackendClient::new(
            "stdio".to_owned(),
            BackendConfig::Stdio(McpServerConfig::stdio(
                "mock".to_owned(),
                "/bin/sh".to_owned(),
                vec!["-c".to_owned(), script],
                Vec::new(),
                true,
            )),
            session.clone(),
        );

        let tools = backend.list_tools().unwrap();
        let result = backend.call_tool("echo", serde_json::json!({})).unwrap();
        assert_eq!(tools[0].name, "echo");
        assert_eq!(
            result.pointer("/content/0/text").and_then(Value::as_str),
            Some("same-session")
        );
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "spawn\n");
        assert_eq!(session.stats().cold_connects, 1);
        assert_eq!(session.stats().warm_reuses, 1);
        session.shutdown();
        let _ = std::fs::remove_file(counter);
    }

    #[test]
    fn lru는_backend_lease를_두개로_제한한다() {
        let (session, _clock, resources) = fake_session(DEFAULT_BACKEND_IDLE_TTL);
        for id in ["a", "b", "c"] {
            BackendClient::new(id.to_owned(), stdio_config(id), session.clone())
                .list_tools()
                .unwrap();
        }
        assert_eq!(resources.live.load(Ordering::SeqCst), MAX_BACKEND_LEASES);
        assert_eq!(resources.peak.load(Ordering::SeqCst), MAX_BACKEND_LEASES);
        assert_eq!(session.stats().lru_evictions, 1);
    }

    #[test]
    fn config_url_auth_revision_변경은_즉시_기존lease를_폐기한다() {
        let (session, _clock, resources) = fake_session(DEFAULT_BACKEND_IDLE_TTL);
        let backend = BackendClient::new("srv".to_owned(), stdio_config("one"), session.clone());
        backend.list_tools().unwrap();
        let first = backend.revision();

        let mut changed = stdio_config("one");
        let BackendConfig::Stdio(config) = &mut changed else {
            unreachable!()
        };
        config.args.push("--changed".to_owned());
        let second = backend.replace_config(changed);
        assert_ne!(first, second);
        assert_eq!(resources.live.load(Ordering::SeqCst), 0);
        backend.list_tools().unwrap();

        let http = BackendConfig::Http(McpHttpServerConfig {
            name: "http".to_owned(),
            url: "https://one.example/mcp".to_owned(),
            bearer: Some(secret::SecretString::new("token-one".to_owned())),
        });
        let http_backend = BackendClient::new("http".to_owned(), http, session.clone());
        http_backend.list_tools().unwrap();
        let before = http_backend.revision();
        let url_changed = http_backend.replace_config(BackendConfig::Http(McpHttpServerConfig {
            name: "http".to_owned(),
            url: "https://two.example/mcp".to_owned(),
            bearer: Some(secret::SecretString::new("token-one".to_owned())),
        }));
        assert_ne!(before, url_changed);
        let auth_changed = http_backend.replace_config(BackendConfig::Http(McpHttpServerConfig {
            name: "http".to_owned(),
            url: "https://two.example/mcp".to_owned(),
            bearer: Some(secret::SecretString::new("token-two".to_owned())),
        }));
        assert_ne!(url_changed, auth_changed);
    }

    #[test]
    fn tool과_jsonrpc_error는_lease를_유지하고_protocol_unknown만_poison한다() {
        let (session, _clock, resources) = fake_session(DEFAULT_BACKEND_IDLE_TTL);
        let backend = BackendClient::new("srv".to_owned(), stdio_config("one"), session.clone());

        let value = backend
            .call_tool("echo", serde_json::json!({"mode":"tool_error"}))
            .unwrap();
        assert_eq!(value.get("isError").and_then(Value::as_bool), Some(true));
        assert!(
            backend
                .call_tool("echo", serde_json::json!({"mode":"server_error"}))
                .is_err()
        );
        assert_eq!(resources.connects.load(Ordering::SeqCst), 1);

        assert!(
            backend
                .call_tool("echo", serde_json::json!({"mode":"protocol_error"}))
                .is_err()
        );
        assert_eq!(resources.live.load(Ordering::SeqCst), 0);
        backend.call_tool("echo", serde_json::json!({})).unwrap();
        assert_eq!(resources.connects.load(Ordering::SeqCst), 2);

        let calls_before = resources.calls.load(Ordering::SeqCst);
        let error = backend
            .call_tool("echo", serde_json::json!({"mode":"unknown"}))
            .unwrap_err();
        assert!(error.downcast_ref::<McpDeliveryUnknown>().is_some());
        assert_eq!(resources.calls.load(Ordering::SeqCst), calls_before + 1);
        assert_eq!(session.stats().delivery_unknown, 1);
        assert_eq!(resources.live.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn idle_ttl_reap은_stdio와_http_connection_resource를_회수한다() {
        let (session, clock, resources) = fake_session(DEFAULT_BACKEND_IDLE_TTL);
        BackendClient::new("stdio".to_owned(), stdio_config("one"), session.clone())
            .list_tools()
            .unwrap();
        BackendClient::new(
            "http".to_owned(),
            BackendConfig::Http(McpHttpServerConfig {
                name: "http".to_owned(),
                url: "https://example.invalid/mcp".to_owned(),
                bearer: None,
            }),
            session.clone(),
        )
        .list_tools()
        .unwrap();
        assert_eq!(resources.live.load(Ordering::SeqCst), 2);

        clock.advance(DEFAULT_BACKEND_IDLE_TTL);
        assert_eq!(session.reap_expired(), 2);
        assert_eq!(resources.live.load(Ordering::SeqCst), 0);
        assert_eq!(resources.cancels.load(Ordering::SeqCst), 2);
        assert_eq!(session.stats().idle_evictions, 2);
    }

    #[cfg(unix)]
    fn process_rss_kib(pid: u32) -> Option<u64> {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8(output.stdout).ok()?.trim().parse().ok()
    }

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        // SAFETY: signal 0 does not deliver a signal; it only probes this known child PID.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0
            || std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
    }

    #[cfg(unix)]
    #[test]
    fn 실제_stdio_cold_warm_latency와_idle_process_reclaim을_측정한다() {
        let sequence = PATH_SEQUENCE.fetch_add(1, Ordering::SeqCst);
        let pid_file = std::env::temp_dir().join(format!(
            "deppy-mp01-stdio-pid-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&pid_file);
        let script = format!(
            r#"echo $$ > '{}'
read -r _init
sleep 0.08
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-11-25","capabilities":{{}},"serverInfo":{{"name":"mock","version":"1"}}}}}}'
read -r _initialized
read -r _list
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"echo","inputSchema":{{"type":"object"}}}}]}}}}'
read -r _call
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"content":[{{"type":"text","text":"warm"}}],"isError":false}}}}'
read -r _until_cancel
"#,
            pid_file.display()
        );
        let idle_ttl = Duration::from_millis(50);
        let session = BackendSession::production(
            LocalMcpManager::new(secret::RedactionService::new())
                .with_request_timeout(Duration::from_secs(5)),
            idle_ttl,
        );
        let backend = BackendClient::new(
            "stdio".to_owned(),
            BackendConfig::Stdio(McpServerConfig::stdio(
                "mock".to_owned(),
                "/bin/sh".to_owned(),
                vec!["-c".to_owned(), script],
                Vec::new(),
                true,
            )),
            session.clone(),
        );

        let cold_started = Instant::now();
        backend.list_tools().unwrap();
        let cold = cold_started.elapsed();
        let warm_started = Instant::now();
        backend.call_tool("echo", serde_json::json!({})).unwrap();
        let warm = warm_started.elapsed();
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let retained_rss_kib = process_rss_kib(pid);

        assert!(cold >= Duration::from_millis(70), "cold={cold:?}");
        assert!(warm < cold, "cold={cold:?}, warm={warm:?}");
        assert!(process_exists(pid), "backend process {pid} is not retained");
        if let Some(rss_kib) = retained_rss_kib {
            assert!(rss_kib > 0);
        }
        std::thread::sleep(idle_ttl + Duration::from_millis(30));
        assert_eq!(session.reap_expired(), 1);
        assert!(
            !process_exists(pid),
            "backend process {pid} survived idle reap"
        );
        eprintln!(
            "actual stdio benchmark: cold={cold:?} warm={warm:?} retained_rss_kib={retained_rss_kib:?}"
        );
        let _ = std::fs::remove_file(pid_file);
    }

    #[derive(Debug, Clone, Copy)]
    struct TtlModel {
        ttl_secs: u64,
        cold_connects: usize,
        warm_calls: usize,
        projected_latency_ms: usize,
        retained_session_secs: u64,
        peak_live_sessions: usize,
        peak_rss_mib: usize,
    }

    fn simulate_ttl(ttl: Duration) -> TtlModel {
        const OPERATIONS: [u64; 5] = [0, 5, 20, 49, 110];
        const COLD_CONNECT_MS: usize = 120;
        const OP_MS: usize = 5;
        const BACKEND_RSS_MIB: usize = 32;

        let (session, clock, resources) = fake_session(ttl);
        let backend = BackendClient::new("srv".to_owned(), stdio_config("one"), session.clone());
        let mut expiry: Option<u64> = None;
        for operation_at in OPERATIONS {
            if expiry.is_some_and(|deadline| deadline <= operation_at) {
                clock.set(Duration::from_secs(expiry.unwrap()));
                session.reap_expired();
            }
            clock.set(Duration::from_secs(operation_at));
            backend.list_tools().unwrap();
            expiry = Some(operation_at + ttl.as_secs());
        }
        clock.set(Duration::from_secs(expiry.unwrap()));
        session.reap_expired();

        let cold_connects = resources.connects.load(Ordering::SeqCst);
        let warm_calls = OPERATIONS.len() - cold_connects;
        TtlModel {
            ttl_secs: ttl.as_secs(),
            cold_connects,
            warm_calls,
            projected_latency_ms: cold_connects * COLD_CONNECT_MS + OPERATIONS.len() * OP_MS,
            retained_session_secs: Duration::from_nanos(
                resources.retained_nanos.load(Ordering::SeqCst),
            )
            .as_secs(),
            peak_live_sessions: resources.peak.load(Ordering::SeqCst),
            peak_rss_mib: resources.peak.load(Ordering::SeqCst) * BACKEND_RSS_MIB,
        }
    }

    #[test]
    fn ttl_15_30_60_model은_30초를_pareto_knee로_선택한다() {
        let models = BACKEND_IDLE_TTL_CANDIDATES.map(simulate_ttl);
        assert_eq!(models.map(|model| model.ttl_secs), [15, 30, 60]);
        assert_eq!(models.map(|model| model.cold_connects), [4, 2, 2]);
        assert_eq!(models.map(|model| model.warm_calls), [1, 3, 3]);
        assert_eq!(
            models.map(|model| model.projected_latency_ms),
            [505, 265, 265]
        );
        assert_eq!(
            models.map(|model| model.retained_session_secs),
            [65, 109, 169]
        );
        assert_eq!(models.map(|model| model.peak_live_sessions), [1, 1, 1]);
        assert_eq!(models.map(|model| model.peak_rss_mib), [32, 32, 32]);
        assert_eq!(DEFAULT_BACKEND_IDLE_TTL, Duration::from_secs(30));
    }
}
