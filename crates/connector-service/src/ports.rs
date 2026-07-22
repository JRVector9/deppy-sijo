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

#[derive(Clone, PartialEq, Eq)]
pub struct Observed<T> {
    pub revision: Revision,
    pub value: T,
}

impl<T> std::fmt::Debug for Observed<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Observed")
            .field("revision", &self.revision)
            .field("value", &"REDACTED")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum RepositoryCas<T> {
    Committed { revision: Revision, value: T },
    Stale { current_revision: Revision },
}

impl<T> std::fmt::Debug for RepositoryCas<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Committed { revision, .. } => formatter
                .debug_struct("RepositoryCas::Committed")
                .field("revision", revision)
                .field("value", &"REDACTED")
                .finish(),
            Self::Stale { current_revision } => formatter
                .debug_struct("RepositoryCas::Stale")
                .field("current_revision", current_revision)
                .finish(),
        }
    }
}

impl<T> RepositoryCas<T> {
    pub fn committed_revision(&self) -> Revision {
        match self {
            Self::Committed { revision, .. } => *revision,
            Self::Stale { current_revision } => *current_revision,
        }
    }
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

/// App-supplied execution scope for one Connector tool invocation.
///
/// The audit subject remains private to this service boundary: UI contracts cannot inspect or
/// retain raw workspace identifiers, and callers can only construct a validated, bounded scope.
#[derive(Clone, PartialEq, Eq)]
pub struct InvocationContext {
    subject: audit::AuthorizationSubject,
}

impl InvocationContext {
    pub fn global() -> Self {
        Self {
            subject: audit::AuthorizationSubject::global(),
        }
    }

    pub fn for_workspace(workspace_id: String) -> Result<Self, ServiceError> {
        let subject =
            audit::AuthorizationSubject::try_new(Some(workspace_id), None).map_err(|_| {
                ServiceError::new(
                    ErrorCode::InvalidInput,
                    "connector invocation workspace is invalid",
                )
            })?;
        Ok(Self { subject })
    }

    pub fn is_global(&self) -> bool {
        self.subject.is_global()
    }

    pub(crate) fn into_subject(self) -> audit::AuthorizationSubject {
        self.subject
    }
}

impl std::fmt::Debug for InvocationContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InvocationContext")
            .field("is_global", &self.is_global())
            .finish()
    }
}

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
            .field("binding", &"REDACTED")
            .field("has_physical_slot", &self.expected_physical_slot.is_some())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct HttpAuthBinding {
    pub credential_id: CredentialId,
    pub physical_slot: secret::PhysicalSecretSlot,
    pub oauth_metadata: Option<auth::StoredOAuthMetadata>,
}

impl std::fmt::Debug for HttpAuthBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpAuthBinding")
            .field("binding", &"REDACTED")
            .field("has_oauth_metadata", &self.oauth_metadata.is_some())
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
            .field("binding", &"REDACTED")
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

pub struct RepositoryMcpTarget {
    pub server: ServerDraft,
    pub credential_revisions: Vec<CredentialResolutionRequest>,
    pub http_auth: Option<HttpAuthBinding>,
}

impl std::fmt::Debug for RepositoryMcpTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RepositoryMcpTarget")
            .field("server", &"REDACTED")
            .field("credential_count", &self.credential_revisions.len())
            .field("has_http_auth", &self.http_auth.is_some())
            .finish()
    }
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
            .field("server", &"REDACTED")
            .field("tool_name", &"REDACTED")
            .field("arguments_json", &"REDACTED")
            .finish()
    }
}

