use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use connector_contract::{
    ErrorCode, OperationId, Revision, ServerDraft, ServerId, ToolId, TransportDraft,
};

use crate::ports::{
    AuthorizedInvokeRequest, CancellationToken, ConnectorMcp, ConnectorSecrets,
    CredentialResolutionRequest, DiscoverOutput, DiscoveredTool, LiveToolSchema, McpRequestTarget,
    ResolvedCredentials, ServiceError,
};

#[derive(Clone, PartialEq, Eq)]
struct LeaseKey {
    server_id: ServerId,
    config_revision: Revision,
    endpoint_fingerprint: String,
    physical_revisions: Vec<String>,
}

enum ConnectConfig {
    Stdio(mcp::McpStdioConnectConfig),
    Http(mcp::McpHttpServerConfig),
}

struct PreparedTarget {
    key: LeaseKey,
    server: ServerDraft,
    credential_revisions: Vec<CredentialResolutionRequest>,
}

struct ConnectedTarget {
    key: LeaseKey,
    config: ConnectConfig,
    redaction: Option<secret::RedactionLease>,
}

impl PreparedTarget {
    fn from_request(target: McpRequestTarget) -> Result<Self, ServiceError> {
        let McpRequestTarget {
            server,
            config_revision,
            credential_revisions,
        } = target;
        let server_id = server.id.clone().ok_or_else(|| {
            ServiceError::new(
                ErrorCode::StorageUnavailable,
                "MCP target has no stable server identifier",
            )
        })?;
        let mut physical_revisions = Vec::with_capacity(credential_revisions.len());
        for request in &credential_revisions {
            let slot = request.expected_physical_slot.as_ref().ok_or_else(|| {
                ServiceError::new(
                    ErrorCode::SecretUnavailable,
                    "MCP credential revision is missing",
                )
            })?;
            physical_revisions.push(slot.as_str().to_owned());
        }
        let endpoint_fingerprint = match &server.transport {
            TransportDraft::Stdio {
                command,
                secret_env,
                ..
            } => {
                if secret_env.len() != credential_revisions.len()
                    || secret_env
                        .iter()
                        .zip(&credential_revisions)
                        .any(|((_, expected_id), request)| expected_id != &request.credential_id)
                {
                    return Err(ServiceError::new(
                        ErrorCode::SecretUnavailable,
                        "stdio credential revision shape mismatch",
                    ));
                }
                audit::schema_hash(&format!("stdio\u{0}{}\u{0}{}", server_id.as_str(), command))
            }
            TransportDraft::Http { url } => {
                if credential_revisions.len() > 1 {
                    return Err(ServiceError::new(
                        ErrorCode::SecretUnavailable,
                        "HTTP MCP target has ambiguous credentials",
                    ));
                }
                audit::schema_hash(&format!("http\u{0}{}\u{0}{url}", server_id.as_str()))
            }
        };
        Ok(Self {
            key: LeaseKey {
                server_id,
                config_revision,
                endpoint_fingerprint,
                physical_revisions,
            },
            server,
            credential_revisions,
        })
    }

    fn resolve(self, credentials: ResolvedCredentials) -> Result<ConnectedTarget, ServiceError> {
        if credentials.entries().len() != self.credential_revisions.len()
            || credentials
                .entries()
                .iter()
                .zip(&self.credential_revisions)
                .any(|(resolved, request)| {
                    resolved.credential_id() != &request.credential_id
                        || request
                            .expected_physical_slot
                            .as_ref()
                            .is_none_or(|slot| resolved.physical_slot() != slot)
                })
        {
            return Err(ServiceError::new(
                ErrorCode::SecretUnavailable,
                "resolved credential revision does not match MCP target",
            ));
        }
        let (credentials, redaction) = credentials.into_parts();
        let config = match self.server.transport {
            TransportDraft::Stdio {
                command,
                args,
                plain_env,
                secret_env,
                inherit_env,
            } => {
                let mut resolved_env = Vec::with_capacity(secret_env.len());
                for ((key, expected_id), credential) in secret_env.into_iter().zip(credentials) {
                    let (credential_id, _, value) = credential.into_parts();
                    if credential_id != expected_id {
                        return Err(ServiceError::new(
                            ErrorCode::SecretUnavailable,
                            "resolved stdio credential identity mismatch",
                        ));
                    }
                    resolved_env.push((key, value));
                }
                ConnectConfig::Stdio(mcp::McpStdioConnectConfig {
                    name: self.server.name,
                    command,
                    args,
                    plain_env,
                    secret_env: resolved_env,
                    inherit_env,
                })
            }
            TransportDraft::Http { url } => {
                let bearer = credentials.into_iter().next().map(|credential| {
                    let (_, _, value) = credential.into_parts();
                    value
                });
                ConnectConfig::Http(mcp::McpHttpServerConfig {
                    name: self.server.name,
                    url,
                    bearer,
                })
            }
        };
        Ok(ConnectedTarget {
            key: self.key,
            config,
            redaction,
        })
    }
}

