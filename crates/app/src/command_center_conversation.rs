//! Actual MAIN records. No terminal/title/summary parsing is a conversation source.
use super::*;
use crate::agent_session::{AgentItemKind, AgentSession};

#[derive(Default)]
pub(super) struct Conversations(BTreeMap<String, cc::MainConversation>);

fn generation(host: &CommandCenterHost, target: &cc::RouteTarget) -> Option<String> {
    let project = host.projects.get(&target.workspace_id)?;
    if target.task_id.is_some() || host.main_target(&target.workspace_id, project) != *target {
        return None;
    }
    let live = host.route(target)?;
    let key = target.session_id.as_deref()?;
    let bound = binding(live, key)?;
    Some(format!(
        "{}:{}:{}:{}",
        key, bound.incarnation.0, bound.session_id.0, bound.execution_generation
    ))
}

impl Conversations {
    fn record(&mut self, target: &cc::RouteTarget, generation: &str, entry: cc::ConversationEntry) {
        if self.0.len() >= 64 && !self.0.contains_key(&target.workspace_id) {
            return;
        }
        let conversation = self.0.entry(target.workspace_id.clone()).or_default();
        if conversation.target != *target || conversation.generation != generation {
            *conversation = cc::MainConversation {
                target: target.clone(), generation: generation.into(),
                status: "실제 전송한 입력·확인된 최종 응답·메인 관리 도구 기록입니다. 실시간 터미널 출력은 원래 세션에서 확인해 주세요.".into(),
                ..Default::default()
            };
        }
        if let Some(old) = conversation
            .entries
            .iter_mut()
            .find(|old| old.id == entry.id)
        {
            *old = entry;
        } else {
            conversation.entries.push(entry);
        }
        *conversation = conversation.clone().bounded();
    }
}

pub(super) fn record_user(
    host: &mut CommandCenterHost,
    target: &cc::RouteTarget,
    operation: &str,
    text: &str,
) {
    record(
        host,
        target,
        cc::ConversationEntry {
            id: format!("input:{operation}"),
            turn_id: None,
            kind: cc::ConversationEntryKind::User,
            text: text.into(),
            tool_name: None,
            state: None,
            observed_at: None,
        },
    );
}

pub(super) fn record(
    host: &mut CommandCenterHost,
    target: &cc::RouteTarget,
    entry: cc::ConversationEntry,
) {
    if let Some(generation) = generation(host, target) {
        host.conversations.record(target, &generation, entry);
        host.publication_dirty = true;
    }
}

fn structured(
    session: &AgentSession,
    target: &cc::RouteTarget,
    epoch: u64,
    redaction: &secret::RedactionService,
) -> cc::MainConversation {
    let entries = session
        .items
        .iter()
        .rev()
        .filter_map(|item| {
            let kind = match item.kind {
                AgentItemKind::UserMessage => cc::ConversationEntryKind::User,
                AgentItemKind::AgentMessage => cc::ConversationEntryKind::Agent,
                AgentItemKind::CommandExecution
                | AgentItemKind::McpToolCall
                | AgentItemKind::WebSearch
                | AgentItemKind::FileChange => cc::ConversationEntryKind::Tool,
                _ => return None,
            };
            let text = if item.output.is_empty() {
                bounded(&item.summary, 8192)
            } else if kind == cc::ConversationEntryKind::Tool {
                format!(
                    "{}\n{}",
                    bounded(&item.summary, 2048),
                    bounded(&item.output, 6144)
                )
            } else {
                bounded(&item.output, 8192)
            };
            Some(cc::ConversationEntry {
                id: item.id.clone(),
                turn_id: None,
                kind,
                text: redaction.redact_wrapped_text(&text),
                tool_name: (kind == cc::ConversationEntryKind::Tool)
                    .then(|| redaction.redact_wrapped_text(&bounded(&item.summary, 256))),
                state: if kind == cc::ConversationEntryKind::Tool {
                    Some(match item.status.as_deref() {
                        Some("inProgress" | "running") => cc::ConversationActivityState::Running,
                        Some("completed" | "success") => cc::ConversationActivityState::Completed,
                        Some("failed" | "error") => cc::ConversationActivityState::Failed,
                        Some("cancelled" | "canceled") => cc::ConversationActivityState::Cancelled,
                        _ => cc::ConversationActivityState::Unknown,
                    })
                } else {
                    None
                },
                observed_at: None,
            })
        })
        .take(100)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    cc::MainConversation {
        target: target.clone(),
        generation: format!(
            "structured:{epoch}:{}",
            session.thread_id.as_deref().unwrap_or_default()
        ),
        entries,
        truncated: session.items.len() > 100,
        status: "실제 App Server 대화와 도구 기록입니다.".into(),
    }
    .bounded()
}

