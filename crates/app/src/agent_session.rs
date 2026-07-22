//! Structured agent-session state shared by App Server providers and native UI.
//!
//! This deliberately sits beside the PTY stack rather than inside it. A terminal
//! stream can contain arbitrary ANSI/full-screen applications, while an app-server
//! thread/turn/item stream is already structured and safe to render as rows.

use std::collections::HashMap;
use std::fmt;

use serde_json::Value;

/// App-owned ID. It is distinct from a Codex thread ID because one application
/// can later support other providers or resume a different thread.
pub type AgentSessionId = String;

/// High-level session lifecycle used by the native workspace panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSessionStatus {
    Starting,
    Ready,
    Running,
    AwaitingApproval,
    Completed,
    Interrupted,
    Failed,
    Stopped,
}

/// Authoritative runtime status emitted by Codex App Server through
/// `thread/status/changed`.
///
/// Keep this wire-facing type separate from [`AgentSessionStatus`]: the latter
/// also carries Deppy-owned presentation states such as an unacknowledged
/// completed turn, while this enum describes only the server's current thread
/// runtime state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentThreadStatus {
    NotLoaded,
    Idle,
    Active {
        waiting_on_approval: bool,
        waiting_on_user_input: bool,
    },
    SystemError,
}

impl AgentThreadStatus {
    /// Parse the stable tagged-union shape from `thread/status/changed`.
    /// Unknown future variants are ignored by returning `None`; they must not
    /// tear down an otherwise healthy App Server connection.
    pub fn from_codex(value: &Value) -> Option<Self> {
        match value.get("type").and_then(Value::as_str)? {
            "notLoaded" => Some(Self::NotLoaded),
            "idle" => Some(Self::Idle),
            "systemError" => Some(Self::SystemError),
            "active" => {
                let mut waiting_on_approval = false;
                let mut waiting_on_user_input = false;
                for flag in value
                    .get("activeFlags")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    match flag {
                        "waitingOnApproval" => waiting_on_approval = true,
                        "waitingOnUserInput" => waiting_on_user_input = true,
                        _ => {}
                    }
                }
                Some(Self::Active {
                    waiting_on_approval,
                    waiting_on_user_input,
                })
            }
            _ => None,
        }
    }
}

impl AgentSessionStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::AwaitingApproval => "approval",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Interrupted | Self::Failed | Self::Stopped
        )
    }
}

/// Item categories exposed by the stable Codex App Server item lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentItemKind {
    UserMessage,
    AgentMessage,
    Plan,
    Reasoning,
    CommandExecution,
    FileChange,
    McpToolCall,
    WebSearch,
    ImageView,
    Review,
    ContextCompaction,
    Other,
}

impl AgentItemKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::UserMessage => "input",
            Self::AgentMessage => "answer",
            Self::Plan => "plan",
            Self::Reasoning => "reasoning",
            Self::CommandExecution => "command",
            Self::FileChange => "file change",
            Self::McpToolCall => "connector",
            Self::WebSearch => "web search",
            Self::ImageView => "image",
            Self::Review => "review",
            Self::ContextCompaction => "context",
            Self::Other => "event",
        }
    }
}

/// A file change announced by Codex. `diff` is retained for the details pane,
/// but table rows only show the path/kind summary.
#[derive(PartialEq, Eq)]
pub struct AgentFileChange {
    pub path: String,
    pub kind: String,
    pub diff: Option<String>,
}

impl fmt::Debug for AgentFileChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentFileChange")
            .field("path_bytes", &self.path.len())
            .field("kind_bytes", &self.kind.len())
            .field("diff_bytes", &self.diff.as_ref().map(String::len))
            .finish()
    }
}

/// Provider-neutral representation of a streamed agent item.
#[derive(PartialEq, Eq)]
pub struct AgentItem {
    pub id: String,
    pub kind: AgentItemKind,
    pub status: Option<String>,
    pub summary: String,
    pub location: Option<String>,
    pub detail: Option<String>,
    pub output: String,
    pub files: Vec<AgentFileChange>,
}

impl fmt::Debug for AgentItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentItem")
            .field("id_bytes", &self.id.len())
            .field("kind", &self.kind)
            .field("status_present", &self.status.is_some())
            .field("summary_bytes", &self.summary.len())
            .field("location_present", &self.location.is_some())
            .field("detail_present", &self.detail.is_some())
            .field("output_bytes", &self.output.len())
            .field("file_count", &self.files.len())
            .finish()
    }
}

impl AgentItem {
    /// Convert the common Codex `ThreadItem` tagged union into the durable view
    /// model. Unknown future item kinds remain visible rather than causing a
    /// session to fail.
    pub fn from_codex(value: &Value) -> Option<Self> {
        let id = value.get("id")?.as_str()?;
        if !valid_identifier(id) {
            return None;
        }
        let id = id.to_owned();
        let wire_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let status = value
            .get("status")
            .and_then(Value::as_str)
            .map(|status| bounded_copy(status, MAX_STATUS_BYTES));

        let mut item = Self {
            id,
            kind: AgentItemKind::Other,
            status,
            summary: bounded_copy(wire_type, MAX_STATUS_BYTES),
            location: None,
            detail: None,
            output: String::new(),
            files: Vec::new(),
        };

        match wire_type {
            "userMessage" => {
                item.kind = AgentItemKind::UserMessage;
                item.summary = text_from(value.get("content"));
            }
            "agentMessage" => {
                item.kind = AgentItemKind::AgentMessage;
                item.summary = text_from(value.get("text"));
                item.location = value
                    .get("phase")
                    .and_then(Value::as_str)
                    .map(|phase| bounded_copy(phase, MAX_STATUS_BYTES));
            }
            "plan" => {
                item.kind = AgentItemKind::Plan;
                item.summary = text_from(value.get("text"));
            }
            "reasoning" => {
                item.kind = AgentItemKind::Reasoning;
                item.summary = text_from(value.get("summary"));
                item.output = text_from(value.get("content"));
            }
            "commandExecution" => {
                item.kind = AgentItemKind::CommandExecution;
                item.summary = text_from(value.get("command"));
                item.location = value
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(|cwd| bounded_copy(cwd, MAX_PATH_BYTES));
                item.output = text_from(value.get("aggregatedOutput"));
                item.detail = command_detail(value);
            }
            "fileChange" => {
                item.kind = AgentItemKind::FileChange;
                item.files = file_changes(value);
                item.summary = match item.files.len() {
                    0 => "file changes".to_owned(),
                    1 => item.files[0].path.clone(),
                    n => format!("{n} files changed"),
                };
                item.location = file_location(&item.files);
                item.detail = file_kinds(&item.files);
                for diff in item
                    .files
                    .iter()
                    .filter_map(|change| change.diff.as_deref())
                {
                    if !item.output.is_empty() {
                        append_limited(&mut item.output, "\n\n");
                    }
                    append_limited(&mut item.output, diff);
                }
            }
            "mcpToolCall" => {
                item.kind = AgentItemKind::McpToolCall;
                let server = value.get("server").and_then(Value::as_str).unwrap_or("MCP");
                let tool = value.get("tool").and_then(Value::as_str).unwrap_or("tool");
                item.summary.clear();
                append_limited(&mut item.summary, server);
                append_limited(&mut item.summary, " · ");
                append_limited(&mut item.summary, tool);
                item.output = text_from(value.get("result"));
                item.detail = value
                    .get("error")
                    .filter(|error| !error.is_null())
                    .map(|error| text_from(Some(error)));
            }
            "webSearch" => {
                item.kind = AgentItemKind::WebSearch;
                item.summary = text_from(value.get("query"));
                item.detail = text_from_optional(value.get("action"));
            }
            "imageView" => {
                item.kind = AgentItemKind::ImageView;
                item.summary = text_from(value.get("path"));
            }
            "enteredReviewMode" | "exitedReviewMode" => {
                item.kind = AgentItemKind::Review;
                item.summary = wire_type.to_owned();
                item.detail = text_from_optional(value.get("review"));
            }
            "contextCompaction" => {
                item.kind = AgentItemKind::ContextCompaction;
                item.summary = "context compacted".to_owned();
            }
            _ => {
                item.detail = text_from_optional(Some(value));
            }
        }

        item.summary = limit_text(item.summary);
        item.output = limit_text(item.output);
        item.detail = item.detail.map(limit_text);
        // 상세 패널용 원본 diff도 같은 상한 — 파생본(output)만 캡하고 원본을 무캡
        // 보존하면 대형 파일 변경이 세션 수명 내내 통째로 상주한다 (2026-07-16 리뷰).
        for change in &mut item.files {
            change.diff = change.diff.take().map(limit_text);
        }
        item.enforce_retained_limit();
        Some(item)
    }