struct LeaseEntry {
    key: LeaseKey,
    connection: mcp::McpConnection,
    _redaction: Option<secret::RedactionLease>,
    last_used: Instant,
}

struct ActiveUse {
    key: LeaseKey,
    cancellation: Option<mcp::McpCancellationHandle>,
    cancelled: bool,
}

#[derive(Default)]
struct LeaseState {
    available: VecDeque<LeaseEntry>,
    pinned: HashMap<OperationId, LeaseEntry>,
    active: HashMap<OperationId, ActiveUse>,
}

/// Production connector MCP adapter. It is entirely lazy: construction creates no thread,
/// process, socket, connection, or timer. At most two available/pinned/active leases exist, and
/// expiry is driven only by coordinator deadlines or explicit calls.
pub struct ProductionConnectorMcp {
    manager: mcp::LocalMcpManager,
    secrets: Arc<dyn ConnectorSecrets>,
    idle_ttl: Duration,
    max_leases: usize,
    state: Mutex<LeaseState>,
}

impl ProductionConnectorMcp {
    pub fn new(
        manager: mcp::LocalMcpManager,
        secrets: Arc<dyn ConnectorSecrets>,
        idle_ttl: Duration,
        max_leases: usize,
    ) -> Result<Self, ServiceError> {
        if idle_ttl.is_zero() || !(1..=2).contains(&max_leases) {
            return Err(ServiceError::new(
                ErrorCode::LimitExceeded,
                "MCP lease limits are invalid",
            ));
        }
        Ok(Self {
            manager,
            secrets,
            idle_ttl,
            max_leases,
            state: Mutex::new(LeaseState::default()),
        })
    }

