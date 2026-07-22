//! Transport-neutral Connector boundary types.
//!
//! This crate deliberately contains no storage rows, protocol runtime types, OAuth tokens, or
//! secret-store types. UI code consumes immutable snapshots and emits local intents; services
//! translate those values at the composition boundary.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

string_id!(ServerId);
string_id!(ToolId);
string_id!(CredentialId);

/// Correlation identifier shared by validation, authorization, audit, call, and outcome records.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(String);

impl OperationId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Revision(pub u64);

impl Revision {
    pub const ZERO: Self = Self(0);

    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// Plain user input that may contain credentials, OAuth codes, or tool arguments.
///
/// It is intentionally neither `Clone` nor `Serialize`. Debug output is always redacted and the
/// backing bytes are overwritten with volatile writes on drop.
pub struct SensitiveInput {
    bytes: Box<[u8]>,
}

impl SensitiveInput {
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: value.into().into_boxed_slice(),
        }
    }

    pub fn expose_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes).into_vec()
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl From<String> for SensitiveInput {
    fn from(value: String) -> Self {
        Self::new(value.into_bytes())
    }
}

impl fmt::Debug for SensitiveInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SensitiveInput(REDACTED)")
    }
}

impl Drop for SensitiveInput {
    fn drop(&mut self) {
        for byte in &mut self.bytes {
            // SAFETY: `byte` is a valid, exclusively borrowed byte in the owned allocation.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

/// Initial production ceilings. A configured limit may be lowered, but raising a ceiling requires
/// a measured source change and review rather than a runtime setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub command_queue: usize,
    pub host_actions: usize,
    pub snapshot_backlog: usize,
    pub mcp_operations: usize,
    pub oauth_flows: usize,
    pub session_trust_entries: usize,
    pub import_input_bytes: usize,
    pub import_servers: usize,
    pub import_report_items: usize,
    pub import_report_bytes: usize,
    pub tools_per_server: usize,
    pub tool_descriptor_bytes: usize,
    pub tool_input_bytes: usize,
    pub raw_mcp_response_bytes: usize,
    pub ui_result_bytes: usize,
    pub diagnostic_transitions: usize,
    pub backend_leases: usize,
}

impl ResourceLimits {
    pub const PRODUCTION_CEILING: Self = Self {
        command_queue: 8,
        host_actions: 8,
        snapshot_backlog: 1,
        mcp_operations: 2,
        oauth_flows: 1,
        session_trust_entries: 256,
        import_input_bytes: 1024 * 1024,
        import_servers: 256,
        import_report_items: 256,
        import_report_bytes: 64 * 1024,
        tools_per_server: 4_096,
        tool_descriptor_bytes: 8 * 1024 * 1024,
        tool_input_bytes: 32 * 1024,
        raw_mcp_response_bytes: 8 * 1024 * 1024,
        ui_result_bytes: 1024 * 1024,
        diagnostic_transitions: 64,
        backend_leases: 2,
    };

    pub fn validate(self) -> Result<Self, ResourceLimitError> {
        let ceiling = Self::PRODUCTION_CEILING;
        let checks = [
            ("command_queue", self.command_queue, ceiling.command_queue),
            ("host_actions", self.host_actions, ceiling.host_actions),
            (
                "snapshot_backlog",
                self.snapshot_backlog,
                ceiling.snapshot_backlog,
            ),
            (
                "mcp_operations",
                self.mcp_operations,
                ceiling.mcp_operations,
            ),
            ("oauth_flows", self.oauth_flows, ceiling.oauth_flows),
            (
                "session_trust_entries",
                self.session_trust_entries,
                ceiling.session_trust_entries,
            ),
            (
                "import_input_bytes",
                self.import_input_bytes,
                ceiling.import_input_bytes,
            ),
            (
                "import_servers",
                self.import_servers,
                ceiling.import_servers,
            ),
            (
                "import_report_items",
                self.import_report_items,
                ceiling.import_report_items,
            ),
            (
                "import_report_bytes",
                self.import_report_bytes,
                ceiling.import_report_bytes,
            ),
            (
                "tools_per_server",
                self.tools_per_server,
                ceiling.tools_per_server,
            ),
            (
                "tool_descriptor_bytes",
                self.tool_descriptor_bytes,
                ceiling.tool_descriptor_bytes,
            ),
            (
                "tool_input_bytes",
                self.tool_input_bytes,
                ceiling.tool_input_bytes,
            ),
            (
                "raw_mcp_response_bytes",
                self.raw_mcp_response_bytes,
                ceiling.raw_mcp_response_bytes,
            ),
            (
                "ui_result_bytes",
                self.ui_result_bytes,
                ceiling.ui_result_bytes,
            ),
            (
                "diagnostic_transitions",
                self.diagnostic_transitions,
                ceiling.diagnostic_transitions,
            ),
            (
                "backend_leases",
                self.backend_leases,
                ceiling.backend_leases,
            ),
        ];
        for (field, value, maximum) in checks {
            if value == 0 || value > maximum {
                return Err(ResourceLimitError {
                    field,
                    value,
                    maximum,
                });
            }
        }
        if self.snapshot_backlog != 1 {
            return Err(ResourceLimitError {
                field: "snapshot_backlog",
                value: self.snapshot_backlog,
                maximum: 1,
            });
        }
        Ok(self)
    }
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self::PRODUCTION_CEILING
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceLimitError {
    pub field: &'static str,
    pub value: usize,
    pub maximum: usize,
}

impl fmt::Display for ResourceLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} must be within 1..={} (got {})",
            self.field, self.maximum, self.value
        )
    }
}

