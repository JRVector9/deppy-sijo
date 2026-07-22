use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use connector_contract::{
    CredentialId, EndpointDisplay, ErrorCode, ImportReportItem, OperationId, PermissionRule,
    Revision, SensitiveInput, ServerDraft, ServerId, ServerSummary, SlackProjection,
    SlackRecoveryKind, ToolId, ToolListItem,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationState {
    /// Exact durable row observed by the repository. `Absent` must not be collapsed into the
    /// effective default Ask policy: preflight uses this fingerprint to reject a concurrent
    /// insert just as it rejects a concurrent update or delete.
    pub permission: audit::PermissionFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceError {
    pub code: ErrorCode,
    pub message: &'static str,
}

impl ServiceError {
    pub const fn new(code: ErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for ServiceError {}

#[derive(Debug, Clone)]
pub struct OverviewData {
    pub config_revision: Revision,
    pub slack: SlackProjection,
    pub servers: Vec<ServerSummary>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct CredentialResolutionRequest {
    pub credential_id: CredentialId,
    pub expected_physical_slot: Option<secret::PhysicalSecretSlot>,
}

impl std::fmt::Debug for CredentialResolutionRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialResolutionRequest")
            .field("credential_id", &self.credential_id)
            .field(
                "expected_physical_slot",
                &self.expected_physical_slot.as_ref().map(|_| "REDACTED"),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct HttpAuthBinding {
    pub credential_id: CredentialId,
    pub physical_slot: secret::PhysicalSecretSlot,
}

impl std::fmt::Debug for HttpAuthBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpAuthBinding")
            .field("credential_id", &self.credential_id)
            .field("physical_slot", &"REDACTED")
            .finish()
    }
}

pub struct ResolvedCredential {
    credential_id: CredentialId,
    physical_slot: secret::PhysicalSecretSlot,
    value: secret::SecretString,
}

impl ResolvedCredential {
    pub fn new(
        credential_id: CredentialId,
        physical_slot: secret::PhysicalSecretSlot,
        value: secret::SecretString,
    ) -> Result<Self, ServiceError> {
        let logical_id =
            secret::LogicalCredentialId::new(credential_id.as_str()).map_err(|_| {
                ServiceError::new(
                    ErrorCode::SecretUnavailable,
                    "credential logical identifier is invalid",
                )
            })?;
        if !physical_slot.belongs_to(&logical_id) {
            return Err(ServiceError::new(
                ErrorCode::SecretUnavailable,
                "credential physical revision does not match its logical identifier",
            ));
        }
        Ok(Self {
            credential_id,
            physical_slot,
            value,
        })
    }

    pub fn credential_id(&self) -> &CredentialId {
        &self.credential_id
    }

    pub fn physical_slot(&self) -> &secret::PhysicalSecretSlot {
        &self.physical_slot
    }

    pub fn value(&self) -> &secret::SecretString {
        &self.value
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        CredentialId,
        secret::PhysicalSecretSlot,
        secret::SecretString,
    ) {
        (self.credential_id, self.physical_slot, self.value)
    }
}

impl std::fmt::Debug for ResolvedCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedCredential")
            .field("credential_id", &self.credential_id)
            .field("physical_slot", &"REDACTED")
            .field("value", &"REDACTED")
            .finish()
    }
}

pub struct ResolvedCredentials {
    entries: Vec<ResolvedCredential>,
    redaction: Option<secret::RedactionLease>,
}

impl ResolvedCredentials {
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
            redaction: None,
        }
    }

    pub fn new(
        entries: Vec<ResolvedCredential>,
        redaction: secret::RedactionLease,
    ) -> Result<Self, ServiceError> {
        if entries.is_empty() {
            return Err(ServiceError::new(
                ErrorCode::SecretUnavailable,
                "redaction lease has no resolved credential",
            ));
        }
        Ok(Self {
            entries,
            redaction: Some(redaction),
        })
    }

    pub fn entries(&self) -> &[ResolvedCredential] {
        &self.entries
    }

    pub(crate) fn into_parts(self) -> (Vec<ResolvedCredential>, Option<secret::RedactionLease>) {
        (self.entries, self.redaction)
    }
}

