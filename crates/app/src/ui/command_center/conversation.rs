//! Bounded, exact-main conversation projection. Source collection belongs to the host.

use super::{Deserialize, Project, RouteTarget, Serialize};

pub const MAX_CONVERSATION_ENTRIES: usize = 100;
pub const MAX_CONVERSATION_TEXT_BYTES: usize = 8 * 1024;
pub const MAX_CONVERSATION_TOTAL_BYTES: usize = 128 * 1024;
const MAX_ID_BYTES: usize = 1024;
const MAX_GENERATION_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationEntryKind {
    User,
    Agent,
    Tool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationActivityState {
    Running,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}

impl ConversationActivityState {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Running => "실행 중",
            Self::Completed => "완료",
            Self::Failed => "실패",
            Self::Cancelled => "취소됨",
            Self::Unknown => "관측 불명",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationEntry {
    pub id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    pub kind: ConversationEntryKind,
    pub text: String,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub state: Option<ConversationActivityState>,
    /// True source observation time, absent when not known. Never use the frame clock.
    #[serde(default)]
    pub observed_at: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MainConversation {
    pub target: RouteTarget,
    /// The bound runtime incarnation and execution generation, supplied by the host.
    pub generation: String,
    #[serde(default)]
    pub entries: Vec<ConversationEntry>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub status: String,
}

fn prefix(value: &str, limit: usize) -> String {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

impl MainConversation {
    pub fn bounded(self) -> Self {
        self.bounded_copy()
    }

    fn bounded_copy(&self) -> Self {
        let mut budget = MAX_CONVERSATION_TOTAL_BYTES;
        let mut truncated = self.truncated
            || self.entries.len() > MAX_CONVERSATION_ENTRIES
            || self.generation.len() > MAX_GENERATION_BYTES;
        let mut entries = Vec::with_capacity(self.entries.len().min(MAX_CONVERSATION_ENTRIES));
        for entry in self.entries.iter().rev().take(MAX_CONVERSATION_ENTRIES) {
            if entry.id.is_empty()
                || entry.id.len() > MAX_ID_BYTES
                || entry
                    .turn_id
                    .as_ref()
                    .is_some_and(|id| id.len() > MAX_ID_BYTES)
            {
                truncated = true;
                continue;
            }
            if budget == 0 {
                truncated = true;
                break;
            }
            let text = prefix(&entry.text, MAX_CONVERSATION_TEXT_BYTES.min(budget));
            truncated |= text.len() < entry.text.len();
            budget -= text.len();
            entries.push(ConversationEntry {
                id: entry.id.clone(),
                turn_id: entry.turn_id.clone(),
                kind: entry.kind,
                text,
                tool_name: entry.tool_name.as_deref().map(|v| prefix(v, 256)),
                state: entry.state,
                observed_at: entry.observed_at.as_deref().map(|v| prefix(v, 256)),
            });
        }
        entries.reverse();
        Self {
            target: self.target.clone(),
            generation: if self.generation.len() <= MAX_GENERATION_BYTES {
                self.generation.clone()
            } else {
                String::new()
            },
            entries,
            truncated,
            status: prefix(&self.status, 1024),
        }
    }
}

impl Project {
    /// Read no transcript or session catalog: only the host's current exact-main projection.
    pub fn visible_main_conversation(&self) -> Option<MainConversation> {
        let source = self.main_conversation.as_ref()?;
        if self.main_target.workspace_id != self.id
            || self
                .main_target
                .session_id
                .as_deref()
                .is_none_or(str::is_empty)
            || self.main_target.task_id.is_some()
            || source.target != self.main_target
            || source.generation.trim().is_empty()
            || source.generation.len() > MAX_GENERATION_BYTES
        {
            return None;
        }
        Some(source.bounded_copy())
    }
}
