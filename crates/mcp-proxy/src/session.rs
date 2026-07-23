//! Reusable backend MCP sessions for the proxy.
//!
//! Connections are opened lazily, kept in a two-entry LRU, invalidated when their
//! non-secret config generation changes, and closed at the idle deadline. There is no
//! automatic call retry: in particular `McpDeliveryUnknown` is returned unchanged after the
//! connection is poisoned and released.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use mcp::{
    LocalMcpManager, McpConnection, McpDeliveryUnknown, McpHttpServerConfig, McpServerConfig,
    McpServerResponseError, McpTool, validate_mcp_url,
};
use mcp_store::McpServerRow;
use secret::{
    LogicalCredentialId, PhysicalSecretSlot, RedactionLease, RedactionService, SecretStore,
    SecretString,
};
use serde_json::Value;

pub const MAX_BACKEND_LEASES: usize = 2;
pub const MAX_BACKEND_CREDENTIALS: usize = 64;
pub const DEFAULT_BACKEND_IDLE_TTL: Duration = Duration::from_secs(30);
#[cfg(test)]
const BACKEND_IDLE_TTL_CANDIDATES: [Duration; 3] = [
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
];

/// Non-secret backend target retained while idle. Keyring values are resolved only on a cold
/// connection and remain inside that connection's bounded secret/redaction guard.
#[derive(Clone, PartialEq, Eq)]
pub enum BackendConfig {
    Stdio(StdioBackendConfig),
    Http(HttpBackendConfig),
}

#[derive(Clone, PartialEq, Eq)]
pub struct StdioBackendConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env_plain: Vec<(String, String)>,
    pub env_credentials: Vec<(String, String)>,
    pub inherit_env: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct HttpBackendConfig {
    pub name: String,
    pub url: String,
    /// Logical credential ID only. The current physical access slot is resolved at cold connect.
    pub bearer_credential_id: Option<String>,
}

impl BackendConfig {
    pub fn from_server_row(row: &McpServerRow) -> anyhow::Result<Self> {
        anyhow::ensure!(row.enabled, "MCP server is disabled");
        anyhow::ensure!(
            row.env_secrets.len() <= MAX_BACKEND_CREDENTIALS,
            "backend credential item limit exceeded"
        );
        match row.kind.as_str() {
            "stdio" => {
                let command = row
                    .command
                    .as_ref()
                    .filter(|command| !command.trim().is_empty())
                    .ok_or_else(|| anyhow::anyhow!("backend_stdio_command_missing"))?;
                Ok(Self::Stdio(StdioBackendConfig {
                    name: row.name.clone(),
                    command: command.clone(),
                    args: row.args.clone(),
                    env_plain: row.env_plain.clone(),
                    env_credentials: row.env_secrets.clone(),
                    inherit_env: row.inherit_env,
                }))
            }
            "http" => {
                anyhow::ensure!(
                    row.env_secrets.is_empty(),
                    "HTTP credential mapping is not supported by this proxy target"
                );
                let url = row
                    .url
                    .as_ref()
                    .filter(|url| !url.trim().is_empty())
                    .ok_or_else(|| anyhow::anyhow!("backend_http_url_missing"))?;
                validate_mcp_url(url).map_err(|_| anyhow::anyhow!("backend_http_url_invalid"))?;
                Ok(Self::Http(HttpBackendConfig {
                    name: row.name.clone(),
                    url: url.clone(),
                    bearer_credential_id: None,
                }))
            }
            _ => anyhow::bail!("backend_transport_unsupported"),
        }
    }

    fn credential_ids(&self) -> Vec<&str> {
        match self {
            Self::Stdio(config) => config
                .env_credentials
                .iter()
                .map(|(_, credential_id)| credential_id.as_str())
                .collect(),
            Self::Http(config) => config
                .bearer_credential_id
                .iter()
                .map(String::as_str)
                .collect(),
        }
    }
}

static NEXT_CONFIG_REVISION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConfigRevision(u64);

impl ConfigRevision {
    pub(crate) fn next() -> Self {
        Self(NEXT_CONFIG_REVISION.fetch_add(1, Ordering::Relaxed))
    }
}

/// Exact non-secret target generation used for one live-schema -> authorized-call handoff.
/// Physical slot names are bounded metadata, not secret values; the token is intentionally
/// non-Clone and exists only for the duration of one request.
pub(crate) struct BackendVersion {
    pub(crate) config_revision: ConfigRevision,
    pub(crate) auth_revision: Vec<String>,
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
    fn auth_revision(&self, config: &BackendConfig) -> anyhow::Result<Vec<String>>;
    fn connect(
        &self,
        config: &BackendConfig,
        auth_revision: &[String],
    ) -> anyhow::Result<ConnectedTransport>;
}

type SecretStoreInitializer = Arc<dyn Fn() -> anyhow::Result<()> + Send + Sync>;

struct ManagerBackendFactory {
    manager: LocalMcpManager,
    db: Arc<Mutex<storage::Db>>,
    redaction: RedactionService,
    secret_store: Arc<dyn SecretStore>,
    secret_store_initializer: Option<SecretStoreInitializer>,
}

struct SecretConnectionGuard {
    _secrets: Vec<SecretString>,
    _redaction: RedactionLease,
}

struct ConnectedTransport {
    connection: Box<dyn SessionTransport>,
    _secret_guard: Option<SecretConnectionGuard>,
}

impl BackendFactory for ManagerBackendFactory {
    fn auth_revision(&self, config: &BackendConfig) -> anyhow::Result<Vec<String>> {
        let credential_ids = config.credential_ids();
        anyhow::ensure!(
            credential_ids.len() <= MAX_BACKEND_CREDENTIALS,
            "backend credential item limit exceeded"
        );
        if credential_ids.is_empty() {
            return Ok(Vec::new());
        }
        let db = self.db.lock().expect("backend credential DB lock");
        credential_ids
            .into_iter()
            .map(|logical_id| resolve_credential_entry(&db, logical_id))
            .collect()
    }

    fn connect(
        &self,
        config: &BackendConfig,
        auth_revision: &[String],
    ) -> anyhow::Result<ConnectedTransport> {
        let resolved = self.resolve(config, auth_revision)?;
        let connection = match &resolved.config {
            ResolvedBackendConfig::Stdio(config) => self.manager.connect(config),
            ResolvedBackendConfig::Http(config) => self.manager.connect_http(config),
        }
        .map_err(|_| anyhow::anyhow!("backend_connect_failed"))?;
        Ok(ConnectedTransport {
            connection: Box::new(connection),
            _secret_guard: resolved.secret_guard,
        })
    }
}

enum ResolvedBackendConfig {
    Stdio(McpServerConfig),
    Http(McpHttpServerConfig),
}

struct ResolvedConnection {
    config: ResolvedBackendConfig,
    secret_guard: Option<SecretConnectionGuard>,
}