impl AuthorizedInvokeRequest {
    pub(crate) fn new(
        grant: audit::AuthorizationGrant,
        expected_subject: &audit::AuthorizationSubject,
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
            .bind_call_for_subject(
                expected_subject,
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

pub struct StoredOAuthClient {
    pub server_id: ServerId,
    pub logical_id: secret::LogicalCredentialId,
    pub client_id: String,
    pub client_secret: Option<SensitiveInput>,
    pub workspace_hint: Option<String>,
    pub metadata: Option<auth::StoredOAuthMetadata>,
    /// `true` only for user-supplied client credentials. DCR and loaded DCR clients are false.
    pub manual_client: bool,
}

impl std::fmt::Debug for StoredOAuthClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredOAuthClient")
            .field("binding", &"REDACTED")
            .field("has_client_secret", &self.client_secret.is_some())
            .field("has_workspace", &self.workspace_hint.is_some())
            .field("has_metadata", &self.metadata.is_some())
            .field("manual_client", &self.manual_client)
            .finish()
    }
}

/// Non-Clone OAuth publish payload. Secrets return to the coordinator worker and never cross the
/// UI contract or enter a snapshot. Metadata is deliberately hidden from Debug because it may
/// contain raw provider URLs even though it must not contain tokens.
pub struct OAuthCredentialUpdate {
    pub logical_id: secret::LogicalCredentialId,
    pub label: String,
    pub bundle: secret::SecretBundle,
    pub metadata: auth::StoredOAuthMetadata,
    pub masked_hint: Option<String>,
}

pub struct OAuthRefreshRequest {
    pub logical_id: secret::LogicalCredentialId,
    pub current_slot: secret::PhysicalSecretSlot,
    pub metadata: auth::StoredOAuthMetadata,
    pub label: String,
}

impl std::fmt::Debug for OAuthRefreshRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthRefreshRequest")
            .field("binding", &"REDACTED")
            .finish()
    }
}

pub enum OAuthRefreshOutcome {
    Refreshed(Box<OAuthCredentialUpdate>),
    ReauthorizationRequired,
}

impl std::fmt::Debug for OAuthRefreshOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refreshed(_) => formatter.write_str("OAuthRefreshOutcome::Refreshed(REDACTED)"),
            Self::ReauthorizationRequired => {
                formatter.write_str("OAuthRefreshOutcome::ReauthorizationRequired")
            }
        }
    }
}

impl std::fmt::Debug for OAuthCredentialUpdate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthCredentialUpdate")
            .field("credential", &"REDACTED")
            .field("has_masked_hint", &self.masked_hint.is_some())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum OAuthPublishMode {
    FirstInsert,
    Rotation {
        expected_previous: secret::PhysicalSecretSlot,
    },
}

impl std::fmt::Debug for OAuthPublishMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FirstInsert => formatter.write_str("OAuthPublishMode::FirstInsert"),
            Self::Rotation { .. } => formatter.write_str("OAuthPublishMode::Rotation(REDACTED)"),
        }
    }
}

pub struct OAuthPublishDescriptor<'a> {
    pub staged: &'a secret::StagedSecretBundle,
    pub mode: OAuthPublishMode,
    pub metadata: &'a auth::StoredOAuthMetadata,
    pub label: &'a str,
    pub masked_hint: Option<&'a str>,
}

impl std::fmt::Debug for OAuthPublishDescriptor<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthPublishDescriptor")
            .field("staged", &"REDACTED")
            .field("mode", &self.mode)
            .field("metadata", &"REDACTED")
            .field("has_masked_hint", &self.masked_hint.is_some())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum OAuthPublishResult {
    Committed {
        revision: Revision,
        previous_slot: Option<secret::PhysicalSecretSlot>,
    },
    RevisionStale {
        current_revision: Revision,
    },
    PointerStale {
        revision: Revision,
    },
}

impl std::fmt::Debug for OAuthPublishResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Committed {
                revision,
                previous_slot,
            } => formatter
                .debug_struct("OAuthPublishResult::Committed")
                .field("revision", revision)
                .field("had_previous_slot", &previous_slot.is_some())
                .finish(),
            Self::RevisionStale { current_revision } => formatter
                .debug_struct("OAuthPublishResult::RevisionStale")
                .field("current_revision", current_revision)
                .finish(),
            Self::PointerStale { revision } => formatter
                .debug_struct("OAuthPublishResult::PointerStale")
                .field("revision", revision)
                .finish(),
        }
    }
}

