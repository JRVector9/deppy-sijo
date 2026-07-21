use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use connector_contract::{
    CredentialId, ErrorCode, OAuthClientPrompt, OperationId, PermissionRule, Revision,
    SensitiveInput, ServerDraft, ServerId, ServerSummary, SlackStatus, ToolId, ToolListItem,
};

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
    pub slack_status: SlackStatus,
    pub slack_tool_count: usize,
    pub servers: Vec<ServerSummary>,
}

#[derive(Debug, Clone)]
pub struct RepositoryToolPage {
    pub total: usize,
    pub items: Vec<ToolListItem>,
}

#[derive(Debug)]
pub struct ImportPlan {
    pub servers: Vec<ServerDraft>,
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

#[derive(Debug)]
pub struct InvokeRequest {
    pub operation_id: OperationId,
    pub server: ServerDraft,
    pub tool_id: ToolId,
    pub arguments_json: SensitiveInput,
}

#[derive(Debug)]
pub struct StoredOAuthClient {
    pub server_id: ServerId,
    pub client_id: String,
    pub client_secret: SensitiveInput,
    pub workspace_hint: Option<String>,
}

#[derive(Debug, Clone)]
pub enum OAuthOutput {
    Completed,
    ClientInputRequired(OAuthClientPrompt),
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
}

/// Keyring-neutral secret capability. Secret values cross the boundary only as
/// non-Clone/non-Serialize/redacted `SensitiveInput` and never enter snapshots.
pub trait ConnectorSecrets: Send + Sync + 'static {
    fn resolve_credential(
        &self,
        credential_id: &CredentialId,
    ) -> Result<SensitiveInput, ServiceError>;
    fn store_oauth_client(&self, client: StoredOAuthClient) -> Result<(), ServiceError>;
}

pub trait ConnectorMcp: Send + Sync + 'static {
    fn discover(
        &self,
        operation_id: &OperationId,
        server: ServerDraft,
        cancellation: CancellationToken,
    ) -> Result<DiscoverOutput, ServiceError>;

    /// Implementations must load live schema immediately before the call and must not
    /// retry `mcp::McpDeliveryUnknown` outcomes.
    fn load_schema_and_invoke(
        &self,
        request: InvokeRequest,
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
    fn begin(
        &self,
        operation_id: &OperationId,
        server: ServerDraft,
        cancellation: CancellationToken,
    ) -> Result<OAuthOutput, ServiceError>;

    fn submit_client(
        &self,
        operation_id: &OperationId,
        client: StoredOAuthClient,
        cancellation: CancellationToken,
    ) -> Result<OAuthOutput, ServiceError>;

    fn cancel(&self, operation_id: &OperationId);
}

/// Adapter helper for `ConnectorMcp::cancel`: implementations that own a live MC01
/// connection call this to guarantee process-group kill/reap for stdio.
pub fn cancel_live_mcp_connection(connection: &mut mcp::McpConnection) {
    connection.cancel();
}