    fn checkout(
        &self,
        operation_id: &OperationId,
        target: McpRequestTarget,
        cancellation: &CancellationToken,
    ) -> Result<LeaseEntry, ServiceError> {
        let prepared = PreparedTarget::from_request(target)?;
        if cancellation.is_cancelled() {
            return Err(cancelled_error());
        }

        let mut state = self.state.lock().expect("connector MCP lease lock");
        self.reap_expired_locked(&mut state, Instant::now());
        self.invalidate_mismatched_locked(&mut state, &prepared.key);
        if state.active.contains_key(operation_id) || state.pinned.contains_key(operation_id) {
            return Err(ServiceError::new(
                ErrorCode::Backpressure,
                "MCP operation already owns a connection",
            ));
        }
        if let Some(index) = state
            .available
            .iter()
            .position(|lease| lease.key == prepared.key)
        {
            let lease = state
                .available
                .remove(index)
                .expect("matching MCP lease index");
            let handle = lease.connection.cancellation_handle();
            state.active.insert(
                operation_id.clone(),
                ActiveUse {
                    key: lease.key.clone(),
                    cancellation: Some(handle.clone()),
                    cancelled: false,
                },
            );
            drop(state);
            drop(prepared);
            if cancellation.is_cancelled() {
                handle.cancel();
                self.state
                    .lock()
                    .expect("connector MCP lease lock")
                    .active
                    .remove(operation_id);
                return Err(cancelled_error());
            }
            return Ok(lease);
        }

        while lease_count(&state) >= self.max_leases {
            let Some(mut evicted) = state.available.pop_front() else {
                return Err(ServiceError::new(
                    ErrorCode::Backpressure,
                    "all MCP connection leases are active",
                ));
            };
            evicted.connection.cancel();
        }
        state.active.insert(
            operation_id.clone(),
            ActiveUse {
                key: prepared.key.clone(),
                cancellation: None,
                cancelled: false,
            },
        );
        drop(state);

        let credentials = if prepared.credential_revisions.is_empty() {
            ResolvedCredentials::empty()
        } else {
            match self
                .secrets
                .resolve_credentials(prepared.credential_revisions.clone())
            {
                Ok(credentials) => credentials,
                Err(error) => {
                    self.state
                        .lock()
                        .expect("connector MCP lease lock")
                        .active
                        .remove(operation_id);
                    return Err(error);
                }
            }
        };
        let ConnectedTarget {
            key,
            config,
            redaction,
        } = match prepared.resolve(credentials) {
            Ok(target) => target,
            Err(error) => {
                self.state
                    .lock()
                    .expect("connector MCP lease lock")
                    .active
                    .remove(operation_id);
                return Err(error);
            }
        };
        let connection = match config {
            ConnectConfig::Stdio(config) => {
                let operation = operation_id.clone();
                let cancellation = cancellation.clone();
                self.manager.connect_scoped_cancellable(config, |handle| {
                    self.register_cancellation(&operation, handle, &cancellation);
                })
            }
            ConnectConfig::Http(config) => self.manager.connect_http_owned(config),
        };
        let mut connection = match connection {
            Ok(connection) => connection,
            Err(error) if error.downcast_ref::<mcp::McpAuthRequired>().is_some() => {
                self.state
                    .lock()
                    .expect("connector MCP lease lock")
                    .active
                    .remove(operation_id);
                return Err(ServiceError::new(
                    ErrorCode::AuthenticationRequired,
                    "MCP authentication is required",
                ));
            }
            Err(_error) => {
                self.state
                    .lock()
                    .expect("connector MCP lease lock")
                    .active
                    .remove(operation_id);
                return Err(ServiceError::new(
                    ErrorCode::TransportFailed,
                    "MCP connection failed",
                ));
            }
        };
        let handle = connection.cancellation_handle();
        self.register_cancellation(operation_id, handle, cancellation);
        let cancelled = self
            .state
            .lock()
            .expect("connector MCP lease lock")
            .active
            .get(operation_id)
            .is_none_or(|active| active.cancelled)
            || cancellation.is_cancelled();
        if cancelled {
            connection.cancel();
            self.state
                .lock()
                .expect("connector MCP lease lock")
                .active
                .remove(operation_id);
            return Err(cancelled_error());
        }
        Ok(LeaseEntry {
            key,
            connection,
            _redaction: redaction,
            last_used: Instant::now(),
        })
    }

    fn register_cancellation(
        &self,
        operation_id: &OperationId,
        handle: mcp::McpCancellationHandle,
        token: &CancellationToken,
    ) {
        let cancel = {
            let mut state = self.state.lock().expect("connector MCP lease lock");
            if let Some(active) = state.active.get_mut(operation_id) {
                active.cancellation = Some(handle.clone());
                active.cancelled || token.is_cancelled()
            } else {
                true
            }
        };
        if cancel {
            handle.cancel();
        }
    }

    fn finish_available(
        &self,
        operation_id: &OperationId,
        mut lease: LeaseEntry,
        reusable: bool,
        pin: bool,
    ) {
        let mut state = self.state.lock().expect("connector MCP lease lock");
        let cancelled = state
            .active
            .remove(operation_id)
            .is_none_or(|active| active.cancelled);
        if cancelled || !reusable {
            drop(state);
            lease.connection.cancel();
            return;
        }
        lease.last_used = Instant::now();
        if pin {
            state.pinned.insert(operation_id.clone(), lease);
        } else {
            state.available.push_back(lease);
        }
    }

