//! Exact structured provider approvals, independent of plan tasks and PTY heuristics.
use super::*;
use crate::agent_session::{AgentApprovalDecision, AgentApprovalKind};

const MAX_APPROVALS: usize = 32;
const MAX_COMMAND_BYTES: usize = 8 * 1024;

impl App {
    pub(super) fn command_center_project_approvals(
        &self,
        host: &CommandCenterHost,
        workspace: &str,
        budget: &mut usize,
    ) -> Vec<cc::PendingApproval> {
        let mut pending = Vec::new();
        for (session, request) in self
            .agent_sessions_ui
            .command_center_pending_approvals(workspace)
        {
            if pending.len() == MAX_APPROVALS || *budget < 1024 {
                break;
            }
            let Some(live) = host.live.iter().find(|live| live.capture.workspace == workspace
                && matches!(&live.capture.target, FleetTarget::Structured { session_id } if session_id == &session.id)) else { continue; };
            let mut target = host.target(live);
            target.task_id = None;
            let command = request.command.as_deref().unwrap_or(match request.kind {
                AgentApprovalKind::CommandExecution => "명령 실행 요청 · 원문 세션에서 확인",
                AgentApprovalKind::FileChange => "파일 변경 승인 요청",
            });
            let directory = request
                .cwd
                .as_deref()
                .or(session.cwd.as_deref())
                .unwrap_or("폴더 정보 없음");
            let supported = request.command_center_scope.is_some()
                && request.kind == AgentApprovalKind::CommandExecution
                && directory.len() <= 1024
                && command.len() <= MAX_COMMAND_BYTES
                && command.len() + directory.len() + 512 <= *budget
                && request.request_key.len() <= 128
                && (request.kind != AgentApprovalKind::CommandExecution
                    || request.command.is_some());
            let action = if !self.agent_sessions_ui.command_center_approval_connected() {
                no("현재 App Server 연결이 없습니다. 원문 세션을 다시 연결해 주세요.")
            } else if request.kind == AgentApprovalKind::FileChange {
                no("파일 변경 원문은 실제 세션의 승인 화면에서 확인해 주세요.")
            } else if request.command_center_scope.is_none() {
                no("실행 환경·네트워크·입력 승인 범위는 실제 세션의 승인 화면에서 확인해 주세요.")
            } else if !supported {
                no("승인 원문이 길거나 불완전합니다. 실제 세션의 승인 화면에서 확인해 주세요.")
            } else {
                yes()
            };
            let command = if supported {
                command.to_owned()
            } else {
                format!(
                    "{}\n[미리보기만 표시 · 실제 세션에서 원문 확인 필요]",
                    bounded(command, 1024)
                )
            };
            let directory = bounded(directory, 1024);
            *budget = budget.saturating_sub(command.len() + directory.len() + 512);
            pending.push(cc::PendingApproval {
                target,
                session_title: bounded(&live.title, 256),
                approval: cc::Approval {
                    id: ui::agent_sessions::command_center_approval_id(session, request),
                    command,
                    directory,
                    scope: match request.kind {
                        AgentApprovalKind::CommandExecution => "Codex 명령 실행 · 이 컴퓨터",
                        AgentApprovalKind::FileChange => "Codex 파일 변경 · 이 컴퓨터",
                    }
                    .into(),
                    action,
                },
            });
        }
        pending
    }

    pub(super) fn respond_command_center_project_approval(
        &mut self,
        host: &mut CommandCenterHost,
        operation: &str,
        target: &cc::RouteTarget,
        approval_id: &str,
        allow: bool,
    ) -> Result<(), String> {
        if target.task_id.is_some() {
            return Err(
                "작업의 승인 소유권을 확인할 수 없습니다. 연결된 실제 세션의 요청을 사용해 주세요."
                    .into(),
            );
        }
        let live = host
            .route(target)
            .ok_or("승인 세션이 바뀌었습니다. 현재 요청을 다시 확인해 주세요.")?;
        let FleetTarget::Structured { session_id } = &live.capture.target else {
            return Err(
                "일반 터미널의 승인에는 정확한 요청 API가 없습니다. 실제 터미널에서 확인해 주세요."
                    .into(),
            );
        };
        let session_id = session_id.clone();
        let displayed = self
            .command_center_project_approvals(host, &target.workspace_id, &mut (64 * 1024))
            .into_iter()
            .find(|pending| &pending.target == target && pending.approval.id == approval_id)
            .ok_or("승인 요청이 바뀌었습니다. 현재 요청을 다시 확인해 주세요.")?;
        if !displayed.approval.action.enabled {
            return Err(displayed.approval.action.reason);
        }
        let guard = host.remote_permits.get(operation).cloned().map(|permit| {
            crate::codex_app_server::ApprovalResponseGuard::new(move |effect| {
                permit.with_current(effect)
            })
        });
        self.agent_sessions_ui
            .respond_command_center_approval(
                &target.workspace_id,
                &session_id,
                approval_id,
                if allow {
                    AgentApprovalDecision::Accept
                } else {
                    AgentApprovalDecision::Decline
                },
                guard,
                &self.egui_ctx,
            )
            .map_err(|error| error.to_string())?;
        host.publication_dirty = true;
        host.finish(
            operation,
            cc::ReceiptStatus::Acknowledged,
            "표시한 요청의 일회성 승인 응답을 전송 대기열에 접수했습니다. 실제 승인 결과는 세션에서 확인해 주세요.",
        );
        Ok(())
    }
}