/// Adapter-owned, non-serializable OAuth continuation. It may contain provider metadata but must
/// never contain an authorization code or token. Debug output is inherited from `SensitiveInput`
/// and is therefore always redacted.
pub struct OAuthContinuation {
    state: OAuthContinuationState,
    retained_bytes: usize,
}

enum OAuthContinuationState {
    Opaque(SensitiveInput),
    Typed(Box<dyn std::any::Any + Send>),
}

impl OAuthContinuation {
    pub fn new(state: SensitiveInput) -> Self {
        let retained_bytes = state.len();
        Self {
            state: OAuthContinuationState::Opaque(state),
            retained_bytes,
        }
    }

    pub(crate) fn typed<T: Send + 'static>(state: T, retained_bytes: usize) -> Self {
        Self {
            state: OAuthContinuationState::Typed(Box::new(state)),
            retained_bytes,
        }
    }

    pub(crate) fn into_typed<T: Send + 'static>(self) -> Result<T, ServiceError> {
        match self.state {
            OAuthContinuationState::Typed(state) => {
                state.downcast::<T>().map(|state| *state).map_err(|_| {
                    ServiceError::new(ErrorCode::Internal, "OAuth continuation type mismatch")
                })
            }
            OAuthContinuationState::Opaque(_) => Err(ServiceError::new(
                ErrorCode::Internal,
                "OAuth continuation type mismatch",
            )),
        }
    }

    pub fn retained_bytes(&self) -> usize {
        if let OAuthContinuationState::Opaque(state) = &self.state {
            debug_assert_eq!(state.len(), self.retained_bytes);
        }
        self.retained_bytes
    }
}

impl std::fmt::Debug for OAuthContinuation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OAuthContinuation(REDACTED)")
    }
}

pub struct OAuthDiscovery {
    pub continuation: OAuthContinuation,
    pub authority: EndpointDisplay,
    pub resource: EndpointDisplay,
    pub scopes: Vec<String>,
}

impl std::fmt::Debug for OAuthDiscovery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthDiscovery")
            .field("continuation", &"REDACTED")
            .field("endpoints", &"REDACTED")
            .field("scope_count", &self.scopes.len())
            .finish()
    }
}

pub struct OAuthClientRequest {
    pub continuation: OAuthContinuation,
    pub reason: ErrorCode,
    pub workspace_hint: Option<String>,
}

impl std::fmt::Debug for OAuthClientRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthClientRequest")
            .field("continuation", &"REDACTED")
            .field("reason", &self.reason)
            .field("has_workspace_hint", &self.workspace_hint.is_some())
            .finish()
    }
}

pub struct OAuthCompletion {
    pub credential: OAuthCredentialUpdate,
    pub workspace_label: Option<String>,
    pub can_choose_workspace: bool,
}

impl std::fmt::Debug for OAuthCompletion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthCompletion")
            .field("credential", &"REDACTED")
            .field("has_workspace_label", &self.workspace_label.is_some())
            .field("can_choose_workspace", &self.can_choose_workspace)
            .finish()
    }
}

pub struct OAuthRecoveryTarget {
    pub kind: SlackRecoveryKind,
    /// Dynamic provider URLs are non-Clone/non-Serialize/redacted. Static recovery actions use
    /// `None` and are mapped by the app composition root.
    pub url: Option<SensitiveInput>,
}

impl std::fmt::Debug for OAuthRecoveryTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthRecoveryTarget")
            .field("kind", &self.kind)
            .field("has_url", &self.url.is_some())
            .finish()
    }
}

pub struct OAuthFailure {
    pub error_code: ErrorCode,
    pub recovery: Vec<OAuthRecoveryTarget>,
}