    fn take_pinned(
        &self,
        operation_id: &OperationId,
        server: &ServerDraft,
        cancellation: &CancellationToken,
    ) -> Result<LeaseEntry, ServiceError> {
        let mut state = self.state.lock().expect("connector MCP lease lock");
        let mut lease = state.pinned.remove(operation_id).ok_or_else(|| {
            ServiceError::new(
                ErrorCode::StaleResult,
                "authorized MCP connection lease is unavailable",
            )
        })?;
        let server_matches = server.id.as_ref() == Some(&lease.key.server_id)
            && match &server.transport {
                TransportDraft::Http { url } => {
                    lease.key.endpoint_fingerprint
                        == audit::schema_hash(&format!(
                            "http\u{0}{}\u{0}{url}",
                            lease.key.server_id.as_str()
                        ))
                }
                TransportDraft::Stdio { command, .. } => {
                    lease.key.endpoint_fingerprint
                        == audit::schema_hash(&format!(
                            "stdio\u{0}{}\u{0}{command}",
                            lease.key.server_id.as_str()
                        ))
                }
            };
        if !server_matches || cancellation.is_cancelled() {
            drop(state);
            lease.connection.cancel();
            return Err(if cancellation.is_cancelled() {
                cancelled_error()
            } else {
                ServiceError::new(ErrorCode::StaleResult, "MCP lease target changed")
            });
        }
        let handle = lease.connection.cancellation_handle();
        state.active.insert(
            operation_id.clone(),
            ActiveUse {
                key: lease.key.clone(),
                cancellation: Some(handle),
                cancelled: false,
            },
        );
        Ok(lease)
    }

    fn invalidate_mismatched_locked(&self, state: &mut LeaseState, key: &LeaseKey) {
        let mut available = VecDeque::with_capacity(state.available.len());
        while let Some(mut lease) = state.available.pop_front() {
            if lease.key.server_id == key.server_id && lease.key != *key {
                lease.connection.cancel();
            } else {
                available.push_back(lease);
            }
        }
        state.available = available;

        let stale_pinned = state
            .pinned
            .iter()
            .filter(|(_, lease)| lease.key.server_id == key.server_id && lease.key != *key)
            .map(|(operation_id, _)| operation_id.clone())
            .collect::<Vec<_>>();
        for operation_id in stale_pinned {
            if let Some(mut lease) = state.pinned.remove(&operation_id) {
                lease.connection.cancel();
            }
        }
        for active in state.active.values_mut() {
            if active.key.server_id == key.server_id && active.key != *key {
                active.cancelled = true;
                if let Some(handle) = &active.cancellation {
                    handle.cancel();
                }
            }
        }
    }

    fn reap_expired_locked(&self, state: &mut LeaseState, now: Instant) {
        let mut retained = VecDeque::with_capacity(state.available.len());
        while let Some(mut lease) = state.available.pop_front() {
            if now.saturating_duration_since(lease.last_used) >= self.idle_ttl {
                lease.connection.cancel();
            } else {
                retained.push_back(lease);
            }
        }
        state.available = retained;
    }
}

impl ConnectorMcp for ProductionConnectorMcp {
    fn discover(
        &self,
        operation_id: &OperationId,
        target: McpRequestTarget,
        cancellation: CancellationToken,
    ) -> Result<DiscoverOutput, ServiceError> {
        let mut lease = self.checkout(operation_id, target, &cancellation)?;
        let result = lease.connection.list_tools();
        match result {
            Ok(tools) if !cancellation.is_cancelled() => {
                let output = DiscoverOutput {
                    tools: tools
                        .into_iter()
                        .map(|tool| DiscoveredTool {
                            id: ToolId::new(tool.name.clone()),
                            descriptor_bytes: tool
                                .name
                                .len()
                                .saturating_add(tool.description.as_ref().map_or(0, String::len))
                                .saturating_add(tool.input_schema_json.len()),
                            name: tool.name,
                            description: tool.description,
                        })
                        .collect(),
                };
                self.finish_available(operation_id, lease, true, false);
                Ok(output)
            }
            Ok(_) => {
                self.finish_available(operation_id, lease, false, false);
                Err(cancelled_error())
            }
            Err(error) if error.downcast_ref::<mcp::McpAuthRequired>().is_some() => {
                self.finish_available(operation_id, lease, false, false);
                Err(ServiceError::new(
                    ErrorCode::AuthenticationRequired,
                    "MCP authentication is required",
                ))
            }
            Err(_error) => {
                self.finish_available(operation_id, lease, false, false);
                Err(ServiceError::new(
                    ErrorCode::TransportFailed,
                    "MCP tool discovery failed",
                ))
            }
        }
    }