impl ManagerBackendFactory {
    fn resolve(
        &self,
        config: &BackendConfig,
        auth_revision: &[String],
    ) -> anyhow::Result<ResolvedConnection> {
        anyhow::ensure!(
            config.credential_ids().len() == auth_revision.len(),
            "backend auth revision shape mismatch"
        );
        let (secrets, secret_guard) = if auth_revision.is_empty() {
            (Vec::new(), None)
        } else {
            if let Some(initialize) = &self.secret_store_initializer {
                initialize().map_err(|_| anyhow::anyhow!("keyring_backend_init_failed"))?;
            }
            let mut secrets = Vec::with_capacity(auth_revision.len());
            for entry in auth_revision {
                secrets.push(
                    self.secret_store
                        .get_secret(entry)
                        .map_err(|_| anyhow::anyhow!("active_credential_read_failed"))?,
                );
            }
            let refs = secrets.iter().collect::<Vec<_>>();
            let lease = self
                .redaction
                .acquire_execution_lease(&refs)
                .map_err(|_| anyhow::anyhow!("secret_redaction_capacity_unavailable"))?;
            (
                secrets,
                Some(SecretConnectionGuard {
                    _secrets: Vec::new(),
                    _redaction: lease,
                }),
            )
        };
        let resolved_config = match config {
            BackendConfig::Stdio(config) => {
                let mut env = config.env_plain.clone();
                for ((key, _), secret) in config.env_credentials.iter().zip(&secrets) {
                    env.push((key.clone(), secret.expose().to_owned()));
                }
                ResolvedBackendConfig::Stdio(McpServerConfig::stdio(
                    config.name.clone(),
                    config.command.clone(),
                    config.args.clone(),
                    env,
                    config.inherit_env,
                ))
            }
            BackendConfig::Http(config) => ResolvedBackendConfig::Http(McpHttpServerConfig {
                name: config.name.clone(),
                url: config.url.clone(),
                bearer: secrets
                    .first()
                    .map(|secret| SecretString::new(secret.expose().to_owned())),
            }),
        };
        let secret_guard = secret_guard.map(|mut guard| {
            guard._secrets = secrets;
            guard
        });
        Ok(ResolvedConnection {
            config: resolved_config,
            secret_guard,
        })
    }
}