impl std::fmt::Debug for OAuthFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthFailure")
            .field("error_code", &self.error_code)
            .field("recovery_count", &self.recovery.len())
            .finish()
    }
}

pub enum OAuthAuthorizeOutput {
    Completed(Box<OAuthCompletion>),
    ClientInputRequired(OAuthClientRequest),
    Failed(OAuthFailure),
}

impl std::fmt::Debug for OAuthAuthorizeOutput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed(completion) => formatter
                .debug_tuple("OAuthAuthorizeOutput::Completed")
                .field(completion)
                .finish(),
            Self::ClientInputRequired(request) => formatter
                .debug_tuple("OAuthAuthorizeOutput::ClientInputRequired")
                .field(request)
                .finish(),
            Self::Failed(failure) => formatter
                .debug_tuple("OAuthAuthorizeOutput::Failed")
                .field(failure)
                .finish(),
        }
    }
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
    fn load_server(&mut self, server_id: &ServerId) -> Result<Observed<ServerDraft>, ServiceError>;
    fn load_mcp_target(
        &mut self,
        server_id: &ServerId,
    ) -> Result<Observed<RepositoryMcpTarget>, ServiceError>;
    fn load_tool_page(
        &mut self,
        server_id: &ServerId,
        offset: usize,
        limit: usize,
    ) -> Result<Observed<RepositoryToolPage>, ServiceError>;
    fn load_tool_name(
        &mut self,
        server_id: &ServerId,
        tool_id: &ToolId,
    ) -> Result<Observed<String>, ServiceError>;
    fn save_server(
        &mut self,
        expected_revision: Revision,
        draft: ServerDraft,
    ) -> Result<RepositoryCas<()>, ServiceError>;
    fn delete_server(
        &mut self,
        expected_revision: Revision,
        server_id: &ServerId,
    ) -> Result<RepositoryCas<()>, ServiceError>;
    fn replace_tools(
        &mut self,
        expected_revision: Revision,
        server_id: &ServerId,
        tools: &[DiscoveredTool],
    ) -> Result<RepositoryCas<()>, ServiceError>;
    fn set_permission(
        &mut self,
        expected_revision: Revision,
        server_id: &ServerId,
        tool_id: &ToolId,
        rule: PermissionRule,
    ) -> Result<RepositoryCas<()>, ServiceError>;
    fn ensure_slack_server(
        &mut self,
        expected_revision: Revision,
    ) -> Result<RepositoryCas<()>, ServiceError>;
    fn parse_import(&mut self, source_name: &str, bytes: &[u8])
    -> Result<ImportPlan, ServiceError>;
    fn import_servers(
        &mut self,
        expected_revision: Revision,
        servers: Vec<ServerDraft>,
    ) -> Result<RepositoryCas<()>, ServiceError>;

    /// Returns at most one exact OAuth binding for this server and its currently loaded URL.
    /// Implementations must reject ambiguous, corrupt, cross-owned, unversioned, or URL-mismatched
    /// metadata rather than selecting a row heuristically.
    fn load_http_auth_binding(
        &mut self,
        server_id: &ServerId,
        exact_url: &str,
    ) -> Result<Observed<Option<HttpAuthBinding>>, ServiceError>;

    fn load_oauth_secret_slot(
        &mut self,
        logical_id: &secret::LogicalCredentialId,
    ) -> Result<Observed<Option<secret::PhysicalSecretSlot>>, ServiceError>;

    /// Records the exact new physical slot on the coordinator-owned database connection before
    /// any keyring write. This ledger-only operation must not advance the Connector revision.
    fn register_oauth_secret_staging(
        &mut self,
        plan: &secret::SecretBundleStagePlan,
    ) -> Result<(), ServiceError>;

    /// Removes an exact staging/orphan ledger row only after the corresponding keyring bundle is
    /// known absent. Published slots must be rejected by concrete adapters.
    fn acknowledge_oauth_secret_deleted(
        &mut self,
        logical_id: &secret::LogicalCredentialId,
        slot: &secret::PhysicalSecretSlot,
    ) -> Result<(), ServiceError>;

    /// Compare-and-swap publishes the already-staged physical slot plus nonsecret metadata on this
    /// repository's sole coordinator-owned SQLite connection.
    fn publish_oauth_secret_slot(
        &mut self,
        expected_revision: Revision,
        descriptor: OAuthPublishDescriptor<'_>,
    ) -> Result<OAuthPublishResult, ServiceError>;

    fn load_authorization_state(
        &mut self,
        server_id: &ServerId,
        tool_name: &str,
    ) -> Result<Observed<AuthorizationState>, ServiceError>;

    /// Atomically persists an optional remembered decision and durable audit preflight. The plan
    /// is consumed and only the storage-produced opaque grant can unlock an external tool call.
    fn commit_authorization_preflight(
        &mut self,
        expected_revision: Revision,
        plan: audit::AuthorizationPlan,
        arguments_json: &SensitiveInput,
    ) -> Result<RepositoryCas<audit::AuthorizationPreflight>, ServiceError>;

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
    /// Resolves an already-bound OAuth client only after a user initiated OAuth work. The adapter
    /// must read the exact physical slot in `binding`; it must not query storage or guess aliases.
    fn load_stored_oauth_client(
        &self,
        binding: &HttpAuthBinding,
    ) -> Result<Option<StoredOAuthClient>, ServiceError>;

    /// Reads only the exact current physical slot, performs one bounded refresh exchange, and
    /// returns an unpersisted replacement bundle. It must not write keyring/storage: the
    /// coordinator registers the new ledger slot before staging and publishes it by revision CAS.
    fn exchange_oauth_refresh(
        &self,
        request: OAuthRefreshRequest,
        cancellation: CancellationToken,
    ) -> Result<OAuthRefreshOutcome, ServiceError>;

    /// Resolves all requested values and acquires one corpus lease atomically. A partial result or
    /// a redaction-capacity failure is an error and external execution must not start.
    fn resolve_credentials(
        &self,
        requests: Vec<CredentialResolutionRequest>,
    ) -> Result<ResolvedCredentials, ServiceError>;
    fn stage_oauth_bundle(
        &self,
        plan: &secret::SecretBundleStagePlan,
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
        stored_client: Option<StoredOAuthClient>,
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

    fn prepared_grant(
        operation_id: &str,
        subject: audit::AuthorizationSubject,
    ) -> audit::AuthorizationGrant {
        let schema_hash = audit::schema_hash(r#"{"type":"object"}"#);
        let evaluation = audit::evaluate_authorization_with_fingerprint(
            operation_id.to_owned(),
            "fixture-server".to_owned(),
            "fixture-tool".to_owned(),
            audit::PermissionFingerprint::Persisted {
                rule: audit::PermissionRule::Allow,
                approved_schema_hash: Some(schema_hash.clone()),
            },
            schema_hash,
        )
        .expect("fixture authorization");
        let audit::AuthorizationEvaluation::Plan(plan) = evaluation
            .bind_subject(subject)
            .expect("fixture subject binding")
        else {
            panic!("fixture authorization should be immediate")
        };
        let ledger = audit::InMemoryAuthorizationLedger::new(operation_id)
            .expect("fixture authorization ledger");
        let audit::AuthorizationPreflight::Prepared(grant) =
            ledger.preflight(plan, b"{}").expect("fixture preflight")
        else {
            panic!("fixture authorization should prepare")
        };
        grant
    }

    fn fixture_server() -> ServerDraft {
        ServerDraft {
            id: Some(ServerId::new("fixture-server")),
            name: "fixture-server".to_owned(),
            transport: connector_contract::TransportDraft::Stdio {
                command: "fixture-mcp".to_owned(),
                args: Vec::new(),
                plain_env: Vec::new(),
                secret_env: Vec::new(),
                inherit_env: false,
            },
            enabled: true,
        }
    }

    #[test]
    fn invocation_context_is_bounded_and_debug_never_exposes_workspace() {
        let marker = "workspace-debug-marker";
        let context = InvocationContext::for_workspace(marker.to_owned()).unwrap();
        let debug = format!("{context:?}");

        assert!(!context.is_global());
        assert!(!debug.contains(marker), "Debug leaked workspace: {debug}");
        assert!(InvocationContext::for_workspace(String::new()).is_err());
        assert!(InvocationContext::for_workspace(" padded".to_owned()).is_err());
        assert!(InvocationContext::for_workspace("nul\0workspace".to_owned()).is_err());
        assert!(InvocationContext::for_workspace("w".repeat(129)).is_err());
        assert!(InvocationContext::for_workspace("w".repeat(128)).is_ok());
    }

    #[test]
    fn workspace_grant_cannot_cross_subject_and_request_debug_hides_operation() {
        let subject_a =
            audit::AuthorizationSubject::try_new(Some("workspace-a".to_owned()), None).unwrap();
        let subject_b =
            audit::AuthorizationSubject::try_new(Some("workspace-b".to_owned()), None).unwrap();
        let mismatch = AuthorizedInvokeRequest::new(
            prepared_grant("cross-workspace-operation", subject_a.clone()),
            &subject_b,
            &ServerId::new("fixture-server"),
            fixture_server(),
            "fixture-tool".to_owned(),
            SensitiveInput::new(b"{}".to_vec()),
        )
        .expect_err("workspace B must not consume workspace A grant");
        assert_eq!(mismatch.code, ErrorCode::PermissionDenied);

        let operation_marker = "request-debug-operation-marker";
        let request = AuthorizedInvokeRequest::new(
            prepared_grant(operation_marker, subject_a.clone()),
            &subject_a,
            &ServerId::new("fixture-server"),
            fixture_server(),
            "fixture-tool".to_owned(),
            SensitiveInput::new(b"{}".to_vec()),
        )
        .unwrap();
        let debug = format!("{request:?}");
        assert!(
            !debug.contains(operation_marker),
            "Debug leaked operation: {debug}"
        );
        assert!(debug.contains("REDACTED"));
    }

    fn debug_oauth_metadata() -> auth::StoredOAuthMetadata {
        auth::StoredOAuthMetadata::new(
            auth::StoredOAuthMetadataDraft {
                server_id: "debug-server-marker".to_owned(),
                server_url: "https://debug-provider-marker.invalid/mcp".to_owned(),
                issuer: "https://debug-provider-marker.invalid/".to_owned(),
                authorization_endpoint: "https://debug-provider-marker.invalid/authorize"
                    .to_owned(),
                token_endpoint: "https://debug-provider-marker.invalid/token".to_owned(),
                oauth_resource: "https://debug-provider-marker.invalid/mcp".to_owned(),
                client_id: "debug-client-marker".to_owned(),
                token_endpoint_auth_method: auth::TokenEndpointAuthMethod::None,
                manual_client: false,
                provider_workspace_id: None,
                workspace_domain: None,
                scopes: vec!["debug-scope-marker".to_owned()],
                expires_at_secs: None,
            },
            auth::StoredOAuthMetadataLimits::PRODUCTION,
        )
        .expect("fixture metadata")
    }

    fn debug_oauth_update() -> OAuthCredentialUpdate {
        OAuthCredentialUpdate {
            logical_id: secret::LogicalCredentialId::new("debug-logical-marker")
                .expect("fixture logical credential"),
            label: "debug-label-marker".to_owned(),
            bundle: secret::SecretBundle::new(
                secret::SecretString::new("debug-access-secret-marker".to_owned()),
                None,
                None,
            ),
            metadata: debug_oauth_metadata(),
            masked_hint: Some("debug-hint-marker".to_owned()),
        }
    }

    #[test]
    fn credential_resolution_debug_redacts_physical_slot() {
        let logical = secret::LogicalCredentialId::new("debug-credential")
            .expect("fixture logical credential");
        let slot = secret::PhysicalSecretSlot::allocate(&logical);
        let raw_slot = slot.as_str().to_owned();
        let request = CredentialResolutionRequest {
            credential_id: CredentialId::new("debug-credential-marker"),
            expected_physical_slot: Some(slot),
        };

        let debug = format!("{request:?}");
        assert!(!debug.contains(&raw_slot));
        assert!(!debug.contains("debug-credential-marker"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn generic_repository_debug_never_formats_payloads() {
        let observed = Observed {
            revision: Revision(3),
            value: "debug-observed-payload-marker",
        };
        let committed = RepositoryCas::Committed {
            revision: Revision(4),
            value: "debug-cas-payload-marker",
        };

        let observed_debug = format!("{observed:?}");
        let committed_debug = format!("{committed:?}");
        assert!(!observed_debug.contains("debug-observed-payload-marker"));
        assert!(!committed_debug.contains("debug-cas-payload-marker"));
        assert!(observed_debug.contains("Revision(3)"));
        assert!(committed_debug.contains("Revision(4)"));
    }

    #[test]
    fn oauth_and_http_binding_debug_hide_all_user_provider_coordinates() {
        let logical = secret::LogicalCredentialId::new("debug-logical-marker")
            .expect("fixture logical credential");
        let slot = secret::PhysicalSecretSlot::allocate(&logical);
        let raw_slot = slot.as_str().to_owned();
        let metadata = debug_oauth_metadata();
        let binding = HttpAuthBinding {
            credential_id: CredentialId::new("debug-credential-marker"),
            physical_slot: slot.clone(),
            oauth_metadata: Some(metadata.clone()),
        };
        let update = debug_oauth_update();
        let staged = secret::StagedSecretBundle {
            logical_id: logical,
            new_slot: slot.clone(),
            previous_slot: Some(slot.clone()),
            entries: secret::BundleEntryPresence {
                access: true,
                refresh: false,
                dcr: false,
            },
        };
        let descriptor = OAuthPublishDescriptor {
            staged: &staged,
            mode: OAuthPublishMode::Rotation {
                expected_previous: slot.clone(),
            },
            metadata: &metadata,
            label: "debug-label-marker",
            masked_hint: Some("debug-hint-marker"),
        };
        let result = OAuthPublishResult::Committed {
            revision: Revision(7),
            previous_slot: Some(slot),
        };

        let debug = format!("{binding:?} {update:?} {descriptor:?} {result:?}");
        for marker in [
            "debug-logical-marker",
            "debug-credential-marker",
            "debug-label-marker",
            "debug-provider-marker",
            "debug-client-marker",
            "debug-scope-marker",
            "debug-access-secret-marker",
            "debug-hint-marker",
            raw_slot.as_str(),
        ] {
            assert!(!debug.contains(marker), "Debug leaked {marker}: {debug}");
        }
        assert!(debug.contains("REDACTED"));
        assert!(debug.contains("Revision(7)"));
    }

    #[test]
    fn every_public_oauth_result_debug_is_low_cardinality_and_redacted() {
        let logical = secret::LogicalCredentialId::new("debug-logical-marker")
            .expect("fixture logical credential");
        let slot = secret::PhysicalSecretSlot::allocate(&logical);
        let raw_slot = slot.as_str().to_owned();
        let stored = StoredOAuthClient {
            server_id: ServerId::new("debug-server-marker"),
            logical_id: logical.clone(),
            client_id: "debug-client-marker".to_owned(),
            client_secret: Some(SensitiveInput::from(
                "debug-client-secret-marker".to_owned(),
            )),
            workspace_hint: Some("debug-workspace-marker".to_owned()),
            metadata: Some(debug_oauth_metadata()),
            manual_client: true,
        };
        let refresh = OAuthRefreshRequest {
            logical_id: logical,
            current_slot: slot,
            metadata: debug_oauth_metadata(),
            label: "debug-label-marker".to_owned(),
        };
        let refresh_outcome = OAuthRefreshOutcome::Refreshed(Box::new(debug_oauth_update()));
        let discovery = OAuthDiscovery {
            continuation: OAuthContinuation::new(SensitiveInput::from(
                "debug-continuation-marker".to_owned(),
            )),
            authority: EndpointDisplay::new("https://debug-authority-marker.invalid"),
            resource: EndpointDisplay::new("https://debug-resource-marker.invalid"),
            scopes: vec!["debug-scope-marker".to_owned()],
        };
        let client_request = OAuthClientRequest {
            continuation: OAuthContinuation::new(SensitiveInput::from(
                "debug-client-continuation-marker".to_owned(),
            )),
            reason: ErrorCode::AuthenticationRequired,
            workspace_hint: Some("debug-workspace-marker".to_owned()),
        };
        let completion = OAuthCompletion {
            credential: debug_oauth_update(),
            workspace_label: Some("debug-workspace-marker".to_owned()),
            can_choose_workspace: true,
        };
        let recovery = OAuthRecoveryTarget {
            kind: SlackRecoveryKind::ConfigureApp,
            url: Some(SensitiveInput::from(
                "https://debug-recovery-marker.invalid".to_owned(),
            )),
        };
        let failure = OAuthFailure {
            error_code: ErrorCode::AuthenticationFailed,
            recovery: vec![OAuthRecoveryTarget {
                kind: SlackRecoveryKind::EnableMcpAccess,
                url: Some(SensitiveInput::from(
                    "https://debug-failure-recovery-marker.invalid".to_owned(),
                )),
            }],
        };
        let completed_output = OAuthAuthorizeOutput::Completed(Box::new(OAuthCompletion {
            credential: debug_oauth_update(),
            workspace_label: Some("debug-output-workspace-marker".to_owned()),
            can_choose_workspace: false,
        }));
        let client_output = OAuthAuthorizeOutput::ClientInputRequired(OAuthClientRequest {
            continuation: OAuthContinuation::new(SensitiveInput::from(
                "debug-output-continuation-marker".to_owned(),
            )),
            reason: ErrorCode::AuthenticationRequired,
            workspace_hint: Some("debug-output-hint-marker".to_owned()),
        });
        let failure_output = OAuthAuthorizeOutput::Failed(OAuthFailure {
            error_code: ErrorCode::AuthenticationFailed,
            recovery: vec![OAuthRecoveryTarget {
                kind: SlackRecoveryKind::RetryAuthorization,
                url: Some(SensitiveInput::from(
                    "https://debug-output-recovery-marker.invalid".to_owned(),
                )),
            }],
        });

        let debug = format!(
            "{stored:?} {refresh:?} {refresh_outcome:?} {discovery:?} {client_request:?} \
             {completion:?} {recovery:?} {failure:?} {completed_output:?} {client_output:?} \
             {failure_output:?}"
        );
        for marker in [
            "debug-logical-marker",
            "debug-server-marker",
            "debug-client-marker",
            "debug-client-secret-marker",
            "debug-provider-marker",
            "debug-label-marker",
            "debug-hint-marker",
            "debug-continuation-marker",
            "debug-client-continuation-marker",
            "debug-authority-marker",
            "debug-resource-marker",
            "debug-scope-marker",
            "debug-workspace-marker",
            "debug-recovery-marker",
            "debug-failure-recovery-marker",
            "debug-output-workspace-marker",
            "debug-output-continuation-marker",
            "debug-output-hint-marker",
            "debug-output-recovery-marker",
            raw_slot.as_str(),
        ] {
            assert!(!debug.contains(marker), "Debug leaked {marker}: {debug}");
        }
        assert!(debug.contains("REDACTED"));
        assert!(debug.contains("scope_count: 1"));
        assert!(debug.contains("recovery_count: 1"));
    }
}
