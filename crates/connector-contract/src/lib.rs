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
    pub snapshot_backlog: usize,
    pub mcp_operations: usize,
    pub oauth_flows: usize,
    pub import_input_bytes: usize,
    pub import_servers: usize,
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
        snapshot_backlog: 1,
        mcp_operations: 2,
        oauth_flows: 1,
        import_input_bytes: 1024 * 1024,
        import_servers: 256,
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
    pub slack_status: SlackStatus,
    pub slack_tool_count: usize,
    pub servers: Arc<[ServerSummary]>,
    pub selected_server: Option<ServerId>,
    /// Editable transport configuration is loaded only for the selected server.
    #[serde(default)]
    pub selected_server_config: Option<ServerDraft>,
    pub tool_page: Option<ToolPage>,
    pub approval: Option<ApprovalPrompt>,
    /// Service request for manual OAuth client input. Contains no token/client secret.
    #[serde(default)]
    pub oauth_client_prompt: Option<OAuthClientPrompt>,
    pub operations: Arc<[OperationSummary]>,
    pub result: Option<OperationResult>,
    pub diagnostics: DiagnosticsSnapshot,
}

impl Default for ConnectorSnapshot {
    fn default() -> Self {
        Self {
            revision: Revision::ZERO,
            config_revision: Revision::ZERO,
            slack_status: SlackStatus::NotConfigured,
            slack_tool_count: 0,
            servers: Arc::from([]),
            selected_server: None,
            selected_server_config: None,
            tool_page: None,
            approval: None,
            oauth_client_prompt: None,
            operations: Arc::from([]),
            result: None,
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
pub struct OAuthClientPrompt {
    pub operation_id: OperationId,
    pub server_id: ServerId,
    pub server_name: String,
    pub workspace_hint: Option<String>,
    /// Sanitized, user-facing reason; raw HTTP/OAuth errors are not allowed here.
    pub reason: Option<String>,
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
    DiscoveringSchema,
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
    PermissionDenied,
    AuditUnavailable,
    AuthenticationRequired,
    AuthenticationFailed,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerDraft {
    pub id: Option<ServerId>,
    pub name: String,
    pub transport: TransportDraft,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// A single frame may emit at most one user intent. The UI retains drafts locally and does not
/// clone/serialize secret-bearing variants.
#[derive(Debug)]
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
    BeginOAuth(ServerId),
    SubmitOAuthClient {
        operation_id: Option<OperationId>,
        server_id: ServerId,
        client_id: String,
        client_secret: SensitiveInput,
        workspace_hint: Option<String>,
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
    EnsureSlackServer,
    RequestImportPicker,
    /// App-owned file picker/read path re-enters the same intent dispatcher with bounded bytes.
    /// The service validates the byte/item ceilings before parsing or persistence.
    ImportConfiguration {
        source_name: String,
        contents: SensitiveInput,
    },
    OpenExternalUrl {
        url: String,
    },
    DismissResult(OperationId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorEvent {
    pub operation_id: OperationId,
    pub config_revision: Revision,
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
        assert_eq!(limits.snapshot_backlog, 1);
        assert_eq!(limits.mcp_operations, 2);
        assert_eq!(limits.oauth_flows, 1);
        assert_eq!(limits.import_input_bytes, 1024 * 1024);
        assert_eq!(limits.import_servers, 256);
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
    fn import_intent_never_debugs_file_contents() {
        let intent = ConnectorIntent::ImportConfiguration {
            source_name: "mcp.json".to_owned(),
            contents: SensitiveInput::from("client_secret=do-not-log".to_owned()),
        };
        let debug = format!("{intent:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("do-not-log"));
    }
}