/// Stored event order is chronological. Never manufacture timestamps or bind an old event to a new attempt.
pub(super) fn history(project: &Project, tasks: &[cc::Task]) -> Vec<cc::HistoryEntry> {
    history_entries(&project.snapshot.events, tasks)
}
fn history_entries(events: &[stored::StoredEvent], tasks: &[cc::Task]) -> Vec<cc::HistoryEntry> {
    events
        .iter()
        .skip(events.len().saturating_sub(100))
        .map(|event| {
            let target = event
                .task_id
                .as_ref()
                .and_then(|id| {
                    tasks
                        .iter()
                        .find(|task| task.target.task_id.as_ref() == Some(&id.0))
                })
                .map(|task| {
                    let mut target = task.target.clone();
                    if event.attempt_id.as_ref().map(|id| id.0.as_str())
                        != task.attempt_id.as_deref()
                    {
                        target.session_id = None;
                    }
                    target
                });
            cc::HistoryEntry {
                id: event.id.0.clone(),
                kind: event.kind.clone(),
                text: bounded(&event.display_payload, 2048),
                target,
                ..Default::default()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_kind_projection_preserves_explicit_categories_and_legacy_unknowns() {
        let events: Vec<_> = ["work", "approval", "settings", "", "future-category"]
            .into_iter()
            .enumerate()
            .map(|(i, kind)| stored::StoredEvent {
                id: work::EventId(format!("event-{i}")),
                kind: kind.into(),
                task_id: None,
                attempt_id: None,
                display_payload: "승인 설정 작업이라는 단어가 있어도 분류를 추측하지 않습니다."
                    .into(),
            })
            .collect();
        let history = history_entries(&events, &[]);
        assert_eq!(
            history.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            ["work", "approval", "settings", "", "future-category"]
        );
        assert!(
            history
                .iter()
                .all(|e| e.time.is_empty() && e.target.is_none())
        );
        let old_json = serde_json::json!({"id":"old", "target":null, "time":"", "text":"승인 설정 작업", "detail":""});
        let legacy: cc::HistoryEntry = serde_json::from_value(old_json).unwrap();
        assert!(legacy.kind.is_empty());
    }

    #[test]
    fn command_center_history_keeps_latest_chronological_events_and_exact_attempt_routes() {
        let events: Vec<_> = (0..105)
            .map(|i| stored::StoredEvent {
                id: work::EventId(format!("event-{i}")),
                kind: String::new(),
                task_id: Some(work::TaskId("task".into())),
                attempt_id: Some(work::AttemptId(
                    if i == 104 { "current" } else { "old" }.into(),
                )),
                display_payload: format!("event {i}"),
            })
            .collect();
        let tasks = vec![cc::Task {
            target: cc::RouteTarget {
                workspace_id: "workspace-a".into(),
                session_id: Some("exact-session".into()),
                task_id: Some("task".into()),
            },
            attempt_id: Some("current".into()),
            ..Default::default()
        }];
        let history = history_entries(&events, &tasks);
        assert_eq!(history.len(), 100);
        assert_eq!(history[0].id, "event-5");
        assert_eq!(history[99].id, "event-104");
        assert!(history[0].target.as_ref().unwrap().session_id.is_none());
        assert_eq!(history[99].target, tasks.first().map(|t| t.target.clone()));
        assert!(
            history
                .iter()
                .all(|e| e.time.is_empty() && e.detail.is_empty())
        );
    }
}