    fn append_delta(&mut self, delta: &str) {
        match self.kind {
            AgentItemKind::CommandExecution => append_limited(&mut self.output, delta),
            AgentItemKind::Reasoning => append_limited(&mut self.output, delta),
            _ => append_limited(&mut self.summary, delta),
        }
    }

    fn enforce_retained_limit(&mut self) {
        self.status = self
            .status
            .take()
            .map(|status| bounded_owned(status, MAX_STATUS_BYTES));
        self.summary = bounded_owned(std::mem::take(&mut self.summary), MAX_ITEM_TEXT_BYTES);
        self.location = self
            .location
            .take()
            .map(|location| bounded_owned(location, MAX_PATH_BYTES));
        self.detail = self
            .detail
            .take()
            .map(|detail| bounded_owned(detail, MAX_ITEM_TEXT_BYTES));
        self.output = bounded_owned(std::mem::take(&mut self.output), MAX_ITEM_TEXT_BYTES);
        self.files.truncate(MAX_FILE_CHANGES_PER_ITEM);
        for file in &mut self.files {
            file.path = bounded_owned(std::mem::take(&mut file.path), MAX_PATH_BYTES);
            file.kind = bounded_owned(std::mem::take(&mut file.kind), MAX_STATUS_BYTES);
            file.diff = file
                .diff
                .take()
                .map(|diff| bounded_owned(diff, MAX_ITEM_TEXT_BYTES));
        }

        let mut retained = self.retained_bytes_without_files();
        let mut keep = 0;
        for file in &mut self.files {
            let fixed = file.path.len().saturating_add(file.kind.len());
            if retained.saturating_add(fixed) > MAX_ITEM_RETAINED_BYTES {
                break;
            }
            retained = retained.saturating_add(fixed);
            if let Some(diff) = &mut file.diff {
                let available = MAX_ITEM_RETAINED_BYTES.saturating_sub(retained);
                *diff = bounded_owned(std::mem::take(diff), available.min(MAX_ITEM_TEXT_BYTES));
                retained = retained.saturating_add(diff.len());
            }
            keep += 1;
        }
        self.files.truncate(keep);
    }

    fn retained_bytes_without_files(&self) -> usize {
        self.id
            .len()
            .saturating_add(self.status.as_ref().map_or(0, String::len))
            .saturating_add(self.summary.len())
            .saturating_add(self.location.as_ref().map_or(0, String::len))
            .saturating_add(self.detail.as_ref().map_or(0, String::len))
            .saturating_add(self.output.len())
    }

    fn retained_bytes(&self) -> usize {
        self.files
            .iter()
            .fold(self.retained_bytes_without_files(), |bytes, file| {
                bytes
                    .saturating_add(file.path.len())
                    .saturating_add(file.kind.len())
                    .saturating_add(file.diff.as_ref().map_or(0, String::len))
            })
    }
}

/// A server request which must be explicitly answered by the user. The raw
/// JSON-RPC ID remains in the transport; the UI only handles the opaque key.
#[derive(PartialEq, Eq)]
pub struct AgentApproval {
    pub request_key: String,
    pub kind: AgentApprovalKind,
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub reason: Option<String>,
    pub command: Option<String>,
    pub cwd: Option<String>,
}

impl fmt::Debug for AgentApproval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentApproval")
            .field("request_key_bytes", &self.request_key.len())
            .field("kind", &self.kind)
            .field("thread_id_bytes", &self.thread_id.len())
            .field("turn_id_bytes", &self.turn_id.len())
            .field("item_id_bytes", &self.item_id.len())
            .field("reason", &self.reason.as_ref().map(|_| "[REDACTED]"))
            .field("command", &self.command.as_ref().map(|_| "[REDACTED]"))
            .field("cwd", &self.cwd.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentApprovalKind {
    CommandExecution,
    FileChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentApprovalDecision {
    Accept,
    AcceptForSession,
    Decline,
    Cancel,
}

impl AgentApprovalDecision {
    pub fn wire_value(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::AcceptForSession => "acceptForSession",
            Self::Decline => "decline",
            Self::Cancel => "cancel",
        }
    }
}

impl AgentApproval {
    fn enforce_limits(&mut self) -> bool {
        if !valid_identifier(&self.request_key)
            || !valid_identifier(&self.thread_id)
            || !valid_identifier(&self.turn_id)
            || !valid_identifier(&self.item_id)
        {
            return false;
        }
        self.reason
            .as_ref()
            .is_none_or(|reason| reason.len() <= MAX_ITEM_TEXT_BYTES)
            && self
                .command
                .as_ref()
                .is_none_or(|command| command.len() <= MAX_ITEM_TEXT_BYTES)
            && self
                .cwd
                .as_ref()
                .is_none_or(|cwd| cwd.len() <= MAX_PATH_BYTES)
    }
}

/// Normalized event emitted by a structured agent transport.
#[derive(PartialEq, Eq)]
pub enum AgentSessionEvent {
    ConnectionReady,
    ThreadStarted {
        thread_id: String,
    },
    ThreadStatusChanged {
        status: AgentThreadStatus,
    },
    TurnStarted {
        turn_id: String,
    },
    ItemStarted {
        item: AgentItem,
    },
    ItemDelta {
        item_id: String,
        delta: String,
    },
    ItemCompleted {
        item: AgentItem,
    },
    ApprovalRequested {
        approval: AgentApproval,
    },
    ApprovalResolved {
        request_key: String,
    },
    TurnCompleted {
        status: String,
    },
    /// A control-plane operation (for example stale `turn/steer`) failed, but
    /// the thread lifecycle remains authoritative and must not regress.
    ControlError {
        message: String,
    },
    Failed {
        message: String,
    },
    Stopped,
}

impl fmt::Debug for AgentSessionEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionReady => formatter.write_str("ConnectionReady"),
            Self::ThreadStarted { thread_id } => formatter
                .debug_struct("ThreadStarted")
                .field("thread_id_bytes", &thread_id.len())
                .finish(),
            Self::ThreadStatusChanged { status } => formatter
                .debug_struct("ThreadStatusChanged")
                .field("status", status)
                .finish(),
            Self::TurnStarted { turn_id } => formatter
                .debug_struct("TurnStarted")
                .field("turn_id_bytes", &turn_id.len())
                .finish(),
            Self::ItemStarted { item } => formatter
                .debug_struct("ItemStarted")
                .field("item", item)
                .finish(),
            Self::ItemDelta { item_id, delta } => formatter
                .debug_struct("ItemDelta")
                .field("item_id_bytes", &item_id.len())
                .field("delta", &format_args!("[REDACTED; {} bytes]", delta.len()))
                .finish(),
            Self::ItemCompleted { item } => formatter
                .debug_struct("ItemCompleted")
                .field("item", item)
                .finish(),
            Self::ApprovalRequested { approval } => formatter
                .debug_struct("ApprovalRequested")
                .field("approval", approval)
                .finish(),
            Self::ApprovalResolved { request_key } => formatter
                .debug_struct("ApprovalResolved")
                .field("request_key_bytes", &request_key.len())
                .finish(),
            Self::TurnCompleted { status } => formatter
                .debug_struct("TurnCompleted")
                .field("status_bytes", &status.len())
                .finish(),
            Self::ControlError { message } => formatter
                .debug_struct("ControlError")
                .field(
                    "message",
                    &format_args!("[REDACTED; {} bytes]", message.len()),
                )
                .finish(),
            Self::Failed { message } => formatter
                .debug_struct("Failed")
                .field(
                    "message",
                    &format_args!("[REDACTED; {} bytes]", message.len()),
                )
                .finish(),
            Self::Stopped => formatter.write_str("Stopped"),
        }
    }
}

/// Stable `UserInput::Skill` reference accepted by `turn/start` and
/// `turn/steer`. Keep only the protocol identity fields in session state.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentSkillSelection {
    pub name: String,
    pub path: String,
}

impl fmt::Debug for AgentSkillSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentSkillSelection")
            .field("name_bytes", &self.name.len())
            .field("path", &"[REDACTED]")
            .finish()
    }
}