    fn load_live_schema(
        &self,
        operation_id: &OperationId,
        target: McpRequestTarget,
        tool_id: ToolId,
        tool_name: String,
        cancellation: CancellationToken,
    ) -> Result<LiveToolSchema, ServiceError> {
        let mut lease = self.checkout(operation_id, target, &cancellation)?;
        let result = lease.connection.list_tools();
        match result {
            Ok(tools) if !cancellation.is_cancelled() => {
                let tool = tools.into_iter().find(|tool| tool.name == tool_name);
                let Some(tool) = tool else {
                    self.finish_available(operation_id, lease, false, false);
                    return Err(ServiceError::new(
                        ErrorCode::ProtocolViolation,
                        "live MCP tool is missing",
                    ));
                };
                let live = LiveToolSchema {
                    tool_id,
                    tool_name: tool.name,
                    input_schema_json: tool.input_schema_json,
                };
                self.finish_available(operation_id, lease, true, true);
                Ok(live)
            }
            Ok(_) => {
                self.finish_available(operation_id, lease, false, false);
                Err(cancelled_error())
            }
            Err(error) if error.downcast_ref::<mcp::McpAuthRequired>().is_some() => {
                self.finish_available(operation_id, lease, false, false);
                Err(ServiceError::new(
                    ErrorCode::AuthenticationRequired,
                    "MCP authentication is required",
                ))
            }
            Err(_error) => {
                self.finish_available(operation_id, lease, false, false);
                Err(ServiceError::new(
                    ErrorCode::TransportFailed,
                    "live MCP schema discovery failed",
                ))
            }
        }
    }

    fn invoke_authorized(
        &self,
        request: AuthorizedInvokeRequest,
        cancellation: CancellationToken,
    ) -> Result<String, ServiceError> {
        let operation_id = OperationId::new(request.operation_id().to_owned());
        let mut lease = self.take_pinned(&operation_id, request.server(), &cancellation)?;
        let result = lease
            .connection
            .call_tool_json(request.tool_name(), request.arguments_json());
        match result {
            // Once the backend returned Ok, the external call is known delivered. A cancellation
            // observed after that point may suppress stale UI, but must never rewrite the durable
            // tool outcome to Failed or trigger a retry.
            Ok(output) => {
                self.finish_available(&operation_id, lease, true, false);
                Ok(output)
            }
            Err(error) if error.downcast_ref::<mcp::McpDeliveryUnknown>().is_some() => {
                self.finish_available(&operation_id, lease, false, false);
                Err(ServiceError::new(
                    ErrorCode::UnknownDelivery,
                    "MCP tool call delivery is unknown and was not retried",
                ))
            }
            Err(error) if error.downcast_ref::<mcp::McpAuthRequired>().is_some() => {
                self.finish_available(&operation_id, lease, false, false);
                Err(ServiceError::new(
                    ErrorCode::AuthenticationRequired,
                    "MCP authentication is required and the call was not retried",
                ))
            }
            Err(error)
                if error
                    .downcast_ref::<mcp::McpServerResponseError>()
                    .is_some() =>
            {
                self.finish_available(&operation_id, lease, true, false);
                Err(ServiceError::new(
                    ErrorCode::TransportFailed,
                    "MCP server returned a known tool failure",
                ))
            }
            Err(_error) => {
                self.finish_available(&operation_id, lease, false, false);
                Err(ServiceError::new(
                    ErrorCode::TransportFailed,
                    "MCP tool call failed",
                ))
            }
        }
    }