impl std::error::Error for ResourceLimitError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    Stdio,
    Http,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Disabled,
    Idle,
    Checking,
    NeedsAuthorization,
    Connected,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlackStatus {
    NotConfigured,
    Ready,
    Checking,
    NeedsAuthorization,
    Connected,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlackRecoveryKind {
    ConfigureApp,
    EnableMcpAccess,
    RetryAuthorization,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackProjection {
    pub server_id: Option<ServerId>,
    pub status: SlackStatus,
    pub tool_count: usize,
    /// Sanitized workspace label/domain. OAuth team IDs and tokens are never exposed here.
    pub workspace_label: Option<String>,
    pub can_choose_workspace: bool,
    pub recovery: Option<SlackRecoveryKind>,
}

impl Default for SlackProjection {
    fn default() -> Self {
        Self {
            server_id: None,
            status: SlackStatus::NotConfigured,
            tool_count: 0,
            workspace_label: None,
            can_choose_workspace: false,
            recovery: None,
        }
    }
}

/// Validated endpoint text intended only for direct user display. Debug deliberately hides the
/// raw URL so derived snapshot/prompt diagnostics cannot log it accidentally.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EndpointDisplay(String);

impl EndpointDisplay {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for EndpointDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EndpointDisplay(REDACTED)")
    }
}

/// SHA-256 fingerprint of the exact validated endpoint/config identity. UI echoes this opaque
/// value with its decision; the service still re-checks the live endpoint before any network I/O.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EndpointFingerprint(String);

impl EndpointFingerprint {
    pub fn parse(value: impl Into<String>) -> Result<Self, EndpointFingerprintError> {
        let value = value.into();
        if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            Ok(Self(value.to_ascii_lowercase()))
        } else {
            Err(EndpointFingerprintError)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointFingerprintError;

impl fmt::Display for EndpointFingerprintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("endpoint fingerprint must be 64 hexadecimal characters")
    }
}

impl std::error::Error for EndpointFingerprintError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTrustPurpose {
    Discover,
    Invoke,
    OAuth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteTrustPrompt {
    pub operation_id: OperationId,
    pub server_id: ServerId,
    pub server_name: String,
    pub purpose: RemoteTrustPurpose,
    pub display_endpoint: EndpointDisplay,
    pub endpoint_fingerprint: EndpointFingerprint,
    pub config_revision: Revision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSummary {
    pub id: ServerId,
    pub name: String,
    pub transport: TransportKind,
    pub enabled: bool,
    pub connection: ConnectionState,
    pub tool_count: usize,
    pub error_code: Option<ErrorCode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolListItem {
    pub id: ToolId,
    pub name: String,
    pub description: Option<String>,
    pub permission: PermissionRule,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPage {
    pub server_id: ServerId,
    pub offset: usize,
    pub total: usize,
    pub items: Arc<[ToolListItem]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPrompt {
    pub operation_id: OperationId,
    pub server_id: ServerId,
    pub server_name: String,
    pub tool_name: String,
    pub arguments_preview: String,
    pub reason: ApprovalReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationSummary {
    pub id: OperationId,
    pub server_id: ServerId,
    pub kind: OperationKind,
    pub phase: OperationPhase,
    pub error_code: Option<ErrorCode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorSnapshot {
    pub revision: Revision,
    pub config_revision: Revision,
    pub slack: SlackProjection,
    pub servers: Arc<[ServerSummary]>,
    pub selected_server: Option<ServerId>,
    /// Editable transport configuration is loaded only for the selected server.
    #[serde(default)]
    pub selected_server_config: Option<ServerDraft>,
    pub tool_page: Option<ToolPage>,
    pub approval: Option<ApprovalPrompt>,
    #[serde(default)]
    pub remote_trust: Option<RemoteTrustPrompt>,
    #[serde(default)]
    pub oauth: Option<OAuthUiState>,
    pub operations: Arc<[OperationSummary]>,
    pub result: Option<OperationResult>,
    #[serde(default)]
    pub import_report: Option<ImportReport>,
    pub diagnostics: DiagnosticsSnapshot,
}

impl Default for ConnectorSnapshot {
    fn default() -> Self {
        Self {
            revision: Revision::ZERO,
            config_revision: Revision::ZERO,
            slack: SlackProjection::default(),
            servers: Arc::from([]),
            selected_server: None,
            selected_server_config: None,
            tool_page: None,
            approval: None,
            remote_trust: None,
            oauth: None,
            operations: Arc::from([]),
            result: None,
            import_report: None,
            diagnostics: DiagnosticsSnapshot::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationResult {
    pub operation_id: OperationId,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthUiState {
    pub operation_id: OperationId,
    pub server_id: ServerId,
    pub server_name: String,
    pub config_revision: Revision,
    pub phase: OAuthUiPhase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum OAuthUiPhase {
    DiscoveringAuth,
    AwaitingConsent {
        authority: EndpointDisplay,
        resource: EndpointDisplay,
        scopes: Arc<[String]>,
    },
    AwaitingClient {
        reason: ErrorCode,
        workspace_hint: Option<String>,
    },
    PreparingCallback,
    /// Callback listener is bound and the one-shot app host action is ready to be queued.
    BrowserReady,
    AwaitingCallback,
    Failed {
        error_code: ErrorCode,
        recovery: Arc<[OAuthRecoveryAction]>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthRecoveryAction {
    Retry,
    ChooseWorkspace,
    OpenSlackMcpSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportSource {
    Paste,
    File,
    ClaudeDesktop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportSourceRequest {
    FilePicker,
    ClaudeDesktop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportOutcome {
    Added,
    SkippedDuplicate,
    SkippedUnsupported,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReportItem {
    pub name: String,
    pub outcome: ImportOutcome,
    pub error_code: Option<ErrorCode>,
    pub omitted_secret_env_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReport {
    pub operation_id: OperationId,
    pub source: ImportSource,
    pub added: usize,
    pub skipped: usize,
    pub failed: usize,
    pub items: Arc<[ImportReportItem]>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticsSnapshot {
    pub command_queue_depth: usize,
    pub active_mcp_operations: usize,
    pub active_oauth_flows: usize,
    pub backend_leases: usize,
    pub timeouts: u64,
    pub cancellations: u64,
    pub stale_results: u64,
    pub backpressure_rejections: u64,
    pub transitions: Arc<[DiagnosticTransition]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticTransition {
    pub kind: OperationKind,
    pub phase: OperationPhase,
    pub error_code: Option<ErrorCode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Trust,
    Discover,
    Invoke,
    OAuth,
    Import,
    SaveServer,
    DeleteServer,
    UpdatePermission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    Queued,
    Validating,
    AwaitingTrust,
    DiscoveringSchema,
    DiscoveringAuth,
    AwaitingConsent,
    AwaitingClient,
    PreparingCallback,
    BrowserReady,
    AwaitingCallback,
    Authorizing,
    AuditPreflight,
    Calling,
    Persisting,
    Succeeded,
    Failed,
    Unknown,
    Denied,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidInput,
    InvalidUrl,
    LimitExceeded,
    Backpressure,
    StorageUnavailable,
    SecretUnavailable,
    TrustDenied,
    HostUnavailable,
    PermissionDenied,
    AuditUnavailable,
    AuthenticationRequired,
    AuthenticationFailed,
    OAuthCallbackFailed,
    NetworkTimeout,
    TransportFailed,
    ProtocolViolation,
    StaleResult,
    Cancelled,
    UnknownDelivery,
    Internal,
}

/// Stable failpoint names used by deterministic transaction, cancellation, and crash-recovery
/// tests. Production code does not expose a runtime switch for these points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePoint {
    RepositoryBeforeCommit,
    RepositoryAfterPreparedAudit,
    SecretWrite,
    SecretPointerSwap,
    AuditPreflightCommit,
    McpBeforeSend,
    McpAfterSendUnknown,
    HttpTimeout,
    WorkerPanic,
    ProcessCrash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureInjection {
    pub point: FailurePoint,
    /// Number of matching operations to skip before injecting once.
    pub skip: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRule {
    Ask,
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReason {
    AskRule,
    FirstUse,
    SchemaChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerDraft {
    pub id: Option<ServerId>,
    pub name: String,
    pub transport: TransportDraft,
    pub enabled: bool,
}

impl fmt::Debug for ServerDraft {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerDraft")
            .field("has_id", &self.id.is_some())
            .field("name_bytes", &self.name.len())
            .field("transport", &self.transport)
            .field("enabled", &self.enabled)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransportDraft {
    Stdio {
        command: String,
        args: Vec<String>,
        plain_env: Vec<(String, String)>,
        secret_env: Vec<(String, CredentialId)>,
        inherit_env: bool,
    },
    Http {
        url: String,
    },
}

impl fmt::Debug for TransportDraft {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio {
                args,
                plain_env,
                secret_env,
                inherit_env,
                ..
            } => formatter
                .debug_struct("TransportDraft::Stdio")
                .field("argument_count", &args.len())
                .field("plain_env_count", &plain_env.len())
                .field("secret_env_count", &secret_env.len())
                .field("inherit_env", inherit_env)
                .finish(),
            Self::Http { .. } => formatter.write_str("TransportDraft::Http(REDACTED)"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalLinkKind {
    SlackAppSettings,
}

/// A single frame may emit at most one user intent. The UI retains drafts locally and does not
/// clone/serialize secret-bearing variants.
pub enum ConnectorIntent {
    Activate,
    SelectServer(Option<ServerId>),
    RequestToolPage {
        server_id: ServerId,
        offset: usize,
    },
    SaveServer(ServerDraft),
    DeleteServer(ServerId),
    Discover(ServerId),
    InvokeTool {
        server_id: ServerId,
        tool_id: ToolId,
        arguments_json: SensitiveInput,
    },
    Cancel(OperationId),
    ResolveRemoteTrust {
        operation_id: OperationId,
        config_revision: Revision,
        endpoint_fingerprint: EndpointFingerprint,
        accepted: bool,
    },
    BeginOAuth(ServerId),
    ResolveOAuthConsent {
        operation_id: OperationId,
        config_revision: Revision,
        accepted: bool,
    },
    SubmitOAuthClient {
        operation_id: OperationId,
        config_revision: Revision,
        server_id: ServerId,
        client_id: String,
        client_secret: SensitiveInput,
        workspace_hint: Option<String>,
    },
    SubmitSlackWorkspace {
        operation_id: OperationId,
        config_revision: Revision,
        workspace: String,
    },
    RetryOAuth(ServerId),
    ResolveOAuthRecovery {
        operation_id: OperationId,
        action: OAuthRecoveryAction,
    },
    ResolveApproval {
        operation_id: OperationId,
        decision: ApprovalDecision,
    },
    SetPermission {
        server_id: ServerId,
        tool_id: ToolId,
        rule: PermissionRule,
    },
    ConnectSlack,
    ChooseSlackWorkspace(ServerId),
    OpenSlackRecovery {
        server_id: ServerId,
        kind: SlackRecoveryKind,
    },
    RequestImportSource(ImportSourceRequest),
    /// App-owned file picker/read path re-enters the same intent dispatcher with bounded bytes.
    /// The service validates the byte/item ceilings before parsing or persistence.
    ImportConfiguration {
        source: ImportSource,
        display_name: Option<String>,
        contents: SensitiveInput,
    },
    OpenExternalLink(ExternalLinkKind),
    DismissResult(OperationId),
    DismissImportReport(OperationId),
}

impl fmt::Debug for ConnectorIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Activate => formatter.write_str("ConnectorIntent::Activate"),
            Self::SelectServer(server_id) => formatter
                .debug_struct("ConnectorIntent::SelectServer")
                .field("selected", &server_id.is_some())
                .finish(),
            Self::RequestToolPage { offset, .. } => formatter
                .debug_struct("ConnectorIntent::RequestToolPage")
                .field("offset", offset)
                .finish(),
            Self::SaveServer(draft) => formatter
                .debug_tuple("ConnectorIntent::SaveServer")
                .field(draft)
                .finish(),
            Self::DeleteServer(_) => formatter.write_str("ConnectorIntent::DeleteServer"),
            Self::Discover(_) => formatter.write_str("ConnectorIntent::Discover"),
            Self::InvokeTool { .. } => formatter.write_str("ConnectorIntent::InvokeTool(REDACTED)"),
            Self::Cancel(_) => formatter.write_str("ConnectorIntent::Cancel"),
            Self::ResolveRemoteTrust {
                config_revision,
                accepted,
                ..
            } => formatter
                .debug_struct("ConnectorIntent::ResolveRemoteTrust")
                .field("config_revision", config_revision)
                .field("accepted", accepted)
                .finish(),
            Self::BeginOAuth(_) => formatter.write_str("ConnectorIntent::BeginOAuth"),
            Self::ResolveOAuthConsent {
                config_revision,
                accepted,
                ..
            } => formatter
                .debug_struct("ConnectorIntent::ResolveOAuthConsent")
                .field("config_revision", config_revision)
                .field("accepted", accepted)
                .finish(),
            Self::SubmitOAuthClient {
                config_revision,
                client_secret,
                workspace_hint,
                ..
            } => formatter
                .debug_struct("ConnectorIntent::SubmitOAuthClient")
                .field("config_revision", config_revision)
                .field("has_client_secret", &!client_secret.is_empty())
                .field("has_workspace_hint", &workspace_hint.is_some())
                .finish(),
            Self::SubmitSlackWorkspace {
                config_revision, ..
            } => formatter
                .debug_struct("ConnectorIntent::SubmitSlackWorkspace")
                .field("config_revision", config_revision)
                .finish(),
            Self::RetryOAuth(_) => formatter.write_str("ConnectorIntent::RetryOAuth"),
            Self::ResolveOAuthRecovery { action, .. } => formatter
                .debug_tuple("ConnectorIntent::ResolveOAuthRecovery")
                .field(action)
                .finish(),
            Self::ResolveApproval { decision, .. } => formatter
                .debug_tuple("ConnectorIntent::ResolveApproval")
                .field(decision)
                .finish(),
            Self::SetPermission { rule, .. } => formatter
                .debug_tuple("ConnectorIntent::SetPermission")
                .field(rule)
                .finish(),
            Self::ConnectSlack => formatter.write_str("ConnectorIntent::ConnectSlack"),
            Self::ChooseSlackWorkspace(_) => {
                formatter.write_str("ConnectorIntent::ChooseSlackWorkspace")
            }
            Self::OpenSlackRecovery { kind, .. } => formatter
                .debug_tuple("ConnectorIntent::OpenSlackRecovery")
                .field(kind)
                .finish(),
            Self::RequestImportSource(source) => formatter
                .debug_tuple("ConnectorIntent::RequestImportSource")
                .field(source)
                .finish(),
            Self::ImportConfiguration { source, .. } => formatter
                .debug_struct("ConnectorIntent::ImportConfiguration")
                .field("source", source)
                .field("contents", &"REDACTED")
                .finish(),
            Self::OpenExternalLink(kind) => formatter
                .debug_tuple("ConnectorIntent::OpenExternalLink")
                .field(kind)
                .finish(),
            Self::DismissResult(_) => formatter.write_str("ConnectorIntent::DismissResult"),
            Self::DismissImportReport(_) => {
                formatter.write_str("ConnectorIntent::DismissImportReport")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorEvent {
    pub operation_id: OperationId,
    pub snapshot_revision: Revision,
    pub config_revision: Revision,
    pub server_id: Option<ServerId>,
    pub kind: OperationKind,
    pub phase: OperationPhase,
    pub error_code: Option<ErrorCode>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_debug_is_always_redacted() {
        let input = SensitiveInput::from("oauth-super-secret".to_owned());
        let debug = format!("{input:?}");
        assert_eq!(debug, "SensitiveInput(REDACTED)");
        assert!(!debug.contains("oauth-super-secret"));
    }

    #[test]
    fn production_limits_match_the_frozen_ceiling() {
        let limits = ResourceLimits::default().validate().unwrap();
        assert_eq!(limits.command_queue, 8);
        assert_eq!(limits.host_actions, 8);
        assert_eq!(limits.snapshot_backlog, 1);
        assert_eq!(limits.mcp_operations, 2);
        assert_eq!(limits.oauth_flows, 1);
        assert_eq!(limits.session_trust_entries, 256);
        assert_eq!(limits.import_input_bytes, 1024 * 1024);
        assert_eq!(limits.import_servers, 256);
        assert_eq!(limits.import_report_items, 256);
        assert_eq!(limits.import_report_bytes, 64 * 1024);
        assert_eq!(limits.tools_per_server, 4_096);
        assert_eq!(limits.tool_descriptor_bytes, 8 * 1024 * 1024);
        assert_eq!(limits.tool_input_bytes, 32 * 1024);
        assert_eq!(limits.raw_mcp_response_bytes, 8 * 1024 * 1024);
        assert_eq!(limits.ui_result_bytes, 1024 * 1024);
        assert_eq!(limits.diagnostic_transitions, 64);
        assert_eq!(limits.backend_leases, 2);
    }

    #[test]
    fn configured_limits_can_only_be_lowered() {
        let mut limits = ResourceLimits {
            command_queue: 4,
            ..ResourceLimits::default()
        };
        assert_eq!(limits.validate().unwrap().command_queue, 4);

        limits.command_queue = 9;
        assert_eq!(limits.validate().unwrap_err().field, "command_queue");
    }

    #[test]
    fn snapshot_is_latest_only_friendly() {
        let original = ConnectorSnapshot::default();
        let cloned = original.clone();
        assert!(Arc::ptr_eq(&original.servers, &cloned.servers));
        assert!(Arc::ptr_eq(&original.operations, &cloned.operations));
    }

    #[test]
    fn connector_intent_debug_never_exposes_user_or_provider_coordinates() {
        let intent = ConnectorIntent::ImportConfiguration {
            source: ImportSource::File,
            display_name: Some("debug-import-filename-marker.json".to_owned()),
            contents: SensitiveInput::from("client_secret=do-not-log".to_owned()),
        };
        let debug = format!("{intent:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("do-not-log"));
        assert!(!debug.contains("debug-import-filename-marker"));

        let drafts = [
            ServerDraft {
                id: Some(ServerId::new("debug-server-id-marker")),
                name: "debug-server-name-marker".to_owned(),
                transport: TransportDraft::Http {
                    url: "https://debug-provider-url-marker.example/mcp".to_owned(),
                },
                enabled: true,
            },
            ServerDraft {
                id: None,
                name: "debug-stdio-name-marker".to_owned(),
                transport: TransportDraft::Stdio {
                    command: "debug-command-marker".to_owned(),
                    args: vec!["debug-argument-marker".to_owned()],
                    plain_env: vec![(
                        "DEBUG_ENV_KEY_MARKER".to_owned(),
                        "debug-env-value-marker".to_owned(),
                    )],
                    secret_env: vec![(
                        "DEBUG_SECRET_KEY_MARKER".to_owned(),
                        CredentialId::new("debug-credential-id-marker"),
                    )],
                    inherit_env: false,
                },
                enabled: true,
            },
        ];
        let debug = format!("{:?} {:?}", drafts[0], drafts[1]);
        for marker in [
            "debug-server-id-marker",
            "debug-server-name-marker",
            "debug-provider-url-marker",
            "debug-stdio-name-marker",
            "debug-command-marker",
            "debug-argument-marker",
            "DEBUG_ENV_KEY_MARKER",
            "debug-env-value-marker",
            "DEBUG_SECRET_KEY_MARKER",
            "debug-credential-id-marker",
        ] {
            assert!(!debug.contains(marker), "Debug leaked {marker}: {debug}");
        }

        let oauth = ConnectorIntent::SubmitOAuthClient {
            operation_id: OperationId::new("debug-operation-id-marker"),
            config_revision: Revision(7),
            server_id: ServerId::new("debug-oauth-server-marker"),
            client_id: "debug-client-id-marker".to_owned(),
            client_secret: SensitiveInput::from("debug-client-secret-marker".to_owned()),
            workspace_hint: Some("debug-workspace-marker".to_owned()),
        };
        let slack = ConnectorIntent::SubmitSlackWorkspace {
            operation_id: OperationId::new("debug-slack-operation-marker"),
            config_revision: Revision(8),
            workspace: "debug-slack-workspace-marker".to_owned(),
        };
        let debug = format!("{oauth:?} {slack:?}");
        for marker in [
            "debug-operation-id-marker",
            "debug-oauth-server-marker",
            "debug-client-id-marker",
            "debug-client-secret-marker",
            "debug-workspace-marker",
            "debug-slack-operation-marker",
            "debug-slack-workspace-marker",
        ] {
            assert!(!debug.contains(marker), "Debug leaked {marker}: {debug}");
        }
        assert!(debug.contains("Revision(7)"));
        assert!(debug.contains("Revision(8)"));
    }

    #[test]
    fn endpoint_display_debug_never_exposes_raw_url() {
        let endpoint = EndpointDisplay::new("https://sensitive.example/mcp");
        let debug = format!("{endpoint:?}");
        assert_eq!(debug, "EndpointDisplay(REDACTED)");
        assert!(!debug.contains("sensitive.example"));
        assert_eq!(endpoint.as_str(), "https://sensitive.example/mcp");
    }

    #[test]
    fn endpoint_fingerprint_requires_exact_sha256_hex_shape() {
        let upper = "A".repeat(64);
        let fingerprint = EndpointFingerprint::parse(upper).unwrap();
        assert_eq!(fingerprint.as_str(), "a".repeat(64));
        assert!(EndpointFingerprint::parse("a".repeat(63)).is_err());
        assert!(EndpointFingerprint::parse("z".repeat(64)).is_err());
    }

    #[test]
    fn default_snapshot_has_no_interactive_or_import_backlog() {
        let snapshot = ConnectorSnapshot::default();
        assert_eq!(snapshot.slack, SlackProjection::default());
        assert!(snapshot.remote_trust.is_none());
        assert!(snapshot.oauth.is_none());
        assert!(snapshot.import_report.is_none());
    }
}