fn resolve_credential_entry(db: &storage::Db, logical_id: &str) -> anyhow::Result<String> {
    let logical_id = LogicalCredentialId::new(logical_id.to_owned())
        .map_err(|_| anyhow::anyhow!("backend_logical_credential_invalid"))?;
    let location = db
        .credential_secret_location(logical_id.as_str())
        .map_err(|_| anyhow::anyhow!("credential_metadata_read_failed"))?
        .ok_or_else(|| anyhow::anyhow!("credential_metadata_missing"))?;
    anyhow::ensure!(
        location.keyring_service == secret::KEYRING_SERVICE,
        "credential keyring service가 현재 backend와 일치하지 않습니다"
    );
    let physical_slot = PhysicalSecretSlot::parse(location.keyring_username)
        .map_err(|_| anyhow::anyhow!("credential_physical_slot_invalid"))?;
    anyhow::ensure!(
        physical_slot.belongs_to(&logical_id),
        "credential physical slot이 logical credential에 속하지 않습니다"
    );
    Ok(physical_slot.as_str().to_owned())
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
    auth_revision: Vec<String>,
    connection: Box<dyn SessionTransport>,
    _secret_guard: Option<SecretConnectionGuard>,
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
    pub(crate) fn production(
        manager: LocalMcpManager,
        db: Arc<Mutex<storage::Db>>,
        redaction: RedactionService,
        secret_store: Arc<dyn SecretStore>,
        secret_store_initializer: Option<SecretStoreInitializer>,
        idle_ttl: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            factory: Arc::new(ManagerBackendFactory {
                manager,
                db,
                redaction,
                secret_store,
                secret_store_initializer,
            }),
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

    #[cfg(test)]
    fn use_connection<T>(
        &self,
        target: &BackendTarget,
        operation: impl FnOnce(&mut dyn SessionTransport) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let auth_revision = self.factory.auth_revision(&target.config)?;
        self.use_connection_at(target, auth_revision, operation)
    }

    fn use_connection_at<T>(
        &self,
        target: &BackendTarget,
        auth_revision: Vec<String>,
        operation: impl FnOnce(&mut dyn SessionTransport) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let started = self.clock.now();
        let mut state = self.state.lock().expect("backend session lock");
        self.reap_expired_locked(&mut state, started);
        self.invalidate_target_locked(&mut state, &target.id, target.revision, &auth_revision);

        let (mut lease, warm) = if let Some(index) = state.leases.iter().position(|lease| {
            lease.backend_id == target.id
                && lease.revision == target.revision
                && lease.auth_revision == auth_revision
        }) {
            (state.leases.remove(index), true)
        } else {
            if state.leases.len() >= MAX_BACKEND_LEASES {
                let mut evicted = state.leases.remove(0);
                evicted.connection.cancel();
                state.stats.lru_evictions += 1;
            }
            let connected = self.factory.connect(&target.config, &auth_revision)?;
            state.stats.cold_connects += 1;
            (
                LeaseEntry {
                    backend_id: target.id.clone(),
                    revision: target.revision,
                    auth_revision,
                    connection: connected.connection,
                    _secret_guard: connected._secret_guard,
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

    fn auth_revision(&self, target: &BackendTarget) -> anyhow::Result<Vec<String>> {
        self.factory.auth_revision(&target.config)
    }

    fn invalidate_auth_revision(&self, target: &BackendTarget, auth_revision: &[String]) {
        let mut state = self.state.lock().expect("backend session lock");
        self.reap_expired_locked(&mut state, self.clock.now());
        self.invalidate_target_locked(&mut state, &target.id, target.revision, auth_revision);
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

    fn invalidate_target_locked(
        &self,
        state: &mut SessionState,
        backend_id: &str,
        revision: ConfigRevision,
        auth_revision: &[String],
    ) {
        let mut index = 0;
        while index < state.leases.len() {
            if state.leases[index].backend_id == backend_id
                && (state.leases[index].revision != revision
                    || state.leases[index].auth_revision != auth_revision)
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
        .any(|cause| cause.downcast_ref::<McpServerResponseError>().is_some())
}

/// A config handle shared by the permission hook and the forwarder. Updating it invalidates the
/// old revision immediately; no DB polling or background watcher is created.
pub struct BackendClient {
    session: Arc<BackendSession>,
    target: RwLock<BackendTarget>,
    live_db: Option<Arc<Mutex<storage::Db>>>,
}

impl BackendClient {
    #[cfg(test)]
    pub fn new(backend_id: String, config: BackendConfig, session: Arc<BackendSession>) -> Self {
        Self {
            session,
            target: RwLock::new(BackendTarget::new(backend_id, config)),
            live_db: None,
        }
    }

    pub fn managed(
        backend_id: String,
        config: BackendConfig,
        session: Arc<BackendSession>,
        live_db: Arc<Mutex<storage::Db>>,
    ) -> Self {
        Self {
            session,
            target: RwLock::new(BackendTarget::new(backend_id, config)),
            live_db: Some(live_db),
        }
    }

    #[cfg(test)]
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

    fn refresh_target(&self) -> anyhow::Result<BackendTarget> {
        let Some(db) = &self.live_db else {
            return Ok(self.target.read().expect("backend target lock").clone());
        };
        let backend_id = self.target.read().expect("backend target lock").id.clone();
        let refreshed = db
            .lock()
            .map_err(|_| anyhow::anyhow!("proxy DB unavailable"))
            .and_then(|db| {
                db.mcp_server(&backend_id)
                    .map_err(|_| anyhow::anyhow!("backend_target_read_failed"))?
                    .ok_or_else(|| anyhow::anyhow!("backend_target_missing"))
            })
            .and_then(|row| BackendConfig::from_server_row(&row));
        let refreshed = match refreshed {
            Ok(refreshed) => refreshed,
            Err(error) => {
                let current = self.target.read().expect("backend target lock").clone();
                self.session.invalidate(&current.id, ConfigRevision::next());
                return Err(error);
            }
        };

        let mut target = self.target.write().expect("backend target lock");
        if target.config.as_ref() != &refreshed {
            let replacement = BackendTarget::new(target.id.clone(), refreshed);
            self.session.invalidate(&target.id, replacement.revision);
            *target = replacement;
        }
        Ok(target.clone())
    }

    pub(crate) fn list_tools_versioned(&self) -> anyhow::Result<(BackendVersion, Vec<McpTool>)> {
        let target = self.refresh_target()?;
        let auth_revision = self.session.auth_revision(&target)?;
        let tools =
            self.session
                .use_connection_at(&target, auth_revision.clone(), |connection| {
                    connection.list_tools()
                })?;
        Ok((
            BackendVersion {
                config_revision: target.revision,
                auth_revision,
            },
            tools,
        ))
    }

    #[cfg(test)]
    pub fn list_tools(&self) -> anyhow::Result<Vec<McpTool>> {
        self.list_tools_versioned().map(|(_, tools)| tools)
    }

    pub(crate) fn call_tool_versioned(
        &self,
        expected_version: BackendVersion,
        name: &str,
        arguments: Value,
    ) -> anyhow::Result<Value> {
        let target = self.refresh_target()?;
        let auth_revision = self.session.auth_revision(&target)?;
        if target.revision != expected_version.config_revision
            || auth_revision != expected_version.auth_revision
        {
            self.session
                .invalidate_auth_revision(&target, &auth_revision);
            anyhow::bail!("backend target changed after live schema validation");
        }
        self.session
            .use_connection_at(&target, auth_revision, |connection| {
                connection.call_tool(name, arguments)
            })
    }

    #[cfg(test)]
    pub fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<Value> {
        let target = self.refresh_target()?;
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
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    static PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[derive(Default)]
    struct RecordingSecretStore {
        values: Mutex<HashMap<String, String>>,
        reads: Mutex<Vec<String>>,
    }

    impl RecordingSecretStore {
        fn seed(&self, id: &str, value: &str) {
            self.values
                .lock()
                .unwrap()
                .insert(id.to_owned(), value.to_owned());
        }

        fn take_reads(&self) -> Vec<String> {
            std::mem::take(&mut *self.reads.lock().unwrap())
        }
    }

    impl SecretStore for RecordingSecretStore {
        fn set_secret(&self, id: &str, secret: &SecretString) -> anyhow::Result<()> {
            self.seed(id, secret.expose());
            Ok(())
        }

        fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
            self.reads.lock().unwrap().push(id.to_owned());
            self.values
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .map(SecretString::new)
                .ok_or_else(|| anyhow::anyhow!("missing test secret"))
        }

        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            self.values.lock().unwrap().remove(id);
            Ok(())
        }

        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            Ok(self.values.lock().unwrap().contains_key(id))
        }
    }

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
        auth_revision: Mutex<Vec<String>>,
        secret_reads: AtomicUsize,
    }

    struct FakeFactory {
        resources: Arc<FakeResources>,
        clock: Arc<FakeClock>,
        connect_cost: Duration,
        operation_cost: Duration,
        redaction: Option<RedactionService>,
    }

    impl BackendFactory for FakeFactory {
        fn auth_revision(&self, _config: &BackendConfig) -> anyhow::Result<Vec<String>> {
            Ok(self.resources.auth_revision.lock().unwrap().clone())
        }

        fn connect(
            &self,
            _config: &BackendConfig,
            auth_revision: &[String],
        ) -> anyhow::Result<ConnectedTransport> {
            self.resources.connects.fetch_add(1, Ordering::SeqCst);
            self.resources
                .secret_reads
                .fetch_add(auth_revision.len(), Ordering::SeqCst);
            self.clock.advance(self.connect_cost);
            let live = self.resources.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.resources.peak.fetch_max(live, Ordering::SeqCst);
            let secret_guard = self.redaction.as_ref().map(|redaction| {
                let secret = SecretString::new("fake-lease-secret-material".to_owned());
                let lease = redaction.acquire_execution_lease(&[&secret]).unwrap();
                SecretConnectionGuard {
                    _secrets: vec![secret],
                    _redaction: lease,
                }
            });
            Ok(ConnectedTransport {
                connection: Box::new(FakeTransport {
                    resources: Arc::clone(&self.resources),
                    clock: Arc::clone(&self.clock),
                    connected_at: self.clock.now(),
                    operation_cost: self.operation_cost,
                }),
                _secret_guard: secret_guard,
            })
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
                Some("server_error") => Err(anyhow::Error::new(McpServerResponseError::from_code(
                    Some(-32001),
                ))),
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
        BackendConfig::Stdio(StdioBackendConfig {
            name: name.to_owned(),
            command: "/bin/false".to_owned(),
            args: Vec::new(),
            env_plain: Vec::new(),
            env_credentials: Vec::new(),
            inherit_env: true,
        })
    }

    fn http_server_row(id: &str, url: &str, enabled: bool) -> McpServerRow {
        McpServerRow {
            id: id.to_owned(),
            name: id.to_owned(),
            kind: "http".to_owned(),
            command: None,
            args: Vec::new(),
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            inherit_env: true,
            url: Some(url.to_owned()),
            enabled,
        }
    }

    fn fake_session(ttl: Duration) -> (Arc<BackendSession>, Arc<FakeClock>, Arc<FakeResources>) {
        let clock = Arc::new(FakeClock::default());
        let resources = Arc::new(FakeResources::default());
        let factory = Arc::new(FakeFactory {
            resources: Arc::clone(&resources),
            clock: Arc::clone(&clock),
            connect_cost: Duration::ZERO,
            operation_cost: Duration::ZERO,
            redaction: None,
        });
        let session = BackendSession::with_seams(factory, clock.clone(), ttl);
        (session, clock, resources)
    }

    fn test_session_db(label: &str) -> (PathBuf, Arc<Mutex<storage::Db>>) {
        let dir = std::env::temp_dir().join(format!(
            "deppy-proxy-session-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = storage::Db::open(&dir.join("metadata.sqlite3")).unwrap();
        (dir, Arc::new(Mutex::new(db)))
    }

    fn stage_secret_slot(
        db: &Arc<Mutex<storage::Db>>,
        logical: &LogicalCredentialId,
        slot: &PhysicalSecretSlot,
    ) {
        db.lock()
            .unwrap()
            .register_physical_secret_slot_staging(logical.as_str(), slot.as_str())
            .unwrap();
    }

    fn read_http_json(stream: &mut TcpStream) -> Option<Value> {
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break index;
            }
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                return None;
            }
            bytes.extend_from_slice(&chunk[..read]);
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let body_start = header_end + 4;
        while bytes.len().saturating_sub(body_start) < content_length {
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        serde_json::from_slice(&bytes[body_start..body_start + content_length]).ok()
    }

    fn write_http_json(stream: &mut TcpStream, status: u16, body: Option<Value>) {
        let body = body.map(|value| value.to_string()).unwrap_or_default();
        let content_type = if body.is_empty() {
            ""
        } else {
            "Content-Type: application/json\r\n"
        };
        write!(
            stream,
            "HTTP/1.1 {status} OK\r\nConnection: close\r\n{content_type}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        stream.flush().unwrap();
    }

    type HttpBackendFixture = (String, Arc<Mutex<Vec<String>>>, std::thread::JoinHandle<()>);

    fn spawn_http_backend() -> Option<HttpBackendFixture> {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
            Err(error) => panic!("HTTP mock bind failed: {error}"),
        };
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let methods = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&methods);
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while captured.lock().unwrap().len() < 4 && Instant::now() < deadline {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("HTTP mock accept failed: {error}"),
                };
                let Some(request) = read_http_json(&mut stream) else {
                    continue;
                };
                let method = request
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("response")
                    .to_owned();
                captured.lock().unwrap().push(method.clone());
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                match method.as_str() {
                    "initialize" => write_http_json(
                        &mut stream,
                        200,
                        Some(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2025-11-25",
                                "capabilities": {},
                                "serverInfo": {"name": "proxy-http", "version": "1"}
                            }
                        })),
                    ),
                    "notifications/initialized" => write_http_json(&mut stream, 202, None),
                    "tools/list" => write_http_json(
                        &mut stream,
                        200,
                        Some(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"tools": [{
                                "name": "echo",
                                "description": "Echo",
                                "inputSchema": {"type": "object"}
                            }]}
                        })),
                    ),
                    "tools/call" => write_http_json(
                        &mut stream,
                        200,
                        Some(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{"type": "text", "text": "http-warm"}],
                                "isError": false
                            }
                        })),
                    ),
                    other => panic!("unexpected HTTP MCP method: {other}"),
                }
            }
            assert_eq!(captured.lock().unwrap().len(), 4);
        });
        Some((format!("http://{address}/mcp"), methods, handle))
    }

    #[test]
    fn backend_config_error는_server_controlled_marker를_보존하지_않는다() {
        const MARKER: &str = "HOSTILE_BACKEND_DIAGNOSTIC_MARKER";
        let cases = [
            McpServerRow {
                id: MARKER.to_owned(),
                name: MARKER.to_owned(),
                kind: "stdio".to_owned(),
                command: None,
                args: Vec::new(),
                env_plain: Vec::new(),
                env_secrets: Vec::new(),
                inherit_env: false,
                url: None,
                enabled: true,
            },
            McpServerRow {
                id: MARKER.to_owned(),
                name: MARKER.to_owned(),
                kind: "http".to_owned(),
                command: None,
                args: Vec::new(),
                env_plain: Vec::new(),
                env_secrets: Vec::new(),
                inherit_env: false,
                url: Some(format!("https://[{MARKER}")),
                enabled: true,
            },
            McpServerRow {
                id: MARKER.to_owned(),
                name: MARKER.to_owned(),
                kind: MARKER.to_owned(),
                command: None,
                args: Vec::new(),
                env_plain: Vec::new(),
                env_secrets: Vec::new(),
                inherit_env: false,
                url: None,
                enabled: true,
            },
        ];

        for row in cases {
            let error = BackendConfig::from_server_row(&row)
                .err()
                .expect("hostile backend config must fail");
            let rendered = format!("{error:#}");
            assert!(!rendered.contains(MARKER));
        }
    }

    #[test]
    fn credential은_cold_resolve때만_active_physical_slot을_읽고_lease로_보호된다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-proxy-lazy-secret-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("metadata.sqlite3");
        let db = Arc::new(Mutex::new(storage::Db::open(&db_path).unwrap()));
        let logical = LogicalCredentialId::new("credential-logical").unwrap();
        db.lock()
            .unwrap()
            .insert_credential(&storage::CredentialMeta {
                id: logical.as_str().to_owned(),
                provider: "oauth".to_owned(),
                label: "OAuth".to_owned(),
                credential_kind: "oauth_token".to_owned(),
                masked_hint: None,
                workspace_id: None,
            })
            .unwrap();
        let slot_one = PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        stage_secret_slot(&db, &logical, &slot_one);
        db.lock()
            .unwrap()
            .rotate_credential_secret_slot(logical.as_str(), slot_one.as_str(), "{}", None)
            .unwrap();

        let store = Arc::new(RecordingSecretStore::default());
        store.seed(logical.as_str(), "legacy-secret-must-not-read");
        store.seed(slot_one.as_str(), "access-token-generation-one");
        let redaction = RedactionService::new();
        let initializer_calls = Arc::new(AtomicUsize::new(0));
        let initializer_calls_for_factory = Arc::clone(&initializer_calls);
        let factory = ManagerBackendFactory {
            manager: LocalMcpManager::new(redaction.clone()),
            db: Arc::clone(&db),
            redaction: redaction.clone(),
            secret_store: store.clone(),
            secret_store_initializer: Some(Arc::new(move || {
                initializer_calls_for_factory.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })),
        };
        let config = BackendConfig::Stdio(StdioBackendConfig {
            name: "server".to_owned(),
            command: "/bin/false".to_owned(),
            args: Vec::new(),
            env_plain: Vec::new(),
            env_credentials: vec![("TOKEN".to_owned(), logical.as_str().to_owned())],
            inherit_env: false,
        });

        assert!(
            store.take_reads().is_empty(),
            "startup must not read keyring"
        );
        assert_eq!(initializer_calls.load(Ordering::SeqCst), 0);
        let revision_one = factory.auth_revision(&config).unwrap();
        assert_eq!(revision_one, vec![slot_one.as_str().to_owned()]);
        assert!(
            store.take_reads().is_empty(),
            "pointer check must not read keyring"
        );
        assert_eq!(initializer_calls.load(Ordering::SeqCst), 0);
        let resolved = factory.resolve(&config, &revision_one).unwrap();
        assert_eq!(initializer_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.take_reads(), vec![slot_one.as_str().to_owned()]);
        assert_eq!(redaction.corpus_stats().active_leases, 1);
        let ResolvedBackendConfig::Stdio(resolved_config) = &resolved.config else {
            panic!("stdio config expected")
        };
        assert_eq!(
            resolved_config.env,
            vec![("TOKEN".to_owned(), "access-token-generation-one".to_owned())]
        );
        drop(resolved);
        assert_eq!(redaction.corpus_stats().active_leases, 0);

        let slot_two = PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        store.seed(slot_two.as_str(), "access-token-generation-two");
        stage_secret_slot(&db, &logical, &slot_two);
        db.lock()
            .unwrap()
            .rotate_credential_secret_slot(logical.as_str(), slot_two.as_str(), "{}", None)
            .unwrap();
        store.delete_secret(slot_one.as_str()).unwrap();
        let revision_two = factory.auth_revision(&config).unwrap();
        assert_eq!(revision_two, vec![slot_two.as_str().to_owned()]);
        assert_ne!(revision_one, revision_two);
        let resolved = factory.resolve(&config, &revision_two).unwrap();
        assert_eq!(initializer_calls.load(Ordering::SeqCst), 2);
        assert_eq!(store.take_reads(), vec![slot_two.as_str().to_owned()]);
        drop(resolved);

        drop(factory);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn credential_resolve는_own_versioned_slot만_keyring에서_읽는다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-proxy-owned-slot-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("metadata.sqlite3");
        let db = Arc::new(Mutex::new(storage::Db::open(&db_path).unwrap()));
        let logical = LogicalCredentialId::new("credential-owner").unwrap();
        let other = LogicalCredentialId::new("credential-other").unwrap();
        db.lock()
            .unwrap()
            .insert_credential(&storage::CredentialMeta {
                id: logical.as_str().to_owned(),
                provider: "oauth".to_owned(),
                label: "OAuth".to_owned(),
                credential_kind: "oauth_token".to_owned(),
                masked_hint: None,
                workspace_id: None,
            })
            .unwrap();
        let own_slot = PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        let other_slot = PhysicalSecretSlot::with_version(&other, uuid::Uuid::new_v4());
        stage_secret_slot(&db, &logical, &own_slot);
        db.lock()
            .unwrap()
            .rotate_credential_secret_slot(logical.as_str(), own_slot.as_str(), "{}", None)
            .unwrap();

        let store = Arc::new(RecordingSecretStore::default());
        store.seed(own_slot.as_str(), "owned-access-token");
        store.seed(other_slot.as_str(), "other-access-token");
        store.seed(logical.as_str(), "legacy-access-token");
        let redaction = RedactionService::new();
        let factory = ManagerBackendFactory {
            manager: LocalMcpManager::new(redaction.clone()),
            db: Arc::clone(&db),
            redaction,
            secret_store: store.clone(),
            secret_store_initializer: None,
        };
        let config = BackendConfig::Stdio(StdioBackendConfig {
            name: "server".to_owned(),
            command: "/bin/false".to_owned(),
            args: Vec::new(),
            env_plain: Vec::new(),
            env_credentials: vec![("TOKEN".to_owned(), logical.as_str().to_owned())],
            inherit_env: false,
        });

        let revision = factory.auth_revision(&config).unwrap();
        assert_eq!(revision, vec![own_slot.as_str().to_owned()]);
        let resolved = factory.resolve(&config, &revision).unwrap();
        assert_eq!(store.take_reads(), vec![own_slot.as_str().to_owned()]);
        drop(resolved);

        let raw = rusqlite::Connection::open(&db_path).unwrap();
        for invalid in [
            other_slot.as_str(),
            logical.as_str(),
            "deppy.oauth.v1.not-hex.not-a-uuid",
        ] {
            raw.execute(
                "UPDATE credentials SET keyring_username = ?2 WHERE id = ?1",
                (logical.as_str(), invalid),
            )
            .unwrap();
            assert!(
                factory.auth_revision(&config).is_err(),
                "invalid pointer unexpectedly resolved: {invalid}"
            );
            assert!(
                store.take_reads().is_empty(),
                "invalid pointer reached keyring: {invalid}"
            );
        }

        drop(raw);
        drop(factory);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn redaction_lease_확보실패는_backend_connect전에_fail_closed된다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-proxy-short-secret-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("metadata.sqlite3");
        let db = Arc::new(Mutex::new(storage::Db::open(&db_path).unwrap()));
        let logical = LogicalCredentialId::new("short-secret").unwrap();
        let slot = PhysicalSecretSlot::with_version(&logical, uuid::Uuid::new_v4());
        db.lock()
            .unwrap()
            .insert_credential(&storage::CredentialMeta {
                id: logical.as_str().to_owned(),
                provider: "test".to_owned(),
                label: "test".to_owned(),
                credential_kind: "api_key".to_owned(),
                masked_hint: None,
                workspace_id: None,
            })
            .unwrap();
        stage_secret_slot(&db, &logical, &slot);
        db.lock()
            .unwrap()
            .rotate_credential_secret_slot(logical.as_str(), slot.as_str(), "{}", None)
            .unwrap();
        let store = Arc::new(RecordingSecretStore::default());
        store.seed(slot.as_str(), "tiny");
        let redaction = RedactionService::new();
        let factory = ManagerBackendFactory {
            manager: LocalMcpManager::new(redaction.clone()),
            db: Arc::clone(&db),
            redaction,
            secret_store: store,
            secret_store_initializer: None,
        };
        let config = BackendConfig::Stdio(StdioBackendConfig {
            name: "server".to_owned(),
            command: "/bin/false".to_owned(),
            args: Vec::new(),
            env_plain: Vec::new(),
            env_credentials: vec![("TOKEN".to_owned(), logical.as_str().to_owned())],
            inherit_env: false,
        });
        let revision = factory.auth_revision(&config).unwrap();
        assert!(factory.resolve(&config, &revision).is_err());

        drop(factory);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
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
            redaction: None,
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
        let redaction = secret::RedactionService::new();
        let manager =
            LocalMcpManager::new(redaction.clone()).with_request_timeout(Duration::from_secs(5));
        let (db_dir, db) = test_session_db("same-stdio");
        let session = BackendSession::production(
            manager,
            Arc::clone(&db),
            redaction,
            Arc::new(RecordingSecretStore::default()),
            None,
            DEFAULT_BACKEND_IDLE_TTL,
        );
        let backend = BackendClient::new(
            "stdio".to_owned(),
            BackendConfig::Stdio(StdioBackendConfig {
                name: "mock".to_owned(),
                command: "/bin/sh".to_owned(),
                args: vec!["-c".to_owned(), script],
                env_plain: Vec::new(),
                env_credentials: Vec::new(),
                inherit_env: true,
            }),
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
        drop(backend);
        drop(session);
        drop(db);
        let _ = std::fs::remove_file(counter);
        std::fs::remove_dir_all(db_dir).unwrap();
    }

    #[test]
    fn 실제_http_schema_discovery와_call은_한_backend_session을_공유한다() {
        let Some((url, methods, server)) = spawn_http_backend() else {
            eprintln!("loopback bind is forbidden in this sandbox; HTTP integration test skipped");
            return;
        };
        let redaction = RedactionService::new();
        let (db_dir, db) = test_session_db("same-http");
        let session = BackendSession::production(
            LocalMcpManager::new(redaction.clone()).with_request_timeout(Duration::from_secs(5)),
            Arc::clone(&db),
            redaction,
            Arc::new(RecordingSecretStore::default()),
            None,
            DEFAULT_BACKEND_IDLE_TTL,
        );
        let backend = BackendClient::new(
            "http".to_owned(),
            BackendConfig::Http(HttpBackendConfig {
                name: "proxy-http".to_owned(),
                url,
                bearer_credential_id: None,
            }),
            session.clone(),
        );

        let tools = backend.list_tools().unwrap();
        let result = backend.call_tool("echo", serde_json::json!({})).unwrap();
        assert_eq!(tools[0].name, "echo");
        assert_eq!(
            result.pointer("/content/0/text").and_then(Value::as_str),
            Some("http-warm")
        );
        assert_eq!(session.stats().cold_connects, 1);
        assert_eq!(session.stats().warm_reuses, 1);
        server.join().unwrap();
        assert_eq!(
            *methods.lock().unwrap(),
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/call"
            ]
        );
        session.shutdown();
        drop(backend);
        drop(session);
        drop(db);
        std::fs::remove_dir_all(db_dir).unwrap();
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

        let http = BackendConfig::Http(HttpBackendConfig {
            name: "http".to_owned(),
            url: "https://one.example/mcp".to_owned(),
            bearer_credential_id: Some("credential-one".to_owned()),
        });
        let http_backend = BackendClient::new("http".to_owned(), http, session.clone());
        http_backend.list_tools().unwrap();
        let before = http_backend.revision();
        let url_changed = http_backend.replace_config(BackendConfig::Http(HttpBackendConfig {
            name: "http".to_owned(),
            url: "https://two.example/mcp".to_owned(),
            bearer_credential_id: Some("credential-one".to_owned()),
        }));
        assert_ne!(before, url_changed);
        let auth_changed = http_backend.replace_config(BackendConfig::Http(HttpBackendConfig {
            name: "http".to_owned(),
            url: "https://two.example/mcp".to_owned(),
            bearer_credential_id: Some("credential-two".to_owned()),
        }));
        assert_ne!(url_changed, auth_changed);
    }

    #[test]
    fn managed_target은_point_lookup으로_url변경을_교체하고_permission_reset을_관찰한다() {
        let (db_dir, db) = test_session_db("live-config");
        let initial = http_server_row("srv", "http://127.0.0.1:3010/mcp", true);
        db.lock().unwrap().insert_mcp_server(&initial).unwrap();
        db.lock()
            .unwrap()
            .upsert_permission_rule("srv", "echo", "allow", Some("old-schema"))
            .unwrap();
        let (session, _clock, resources) = fake_session(DEFAULT_BACKEND_IDLE_TTL);
        let backend = BackendClient::managed(
            "srv".to_owned(),
            BackendConfig::from_server_row(&initial).unwrap(),
            session.clone(),
            Arc::clone(&db),
        );

        let (old_version, _) = backend.list_tools_versioned().unwrap();
        let old_revision = old_version.config_revision;
        assert_eq!(resources.connects.load(Ordering::SeqCst), 1);
        db.lock()
            .unwrap()
            .update_mcp_server_url("srv", "http://127.0.0.1:3020/mcp")
            .unwrap();
        assert!(
            db.lock()
                .unwrap()
                .permission_rule("srv", "echo")
                .unwrap()
                .is_none()
        );

        let calls_before = resources.calls.load(Ordering::SeqCst);
        assert!(
            backend
                .call_tool_versioned(old_version, "echo", serde_json::json!({}))
                .is_err()
        );
        assert_eq!(resources.calls.load(Ordering::SeqCst), calls_before);
        assert_eq!(resources.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(resources.live.load(Ordering::SeqCst), 0);

        let (new_version, _) = backend.list_tools_versioned().unwrap();
        assert_ne!(old_revision, new_version.config_revision);
        let target = backend.target.read().unwrap();
        let BackendConfig::Http(config) = target.config.as_ref() else {
            panic!("HTTP target expected")
        };
        assert_eq!(config.url, "http://127.0.0.1:3020/mcp");
        assert_eq!(resources.connects.load(Ordering::SeqCst), 2);

        session.shutdown();
        drop(target);
        drop(backend);
        drop(session);
        drop(db);
        std::fs::remove_dir_all(db_dir).unwrap();
    }

    #[test]
    fn physical_slot_rotation은_live_schema_version을_stale처리하고_call을_하지_않는다() {
        let (session, _clock, resources) = fake_session(DEFAULT_BACKEND_IDLE_TTL);
        *resources.auth_revision.lock().unwrap() = vec!["slot-v1".to_owned()];
        let backend = BackendClient::new("srv".to_owned(), stdio_config("one"), session.clone());

        let (version_v1, _) = backend.list_tools_versioned().unwrap();
        assert_eq!(resources.connects.load(Ordering::SeqCst), 1);
        assert_eq!(resources.secret_reads.load(Ordering::SeqCst), 1);
        *resources.auth_revision.lock().unwrap() = vec!["slot-v2".to_owned()];
        assert!(
            backend
                .call_tool_versioned(version_v1, "echo", serde_json::json!({}))
                .is_err()
        );
        assert_eq!(resources.calls.load(Ordering::SeqCst), 0);
        assert_eq!(resources.secret_reads.load(Ordering::SeqCst), 1);
        assert_eq!(resources.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(session.stats().active_leases, 0);

        let (version_v2, _) = backend.list_tools_versioned().unwrap();
        backend
            .call_tool_versioned(version_v2, "echo", serde_json::json!({}))
            .unwrap();
        assert_eq!(resources.calls.load(Ordering::SeqCst), 1);
        assert_eq!(resources.connects.load(Ordering::SeqCst), 2);
        assert_eq!(resources.secret_reads.load(Ordering::SeqCst), 2);
        assert_eq!(session.stats().warm_reuses, 1);
    }

    #[test]
    fn managed_target은_missing_disabled_malformed를_external_call전에_fail_closed한다() {
        for (label, row) in [
            (
                "disabled",
                Some(http_server_row("srv", "http://127.0.0.1:3030/mcp", false)),
            ),
            (
                "malformed",
                Some(http_server_row("srv", "http://public.example/mcp", true)),
            ),
            ("missing", None),
        ] {
            let (db_dir, db) = test_session_db(label);
            if let Some(row) = &row {
                db.lock().unwrap().insert_mcp_server(row).unwrap();
            }
            let (session, _clock, resources) = fake_session(DEFAULT_BACKEND_IDLE_TTL);
            let backend = BackendClient::managed(
                "srv".to_owned(),
                BackendConfig::Http(HttpBackendConfig {
                    name: "initial-safe-target".to_owned(),
                    url: "http://127.0.0.1:3031/mcp".to_owned(),
                    bearer_credential_id: None,
                }),
                session.clone(),
                Arc::clone(&db),
            );

            assert!(backend.list_tools_versioned().is_err(), "case={label}");
            assert_eq!(resources.connects.load(Ordering::SeqCst), 0, "case={label}");
            assert_eq!(resources.calls.load(Ordering::SeqCst), 0, "case={label}");
            assert_eq!(session.stats().active_leases, 0, "case={label}");

            drop(backend);
            drop(session);
            drop(db);
            std::fs::remove_dir_all(db_dir).unwrap();
        }
    }

    #[test]
    fn malformed_target은_active_secret_guard를_회수하고_수정후_lazy_reconnect한다() {
        let (db_dir, db) = test_session_db("malformed-recovery");
        let initial = http_server_row("srv", "http://127.0.0.1:3040/mcp", true);
        db.lock().unwrap().insert_mcp_server(&initial).unwrap();
        let clock = Arc::new(FakeClock::default());
        let resources = Arc::new(FakeResources::default());
        let redaction = RedactionService::new();
        let factory = Arc::new(FakeFactory {
            resources: Arc::clone(&resources),
            clock: Arc::clone(&clock),
            connect_cost: Duration::ZERO,
            operation_cost: Duration::ZERO,
            redaction: Some(redaction.clone()),
        });
        let session = BackendSession::with_seams(factory, clock, DEFAULT_BACKEND_IDLE_TTL);
        let backend = BackendClient::managed(
            "srv".to_owned(),
            BackendConfig::from_server_row(&initial).unwrap(),
            session.clone(),
            Arc::clone(&db),
        );

        backend.list_tools_versioned().unwrap();
        assert_eq!(session.stats().active_leases, 1);
        assert_eq!(redaction.corpus_stats().active_leases, 1);
        db.lock()
            .unwrap()
            .update_mcp_server_url("srv", "http://public.example/mcp")
            .unwrap();
        assert!(backend.list_tools_versioned().is_err());
        assert_eq!(session.stats().active_leases, 0);
        assert_eq!(redaction.corpus_stats().active_leases, 0);

        db.lock()
            .unwrap()
            .update_mcp_server_url("srv", "http://127.0.0.1:3041/mcp")
            .unwrap();
        backend.list_tools_versioned().unwrap();
        assert_eq!(resources.connects.load(Ordering::SeqCst), 2);
        assert_eq!(redaction.corpus_stats().active_leases, 1);
        session.shutdown();
        assert_eq!(redaction.corpus_stats().active_leases, 0);

        drop(backend);
        drop(session);
        drop(db);
        std::fs::remove_dir_all(db_dir).unwrap();
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
            BackendConfig::Http(HttpBackendConfig {
                name: "http".to_owned(),
                url: "https://example.invalid/mcp".to_owned(),
                bearer_credential_id: None,
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

    #[test]
    fn 반복_reuse_ttl_unknown_cycle은_현재_resource를_baseline으로_복귀한다() {
        const CYCLES: usize = 32;
        const UNKNOWN_INTERVAL: usize = 8;
        let idle_ttl = Duration::from_millis(5);
        let (session, clock, resources) = fake_session(idle_ttl);
        let backend = BackendClient::new("srv".to_owned(), stdio_config("one"), session.clone());
        let mut unknowns = 0;

        for cycle in 1..=CYCLES {
            let connects_before = resources.connects.load(Ordering::SeqCst);
            let calls_before = resources.calls.load(Ordering::SeqCst);
            backend.list_tools().unwrap();
            backend.call_tool("echo", serde_json::json!({})).unwrap();
            assert_eq!(
                resources.connects.load(Ordering::SeqCst),
                connects_before + 1,
                "cycle={cycle}"
            );
            assert_eq!(
                resources.calls.load(Ordering::SeqCst),
                calls_before + 1,
                "cycle={cycle}"
            );
            assert_eq!(session.stats().active_leases, 1, "cycle={cycle}");
            assert_eq!(resources.live.load(Ordering::SeqCst), 1, "cycle={cycle}");

            clock.advance(idle_ttl);
            assert_eq!(session.reap_expired(), 1, "cycle={cycle}");
            assert_eq!(session.stats().active_leases, 0, "cycle={cycle}");
            assert_eq!(resources.live.load(Ordering::SeqCst), 0, "cycle={cycle}");
            assert!(session.next_expiry().is_none(), "cycle={cycle}");

            if cycle % UNKNOWN_INTERVAL == 0 {
                unknowns += 1;
                let connects_before = resources.connects.load(Ordering::SeqCst);
                let calls_before = resources.calls.load(Ordering::SeqCst);
                let error = backend
                    .call_tool("echo", serde_json::json!({"mode":"unknown"}))
                    .unwrap_err();
                assert!(
                    error.downcast_ref::<McpDeliveryUnknown>().is_some(),
                    "cycle={cycle}: {error:#}"
                );
                assert_eq!(
                    resources.connects.load(Ordering::SeqCst),
                    connects_before + 1,
                    "unknown call was automatically retried at cycle={cycle}"
                );
                assert_eq!(
                    resources.calls.load(Ordering::SeqCst),
                    calls_before + 1,
                    "unknown call was automatically retried at cycle={cycle}"
                );
                assert_eq!(session.stats().active_leases, 0, "cycle={cycle}");
                assert_eq!(resources.live.load(Ordering::SeqCst), 0, "cycle={cycle}");
                assert!(session.next_expiry().is_none(), "cycle={cycle}");
            }
        }

        let stats = session.stats();
        assert_eq!(stats.cold_connects, (CYCLES + unknowns) as u64);
        assert_eq!(stats.warm_reuses, CYCLES as u64);
        assert_eq!(stats.idle_evictions, CYCLES as u64);
        assert_eq!(stats.delivery_unknown, unknowns as u64);
        assert_eq!(stats.poisoned_connections, unknowns as u64);
        assert_eq!(stats.active_leases, 0);
        assert_eq!(stats.peak_leases, 1);
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
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct ActiveTransportSnapshot {
        active_threads: usize,
        stdio_stdout_threads: usize,
        stdio_stderr_threads: usize,
        stdio_writer_threads: usize,
        http_sender_threads: usize,
        http_progress_threads: usize,
        active_http_send_permits: usize,
        pending_http_reaper_queue: usize,
    }

    #[cfg(unix)]
    fn active_transport_snapshot() -> ActiveTransportSnapshot {
        let metrics = mcp::transport_metrics();
        ActiveTransportSnapshot {
            active_threads: metrics.active_threads,
            stdio_stdout_threads: metrics.stdio_stdout_threads,
            stdio_stderr_threads: metrics.stdio_stderr_threads,
            stdio_writer_threads: metrics.stdio_writer_threads,
            http_sender_threads: metrics.http_sender_threads,
            http_progress_threads: metrics.http_progress_threads,
            active_http_send_permits: metrics.active_http_send_permits,
            pending_http_reaper_queue: metrics.reaper_pending_http_senders,
        }
    }

    #[cfg(unix)]
    fn assert_transport_returns_to(baseline: ActiveTransportSnapshot, checkpoint: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let current = active_transport_snapshot();
            if current == baseline {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "transport resources did not return to baseline at checkpoint={checkpoint}: baseline={baseline:?}, current={current:?}"
            );
            std::thread::yield_now();
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "env-gated OD01 stdio session soak"]
    fn od01_env_stdio_session_soak은_모든_cycle_boundary에서_baseline을_회복한다() {
        const ENABLE_ENV: &str = "DEPPY_MCP_PROXY_SESSION_SOAK";
        const CHECKPOINTS_ENV: &str = "DEPPY_MCP_PROXY_SESSION_SOAK_CHECKPOINTS";
        const DEFAULT_CHECKPOINTS: usize = 91;
        const MAX_CHECKPOINTS: usize = 901;

        if std::env::var(ENABLE_ENV).as_deref() != Ok("1") {
            eprintln!("set {ENABLE_ENV}=1 to run the ignored OD01 stdio session soak");
            return;
        }
        let checkpoints = std::env::var(CHECKPOINTS_ENV)
            .ok()
            .map(|raw| {
                raw.parse::<usize>()
                    .unwrap_or_else(|_| panic!("{CHECKPOINTS_ENV} must be an integer"))
            })
            .unwrap_or(DEFAULT_CHECKPOINTS);
        assert!(
            (3..=MAX_CHECKPOINTS).contains(&checkpoints),
            "{CHECKPOINTS_ENV} must be between 3 and {MAX_CHECKPOINTS}"
        );
        let cycles = checkpoints - 1;
        let sequence = PATH_SEQUENCE.fetch_add(1, Ordering::SeqCst);
        let fixture_dir = std::env::temp_dir().join(format!(
            "deppy-od01-proxy-stdio-soak-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&fixture_dir).unwrap();
        let pid_file = fixture_dir.join("backend.pid");
        let unknown_marker = fixture_dir.join("unknown.marker");
        let unknown_calls = fixture_dir.join("unknown-calls.log");
        let script = format!(
            r#"echo $$ > '{}'
IFS= read -r _init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-11-25","capabilities":{{}},"serverInfo":{{"name":"od01-soak","version":"1"}}}}}}'
IFS= read -r _initialized
IFS= read -r _list
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"echo","inputSchema":{{"type":"object"}}}}]}}}}'
IFS= read -r _call
if [ -f '{}' ]; then
  printf '%s\n' unknown >> '{}'
  exit 0
fi
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"content":[{{"type":"text","text":"warm"}}],"isError":false}}}}'
IFS= read -r _until_cancel
"#,
            pid_file.display(),
            unknown_marker.display(),
            unknown_calls.display()
        );
        let idle_ttl = Duration::from_millis(10);
        let redaction = secret::RedactionService::new();
        let (db_dir, db) = test_session_db("od01-stdio-soak");
        let session = BackendSession::production(
            LocalMcpManager::new(redaction.clone()).with_request_timeout(Duration::from_secs(5)),
            Arc::clone(&db),
            redaction,
            Arc::new(RecordingSecretStore::default()),
            None,
            idle_ttl,
        );
        let backend = BackendClient::new(
            "stdio-soak".to_owned(),
            BackendConfig::Stdio(StdioBackendConfig {
                name: "od01-soak".to_owned(),
                command: "/bin/sh".to_owned(),
                args: vec!["-c".to_owned(), script],
                env_plain: Vec::new(),
                env_credentials: Vec::new(),
                inherit_env: true,
            }),
            session.clone(),
        );
        let baseline = active_transport_snapshot();
        assert_eq!(session.stats().active_leases, 0);
        assert!(session.next_expiry().is_none());
        let started = Instant::now();

        for cycle in 1..=cycles {
            let is_unknown_cycle = cycle == cycles;
            if is_unknown_cycle {
                std::fs::write(&unknown_marker, b"unknown\n").unwrap();
            }
            let stats_before = session.stats();
            let tools = backend.list_tools().unwrap();
            assert_eq!(tools.len(), 1, "cycle={cycle}");
            assert_eq!(
                session.stats().cold_connects,
                stats_before.cold_connects + 1
            );
            assert_eq!(session.stats().active_leases, 1, "cycle={cycle}");
            let pid = std::fs::read_to_string(&pid_file)
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap();
            assert!(process_exists(pid), "cycle={cycle}, pid={pid}");

            if is_unknown_cycle {
                let error = backend
                    .call_tool("echo", serde_json::json!({"cycle": cycle}))
                    .unwrap_err();
                assert!(
                    error.downcast_ref::<McpDeliveryUnknown>().is_some(),
                    "cycle={cycle}: {error:#}"
                );
                assert_eq!(
                    session.stats().cold_connects,
                    stats_before.cold_connects + 1,
                    "unknown delivery was automatically retried"
                );
                assert_eq!(
                    std::fs::read_to_string(&unknown_calls).unwrap(),
                    "unknown\n",
                    "unknown delivery reached the backend more than once"
                );
                assert_eq!(session.stats().delivery_unknown, 1);
                assert_eq!(session.stats().active_leases, 0);
                assert!(session.next_expiry().is_none());
            } else {
                let result = backend
                    .call_tool("echo", serde_json::json!({"cycle": cycle}))
                    .unwrap();
                assert_eq!(
                    result.pointer("/content/0/text").and_then(Value::as_str),
                    Some("warm")
                );
                assert_eq!(session.stats().warm_reuses, cycle as u64);
                std::thread::sleep(idle_ttl + Duration::from_millis(10));
                assert_eq!(session.reap_expired(), 1, "cycle={cycle}");
                assert_eq!(session.stats().active_leases, 0, "cycle={cycle}");
                assert!(session.next_expiry().is_none(), "cycle={cycle}");
            }

            assert!(!process_exists(pid), "cycle={cycle}, pid={pid}");
            assert_transport_returns_to(baseline, cycle + 1);
        }

        let stats = session.stats();
        assert_eq!(stats.cold_connects, cycles as u64);
        assert_eq!(stats.warm_reuses, cycles as u64);
        assert_eq!(stats.idle_evictions, (cycles - 1) as u64);
        assert_eq!(stats.delivery_unknown, 1);
        assert_eq!(stats.active_leases, 0);
        assert_eq!(stats.peak_leases, 1);
        assert_eq!(active_transport_snapshot(), baseline);
        eprintln!(
            "OD01 stdio session soak: checkpoints={checkpoints} cycles={cycles} elapsed={:?} cold={} warm={} idle_evictions={} unknown={} active_leases={} transport={:?}",
            started.elapsed(),
            stats.cold_connects,
            stats.warm_reuses,
            stats.idle_evictions,
            stats.delivery_unknown,
            stats.active_leases,
            active_transport_snapshot()
        );

        drop(backend);
        drop(session);
        drop(db);
        std::fs::remove_dir_all(db_dir).unwrap();
        std::fs::remove_dir_all(fixture_dir).unwrap();
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
        let redaction = secret::RedactionService::new();
        let (db_dir, db) = test_session_db("latency");
        let session = BackendSession::production(
            LocalMcpManager::new(redaction.clone()).with_request_timeout(Duration::from_secs(5)),
            Arc::clone(&db),
            redaction,
            Arc::new(RecordingSecretStore::default()),
            None,
            idle_ttl,
        );
        let backend = BackendClient::new(
            "stdio".to_owned(),
            BackendConfig::Stdio(StdioBackendConfig {
                name: "mock".to_owned(),
                command: "/bin/sh".to_owned(),
                args: vec!["-c".to_owned(), script],
                env_plain: Vec::new(),
                env_credentials: Vec::new(),
                inherit_env: true,
            }),
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
        drop(backend);
        drop(session);
        drop(db);
        let _ = std::fs::remove_file(pid_file);
        std::fs::remove_dir_all(db_dir).unwrap();
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