impl std::fmt::Debug for ResolvedCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedCredentials")
            .field("entries", &self.entries.len())
            .field("redaction", &self.redaction.as_ref().map(|_| "REDACTED"))
            .finish()
    }
}

pub struct McpRequestTarget {
    pub server: ServerDraft,
    pub config_revision: Revision,
    pub credential_revisions: Vec<CredentialResolutionRequest>,
}

impl std::fmt::Debug for McpRequestTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpRequestTarget")
            .field("server", &"REDACTED")
            .field("config_revision", &self.config_revision)
            .field("credential_count", &self.credential_revisions.len())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct RepositoryToolPage {
    pub total: usize,
    pub items: Vec<ToolListItem>,
}

#[derive(Debug)]
pub struct ImportPlan {
    pub servers: Vec<ServerDraft>,
    /// Sanitized parser outcomes for skipped/unsupported entries. Successfully committed
    /// candidates are appended by the coordinator as `Added` outcomes.
    pub report: Vec<ImportReportItem>,
}

#[derive(Debug, Clone)]
pub struct DiscoveredTool {
    pub id: ToolId,
    pub name: String,
    pub description: Option<String>,
    pub descriptor_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct DiscoverOutput {
    pub tools: Vec<DiscoveredTool>,
}

pub struct AuthorizedInvokeRequest {
    call: audit::AuthorizedCall,
    server: ServerDraft,
    tool_name: String,
    arguments_json: SensitiveInput,
}

impl std::fmt::Debug for AuthorizedInvokeRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthorizedInvokeRequest")
            .field("operation_id", &self.call.operation_id())
            .field("server", &"REDACTED")
            .field("tool_name", &"REDACTED")
            .field("arguments_json", &"REDACTED")
            .finish()
    }
}

impl AuthorizedInvokeRequest {
    pub(crate) fn new(
        grant: audit::AuthorizationGrant,
        server_id: &ServerId,
        server: ServerDraft,
        tool_name: String,
        arguments_json: SensitiveInput,
    ) -> Result<Self, ServiceError> {
        if server.id.as_ref().map(ServerId::as_str) != Some(server_id.as_str()) {
            return Err(ServiceError::new(
                ErrorCode::StorageUnavailable,
                "authorized server configuration binding mismatch",
            ));
        }
        let call = grant
            .bind_call(
                server_id.as_str(),
                &tool_name,
                arguments_json.expose_bytes(),
            )
            .map_err(|_| {
                ServiceError::new(
                    ErrorCode::PermissionDenied,
                    "authorized call binding mismatch",
                )
            })?;
        Ok(Self {
            call,
            server,
            tool_name,
            arguments_json,
        })
    }

    pub fn operation_id(&self) -> &str {
        self.call.operation_id()
    }

    pub fn server(&self) -> &ServerDraft {
        &self.server
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn arguments_json(&self) -> &[u8] {
        self.arguments_json.expose_bytes()
    }

    pub fn call_capability(&self) -> &audit::AuthorizedCall {
        &self.call
    }
}

#[derive(Debug, Clone)]
pub struct LiveToolSchema {
    pub tool_id: ToolId,
    pub tool_name: String,
    pub input_schema_json: String,
}

#[derive(Debug)]
pub struct StoredOAuthClient {
    pub server_id: ServerId,
    pub client_id: String,
    pub client_secret: SensitiveInput,
    pub workspace_hint: Option<String>,
}

/// Non-Clone OAuth publish payload. Secrets return to the coordinator worker and never cross the
/// UI contract or enter a snapshot. Metadata is deliberately hidden from Debug because it may
/// contain raw provider URLs even though it must not contain tokens.
pub struct OAuthCredentialUpdate {
    pub logical_id: secret::LogicalCredentialId,
    pub bundle: secret::SecretBundle,
    pub oauth_metadata_json: String,
    pub masked_hint: Option<String>,
}

impl std::fmt::Debug for OAuthCredentialUpdate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthCredentialUpdate")
            .field("logical_id", &self.logical_id)
            .field("bundle", &"REDACTED")
            .field("oauth_metadata_json", &"REDACTED")
            .field("masked_hint", &self.masked_hint)
            .finish()
    }
}

