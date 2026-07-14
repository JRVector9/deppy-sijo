//! Structured agent-session state shared by App Server providers and native UI.
//!
//! This deliberately sits beside the PTY stack rather than inside it. A terminal
//! stream can contain arbitrary ANSI/full-screen applications, while an app-server
//! thread/turn/item stream is already structured and safe to render as rows.

use std::collections::HashMap;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFileChange {
    pub path: String,
    pub kind: String,
    pub diff: Option<String>,
}

/// Provider-neutral representation of a streamed agent item.
#[derive(Debug, Clone, PartialEq, Eq)]
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

impl AgentItem {
    /// Convert the common Codex `ThreadItem` tagged union into the durable view
    /// model. Unknown future item kinds remain visible rather than causing a
    /// session to fail.
    pub fn from_codex(value: &Value) -> Option<Self> {
        let id = value.get("id")?.as_str()?.to_owned();
        let wire_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let status = value
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned);

        let mut item = Self {
            id,
            kind: AgentItemKind::Other,
            status,
            summary: wire_type.to_owned(),
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
                    .map(str::to_owned);
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
                item.location = value.get("cwd").and_then(Value::as_str).map(str::to_owned);
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
                item.output = item
                    .files
                    .iter()
                    .filter_map(|change| change.diff.as_deref())
                    .collect::<Vec<_>>()
                    .join("\n\n");
            }
            "mcpToolCall" => {
                item.kind = AgentItemKind::McpToolCall;
                let server = value.get("server").and_then(Value::as_str).unwrap_or("MCP");
                let tool = value.get("tool").and_then(Value::as_str).unwrap_or("tool");
                item.summary = format!("{server} · {tool}");
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
        Some(item)
    }

    fn append_delta(&mut self, delta: &str) {
        match self.kind {
            AgentItemKind::CommandExecution => append_limited(&mut self.output, delta),
            AgentItemKind::Reasoning => append_limited(&mut self.output, delta),
            _ => append_limited(&mut self.summary, delta),
        }
    }
}

/// A server request which must be explicitly answered by the user. The raw
/// JSON-RPC ID remains in the transport; the UI only handles the opaque key.
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentApprovalKind {
    CommandExecution,
    FileChange,
}

impl AgentApprovalKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::CommandExecution => "command approval",
            Self::FileChange => "file-change approval",
        }
    }
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

/// Normalized event emitted by a structured agent transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSessionEvent {
    ConnectionReady,
    ThreadStarted { thread_id: String },
    TurnStarted { turn_id: String },
    ItemStarted { item: AgentItem },
    ItemDelta { item_id: String, delta: String },
    ItemCompleted { item: AgentItem },
    ApprovalRequested { approval: AgentApproval },
    ApprovalResolved { request_key: String },
    TurnCompleted { status: String },
    Failed { message: String },
    Stopped,
}

/// The stable UI-facing state of a structured agent conversation.
#[derive(Debug, Clone)]
pub struct AgentSession {
    pub id: AgentSessionId,
    pub prompt: String,
    pub cwd: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub status: AgentSessionStatus,
    pub items: Vec<AgentItem>,
    pub approvals: Vec<AgentApproval>,
    pub error: Option<String>,
    item_indices: HashMap<String, usize>,
}

impl AgentSession {
    pub fn new(id: AgentSessionId, prompt: String, cwd: Option<String>) -> Self {
        Self {
            id,
            prompt,
            cwd,
            thread_id: None,
            turn_id: None,
            status: AgentSessionStatus::Starting,
            items: Vec::new(),
            approvals: Vec::new(),
            error: None,
            item_indices: HashMap::new(),
        }
    }