/// The stable UI-facing state of a structured agent conversation.
pub struct AgentSession {
    pub id: AgentSessionId,
    /// Workspace that owned this structured thread when it was created or
    /// restored. App Server session IDs are process-global, but notification
    /// navigation and history filtering remain workspace-scoped.
    pub workspace_id: Option<String>,
    pub prompt: String,
    pub cwd: Option<String>,
    /// Model override retained for structured-thread recovery. `None` means
    /// the App Server chose its configured default.
    pub model: Option<String>,
    /// Stable next-turn reasoning override. The App Server applies it to the
    /// submitted turn and subsequent turns.
    pub effort: Option<String>,
    pub skills: Vec<AgentSkillSelection>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    /// Latest authoritative App Server runtime state. `status` may deliberately
    /// retain a completed result until it is acknowledged even after this has
    /// already advanced to `Idle`.
    pub thread_status: Option<AgentThreadStatus>,
    pub status: AgentSessionStatus,
    pub items: Vec<AgentItem>,
    pub approvals: Vec<AgentApproval>,
    pub error: Option<String>,
    item_indices: HashMap<String, usize>,
    table_rows: Vec<AgentTableRow>,
    retained_item_bytes: usize,
    retained_table_bytes: usize,
}

impl fmt::Debug for AgentSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentSession")
            .field("id_bytes", &self.id.len())
            .field("workspace_id_present", &self.workspace_id.is_some())
            .field(
                "prompt",
                &format_args!("[REDACTED; {} bytes]", self.prompt.len()),
            )
            .field("cwd", &self.cwd.as_ref().map(|_| "[REDACTED]"))
            .field("model_present", &self.model.is_some())
            .field("effort_present", &self.effort.is_some())
            .field("skill_count", &self.skills.len())
            .field("thread_id_present", &self.thread_id.is_some())
            .field("turn_id_present", &self.turn_id.is_some())
            .field("thread_status", &self.thread_status)
            .field("status", &self.status)
            .field("item_count", &self.items.len())
            .field("approval_count", &self.approvals.len())
            .field("error_present", &self.error.is_some())
            .field("retained_item_bytes", &self.retained_item_bytes)
            .field("retained_table_bytes", &self.retained_table_bytes)
            .finish()
    }
}

impl AgentSession {
    pub fn new(id: AgentSessionId, prompt: String, cwd: Option<String>) -> Self {
        assert!(
            valid_identifier(&id),
            "AgentSessionId must be a non-empty identifier of at most 1 KiB"
        );
        Self {
            id,
            workspace_id: None,
            prompt: bounded_owned(prompt, MAX_PROMPT_BYTES),
            cwd: cwd.map(|cwd| bounded_owned(cwd, MAX_PATH_BYTES)),
            model: None,
            effort: None,
            skills: Vec::new(),
            thread_id: None,
            turn_id: None,
            thread_status: None,
            status: AgentSessionStatus::Starting,
            items: Vec::new(),
            approvals: Vec::new(),
            error: None,
            item_indices: HashMap::new(),
            table_rows: Vec::new(),
            retained_item_bytes: 0,
            retained_table_bytes: 0,
        }
    }

    pub fn apply(&mut self, event: AgentSessionEvent) {
        self.enforce_metadata_limits();
        let previous_status = self.status;
        match event {
            AgentSessionEvent::ConnectionReady => {
                if self.status == AgentSessionStatus::Starting {
                    self.status = AgentSessionStatus::Ready;
                }
            }
            AgentSessionEvent::ThreadStarted { thread_id } => {
                if !valid_identifier(&thread_id) {
                    self.error = Some("protocol_error:thread_identifier".to_owned());
                    self.status = AgentSessionStatus::Failed;
                    self.refresh_fallback_row_states();
                    return;
                }
                self.thread_id = Some(thread_id);
                // A delayed duplicate `thread/started` notification must not
                // regress an already active turn back to the ready state.
                if matches!(
                    self.status,
                    AgentSessionStatus::Starting | AgentSessionStatus::Ready
                ) {
                    self.status = AgentSessionStatus::Ready;
                }
            }
            AgentSessionEvent::ThreadStatusChanged { status } => {
                self.thread_status = Some(status);
                self.apply_thread_status(status);
            }
            AgentSessionEvent::TurnStarted { turn_id } => {
                if !valid_identifier(&turn_id) {
                    self.error = Some("protocol_error:turn_identifier".to_owned());
                    self.status = AgentSessionStatus::Failed;
                    self.refresh_fallback_row_states();
                    return;
                }
                self.turn_id = Some(turn_id);
                // The previous idle status is stale as soon as a new turn is
                // accepted; the next thread/status notification will replace it.
                self.thread_status = None;
                self.error = None;
                self.status = AgentSessionStatus::Running;
            }
            AgentSessionEvent::ItemStarted { mut item } => {
                if !valid_identifier(&item.id) {
                    return;
                }
                item.enforce_retained_limit();
                item.status.get_or_insert_with(|| "inProgress".to_owned());
                self.upsert_item(item, false);
                if !self.status.is_terminal() {
                    self.apply_running_fallback();
                }
            }
            AgentSessionEvent::ItemDelta { item_id, delta } => {
                if valid_identifier(&item_id)
                    && let Some(&index) = self.item_indices.get(&item_id)
                {
                    let before = self.items[index].retained_bytes();
                    self.items[index].append_delta(&delta);
                    let after = self.items[index].retained_bytes();
                    self.retained_item_bytes = self
                        .retained_item_bytes
                        .saturating_sub(before)
                        .saturating_add(after);
                    self.refresh_table_row(index);
                    self.enforce_session_item_limits();
                }
            }
            AgentSessionEvent::ItemCompleted { mut item } => {
                if !valid_identifier(&item.id) {
                    return;
                }
                item.enforce_retained_limit();
                item.status.get_or_insert_with(|| "completed".to_owned());
                self.upsert_item(item, true);
            }
            AgentSessionEvent::ApprovalRequested { mut approval } => {
                if !approval.enforce_limits() {
                    self.error = Some("protocol_error:approval_projection".to_owned());
                    self.status = AgentSessionStatus::Failed;
                    self.refresh_fallback_row_states();
                    return;
                }
                if let Some(index) = self
                    .approvals
                    .iter()
                    .position(|pending| pending.request_key == approval.request_key)
                {
                    self.approvals[index] = approval;
                } else {
                    if self.approvals.len() == MAX_PENDING_APPROVALS {
                        self.error = Some("resource_limit:pending_approvals".to_owned());
                    } else {
                        self.approvals.push(approval);
                    }
                }
                self.status = AgentSessionStatus::AwaitingApproval;
            }
            AgentSessionEvent::ApprovalResolved { request_key } => {
                if valid_identifier(&request_key) {
                    self.approvals
                        .retain(|approval| approval.request_key != request_key);
                }
                if self.approvals.is_empty() && !self.status.is_terminal() {
                    self.apply_running_fallback();
                }
            }
            AgentSessionEvent::TurnCompleted { status } => {
                self.approvals.clear();
                self.status = match status.as_str() {
                    "completed" => AgentSessionStatus::Completed,
                    "interrupted" | "cancelled" => AgentSessionStatus::Interrupted,
                    "failed" | "error" => AgentSessionStatus::Failed,
                    _ => AgentSessionStatus::Completed,
                };
            }
            AgentSessionEvent::ControlError { message } => {
                self.error = Some(sanitize_diagnostic(&message, "control_error"));
            }
            AgentSessionEvent::Failed { message } => {
                self.error = Some(sanitize_diagnostic(&message, "agent_session_failed"));
                self.status = AgentSessionStatus::Failed;
            }
            AgentSessionEvent::Stopped => {
                if !self.status.is_terminal() {
                    self.status = AgentSessionStatus::Stopped;
                }
            }
        }
        if self.status != previous_status {
            self.refresh_fallback_row_states();
        }
    }

    /// Replace the structured item projection from a `thread/read` or
    /// `thread/resume` result. The App Server remains the source of truth for
    /// full turn/item history; Deppy only persists enough metadata to ask for
    /// this snapshot again.
    pub fn load_thread_snapshot(&mut self, result: &Value) -> anyhow::Result<()> {
        let thread = result
            .get("thread")
            .filter(|thread| thread.is_object())
            .ok_or_else(|| anyhow::anyhow!("thread 응답에 thread 객체가 없습니다"))?;
        let thread_id = thread
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("thread 응답에 thread.id가 없습니다"))?;
        anyhow::ensure!(
            valid_identifier(thread_id),
            "protocol_error:thread_identifier"
        );
        if let Some(expected) = self.thread_id.as_deref() {
            anyhow::ensure!(
                expected == thread_id,
                "protocol_error:stale_thread_snapshot"
            );
        }