impl App {
    pub(super) fn command_center_main_conversation(
        &self,
        host: &CommandCenterHost,
        target: &cc::RouteTarget,
    ) -> Option<cc::MainConversation> {
        let project = host.projects.get(&target.workspace_id)?;
        if host.main_target(&target.workspace_id, project) != *target || target.task_id.is_some() {
            return None;
        }
        let live = host.route(target)?;
        if let FleetTarget::Structured { session_id } = &live.capture.target {
            let (source, epoch) = self
                .agent_sessions_ui
                .command_center_session(&target.workspace_id, session_id)?;
            return Some(structured(
                source,
                target,
                epoch,
                &self.command_center_redaction,
            ));
        }
        let generation = generation(host, target)?;
        let mut conversation = host.conversations.0.get(&target.workspace_id).filter(|c| c.target == *target && c.generation == generation).cloned()
            .unwrap_or_else(|| cc::MainConversation { target: target.clone(), generation, status: "현재 실행에서 확인된 입력·응답을 기다립니다. 이전 대화와 실시간 출력은 원래 세션에서 확인해 주세요.".into(), ..Default::default() });
        for entry in &mut conversation.entries {
            entry.text = self
                .command_center_redaction
                .redact_wrapped_text(&entry.text);
        }
        Some(conversation.bounded())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> cc::RouteTarget {
        cc::RouteTarget {
            workspace_id: "w".into(),
            session_id: Some("main".into()),
            task_id: None,
        }
    }
    fn entry(id: &str, text: &str) -> cc::ConversationEntry {
        cc::ConversationEntry {
            id: id.into(),
            turn_id: None,
            kind: cc::ConversationEntryKind::Agent,
            text: text.into(),
            tool_name: None,
            state: None,
            observed_at: None,
        }
    }
    #[test]
    fn command_center_main_conversation_replacement_and_duplicate_are_exact() {
        let mut records = Conversations::default();
        let target = target();
        records.record(&target, "generation-a", entry("same", "first"));
        records.record(&target, "generation-a", entry("same", "completed"));
        assert_eq!(records.0["w"].entries.len(), 1);
        assert_eq!(records.0["w"].entries[0].text, "completed");
        records.record(&target, "generation-b", entry("new", "fresh"));
        assert_eq!(records.0["w"].entries.len(), 1);
        assert_eq!(records.0["w"].entries[0].id, "new");
    }
    #[test]
    fn command_center_main_conversation_structured_hides_reasoning_and_uses_actual_items() {
        use crate::agent_session::AgentSessionEvent;
        let mut session = AgentSession::new("s".into(), "original".into(), None);
        for value in [
            serde_json::json!({"id":"user","type":"userMessage","content":[{"type":"inputText","text":"actual question"}]}),
            serde_json::json!({"id":"private","type":"reasoning","summary":[{"text":"internal"}]}),
            serde_json::json!({"id":"answer","type":"agentMessage","text":"actual answer"}),
            serde_json::json!({"id":"tool","type":"commandExecution","command":"cargo test","aggregatedOutput":"passed","status":"completed"}),
        ] {
            session.apply(AgentSessionEvent::ItemCompleted {
                item: crate::agent_session::AgentItem::from_codex(&value).unwrap(),
            });
        }
        let actual = structured(&session, &target(), 7, &secret::RedactionService::new());
        assert_eq!(
            actual
                .entries
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["user", "answer", "tool"]
        );
        assert!(
            actual
                .entries
                .iter()
                .all(|e| e.turn_id.is_none() && e.observed_at.is_none())
        );
        assert_eq!(actual.entries[1].text, "actual answer");
        assert_eq!(
            actual.entries[2].state,
            Some(cc::ConversationActivityState::Completed)
        );
    }
}