    pub fn apply(&mut self, event: AgentSessionEvent) {
        match event {
            AgentSessionEvent::ConnectionReady => {
                if self.status == AgentSessionStatus::Starting {
                    self.status = AgentSessionStatus::Ready;
                }
            }
            AgentSessionEvent::ThreadStarted { thread_id } => {
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
            AgentSessionEvent::TurnStarted { turn_id } => {
                self.turn_id = Some(turn_id);
                self.error = None;
                self.status = AgentSessionStatus::Running;
            }
            AgentSessionEvent::ItemStarted { mut item } => {
                item.status.get_or_insert_with(|| "inProgress".to_owned());
                self.upsert_item(item, false);
                if !self.status.is_terminal() {
                    self.status = AgentSessionStatus::Running;
                }
            }
            AgentSessionEvent::ItemDelta { item_id, delta } => {
                if let Some(&index) = self.item_indices.get(&item_id) {
                    self.items[index].append_delta(&delta);
                }
            }
            AgentSessionEvent::ItemCompleted { mut item } => {
                item.status.get_or_insert_with(|| "completed".to_owned());
                self.upsert_item(item, true);
            }
            AgentSessionEvent::ApprovalRequested { approval } => {
                if !self
                    .approvals
                    .iter()
                    .any(|pending| pending.request_key == approval.request_key)
                {
                    self.approvals.push(approval);
                }
                self.status = AgentSessionStatus::AwaitingApproval;
            }
            AgentSessionEvent::ApprovalResolved { request_key } => {
                self.approvals
                    .retain(|approval| approval.request_key != request_key);
                if self.approvals.is_empty() && !self.status.is_terminal() {
                    self.status = AgentSessionStatus::Running;
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
            AgentSessionEvent::Failed { message } => {
                self.error = Some(limit_text(message));
                self.status = AgentSessionStatus::Failed;
            }
            AgentSessionEvent::Stopped => {
                if !self.status.is_terminal() {
                    self.status = AgentSessionStatus::Stopped;
                }
            }
        }
    }

    /// Compact rows for the agent-result table. Unlike terminal text, all cells
    /// originate in typed App Server items and are safe to present structurally.
    pub fn table_rows(&self) -> Vec<AgentTableRow> {
        self.items
            .iter()
            .map(|item| AgentTableRow {
                item_id: item.id.clone(),
                state: item
                    .status
                    .clone()
                    .unwrap_or_else(|| self.status.label().to_owned()),
                kind: item.kind.label().to_owned(),
                subject: one_line(&item.summary, 180),
                location: item
                    .location
                    .as_deref()
                    .map(|value| one_line(value, 96))
                    .unwrap_or_else(|| "—".to_owned()),
                outcome: item_outcome(item),
            })
            .collect()
    }

    fn upsert_item(&mut self, item: AgentItem, preserve_live_text: bool) {
        if let Some(&index) = self.item_indices.get(&item.id) {
            let old = &self.items[index];
            let mut item = item;
            // `item/completed` is authoritative, but older server versions can
            // omit a field that was delivered via deltas. Keep that text rather
            // than showing a blank completed row.
            if preserve_live_text && item.summary.is_empty() {
                item.summary = old.summary.clone();
            }
            if preserve_live_text && item.output.is_empty() {
                item.output = old.output.clone();
            }
            self.items[index] = item;
            return;
        }
        let index = self.items.len();
        self.item_indices.insert(item.id.clone(), index);
        self.items.push(item);
    }
}

/// One table row rendered by the native workspace agent panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTableRow {
    pub item_id: String,
    pub state: String,
    pub kind: String,
    pub subject: String,
    pub location: String,
    pub outcome: String,
}

const MAX_ITEM_TEXT_CHARS: usize = 48 * 1024;

fn append_limited(target: &mut String, delta: &str) {
    if target.chars().count() >= MAX_ITEM_TEXT_CHARS {
        return;
    }
    target.push_str(delta);
    *target = limit_text(std::mem::take(target));
}

fn limit_text(text: String) -> String {
    let mut indices = text.char_indices();
    let Some((cut, _)) = indices.nth(MAX_ITEM_TEXT_CHARS) else {
        return text;
    };
    format!("{}\n… output truncated", &text[..cut])
}

fn text_from(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    match value {
        Value::String(text) => text.clone(),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                value
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| text_from(Some(value)))
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        value => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn text_from_optional(value: Option<&Value>) -> Option<String> {
    let text = text_from(value);
    (!text.is_empty()).then_some(text)
}

fn command_detail(value: &Value) -> Option<String> {
    let mut details = Vec::new();
    if let Some(code) = value.get("exitCode").and_then(Value::as_i64) {
        details.push(format!("exit {code}"));
    }
    if let Some(duration) = value.get("durationMs").and_then(Value::as_u64) {
        details.push(format!("{duration} ms"));
    }
    (!details.is_empty()).then(|| details.join(" · "))
}

fn file_changes(value: &Value) -> Vec<AgentFileChange> {
    value
        .get("changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|change| {
            Some(AgentFileChange {
                path: change.get("path")?.as_str()?.to_owned(),
                kind: change
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("changed")
                    .to_owned(),
                diff: change
                    .get("diff")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

fn file_location(files: &[AgentFileChange]) -> Option<String> {
    (!files.is_empty()).then(|| {
        let mut paths = files
            .iter()
            .take(3)
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>();
        if files.len() > paths.len() {
            paths.push("…");
        }
        paths.join(", ")
    })
}

fn file_kinds(files: &[AgentFileChange]) -> Option<String> {
    (!files.is_empty()).then(|| {
        files
            .iter()
            .take(3)
            .map(|change| format!("{} {}", change.kind, change.path))
            .collect::<Vec<_>>()
            .join(" · ")
    })
}

fn one_line(text: &str, max_chars: usize) -> String {
    let condensed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut indices = condensed.char_indices();
    let Some((cut, _)) = indices.nth(max_chars) else {
        return condensed;
    };
    format!("{}…", &condensed[..cut])
}

fn item_outcome(item: &AgentItem) -> String {
    let mut parts = Vec::new();
    if let Some(detail) = item.detail.as_deref().filter(|detail| !detail.is_empty()) {
        parts.push(one_line(detail, 80));
    }
    if !item.output.is_empty() {
        parts.push(one_line(&item.output, 100));
    }
    if parts.is_empty() {
        "—".to_owned()
    } else {
        parts.join(" · ")
    }
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
}