        self.thread_id = Some(thread_id.to_owned());
        if let Some(cwd) = thread.get("cwd").and_then(Value::as_str) {
            self.cwd = Some(bounded_copy(cwd, MAX_PATH_BYTES));
        }
        if let Some(model) = thread.get("model").and_then(Value::as_str) {
            self.model = Some(bounded_copy(model, MAX_MODEL_BYTES));
        }

        self.items.clear();
        self.item_indices.clear();
        self.table_rows.clear();
        self.retained_item_bytes = 0;
        self.retained_table_bytes = 0;
        self.turn_id = None;
        if let Some(turns) = thread.get("turns").and_then(Value::as_array) {
            for turn in turns {
                if let Some(turn_id) = turn.get("id").and_then(Value::as_str)
                    && valid_identifier(turn_id)
                {
                    self.turn_id = Some(turn_id.to_owned());
                }
                for item in turn
                    .get("items")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(item) = AgentItem::from_codex(item) {
                        self.upsert_item(item, false);
                    }
                }
            }
        }

        self.error = None;
        if let Some(status) = thread.get("status").and_then(AgentThreadStatus::from_codex) {
            self.thread_status = Some(status);
            self.apply_thread_status(status);
        } else if matches!(
            self.status,
            AgentSessionStatus::Starting | AgentSessionStatus::Stopped
        ) {
            self.status = AgentSessionStatus::Ready;
        }
        self.refresh_fallback_row_states();
        Ok(())
    }

    /// Compact rows for the agent-result table. Unlike terminal text, all cells
    /// originate in typed App Server items and are safe to present structurally.
    pub fn table_rows(&self) -> &[AgentTableRow] {
        &self.table_rows
    }

    /// Consume the green completed latch after the user opens the session.
    /// The newest authoritative runtime status decides the state underneath it.
    pub fn acknowledge_completion(&mut self) -> bool {
        if self.status != AgentSessionStatus::Completed {
            return false;
        }
        // Drop the presentation latch before applying the raw state; otherwise
        // the ordinary idle transition correctly preserves `Completed` again.
        self.status = AgentSessionStatus::Ready;
        if let Some(status) = self.thread_status {
            self.apply_thread_status(status);
        }
        true
    }

    fn upsert_item(&mut self, item: AgentItem, preserve_live_text: bool) {
        if let Some(&index) = self.item_indices.get(&item.id) {
            let mut item = item;
            let old_bytes = self.items[index].retained_bytes();
            // `item/completed` is authoritative, but older server versions can
            // omit a field that was delivered via deltas. Keep that text rather
            // than showing a blank completed row.
            if preserve_live_text && item.summary.is_empty() {
                item.summary = std::mem::take(&mut self.items[index].summary);
            }
            if preserve_live_text && item.output.is_empty() {
                item.output = std::mem::take(&mut self.items[index].output);
            }
            item.enforce_retained_limit();
            let new_bytes = item.retained_bytes();
            self.items[index] = item;
            self.retained_item_bytes = self
                .retained_item_bytes
                .saturating_sub(old_bytes)
                .saturating_add(new_bytes);
            self.refresh_table_row(index);
            self.enforce_session_item_limits();
            return;
        }
        let index = self.items.len();
        self.item_indices.insert(item.id.clone(), index);
        self.retained_item_bytes = self
            .retained_item_bytes
            .saturating_add(item.retained_bytes());
        let row = project_table_row(&item, self.status);
        self.retained_table_bytes = self
            .retained_table_bytes
            .saturating_add(table_row_retained_bytes(&row));
        self.table_rows.push(row);
        self.items.push(item);
        self.enforce_session_item_limits();
    }

    fn refresh_table_row(&mut self, index: usize) {
        if let Some(item) = self.items.get(index) {
            let old_bytes = table_row_retained_bytes(&self.table_rows[index]);
            let row = project_table_row(item, self.status);
            let new_bytes = table_row_retained_bytes(&row);
            self.table_rows[index] = row;
            self.retained_table_bytes = self
                .retained_table_bytes
                .saturating_sub(old_bytes)
                .saturating_add(new_bytes);
        }
    }

    fn refresh_fallback_row_states(&mut self) {
        let mut retained_table_bytes = self.retained_table_bytes;
        for (item, row) in self.items.iter().zip(&mut self.table_rows) {
            if item.status.is_none() {
                let old_bytes = row.state.len();
                row.state = self.status.label().to_owned();
                retained_table_bytes = retained_table_bytes
                    .saturating_sub(old_bytes)
                    .saturating_add(row.state.len());
            }
        }
        self.retained_table_bytes = retained_table_bytes;
    }

    fn enforce_session_item_limits(&mut self) {
        let mut remove = self.items.len().saturating_sub(MAX_SESSION_ITEMS);
        let mut retained = self
            .retained_item_bytes
            .saturating_add(self.retained_table_bytes);
        for index in 0..remove {
            retained = retained
                .saturating_sub(self.items[index].retained_bytes())
                .saturating_sub(table_row_retained_bytes(&self.table_rows[index]));
        }
        while retained > MAX_SESSION_RETAINED_BYTES && remove < self.items.len() {
            retained = retained
                .saturating_sub(self.items[remove].retained_bytes())
                .saturating_sub(table_row_retained_bytes(&self.table_rows[remove]));
            remove += 1;
        }
        if remove == 0 {
            return;
        }

        let removed_bytes = self.items[..remove].iter().fold(0usize, |bytes, item| {
            bytes.saturating_add(item.retained_bytes())
        });
        let removed_table_bytes = table_rows_retained_bytes(&self.table_rows[..remove]);
        self.items.drain(..remove);
        self.table_rows.drain(..remove);
        self.retained_item_bytes = self.retained_item_bytes.saturating_sub(removed_bytes);
        self.retained_table_bytes = self
            .retained_table_bytes
            .saturating_sub(removed_table_bytes);
        self.rebuild_item_indices();
    }

    fn rebuild_item_indices(&mut self) {
        self.item_indices.clear();
        self.item_indices.reserve(self.items.len());
        for (index, item) in self.items.iter().enumerate() {
            self.item_indices.insert(item.id.clone(), index);
        }
    }

    fn enforce_metadata_limits(&mut self) {
        self.prompt = bounded_owned(std::mem::take(&mut self.prompt), MAX_PROMPT_BYTES);
        self.cwd = self
            .cwd
            .take()
            .map(|cwd| bounded_owned(cwd, MAX_PATH_BYTES));
        self.model = self
            .model
            .take()
            .map(|model| bounded_owned(model, MAX_MODEL_BYTES));
        self.effort = self
            .effort
            .take()
            .map(|effort| bounded_owned(effort, MAX_STATUS_BYTES));
        self.skills.truncate(MAX_SESSION_SKILLS);
        for skill in &mut self.skills {
            skill.name = bounded_owned(std::mem::take(&mut skill.name), MAX_IDENTIFIER_BYTES);
            skill.path = bounded_owned(std::mem::take(&mut skill.path), MAX_PATH_BYTES);
        }
    }

    fn apply_thread_status(&mut self, status: AgentThreadStatus) {
        match status {
            AgentThreadStatus::NotLoaded => {
                // A completed/error result stays visible until the user sees it.
                // A later active status (the next turn) clears that latch.
                if !self.status.is_terminal() {
                    self.status = AgentSessionStatus::Stopped;
                }
            }
            AgentThreadStatus::Idle => {
                if !matches!(
                    self.status,
                    AgentSessionStatus::Completed
                        | AgentSessionStatus::Interrupted
                        | AgentSessionStatus::Failed
                ) {
                    self.status = AgentSessionStatus::Ready;
                }
            }
            AgentThreadStatus::Active {
                waiting_on_approval,
                waiting_on_user_input,
            } => {
                self.error = None;
                self.status = if waiting_on_approval || waiting_on_user_input {
                    AgentSessionStatus::AwaitingApproval
                } else {
                    AgentSessionStatus::Running
                };
            }
            AgentThreadStatus::SystemError => {
                self.error
                    .get_or_insert_with(|| "Codex App Server system error".to_owned());
                self.status = AgentSessionStatus::Failed;
            }
        }
    }

    fn apply_running_fallback(&mut self) {
        self.status = match self.thread_status {
            Some(
                AgentThreadStatus::Active {
                    waiting_on_approval: true,
                    ..
                }
                | AgentThreadStatus::Active {
                    waiting_on_user_input: true,
                    ..
                },
            ) => AgentSessionStatus::AwaitingApproval,
            _ => AgentSessionStatus::Running,
        };
    }
}

