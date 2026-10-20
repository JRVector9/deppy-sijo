//! Explicit local worker release uses the runtime's retained launch owner and opaque execution handle.
use super::*;
use runtime::remote_input::{
    RemoteExecutionProof, RemoteSessionCloseResult, RemoteTaskInputReply, RemoteTaskInputRequest,
    RemoteTaskTarget,
};
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Phase {
    Saved,
    Requested,
    Closed,
    Rejected,
    Unknown,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Record {
    operation: work::OperationId,
    target: cc::RouteTarget,
    attempt: work::AttemptId,
    binding: work::RuntimeBinding,
    owner: RemoteTaskTarget,
    phase: Phase,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Save,
    Observe,
    Observing,
    ClaimSave,
    Send,
    Await,
    Done,
}
struct Pending {
    record: Record,
    capture: Capture,
    runtime_instance: u64,
    selection: work::AgentSelection,
    proof: Option<Box<RemoteExecutionProof>>,
    stage: Stage,
    generation: u64,
    token: u64,
    deadline: std::time::Instant,
    next_observe: std::time::Instant,
}
#[derive(Default)]
pub(super) struct Closes {
    pending: BTreeMap<String, Pending>,
}
struct Owned {
    capture: Capture,
    runtime_instance: u64,
    attempt: work::AttemptId,
    binding: work::RuntimeBinding,
    target: RemoteTaskTarget,
    selection: work::AgentSelection,
}
fn owned(host: &CommandCenterHost, target: &cc::RouteTarget) -> Option<Owned> {
    let route = host.route(target)?;
    let project = host.projects.get(&target.workspace_id)?;
    let task = project
        .snapshot
        .tasks
        .iter()
        .find(|t| Some(&t.id.0) == target.task_id.as_ref() && t.execution_device.is_none())?;
    let attempt = project
        .snapshot
        .attempts
        .iter()
        .find(|a| Some(&a.id) == task.current_attempt_id.as_ref())?;
    let current = binding(route, target.session_id.as_deref()?)?;
    if attempt.runtime_binding.as_ref() != Some(&current) {
        return None;
    }
    let runtime = route.capture.prompt.as_ref()?.runtime_instance;
    let launch = host.launches.values().find(|p| {
        p.record.task_id.as_ref() == Some(&task.id)
            && p.record.attempt_id == attempt.id
            && p.record.workspace.0 == target.workspace_id
            && p.runtime_instance == runtime
    })?;
    let identity = &launch.request.as_ref()?.identity;
    if identity.attempt_id != attempt.id
        || identity.operation_id != attempt.operation_id
        || identity.incarnation != current.incarnation
    {
        return None;
    }
    Some(Owned {
        capture: route.capture.clone(),
        runtime_instance: runtime,
        attempt: attempt.id.clone(),
        binding: current.clone(),
        target: RemoteTaskTarget {
            identity: identity.clone(),
            session: current.session_id,
            execution_generation: current.execution_generation,
        },
        selection: task.assigned_agent.clone(),
    })
}
pub(super) fn has_pending(host: &CommandCenterHost) -> bool {
    host.local_closes
        .pending
        .values()
        .any(|p| p.stage != Stage::Done)
}
pub(super) fn target_pending(host: &CommandCenterHost, target: &cc::RouteTarget) -> bool {
    host.local_closes
        .pending
        .values()
        .any(|p| p.record.target == *target && p.stage != Stage::Done)
}
pub(super) fn project_pending(host: &CommandCenterHost, workspace: &str) -> bool {
    host.local_closes
        .pending
        .values()
        .any(|p| p.record.target.workspace_id == workspace && p.stage != Stage::Done)
}
pub(super) fn capability(host: &CommandCenterHost, target: &cc::RouteTarget) -> cc::Capability {
    if owned(host, target).is_none() {
        return no(
            "이 화면에서 생성한 워커의 정확한 소유권이 없습니다. 기존 터미널의 종료 기능을 사용해 주세요.",
        );
    }
    if host.local_closes.pending.values().any(|p| {
        p.record.target == *target && matches!(p.record.phase, Phase::Unknown | Phase::Closed)
    }) {
        no("이 실행은 이미 종료되었거나 종료 응답이 불명확합니다. 새 종료 요청을 보내지 않습니다.")
    } else if target_pending(host, target) || remote::handoff_pending(host, target) {
        no("현재 실행·입력의 실제 응답을 기다려 주세요.")
    } else {
        yes()
    }
}
fn current(host: &CommandCenterHost, pending: &Pending) -> bool {
    owned(host, &pending.record.target).is_some_and(|owned| {
        owned.capture == pending.capture
            && owned.runtime_instance == pending.runtime_instance
            && owned.attempt == pending.record.attempt
            && owned.binding == pending.record.binding
            && owned.target == pending.record.owner
            && owned.selection == pending.selection
    })
}
fn save(project: &mut Project, record: &Record) -> Result<(), String> {
    set_draft(
        project,
        &format!("local-close:{}", record.operation.0),
        &serde_json::to_string(record).map_err(|e| e.to_string())?,
        matches!(record.phase, Phase::Requested | Phase::Unknown),
    )
}
pub(super) fn begin(
    host: &mut CommandCenterHost,
    id: &str,
    target: &cc::RouteTarget,
    attempt_id: &str,
) -> Result<(), String> {
    if host.remote_permits.contains_key(id) {
        return Err("이 컴퓨터에서 소유한 워커 실행을 종료해 주세요.".into());
    }
    let cap = capability(host, target);
    if !cap.enabled {
        return Err(cap.reason);
    }
    if host.flight.is_some()
        || host
            .local_closes
            .pending
            .values()
            .filter(|p| p.stage != Stage::Done)
            .count()
            >= 32
    {
        return Err("현재 저장·종료 응답을 기다려 주세요.".into());
    }
    let Owned {
        capture,
        runtime_instance,
        attempt,
        binding,
        target: owner,
        selection,
    } = owned(host, target).ok_or("소유한 실행이 바뀌었습니다.")?;
    if attempt.0 != attempt_id {
        return Err("종료하려는 현재 시도가 바뀌었습니다.".into());
    }
    let record = Record {
        operation: work::OperationId::from_string(id.into()).map_err(|e| e.to_string())?,
        target: target.clone(),
        attempt,
        binding,
        owner,
        phase: Phase::Saved,
    };
    let project = host
        .projects
        .get_mut(&target.workspace_id)
        .ok_or("프로젝트가 없습니다.")?;
    if project.storage_blocked || !project.loaded || project.generation != project.saved_generation
    {
        return Err("프로젝트 저장 상태를 먼저 확인해 주세요.".into());
    }
    if project
        .snapshot
        .drafts
        .iter()
        .any(|d| d.target_key == format!("local-close:{id}"))
    {
        return Err("이미 사용한 종료 요청입니다. 다시 보내지 않았습니다.".into());
    }
    save(project, &record)?;
    let generation = project.generation;
    host.save(&target.workspace_id, None, None)?;
    let now = std::time::Instant::now();
    host.local_closes.pending.insert(
        id.into(),
        Pending {
            record,
            capture,
            runtime_instance,
            selection,
            proof: None,
            stage: Stage::Save,
            generation,
            token: 0,
            deadline: now + std::time::Duration::from_secs(30),
            next_observe: now,
        },
    );
    Ok(())
}
fn settle(host: &mut CommandCenterHost, id: &str, phase: Phase, detail: &str) {
    let Some(mut pending) = host.local_closes.pending.remove(id) else {
        return;
    };
    if pending.stage == Stage::Done {
        host.local_closes.pending.insert(id.into(), pending);
        return;
    }
    pending.record.phase = phase;
    pending.stage = Stage::Done;
    if let Some(project) = host.projects.get_mut(&pending.record.target.workspace_id) {
        if phase == Phase::Closed
            && project.snapshot.tasks.iter().any(|t| {
                Some(&t.id.0) == pending.record.target.task_id.as_ref()
                    && t.current_attempt_id.as_ref() == Some(&pending.record.attempt)
                    && matches!(
                        t.state,
                        work::WorkState::Queued
                            | work::WorkState::Running
                            | work::WorkState::WaitingApproval
                            | work::WorkState::WaitingInput
                    )
            })
        {
            let _=apply_user_work_event(project,&pending.record.target,&pending.record.attempt.0,work::WorkEventKind::ObservationLost,"사용자가 정확한 로컬 워커 실행을 종료했습니다. 작업 완료나 검증 통과를 뜻하지 않습니다.".into());
        }
        if let Err(error) = save(project, &pending.record) {
            project.storage_blocked = true;
            host.status = error;
        }
    }
    host.local_closes.pending.insert(id.into(), pending);
    host.finish(
        id,
        if phase == Phase::Closed {
            cc::ReceiptStatus::Acknowledged
        } else if phase == Phase::Unknown {
            cc::ReceiptStatus::Uncertain
        } else {
            cc::ReceiptStatus::Rejected
        },
        detail,
    );
    host.publication_dirty = true;
}
pub(super) fn event(
    host: &mut CommandCenterHost,
    workspace: &str,
    runtime_instance: u64,
    target: &RemoteTaskTarget,
    reply: &RemoteTaskInputReply,
) {
    let id = host
        .local_closes
        .pending
        .iter()
        .find(|(_, p)| {
            p.stage != Stage::Done
                && p.runtime_instance == runtime_instance
                && p.record.target.workspace_id == workspace
                && p.record.owner == *target
                && match reply {
                    RemoteTaskInputReply::Observed { request_id, .. } => {
                        p.stage == Stage::Observing && p.token == *request_id
                    }
                    RemoteTaskInputReply::SessionClosed { operation_id, .. } => {
                        p.stage == Stage::Await && p.record.operation == *operation_id
                    }
                    _ => false,
                }
        })
        .map(|(id, _)| id.clone());
    let Some(id) = id else {
        return;
    };
    match reply {
        RemoteTaskInputReply::Observed { execution, .. } => {
            let valid = execution.as_ref().is_some_and(|proof| {
                proof.target == *target && proof.profile == host.local_closes.pending[&id].selection
            });
            if valid && current(host, &host.local_closes.pending[&id]) {
                let p = host.local_closes.pending.get_mut(&id).unwrap();
                p.proof = execution.clone();
                p.stage = Stage::ClaimSave;
            } else if execution.is_none() {
                let p = host.local_closes.pending.get_mut(&id).unwrap();
                p.stage = Stage::Observe;
                p.next_observe = std::time::Instant::now() + std::time::Duration::from_millis(500);
            } else {
                settle(
                    host,
                    &id,
                    Phase::Rejected,
                    "현재 로컬 실행의 실제 소유권·프로필이 달라 종료하지 않았습니다.",
                );
            }
        }
        RemoteTaskInputReply::SessionClosed { result, .. } => match result {
            RemoteSessionCloseResult::Closed => settle(
                host,
                &id,
                Phase::Closed,
                "정확한 로컬 워커의 프로세스 종료와 실행 용량 반환을 확인했습니다. 작업 결과·검증 기록은 보존합니다.",
            ),
            RemoteSessionCloseResult::Rejected => settle(
                host,
                &id,
                Phase::Rejected,
                "현재 로컬 실행 소유권이 일치하지 않아 종료하지 않았습니다.",
            ),
            RemoteSessionCloseResult::ObservationUnknown => settle(
                host,
                &id,
                Phase::Unknown,
                "로컬 워커 종료 여부를 확인하지 못했습니다. 자동 종료·재실행하지 않습니다.",
            ),
        },
        _ => {}
    }
}
impl App {
    pub(super) fn pump_command_center_local_closes(&mut self, host: &mut CommandCenterHost) {
        let ids: Vec<_> = host
            .local_closes
            .pending
            .iter()
            .filter(|(_, p)| p.stage != Stage::Done)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let mut pending = host.local_closes.pending.remove(&id).unwrap();
            let result = (|| -> Result<(), String> {
                // A sent close can legitimately retire its own capture before its receipt arrives.
                if pending.stage != Stage::Await && !current(host, &pending) {
                    return Err(
                        "로컬 워커의 현재 시도·실행 대상이 바뀌어 종료하지 않았습니다.".into(),
                    );
                }
                if std::time::Instant::now() > pending.deadline {
                    return Err("로컬 워커 종료 응답 시간이 만료되었습니다.".into());
                }
                let project = host
                    .projects
                    .get_mut(&pending.record.target.workspace_id)
                    .ok_or("프로젝트가 없습니다.")?;
                if project.storage_blocked {
                    return Err("저장 결과를 확인하지 못해 종료 요청을 보내지 않았습니다.".into());
                }
                if host.flight.is_some() {
                    return Ok(());
                }
                match pending.stage {
                    Stage::Save if project.saved_generation >= pending.generation => {
                        pending.stage = Stage::Observe
                    }
                    Stage::Observe if std::time::Instant::now() >= pending.next_observe => {
                        host.next_token = host.next_token.saturating_add(1);
                        pending.token = host.next_token;
                        self.runtime_by_instance_mut(pending.runtime_instance)
                            .filter(|r| r.id == pending.record.target.workspace_id)
                            .ok_or("소유한 런타임이 없습니다.")?
                            .runtime
                            .send_command(runtime::RuntimeCommand::RemoteTaskInput {
                                request: RemoteTaskInputRequest::Observe {
                                    target: pending.record.owner.clone(),
                                    request_id: pending.token,
                                },
                            })
                            .map_err(|_| "로컬 실행 소유권 조회 요청이 접수되지 않았습니다.")?;
                        pending.stage = Stage::Observing;
                    }
                    Stage::ClaimSave => {
                        pending.record.phase = Phase::Requested;
                        save(project, &pending.record)?;
                        pending.generation = project.generation;
                        host.save(&pending.record.target.workspace_id, None, None)?;
                        pending.stage = Stage::Send;
                    }
                    Stage::Send if project.saved_generation >= pending.generation => {
                        let proof = pending
                            .proof
                            .as_ref()
                            .filter(|proof| {
                                proof.target == pending.record.owner
                                    && proof.profile == pending.selection
                            })
                            .ok_or("정확한 로컬 실행의 소유권 증명이 없습니다.")?;
                        let deadline = runtime::remote_input::unix_millis()
                            .and_then(|ms| ms.checked_add(15_000))
                            .ok_or("종료 요청 시간을 확인하지 못했습니다.")?;
                        self.runtime_by_instance_mut(pending.runtime_instance)
                            .filter(|r| r.id == pending.record.target.workspace_id)
                            .ok_or("소유한 런타임이 없습니다.")?
                            .runtime
                            .send_command(runtime::RuntimeCommand::RemoteTaskInput {
                                request: RemoteTaskInputRequest::CloseSession {
                                    target: pending.record.owner.clone(),
                                    execution_handle: proof.execution_handle.clone(),
                                    operation_id: pending.record.operation.clone(),
                                    deadline_unix_ms: deadline,
                                },
                            })
                            .map_err(|_| "로컬 종료 요청이 접수되지 않았습니다.")?;
                        pending.stage = Stage::Await;
                    }
                    _ => {}
                }
                Ok(())
            })();
            let unknown = pending.stage == Stage::Await;
            host.local_closes.pending.insert(id.clone(), pending);
            if let Err(error) = result {
                settle(
                    host,
                    &id,
                    if unknown {
                        Phase::Unknown
                    } else {
                        Phase::Rejected
                    },
                    &error,
                );
            }
        }
        if has_pending(host) {
            self.egui_ctx
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (CommandCenterHost, String, cc::RouteTarget) {
        let (mut project, assignment) = super::super::tests::attempted();
        let binding = assignment.attempt.runtime_binding.clone().unwrap();
        let capture = Capture {
            workspace: "workspace-a".into(),
            target: FleetTarget::Pty {
                session: binding.session_id,
                tab: runtime::MuxTabId::new(),
                pane: runtime::MuxPaneId::new(),
            },
            prompt: Some(FleetPromptTarget {
                workspace_id: "workspace-a".into(),
                runtime_instance: 91,
                session: binding.session_id,
                execution: crate::agent_detect::AgentExecutionIdentity::fixture(
                    crate::agent_detect::AgentKind::Codex,
                    17,
                ),
            }),
        };
        project.snapshot.tasks[0].execution_device = None;
        let mut host = super::super::tests::host();
        host.projects.insert("workspace-a".into(), project);
        host.routes.refresh(vec![capture.clone()]);
        let mut target = cc::RouteTarget {
            workspace_id: "workspace-a".into(),
            session_id: host.routes.key(&capture).map(str::to_owned),
            task_id: Some(assignment.task.id.0.clone()),
        };
        host.live.push(LiveRoute {
            capture: capture.clone(),
            title: "owned worker".into(),
            agent: Some(assignment.task.assigned_agent.clone()),
            state: cc::TaskState::Queued,
            cwd: None,
            watch: None,
            runtime_binding: Some(binding.clone()),
        });
        let identity = runtime::remote_worker::RemoteWorkIdentity {
            device_id: "local-device".into(),
            workspace_id: WorkspaceId("workspace-a".into()),
            attempt_id: assignment.attempt.id.clone(),
            operation_id: assignment.attempt.operation_id.clone(),
            incarnation: binding.incarnation.clone(),
            grant_epoch: 1,
            lease_epoch: 1,
        };
        host.launches.insert(
            assignment.attempt.operation_id.0.clone(),
            launch::Pending {
                record: launch::Record {
                    operation_id: assignment.attempt.operation_id.clone(),
                    attempt_id: assignment.attempt.id.clone(),
                    workspace: WorkspaceId("workspace-a".into()),
                    task_id: Some(assignment.task.id.clone()),
                    config_revision: assignment.attempt.config.revision,
                    selection: assignment.task.assigned_agent.clone(),
                    folder: assignment.attempt.folder.clone(),
                    phase: launch::Phase::Completed,
                },
                runtime_instance: 91,
                generation: 0,
                stage: launch::Stage::Done,
                request: Some(runtime::remote_worker::RemoteWorkRequest {
                    identity: identity.clone(),
                    folder: assignment.attempt.folder.clone(),
                    folder_anchor: "owned-anchor".into(),
                    profile_id: "owned-profile".into(),
                    initial_prompt: None,
                }),
                deadline: std::time::Instant::now(),
                owned: None,
                grant: None,
            },
        );
        let id = work::OperationId::new();
        let now = std::time::Instant::now();
        host.local_closes.pending.insert(
            id.0.clone(),
            Pending {
                record: Record {
                    operation: id.clone(),
                    target: target.clone(),
                    attempt: assignment.attempt.id,
                    binding: binding.clone(),
                    owner: RemoteTaskTarget {
                        identity,
                        session: binding.session_id,
                        execution_generation: binding.execution_generation,
                    },
                    phase: Phase::Requested,
                },
                capture,
                runtime_instance: 91,
                selection: assignment.task.assigned_agent,
                proof: None,
                stage: Stage::Await,
                generation: 0,
                token: 9,
                deadline: now + std::time::Duration::from_secs(5),
                next_observe: now,
            },
        );
        target.task_id = Some(assignment.task.id.0);
        (host, id.0, target)
    }
    #[test]
    fn local_unknown_close_cannot_be_replaced_with_a_new_kill_operation() {
        let (mut host, id, target) = fixture();
        settle(&mut host, &id, Phase::Unknown, "unknown");
        assert!(!capability(&host, &target).enabled);
    }
    #[test]
    fn local_close_receipt_is_exact_runtime_and_owner_and_preserves_completed_result() {
        let (mut host, id, _) = fixture();
        host.projects.get_mut("workspace-a").unwrap().snapshot.tasks[0].state =
            work::WorkState::Completed;
        let owner = host.local_closes.pending[&id].record.owner.clone();
        let reply = RemoteTaskInputReply::SessionClosed {
            operation_id: work::OperationId::from_string(id.clone()).unwrap(),
            result: RemoteSessionCloseResult::Closed,
        };
        event(&mut host, "workspace-a", 92, &owner, &reply);
        assert!(host.local_closes.pending[&id].stage == Stage::Await);
        event(&mut host, "workspace-a", 91, &owner, &reply);
        assert!(host.local_closes.pending[&id].record.phase == Phase::Closed);
        assert_eq!(
            host.projects["workspace-a"].snapshot.tasks[0].state,
            work::WorkState::Completed
        );
    }
    #[test]
    fn local_owned_route_does_not_authorize_a_replacement_attempt_or_manual_session() {
        let (mut host, id, target) = fixture();
        assert!(current(&host, &host.local_closes.pending[&id]));
        host.projects.get_mut("workspace-a").unwrap().snapshot.tasks[0].current_attempt_id =
            Some(work::AttemptId::new());
        assert!(!current(&host, &host.local_closes.pending[&id]));
        host.launches.clear();
        assert!(!capability(&host, &target).enabled);
    }
}