/// Adapter-owned, non-serializable OAuth continuation. It may contain provider metadata but must
/// never contain an authorization code or token. Debug output is inherited from `SensitiveInput`
/// and is therefore always redacted.
#[derive(Debug)]
pub struct OAuthContinuation {
    state: SensitiveInput,
}

impl OAuthContinuation {
    pub fn new(state: SensitiveInput) -> Self {
        Self { state }
    }

    pub fn expose_bytes(&self) -> &[u8] {
        self.state.expose_bytes()
    }
}

#[derive(Debug)]
pub struct OAuthDiscovery {
    pub continuation: OAuthContinuation,
    pub authority: EndpointDisplay,
    pub resource: EndpointDisplay,
    pub scopes: Vec<String>,
}

#[derive(Debug)]
pub struct OAuthClientRequest {
    pub continuation: OAuthContinuation,
    pub reason: ErrorCode,
    pub workspace_hint: Option<String>,
}

#[derive(Debug)]
pub struct OAuthCompletion {
    pub credential: OAuthCredentialUpdate,
    pub workspace_label: Option<String>,
    pub can_choose_workspace: bool,
}

#[derive(Debug)]
pub struct OAuthRecoveryTarget {
    pub kind: SlackRecoveryKind,
    /// Dynamic provider URLs are non-Clone/non-Serialize/redacted. Static recovery actions use
    /// `None` and are mapped by the app composition root.
    pub url: Option<SensitiveInput>,
}

#[derive(Debug)]
pub struct OAuthFailure {
    pub error_code: ErrorCode,
    pub recovery: Vec<OAuthRecoveryTarget>,
}

#[derive(Debug)]
pub enum OAuthAuthorizeOutput {
    Completed(OAuthCompletion),
    ClientInputRequired(OAuthClientRequest),
    Failed(OAuthFailure),
}

/// Single-use bridge from an OAuth worker to the coordinator. Implementations must call this only
/// after the callback listener is bound. The call does not return success until the bounded host
/// action has been accepted, so queue-full/stale paths fail closed and release the listener.
pub trait OAuthEventSink: Send + Sync + 'static {
    fn callback_bound(&self, authorization_url: SensitiveInput) -> Result<(), ServiceError>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpTransportSnapshot {
    pub active_threads: usize,
    pub peak_threads: usize,
    pub active_http_send_permits: usize,
    pub peak_http_send_permits: usize,
    pub pending_http_senders: usize,
}