    fn cancel(&self, operation_id: &OperationId) {
        let (mut pinned, active_handle) = {
            let mut state = self.state.lock().expect("connector MCP lease lock");
            let pinned = state.pinned.remove(operation_id);
            let active_handle = state.active.get_mut(operation_id).and_then(|active| {
                active.cancelled = true;
                active.cancellation.clone()
            });
            (pinned, active_handle)
        };
        if let Some(handle) = active_handle {
            handle.cancel();
        }
        if let Some(lease) = pinned.as_mut() {
            lease.connection.cancel();
        }
    }

    fn active_leases(&self) -> usize {
        lease_count(&self.state.lock().expect("connector MCP lease lock"))
    }

    fn reap_idle_leases(&self) {
        let mut state = self.state.lock().expect("connector MCP lease lock");
        self.reap_expired_locked(&mut state, Instant::now());
    }
}

impl Drop for ProductionConnectorMcp {
    fn drop(&mut self) {
        let Ok(state) = self.state.get_mut() else {
            return;
        };
        for lease in &mut state.available {
            lease.connection.cancel();
        }
        for lease in state.pinned.values_mut() {
            lease.connection.cancel();
        }
        for active in state.active.values() {
            if let Some(handle) = &active.cancellation {
                handle.cancel();
            }
        }
    }
}

fn lease_count(state: &LeaseState) -> usize {
    state.available.len() + state.pinned.len() + state.active.len()
}