/// One table row rendered by the native workspace agent panel.
#[derive(PartialEq, Eq)]
pub struct AgentTableRow {
    pub item_id: String,
    pub state: String,
    pub kind: String,
    pub subject: String,
    pub location: String,
    pub outcome: String,
}

impl fmt::Debug for AgentTableRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentTableRow")
            .field("item_id_bytes", &self.item_id.len())
            .field("state_bytes", &self.state.len())
            .field("kind", &self.kind)
            .field("subject_bytes", &self.subject.len())
            .field("location_present", &(self.location != "—"))
            .field("outcome_present", &(self.outcome != "—"))
            .finish()
    }
}

const MAX_IDENTIFIER_BYTES: usize = 1024;
const MAX_STATUS_BYTES: usize = 128;
const MAX_MODEL_BYTES: usize = 1024;
const MAX_PATH_BYTES: usize = 4 * 1024;
const MAX_PROMPT_BYTES: usize = 256 * 1024;
const MAX_ITEM_TEXT_BYTES: usize = 48 * 1024;
const MAX_ITEM_RETAINED_BYTES: usize = 256 * 1024;
const MAX_FILE_CHANGES_PER_ITEM: usize = 256;
const MAX_NESTED_PROJECTION_ITEMS: usize = 4096;
const MAX_PROJECTION_DEPTH: usize = 64;
const MAX_SESSION_ITEMS: usize = 4096;
const MAX_SESSION_SKILLS: usize = 64;
const MAX_PENDING_APPROVALS: usize = 32;
const MAX_SESSION_RETAINED_BYTES: usize = 8 * 1024 * 1024;
const TRUNCATION_MARKER: &str = "… [truncated]";

fn valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES && !value.as_bytes().contains(&0)
}

fn append_limited(target: &mut String, delta: &str) {
    if target.len() >= MAX_ITEM_TEXT_BYTES {
        return;
    }
    let remaining = MAX_ITEM_TEXT_BYTES - target.len();
    if delta.len() <= remaining {
        target.push_str(delta);
        return;
    }
    append_truncated(target, delta, remaining);
}

fn limit_text(text: String) -> String {
    bounded_owned(text, MAX_ITEM_TEXT_BYTES)
}

fn bounded_owned(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        text
    } else {
        bounded_copy(&text, max_bytes)
    }
}

fn bounded_copy(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut bounded = String::with_capacity(max_bytes);
    append_truncated(&mut bounded, text, max_bytes);
    bounded
}