impl From<mcp::McpTransportMetrics> for McpTransportSnapshot {
    fn from(metrics: mcp::McpTransportMetrics) -> Self {
        Self {
            active_threads: metrics.active_threads,
            peak_threads: metrics.peak_threads,
            active_http_send_permits: metrics.active_http_send_permits,
            peak_http_send_permits: metrics.peak_http_send_permits,
            pending_http_senders: metrics.reaper_pending_http_senders,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Created by the app from paths/config only. `open` is called on the lazy worker,
/// allowing the app adapter to open exactly one dedicated SQLite connection per
/// active coordinator lifetime without exposing `Db` here.
pub trait ConnectorRepositoryFactory: Send + Sync + 'static {
    fn open(&self) -> Result<Box<dyn ConnectorRepository>, ServiceError>;
}

/// Storage-neutral repository DTO boundary. Implementations must use atomic repository
/// transactions for mutating methods and return the committed configuration revision.
pub trait ConnectorRepository: Send + 'static {
    fn load_overview(&mut self) -> Result<OverviewData, ServiceError>;
    fn load_server(&mut self, server_id: &ServerId) -> Result<ServerDraft, ServiceError>;
    fn load_tool_page(
        &mut self,
        server_id: &ServerId,
        offset: usize,
        limit: usize,
    ) -> Result<RepositoryToolPage, ServiceError>;
    fn load_tool_name(
        &mut self,
        server_id: &ServerId,
        tool_id: &ToolId,
    ) -> Result<String, ServiceError>;
    fn save_server(&mut self, draft: ServerDraft) -> Result<Revision, ServiceError>;
    fn delete_server(&mut self, server_id: &ServerId) -> Result<Revision, ServiceError>;
    fn replace_tools(
        &mut self,
        server_id: &ServerId,
        tools: &[DiscoveredTool],
    ) -> Result<Revision, ServiceError>;
    fn set_permission(
        &mut self,
        server_id: &ServerId,
        tool_id: &ToolId,
        rule: PermissionRule,
    ) -> Result<Revision, ServiceError>;
    fn ensure_slack_server(&mut self) -> Result<Revision, ServiceError>;
    fn parse_import(&mut self, source_name: &str, bytes: &[u8])
    -> Result<ImportPlan, ServiceError>;
    fn import_servers(&mut self, servers: Vec<ServerDraft>) -> Result<Revision, ServiceError>;

    /// Returns at most one exact OAuth binding for this server and its currently loaded URL.
    /// Implementations must reject ambiguous, corrupt, cross-owned, unversioned, or URL-mismatched
    /// metadata rather than selecting a row heuristically.
    fn load_http_auth_binding(
        &mut self,
        server_id: &ServerId,
        exact_url: &str,
    ) -> Result<Option<HttpAuthBinding>, ServiceError>;

    fn load_oauth_secret_slot(
        &mut self,
        logical_id: &secret::LogicalCredentialId,
    ) -> Result<Option<secret::PhysicalSecretSlot>, ServiceError>;

    /// Compare-and-swap publishes the already-staged physical slot plus nonsecret metadata on this
    /// repository's sole coordinator-owned SQLite connection.
    fn publish_oauth_secret_slot(
        &mut self,
        staged: &secret::StagedSecretBundle,
        expected_previous: Option<&secret::PhysicalSecretSlot>,
        oauth_metadata_json: &str,
        masked_hint: Option<&str>,
    ) -> Result<bool, ServiceError>;

    fn load_authorization_state(
        &mut self,
        server_id: &ServerId,
        tool_name: &str,
    ) -> Result<AuthorizationState, ServiceError>;

    /// Atomically persists an optional remembered decision and durable audit preflight. The plan
    /// is consumed and only the storage-produced opaque grant can unlock an external tool call.
    fn commit_authorization_preflight(
        &mut self,
        plan: audit::AuthorizationPlan,
        arguments_json: &SensitiveInput,
    ) -> Result<audit::AuthorizationPreflight, ServiceError>;

    fn complete_authorization(
        &mut self,
        operation_id: &OperationId,
        outcome: audit::AuthorizationOutcome,
    ) -> Result<(), ServiceError>;

    /// Gracefully closes the repository authorization-owner lifetime exactly once. Concrete
    /// adapters use this hook to reconcile this run's still-Prepared rows before releasing the
    /// exclusive owner lock. Drop remains the crash fallback.
    fn shutdown(&mut self) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// Keyring-neutral secret capability. Secret values cross the boundary only as
/// non-Clone/non-Serialize/redacted `SensitiveInput` and never enter snapshots.
pub trait ConnectorSecrets: Send + Sync + 'static {
    /// In-memory pointer-index lookup only. Implementations must not touch keyring/storage here.
    /// Every returned request carries a typed physical revision and preserves input order.
    fn credential_revisions(
        &self,
        credential_ids: Vec<CredentialId>,
    ) -> Result<Vec<CredentialResolutionRequest>, ServiceError>;

    /// Resolves all requested values and acquires one corpus lease atomically. A partial result or
    /// a redaction-capacity failure is an error and external execution must not start.
    fn resolve_credentials(
        &self,
        requests: Vec<CredentialResolutionRequest>,
    ) -> Result<ResolvedCredentials, ServiceError>;
    fn stage_oauth_bundle(
        &self,
        logical_id: secret::LogicalCredentialId,
        previous_slot: Option<secret::PhysicalSecretSlot>,
        bundle: secret::SecretBundle,
    ) -> Result<secret::StagedSecretBundle, ServiceError>;

    fn delete_oauth_bundle(&self, slot: &secret::PhysicalSecretSlot) -> Result<(), ServiceError>;

    /// Produces a registered-pattern and sensitive-key redacted preview. Implementations must
    /// validate JSON and return at most `max_chars` Unicode scalar values.
    fn sanitized_input_preview(
        &self,
        arguments_json: &SensitiveInput,
        max_chars: usize,
    ) -> Result<String, ServiceError>;
}

pub trait ConnectorMcp: Send + Sync + 'static {
    fn discover(
        &self,
        operation_id: &OperationId,
        target: McpRequestTarget,
        cancellation: CancellationToken,
    ) -> Result<DiscoverOutput, ServiceError>;