fn cancelled_error() -> ServiceError {
    ServiceError::new(ErrorCode::Cancelled, "MCP operation was cancelled")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use connector_contract::{CredentialId, SensitiveInput};

    struct TestSecrets {
        slot: secret::PhysicalSecretSlot,
        redaction: secret::RedactionService,
        keyring_resolves: AtomicUsize,
    }

    impl TestSecrets {
        fn new(credential_id: &CredentialId) -> Self {
            let logical = secret::LogicalCredentialId::new(credential_id.as_str()).unwrap();
            Self {
                slot: secret::PhysicalSecretSlot::allocate(&logical),
                redaction: secret::RedactionService::new(),
                keyring_resolves: AtomicUsize::new(0),
            }
        }
    }

    impl ConnectorSecrets for TestSecrets {
        fn load_stored_oauth_client(
            &self,
            _binding: &crate::HttpAuthBinding,
        ) -> Result<Option<crate::StoredOAuthClient>, ServiceError> {
            Ok(None)
        }

        fn exchange_oauth_refresh(
            &self,
            _request: crate::OAuthRefreshRequest,
            _cancellation: CancellationToken,
        ) -> Result<crate::OAuthRefreshOutcome, ServiceError> {
            Ok(crate::OAuthRefreshOutcome::ReauthorizationRequired)
        }

        fn resolve_credentials(
            &self,
            requests: Vec<CredentialResolutionRequest>,
        ) -> Result<ResolvedCredentials, ServiceError> {
            self.keyring_resolves.fetch_add(1, Ordering::AcqRel);
            let values = requests
                .iter()
                .map(|_| secret::SecretString::new("fixture-secret-value".to_owned()))
                .collect::<Vec<_>>();
            let lease = self
                .redaction
                .acquire_execution_lease(&values.iter().collect::<Vec<_>>())
                .map_err(|_| {
                    ServiceError::new(ErrorCode::SecretUnavailable, "fixture redaction failed")
                })?;
            let entries = requests
                .into_iter()
                .zip(values)
                .map(|(request, value)| {
                    let slot = request.expected_physical_slot.ok_or_else(|| {
                        ServiceError::new(
                            ErrorCode::SecretUnavailable,
                            "fixture physical revision missing",
                        )
                    })?;
                    crate::ResolvedCredential::new(request.credential_id, slot, value)
                })
                .collect::<Result<Vec<_>, _>>()?;
            ResolvedCredentials::new(entries, lease)
        }

        fn stage_oauth_bundle(
            &self,
            _plan: &secret::SecretBundleStagePlan,
            _bundle: secret::SecretBundle,
        ) -> Result<secret::StagedSecretBundle, ServiceError> {
            Err(ServiceError::new(
                ErrorCode::Internal,
                "fixture OAuth staging unsupported",
            ))
        }

        fn delete_oauth_bundle(
            &self,
            _slot: &secret::PhysicalSecretSlot,
        ) -> Result<(), ServiceError> {
            Ok(())
        }

        fn sanitized_input_preview(
            &self,
            _arguments_json: &SensitiveInput,
            _max_chars: usize,
        ) -> Result<String, ServiceError> {
            Ok("{}".to_owned())
        }
    }

    fn stdio_target(
        script: &str,
        credential: Option<(&CredentialId, &secret::PhysicalSecretSlot)>,
    ) -> McpRequestTarget {
        let (secret_env, credential_revisions) = credential.map_or_else(
            || (Vec::new(), Vec::new()),
            |(credential_id, slot)| {
                (
                    vec![("FIXTURE_TOKEN".to_owned(), credential_id.clone())],
                    vec![CredentialResolutionRequest {
                        credential_id: credential_id.clone(),
                        expected_physical_slot: Some(slot.clone()),
                    }],
                )
            },
        );
        McpRequestTarget {
            server: ServerDraft {
                id: Some(ServerId::new("fixture-server")),
                name: "fixture".to_owned(),
                transport: TransportDraft::Stdio {
                    command: "/bin/sh".to_owned(),
                    args: vec!["-c".to_owned(), script.to_owned()],
                    plain_env: Vec::new(),
                    secret_env,
                    inherit_env: true,
                },
                enabled: true,
            },
            config_revision: Revision(7),
            credential_revisions,
        }
    }

    fn manager(redaction: secret::RedactionService) -> mcp::LocalMcpManager {
        mcp::LocalMcpManager::new(redaction).with_request_timeout(Duration::from_secs(3))
    }

    #[test]
    fn warm_discovery_reuses_connection_without_second_keyring_read_and_ttl_reaps() {
        let script = r#"
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"fixture","version":"0"}}}'
read -r _initialized
read -r _list1
printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"%s","inputSchema":{}}]}}\n' "$FIXTURE_TOKEN"
read -r _list2
printf '{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"%s","inputSchema":{}}]}}\n' "$FIXTURE_TOKEN"
sleep 30
"#;
        let credential_id = CredentialId::new("fixture-credential");
        let secrets = Arc::new(TestSecrets::new(&credential_id));
        let adapter = ProductionConnectorMcp::new(
            manager(secrets.redaction.clone()),
            secrets.clone(),
            Duration::from_millis(20),
            2,
        )
        .unwrap();

        for operation in ["warm-1", "warm-2"] {
            let output = adapter
                .discover(
                    &OperationId::new(operation),
                    stdio_target(script, Some((&credential_id, &secrets.slot))),
                    CancellationToken::default(),
                )
                .unwrap();
            assert_eq!(output.tools[0].name, "fixture-secret-value");
        }
        assert_eq!(secrets.keyring_resolves.load(Ordering::Acquire), 1);
        assert_eq!(adapter.active_leases(), 1);
        std::thread::sleep(Duration::from_millis(30));
        adapter.reap_idle_leases();
        assert_eq!(adapter.active_leases(), 0);
        assert_eq!(secrets.redaction.corpus_stats().active_leases, 0);
    }

    #[test]
    fn live_schema_and_authorized_call_use_same_pinned_connection() {
        let script = r#"
session=$$
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"fixture","version":"0"}}}'
read -r _initialized
read -r _list
printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object","xSession":"%s"}}]}}\n' "$session"
read -r _call
printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"%s"}],"isError":false}}\n' "$session"
sleep 30
"#;
        let credential_id = CredentialId::new("unused-credential");
        let secrets = Arc::new(TestSecrets::new(&credential_id));
        let adapter = ProductionConnectorMcp::new(
            manager(secrets.redaction.clone()),
            secrets,
            Duration::from_secs(1),
            2,
        )
        .unwrap();
        let operation_id = OperationId::new("pinned-operation");
        let tool_id = ToolId::new("stored-tool-id");
        let target = stdio_target(script, None);
        let server = target.server.clone();
        let live = adapter
            .load_live_schema(
                &operation_id,
                target,
                tool_id.clone(),
                "echo".to_owned(),
                CancellationToken::default(),
            )
            .unwrap();
        assert_eq!(adapter.active_leases(), 1);
        let marker = live
            .input_schema_json
            .split("\"xSession\":\"")
            .nth(1)
            .and_then(|tail| tail.split('"').next())
            .unwrap()
            .to_owned();

        let schema_hash = audit::schema_hash(&live.input_schema_json);
        let evaluation = audit::evaluate_authorization_with_fingerprint(
            operation_id.as_str().to_owned(),
            "fixture-server".to_owned(),
            "echo".to_owned(),
            audit::PermissionFingerprint::Persisted {
                rule: audit::PermissionRule::Allow,
                approved_schema_hash: Some(schema_hash.clone()),
            },
            schema_hash,
        )
        .unwrap()
        .bind_subject(audit::AuthorizationSubject::global())
        .unwrap();
        let audit::AuthorizationEvaluation::Plan(plan) = evaluation else {
            panic!("fixture authorization should be immediate");
        };
        let ledger = audit::InMemoryAuthorizationLedger::new("mcp-adapter-pinned").unwrap();
        let audit::AuthorizationPreflight::Prepared(grant) = ledger.preflight(plan, b"{}").unwrap()
        else {
            panic!("fixture authorization should prepare");
        };
        let request = AuthorizedInvokeRequest::new(
            grant,
            &audit::AuthorizationSubject::global(),
            &ServerId::new("fixture-server"),
            server,
            "echo".to_owned(),
            SensitiveInput::new(b"{}".to_vec()),
        )
        .unwrap();
        let output = adapter
            .invoke_authorized(request, CancellationToken::default())
            .unwrap();
        assert!(output.contains(&format!("\"text\":\"{marker}\"")));
        assert_eq!(adapter.active_leases(), 1);
    }

    #[test]
    fn cancelling_pinned_schema_releases_process_and_lease() {
        let script = r#"
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"fixture","version":"0"}}}'
read -r _initialized
read -r _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","inputSchema":{}}]}}'
sleep 30
"#;
        let credential_id = CredentialId::new("unused-credential");
        let secrets = Arc::new(TestSecrets::new(&credential_id));
        let adapter = ProductionConnectorMcp::new(
            manager(secrets.redaction.clone()),
            secrets,
            Duration::from_secs(30),
            2,
        )
        .unwrap();
        let operation_id = OperationId::new("cancel-pinned");
        adapter
            .load_live_schema(
                &operation_id,
                stdio_target(script, None),
                ToolId::new("stored-tool-id"),
                "echo".to_owned(),
                CancellationToken::default(),
            )
            .unwrap();
        assert_eq!(adapter.active_leases(), 1);
        adapter.cancel(&operation_id);
        assert_eq!(adapter.active_leases(), 0);
    }

    #[test]
    fn two_pinned_operations_backpressure_third_before_spawn() {
        let script = r#"
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"fixture","version":"0"}}}'
read -r _initialized
read -r _list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","inputSchema":{}}]}}'
sleep 30
"#;
        let credential_id = CredentialId::new("unused-credential");
        let secrets = Arc::new(TestSecrets::new(&credential_id));
        let adapter = ProductionConnectorMcp::new(
            manager(secrets.redaction.clone()),
            secrets,
            Duration::from_secs(30),
            2,
        )
        .unwrap();
        let first = OperationId::new("pinned-first");
        let second = OperationId::new("pinned-second");
        for operation_id in [&first, &second] {
            adapter
                .load_live_schema(
                    operation_id,
                    stdio_target(script, None),
                    ToolId::new("stored-tool-id"),
                    "echo".to_owned(),
                    CancellationToken::default(),
                )
                .unwrap();
        }
        assert_eq!(adapter.active_leases(), 2);

        let error = adapter
            .load_live_schema(
                &OperationId::new("pinned-third"),
                stdio_target(script, None),
                ToolId::new("stored-tool-id"),
                "echo".to_owned(),
                CancellationToken::default(),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Backpressure);
        assert_eq!(adapter.active_leases(), 2);

        adapter.cancel(&first);
        adapter.cancel(&second);
        assert_eq!(adapter.active_leases(), 0);
    }
}