fn append_truncated(target: &mut String, text: &str, available: usize) {
    if available == 0 {
        return;
    }
    let marker_bytes = TRUNCATION_MARKER.len().min(available);
    let prefix_budget = available.saturating_sub(marker_bytes);
    let prefix_end = floor_char_boundary(text, prefix_budget);
    target.push_str(&text[..prefix_end]);
    if marker_bytes == TRUNCATION_MARKER.len() {
        target.push_str(TRUNCATION_MARKER);
    } else {
        let marker_end = floor_char_boundary(TRUNCATION_MARKER, marker_bytes);
        target.push_str(&TRUNCATION_MARKER[..marker_end]);
    }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn text_from(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let mut text = String::new();
    let mut items = 0;
    append_value_text(&mut text, value, 0, &mut items);
    text
}

fn append_value_text(target: &mut String, value: &Value, depth: usize, items: &mut usize) {
    if target.len() >= MAX_ITEM_TEXT_BYTES {
        return;
    }
    if depth >= MAX_PROJECTION_DEPTH || *items >= MAX_NESTED_PROJECTION_ITEMS {
        append_limited(target, "[projection omitted]");
        return;
    }
    *items = items.saturating_add(1);
    match value {
        Value::Null => {}
        Value::Bool(value) => append_limited(target, if *value { "true" } else { "false" }),
        Value::Number(value) => append_limited(target, &value.to_string()),
        Value::String(value) => append_limited(target, value),
        Value::Array(values) => {
            for value in values.iter().take(MAX_NESTED_PROJECTION_ITEMS) {
                let value = value.get("text").unwrap_or(value);
                let before = target.len();
                append_value_text(target, value, depth + 1, items);
                if target.len() > before && target.len() < MAX_ITEM_TEXT_BYTES {
                    append_limited(target, "\n");
                }
                if *items >= MAX_NESTED_PROJECTION_ITEMS {
                    break;
                }
            }
            if target.ends_with('\n') {
                target.pop();
            }
        }
        Value::Object(values) => {
            append_limited(target, "{");
            for (index, (key, value)) in values.iter().enumerate() {
                if index != 0 {
                    append_limited(target, ", ");
                }
                append_limited(target, key);
                append_limited(target, ": ");
                append_value_text(target, value, depth + 1, items);
                if *items >= MAX_NESTED_PROJECTION_ITEMS {
                    break;
                }
            }
            append_limited(target, "}");
        }
    }
}

fn text_from_optional(value: Option<&Value>) -> Option<String> {
    let text = text_from(value);
    (!text.is_empty()).then_some(text)
}

fn command_detail(value: &Value) -> Option<String> {
    let code = value.get("exitCode").and_then(Value::as_i64);
    let duration = value.get("durationMs").and_then(Value::as_u64);
    match (code, duration) {
        (Some(code), Some(duration)) => Some(format!("exit {code} · {duration} ms")),
        (Some(code), None) => Some(format!("exit {code}")),
        (None, Some(duration)) => Some(format!("{duration} ms")),
        (None, None) => None,
    }
}

fn file_changes(value: &Value) -> Vec<AgentFileChange> {
    let mut files = Vec::new();
    let mut retained = 0usize;
    for change in value
        .get("changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_FILE_CHANGES_PER_ITEM)
    {
        let Some(path) = change.get("path").and_then(Value::as_str) else {
            continue;
        };
        let path = bounded_copy(path, MAX_PATH_BYTES);
        let kind = bounded_copy(
            change
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("changed"),
            MAX_STATUS_BYTES,
        );
        let fixed = path.len().saturating_add(kind.len());
        if retained.saturating_add(fixed) > MAX_ITEM_RETAINED_BYTES {
            break;
        }
        retained = retained.saturating_add(fixed);
        let diff = change.get("diff").and_then(Value::as_str).map(|diff| {
            let available = MAX_ITEM_RETAINED_BYTES.saturating_sub(retained);
            let diff = bounded_copy(diff, available.min(MAX_ITEM_TEXT_BYTES));
            retained = retained.saturating_add(diff.len());
            diff
        });
        files.push(AgentFileChange { path, kind, diff });
        if retained == MAX_ITEM_RETAINED_BYTES {
            break;
        }
    }
    files
}

fn file_location(files: &[AgentFileChange]) -> Option<String> {
    (!files.is_empty()).then(|| {
        let mut location = String::new();
        for (index, change) in files.iter().take(3).enumerate() {
            if index != 0 {
                location.push_str(", ");
            }
            location.push_str(&change.path);
        }
        if files.len() > 3 {
            location.push_str(", …");
        }
        location
    })
}

fn file_kinds(files: &[AgentFileChange]) -> Option<String> {
    (!files.is_empty()).then(|| {
        let mut kinds = String::new();
        for (index, change) in files.iter().take(3).enumerate() {
            if index != 0 {
                kinds.push_str(" · ");
            }
            kinds.push_str(&change.kind);
            kinds.push(' ');
            kinds.push_str(&change.path);
        }
        kinds
    })
}

fn one_line(text: &str, max_chars: usize) -> String {
    let mut condensed = String::with_capacity(text.len().min(max_chars.saturating_mul(4)));
    let mut pending_space = false;
    let mut chars = 0;
    let mut truncated = false;
    for character in text.chars() {
        if character.is_whitespace() {
            pending_space = !condensed.is_empty();
            continue;
        }
        if pending_space {
            if chars == max_chars {
                truncated = true;
                break;
            }
            condensed.push(' ');
            chars += 1;
            pending_space = false;
        }
        if chars == max_chars {
            truncated = true;
            break;
        }
        condensed.push(character);
        chars += 1;
    }
    if truncated {
        condensed.push('…');
    }
    condensed
}

fn item_outcome(item: &AgentItem) -> String {
    match (
        item.detail.as_deref().filter(|detail| !detail.is_empty()),
        (!item.output.is_empty()).then_some(item.output.as_str()),
    ) {
        (Some(detail), Some(output)) => {
            format!("{} · {}", one_line(detail, 80), one_line(output, 100))
        }
        (Some(detail), None) => one_line(detail, 80),
        (None, Some(output)) => one_line(output, 100),
        (None, None) => "—".to_owned(),
    }
}

fn project_table_row(item: &AgentItem, session_status: AgentSessionStatus) -> AgentTableRow {
    AgentTableRow {
        item_id: item.id.clone(),
        state: item
            .status
            .clone()
            .unwrap_or_else(|| session_status.label().to_owned()),
        kind: item.kind.label().to_owned(),
        subject: one_line(&item.summary, 180),
        location: item
            .location
            .as_deref()
            .map(|value| one_line(value, 96))
            .unwrap_or_else(|| "—".to_owned()),
        outcome: item_outcome(item),
    }
}

fn table_row_retained_bytes(row: &AgentTableRow) -> usize {
    row.item_id
        .len()
        .saturating_add(row.state.len())
        .saturating_add(row.kind.len())
        .saturating_add(row.subject.len())
        .saturating_add(row.location.len())
        .saturating_add(row.outcome.len())
}

fn table_rows_retained_bytes(rows: &[AgentTableRow]) -> usize {
    rows.iter().fold(0usize, |bytes, row| {
        bytes.saturating_add(table_row_retained_bytes(row))
    })
}

fn sanitize_diagnostic(message: &str, fallback: &'static str) -> String {
    if message.contains("stale expectedTurnId") {
        return "control_error:stale_turn".to_owned();
    }
    if matches!(
        message,
        "protocol_error:invalid_json"
            | "protocol_error:rpc_error"
            | "protocol_error:thread_resume_response"
            | "protocol_error:unknown_message"
            | "protocol_error:unsupported_request"
            | "resource_backpressure:command_queue_full"
            | "resource_backpressure:event_queue_full"
            | "resource_backpressure:identifier_bytes"
            | "resource_backpressure:request_limit"
            | "resource_backpressure:session_limit"
            | "resource_limit:command_items"
            | "resource_limit:command_too_large"
            | "resource_limit:identifier"
            | "resource_limit:item_projection"
            | "resource_limit:model_catalog_items"
            | "resource_limit:model_effort_items"
            | "resource_limit:skill_catalog_items"
            | "resource_limit:skill_groups"
            | "resource_limit:thread_identifier"
            | "resource_limit:thread_list_items"
            | "resource_limit:thread_result_items"
            | "resource_limit:turn_identifier"
    ) {
        return message.to_owned();
    }
    fallback.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn command_event_is_a_structured_row() {
        let item = AgentItem::from_codex(&json!({
            "id": "item-1",
            "type": "commandExecution",
            "command": "cargo test -p deppy-sijo",
            "cwd": "/repo",
            "status": "completed",
            "aggregatedOutput": "12 passed",
            "exitCode": 0,
            "durationMs": 431
        }))
        .unwrap();
        let mut session = AgentSession::new("local-1".to_owned(), "test".to_owned(), None);
        session.apply(AgentSessionEvent::ItemCompleted { item });

        assert_eq!(session.table_rows()[0].kind, "command");
        assert_eq!(session.table_rows()[0].location, "/repo");
        assert!(session.table_rows()[0].outcome.contains("exit 0"));
    }

    #[test]
    fn agent_deltas_are_retained_when_completed_item_is_sparse() {
        let mut session = AgentSession::new("local-1".to_owned(), "hello".to_owned(), None);
        session.apply(AgentSessionEvent::ItemStarted {
            item: AgentItem::from_codex(&json!({
                "id": "message-1",
                "type": "agentMessage",
                "text": ""
            }))
            .unwrap(),
        });
        session.apply(AgentSessionEvent::ItemDelta {
            item_id: "message-1".to_owned(),
            delta: "Hello, ".to_owned(),
        });
        session.apply(AgentSessionEvent::ItemDelta {
            item_id: "message-1".to_owned(),
            delta: "world".to_owned(),
        });
        session.apply(AgentSessionEvent::ItemCompleted {
            item: AgentItem::from_codex(&json!({
                "id": "message-1",
                "type": "agentMessage"
            }))
            .unwrap(),
        });

        assert_eq!(session.items[0].summary, "Hello, world");
        assert_eq!(session.items[0].status.as_deref(), Some("completed"));
    }

    #[test]
    fn approval_blocks_then_returns_session_to_running() {
        let mut session = AgentSession::new("local-1".to_owned(), "hello".to_owned(), None);
        session.apply(AgentSessionEvent::TurnStarted {
            turn_id: "turn-1".to_owned(),
        });
        session.apply(AgentSessionEvent::ApprovalRequested {
            approval: AgentApproval {
                request_key: "request-1".to_owned(),
                kind: AgentApprovalKind::CommandExecution,
                thread_id: "thread-1".to_owned(),
                turn_id: "turn-1".to_owned(),
                item_id: "item-1".to_owned(),
                reason: None,
                command: Some("rm -rf build".to_owned()),
                cwd: Some("/repo".to_owned()),
            },
        });
        assert_eq!(session.status, AgentSessionStatus::AwaitingApproval);
        session.apply(AgentSessionEvent::ApprovalResolved {
            request_key: "request-1".to_owned(),
        });

        assert!(session.approvals.is_empty());
        assert_eq!(session.status, AgentSessionStatus::Running);
    }

    #[test]
    fn delayed_thread_notification_does_not_regress_an_active_turn() {
        let mut session = AgentSession::new("local-1".to_owned(), "hello".to_owned(), None);
        session.apply(AgentSessionEvent::TurnStarted {
            turn_id: "turn-1".to_owned(),
        });
        session.apply(AgentSessionEvent::ThreadStarted {
            thread_id: "thread-1".to_owned(),
        });

        assert_eq!(session.status, AgentSessionStatus::Running);
        assert_eq!(session.thread_id.as_deref(), Some("thread-1"));
    }

    #[test]
    fn authoritative_thread_status_maps_all_stable_wire_variants() {
        let cases = [
            (json!({"type": "notLoaded"}), AgentThreadStatus::NotLoaded),
            (json!({"type": "idle"}), AgentThreadStatus::Idle),
            (
                json!({"type": "active", "activeFlags": []}),
                AgentThreadStatus::Active {
                    waiting_on_approval: false,
                    waiting_on_user_input: false,
                },
            ),
            (
                json!({
                    "type": "active",
                    "activeFlags": ["waitingOnApproval", "waitingOnUserInput"]
                }),
                AgentThreadStatus::Active {
                    waiting_on_approval: true,
                    waiting_on_user_input: true,
                },
            ),
            (
                json!({"type": "systemError"}),
                AgentThreadStatus::SystemError,
            ),
        ];

        for (wire, expected) in cases {
            assert_eq!(AgentThreadStatus::from_codex(&wire), Some(expected));
        }
        assert_eq!(
            AgentThreadStatus::from_codex(&json!({"type": "futureStatus"})),
            None
        );
    }

    #[test]
    fn authoritative_wait_flags_project_to_waiting_and_active() {
        let mut session = AgentSession::new("local-1".to_owned(), "hello".to_owned(), None);
        session.apply(AgentSessionEvent::ThreadStatusChanged {
            status: AgentThreadStatus::Active {
                waiting_on_approval: false,
                waiting_on_user_input: true,
            },
        });
        assert_eq!(session.status, AgentSessionStatus::AwaitingApproval);

        session.apply(AgentSessionEvent::ThreadStatusChanged {
            status: AgentThreadStatus::Active {
                waiting_on_approval: false,
                waiting_on_user_input: false,
            },
        });
        assert_eq!(session.status, AgentSessionStatus::Running);
    }

    #[test]
    fn idle_duplicate_does_not_clear_unacknowledged_completion() {
        let mut session = AgentSession::new("local-1".to_owned(), "hello".to_owned(), None);
        session.apply(AgentSessionEvent::TurnCompleted {
            status: "completed".to_owned(),
        });
        session.apply(AgentSessionEvent::ThreadStatusChanged {
            status: AgentThreadStatus::Idle,
        });
        session.apply(AgentSessionEvent::ThreadStatusChanged {
            status: AgentThreadStatus::Idle,
        });
        assert_eq!(session.status, AgentSessionStatus::Completed);

        assert!(session.acknowledge_completion());
        assert_eq!(session.status, AgentSessionStatus::Ready);

        session.apply(AgentSessionEvent::TurnCompleted {
            status: "completed".to_owned(),
        });

        session.apply(AgentSessionEvent::ThreadStatusChanged {
            status: AgentThreadStatus::Active {
                waiting_on_approval: false,
                waiting_on_user_input: false,
            },
        });
        assert_eq!(session.status, AgentSessionStatus::Running);
    }

    #[test]
    fn heuristic_item_events_do_not_override_authoritative_waiting() {
        let mut session = AgentSession::new("local-1".to_owned(), "hello".to_owned(), None);
        session.apply(AgentSessionEvent::ThreadStatusChanged {
            status: AgentThreadStatus::Active {
                waiting_on_approval: false,
                waiting_on_user_input: true,
            },
        });
        session.apply(AgentSessionEvent::ItemStarted {
            item: AgentItem::from_codex(&json!({
                "id": "message-1",
                "type": "agentMessage",
                "text": "waiting"
            }))
            .unwrap(),
        });

        assert_eq!(session.status, AgentSessionStatus::AwaitingApproval);
    }

    #[test]
    fn system_error_is_authoritative_even_after_completion() {
        let mut session = AgentSession::new("local-1".to_owned(), "hello".to_owned(), None);
        session.apply(AgentSessionEvent::TurnCompleted {
            status: "completed".to_owned(),
        });
        session.apply(AgentSessionEvent::ThreadStatusChanged {
            status: AgentThreadStatus::SystemError,
        });

        assert_eq!(session.status, AgentSessionStatus::Failed);
        assert_eq!(
            session.error.as_deref(),
            Some("Codex App Server system error")
        );
    }

    #[test]
    fn idle_reactivates_a_thread_that_was_previously_not_loaded() {
        let mut session = AgentSession::new("local-1".to_owned(), "hello".to_owned(), None);
        session.apply(AgentSessionEvent::ThreadStatusChanged {
            status: AgentThreadStatus::NotLoaded,
        });
        assert_eq!(session.status, AgentSessionStatus::Stopped);

        session.apply(AgentSessionEvent::ThreadStatusChanged {
            status: AgentThreadStatus::Idle,
        });
        assert_eq!(session.status, AgentSessionStatus::Ready);
    }

    #[test]
    fn thread_snapshot_rebuilds_turn_items_and_authoritative_status() {
        let mut session = AgentSession::new(
            "local-1".to_owned(),
            "restored title".to_owned(),
            Some("/old".to_owned()),
        );
        session.thread_id = Some("thread-1".to_owned());
        session.status = AgentSessionStatus::Stopped;

        session
            .load_thread_snapshot(&json!({
                "thread": {
                    "id": "thread-1",
                    "cwd": "/repo",
                    "model": "gpt-test",
                    "status": {"type": "idle"},
                    "turns": [
                        {
                            "id": "turn-1",
                            "items": [{
                                "id": "user-1",
                                "type": "userMessage",
                                "content": [{"type": "text", "text": "first"}]
                            }]
                        },
                        {
                            "id": "turn-2",
                            "items": [{
                                "id": "agent-1",
                                "type": "agentMessage",
                                "text": "restored answer",
                                "status": "completed"
                            }]
                        }
                    ]
                }
            }))
            .unwrap();

        assert_eq!(session.prompt, "restored title");
        assert_eq!(session.cwd.as_deref(), Some("/repo"));
        assert_eq!(session.model.as_deref(), Some("gpt-test"));
        assert_eq!(session.turn_id.as_deref(), Some("turn-2"));
        assert_eq!(session.items.len(), 2);
        assert_eq!(session.items[1].summary, "restored answer");
        assert_eq!(session.thread_status, Some(AgentThreadStatus::Idle));
        assert_eq!(session.status, AgentSessionStatus::Ready);
    }

    #[test]
    fn late_steer_control_error_does_not_regress_completed_status() {
        let mut session = AgentSession::new("local-1".to_owned(), "done".to_owned(), None);
        session.apply(AgentSessionEvent::TurnCompleted {
            status: "completed".to_owned(),
        });
        session.apply(AgentSessionEvent::ControlError {
            message: "stale expectedTurnId".to_owned(),
        });

        assert_eq!(session.status, AgentSessionStatus::Completed);
        assert_eq!(session.error.as_deref(), Some("control_error:stale_turn"));
    }

    #[test]
    fn identifier_and_utf8_text_limits_accept_exact_and_bound_plus_one() {
        let exact_id = "i".repeat(MAX_IDENTIFIER_BYTES);
        assert!(
            AgentItem::from_codex(&json!({
                "id": exact_id,
                "type": "agentMessage",
                "text": "ok"
            }))
            .is_some()
        );
        assert!(
            AgentItem::from_codex(&json!({
                "id": "i".repeat(MAX_IDENTIFIER_BYTES + 1),
                "type": "agentMessage",
                "text": "ok"
            }))
            .is_none()
        );

        let exact_text = "x".repeat(MAX_ITEM_TEXT_BYTES);
        assert_eq!(bounded_copy(&exact_text, MAX_ITEM_TEXT_BYTES), exact_text);
        let plus_one = bounded_copy(&"x".repeat(MAX_ITEM_TEXT_BYTES + 1), MAX_ITEM_TEXT_BYTES);
        assert!(plus_one.len() <= MAX_ITEM_TEXT_BYTES);
        assert!(plus_one.ends_with(TRUNCATION_MARKER));

        let unicode = bounded_copy(&"가".repeat(MAX_ITEM_TEXT_BYTES), MAX_ITEM_TEXT_BYTES);
        assert!(unicode.len() <= MAX_ITEM_TEXT_BYTES);
        assert!(unicode.is_char_boundary(unicode.len()));
    }

    #[test]
    fn file_and_nested_projections_are_bounded() {
        let item = AgentItem::from_codex(&json!({
            "id": "files",
            "type": "fileChange",
            "changes": (0..=MAX_FILE_CHANGES_PER_ITEM)
                .map(|index| json!({
                    "path": format!("/repo/{index}"),
                    "kind": "update",
                    "diff": "x".repeat(MAX_ITEM_TEXT_BYTES)
                }))
                .collect::<Vec<_>>()
        }))
        .unwrap();
        assert!(item.files.len() <= MAX_FILE_CHANGES_PER_ITEM);
        assert!(item.retained_bytes() <= MAX_ITEM_RETAINED_BYTES);

        let mut nested = json!("leaf");
        for _ in 0..=MAX_PROJECTION_DEPTH {
            nested = json!({"next": nested});
        }
        let projected = text_from(Some(&nested));
        assert!(projected.len() <= MAX_ITEM_TEXT_BYTES);
        assert!(projected.contains("[projection omitted]"));
    }

    #[test]
    fn item_eviction_keeps_indices_and_cached_rows_consistent() {
        let mut session = AgentSession::new("local-1".to_owned(), "bounded".to_owned(), None);
        for index in 0..=MAX_SESSION_ITEMS {
            session.apply(AgentSessionEvent::ItemCompleted {
                item: AgentItem::from_codex(&json!({
                    "id": format!("item-{index}"),
                    "type": "agentMessage",
                    "text": format!("answer-{index}")
                }))
                .unwrap(),
            });
        }

        assert_eq!(session.items.len(), MAX_SESSION_ITEMS);
        assert_eq!(session.table_rows().len(), MAX_SESSION_ITEMS);
        assert!(!session.item_indices.contains_key("item-0"));
        assert_eq!(session.item_indices.get("item-1"), Some(&0));

        session.apply(AgentSessionEvent::ItemDelta {
            item_id: format!("item-{MAX_SESSION_ITEMS}"),
            delta: " updated".to_owned(),
        });
        let index = session.item_indices[&format!("item-{MAX_SESSION_ITEMS}")];
        assert!(session.items[index].summary.ends_with(" updated"));
        assert!(session.table_rows()[index].subject.ends_with(" updated"));
    }

    #[test]
    fn repeated_updates_replace_without_retention_growth() {
        let mut session = AgentSession::new("local-1".to_owned(), "bounded".to_owned(), None);
        for index in 0..1000 {
            session.apply(AgentSessionEvent::ItemCompleted {
                item: AgentItem::from_codex(&json!({
                    "id": "same-item",
                    "type": "agentMessage",
                    "text": format!("answer-{index}")
                }))
                .unwrap(),
            });
            session.apply(AgentSessionEvent::ApprovalRequested {
                approval: AgentApproval {
                    request_key: "same-approval".to_owned(),
                    kind: AgentApprovalKind::CommandExecution,
                    thread_id: "thread-1".to_owned(),
                    turn_id: "turn-1".to_owned(),
                    item_id: "same-item".to_owned(),
                    reason: Some(format!("reason-{index}")),
                    command: Some(format!("command-{index}")),
                    cwd: Some("/repo".to_owned()),
                },
            });
        }

        assert_eq!(session.items.len(), 1);
        assert_eq!(session.item_indices.len(), 1);
        assert_eq!(session.table_rows().len(), 1);
        assert_eq!(session.approvals.len(), 1);
        assert_eq!(session.approvals[0].command.as_deref(), Some("command-999"));
        assert!(
            session.retained_item_bytes + session.retained_table_bytes
                <= MAX_SESSION_RETAINED_BYTES
        );
        assert_eq!(
            session.retained_table_bytes,
            table_rows_retained_bytes(session.table_rows())
        );
    }

    #[test]
    fn session_byte_budget_includes_cached_rows() {
        let mut session = AgentSession::new("local-1".to_owned(), "bounded".to_owned(), None);
        for index in 0..256 {
            session.apply(AgentSessionEvent::ItemCompleted {
                item: AgentItem::from_codex(&json!({
                    "id": format!("large-{index}"),
                    "type": "agentMessage",
                    "text": "x".repeat(MAX_ITEM_TEXT_BYTES)
                }))
                .unwrap(),
            });
        }
        let combined = session
            .retained_item_bytes
            .saturating_add(session.retained_table_bytes);
        assert!(combined <= MAX_SESSION_RETAINED_BYTES);
        assert_eq!(
            session.retained_table_bytes,
            table_rows_retained_bytes(session.table_rows())
        );
        assert!(session.items.len() < 256);
        assert_eq!(session.items.len(), session.item_indices.len());
        assert_eq!(session.items.len(), session.table_rows().len());
    }

    #[test]
    fn stale_snapshot_is_rejected_without_raw_identifiers_or_state_change() {
        let mut session = AgentSession::new("local-1".to_owned(), "bounded".to_owned(), None);
        session.thread_id = Some("expected-secret-id".to_owned());
        session.apply(AgentSessionEvent::ItemCompleted {
            item: AgentItem::from_codex(&json!({
                "id": "existing",
                "type": "agentMessage",
                "text": "keep"
            }))
            .unwrap(),
        });

        let error = session
            .load_thread_snapshot(&json!({
                "thread": {
                    "id": "hostile-secret-id",
                    "turns": []
                }
            }))
            .unwrap_err()
            .to_string();
        assert_eq!(error, "protocol_error:stale_thread_snapshot");
        assert_eq!(session.thread_id.as_deref(), Some("expected-secret-id"));
        assert_eq!(session.items.len(), 1);
    }

    #[test]
    fn debug_and_diagnostics_redact_hostile_content() {
        let secret = "Bearer super-secret-token";
        let approval = AgentApproval {
            request_key: "request-1".to_owned(),
            kind: AgentApprovalKind::CommandExecution,
            thread_id: "thread-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            item_id: "item-1".to_owned(),
            reason: Some(secret.to_owned()),
            command: Some(format!("curl -H '{secret}'")),
            cwd: Some("/secret/workspace".to_owned()),
        };
        let debug = format!("{approval:?}");
        assert!(!debug.contains(secret));
        assert!(!debug.contains("/secret/workspace"));
        assert!(debug.contains("[REDACTED]"));

        let event = AgentSessionEvent::ControlError {
            message: secret.to_owned(),
        };
        assert!(!format!("{event:?}").contains(secret));

        let mut session = AgentSession::new("local-1".to_owned(), secret.to_owned(), None);
        session.apply(event);
        assert_eq!(session.error.as_deref(), Some("control_error"));
        assert!(!format!("{session:?}").contains(secret));
    }

    #[test]
    fn approval_count_is_capped_at_transport_limit() {
        let mut session = AgentSession::new("local-1".to_owned(), "bounded".to_owned(), None);
        for index in 0..=MAX_PENDING_APPROVALS {
            session.apply(AgentSessionEvent::ApprovalRequested {
                approval: AgentApproval {
                    request_key: format!("request-{index}"),
                    kind: AgentApprovalKind::CommandExecution,
                    thread_id: "thread-1".to_owned(),
                    turn_id: "turn-1".to_owned(),
                    item_id: format!("item-{index}"),
                    reason: None,
                    command: None,
                    cwd: None,
                },
            });
        }
        assert_eq!(session.approvals.len(), MAX_PENDING_APPROVALS);
        assert_eq!(
            session.error.as_deref(),
            Some("resource_limit:pending_approvals")
        );
    }

    #[test]
    fn approval_display_fields_accept_exact_and_fail_closed_at_plus_one() {
        let mut exact = AgentSession::new("local-1".to_owned(), "bounded".to_owned(), None);
        exact.apply(AgentSessionEvent::ApprovalRequested {
            approval: AgentApproval {
                request_key: "request-exact".to_owned(),
                kind: AgentApprovalKind::CommandExecution,
                thread_id: "thread-1".to_owned(),
                turn_id: "turn-1".to_owned(),
                item_id: "item-1".to_owned(),
                reason: None,
                command: Some("x".repeat(MAX_ITEM_TEXT_BYTES)),
                cwd: None,
            },
        });
        assert_eq!(exact.approvals.len(), 1);
        assert_eq!(
            exact.approvals[0].command.as_ref().map(String::len),
            Some(MAX_ITEM_TEXT_BYTES)
        );

        let mut oversized = AgentSession::new("local-2".to_owned(), "bounded".to_owned(), None);
        oversized.apply(AgentSessionEvent::ApprovalRequested {
            approval: AgentApproval {
                request_key: "request-oversized".to_owned(),
                kind: AgentApprovalKind::CommandExecution,
                thread_id: "thread-1".to_owned(),
                turn_id: "turn-1".to_owned(),
                item_id: "item-1".to_owned(),
                reason: None,
                command: Some("x".repeat(MAX_ITEM_TEXT_BYTES + 1)),
                cwd: None,
            },
        });
        assert!(oversized.approvals.is_empty());
        assert_eq!(oversized.status, AgentSessionStatus::Failed);
        assert_eq!(
            oversized.error.as_deref(),
            Some("protocol_error:approval_projection")
        );
    }

    #[test]
    #[should_panic(expected = "AgentSessionId must be a non-empty identifier")]
    fn oversized_session_identity_is_rejected_instead_of_truncated() {
        let _ = AgentSession::new(
            "i".repeat(MAX_IDENTIFIER_BYTES + 1),
            "bounded".to_owned(),
            None,
        );
    }

    #[test]
    fn cached_table_rows_are_stable_between_render_reads() {
        let mut session = AgentSession::new("local-1".to_owned(), "bounded".to_owned(), None);
        session.apply(AgentSessionEvent::ItemCompleted {
            item: AgentItem::from_codex(&json!({
                "id": "item-1",
                "type": "agentMessage",
                "text": "answer"
            }))
            .unwrap(),
        });
        let first = session.table_rows().as_ptr();
        for _ in 0..300 {
            assert_eq!(session.table_rows().as_ptr(), first);
        }
    }

    #[test]
    fn source_laws_prevent_projection_allocation_amplifiers() {
        let source = include_str!("agent_session.rs");
        let collect_join = ["collect::<Vec<_>>()", "\n            ", ".join"].concat();
        assert!(!source.contains(&collect_join));
        assert!(source.contains("pub fn table_rows(&self) -> &[AgentTableRow]"));
        assert!(source.contains("MAX_SESSION_RETAINED_BYTES"));
        assert!(source.contains("MAX_PROJECTION_DEPTH"));

        for declaration in [
            "pub struct AgentFileChange",
            "pub struct AgentItem",
            "pub struct AgentApproval",
            "pub enum AgentSessionEvent",
        ] {
            let line = source
                .lines()
                .position(|line| line.contains(declaration))
                .unwrap();
            let derive = source.lines().nth(line.saturating_sub(1)).unwrap();
            assert!(derive.trim_start().starts_with("#[derive("));
            assert!(
                !derive.contains("Clone"),
                "{declaration} must stay non-Clone"
            );
        }
    }
}