    fn load_live_schema(
        &self,
        operation_id: &OperationId,
        target: McpRequestTarget,
        tool_id: ToolId,
        tool_name: String,
        cancellation: CancellationToken,
    ) -> Result<LiveToolSchema, ServiceError>;

    /// The opaque preflight grant is consumed with the call, making a prepared authorization
    /// single-use. Implementations must not retry `mcp::McpDeliveryUnknown` outcomes.
    fn invoke_authorized(
        &self,
        request: AuthorizedInvokeRequest,
        cancellation: CancellationToken,
    ) -> Result<String, ServiceError>;

    fn cancel(&self, operation_id: &OperationId);

    fn active_leases(&self) -> usize {
        0
    }

    fn reap_idle_leases(&self) {}

    fn transport_metrics(&self) -> McpTransportSnapshot {
        mcp::transport_metrics().into()
    }
}

pub trait ConnectorOAuth: Send + Sync + 'static {
    fn discover(
        &self,
        operation_id: &OperationId,
        server: ServerDraft,
        choose_workspace: bool,
        cancellation: CancellationToken,
    ) -> Result<OAuthDiscovery, ServiceError>;

    #[allow(clippy::too_many_arguments)]
    fn authorize(
        &self,
        operation_id: &OperationId,
        continuation: OAuthContinuation,
        client: Option<StoredOAuthClient>,
        workspace: Option<String>,
        events: Arc<dyn OAuthEventSink>,
        cancellation: CancellationToken,
    ) -> Result<OAuthAuthorizeOutput, ServiceError>;

    fn cancel(&self, operation_id: &OperationId);
}

/// Adapter helper for `ConnectorMcp::cancel`: implementations that own a live MC01
/// connection call this to guarantee process-group kill/reap for stdio.
pub fn cancel_live_mcp_connection(connection: &mut mcp::McpConnection) {
    connection.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_resolution_debug_redacts_physical_slot() {
        let logical = secret::LogicalCredentialId::new("debug-credential")
            .expect("fixture logical credential");
        let slot = secret::PhysicalSecretSlot::allocate(&logical);
        let raw_slot = slot.as_str().to_owned();
        let request = CredentialResolutionRequest {
            credential_id: CredentialId::new("debug-credential"),
            expected_physical_slot: Some(slot),
        };

        let debug = format!("{request:?}");
        assert!(!debug.contains(&raw_slot));
        assert!(debug.contains("REDACTED"));
    }
}
