//! Native remote effects are bound to a confirmed task, a durable marker and a live TLS lease.
use super::*;
use serde::{Deserialize, Serialize};
use wm::devices::{ExecutionLease, VerifiedExecutionLease};
#[path = "command_center_remote_reconcile.rs"]
pub(super) mod reconcile;
#[path = "command_center_remote_sessions.rs"]
pub(super) mod sessions;

#[derive(Clone)]
pub(super) struct EffectFence(Arc<std::sync::Mutex<bool>>);
impl EffectFence {
    pub fn new() -> Self {
        Self(Arc::new(std::sync::Mutex::new(true)))
    }
    pub fn revoke(&self) {
        if let Ok(mut current) = self.0.lock() {
            *current = false;
        }
    }
    fn with_current(&self, action: &mut dyn FnMut()) {
        if let Ok(current) = self.0.lock()
            && *current
        {
            action();
        }
    }
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) enum Phase {
    Reserved,
    Requested,
    Spawned,
    Prepared,
    InputClaimed,
    Admitted,
    Rejected,
    Unknown,
    ObservedIdle,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Record {
    operation_id: work::OperationId,
    attempt_id: work::AttemptId,
    task_id: work::TaskId,
    workspace: WorkspaceId,
    config_revision: work::ConfigRevision,
    device: String,
    profile: String,
    agent: work::AgentSelection,
    folder: String,
    binding: Option<work::RuntimeBinding>,
    phase: Phase,
    #[serde(default)]
    accepted_sequence: Option<u64>,
    #[serde(default)]
    accepted_at_micros: Option<i64>,
}

fn lease_matches(record: &Record, lease: &ExecutionLease) -> bool {
    record.operation_id == lease.request.operation_id
        && record.attempt_id == lease.request.attempt_id
        && record.workspace == lease.request.workspace_id
        && record.device == lease.request.device_id
        && record.profile == lease.profile_id
        && record.agent == lease.request.agent
        && record.folder == lease.request.folder
        && lease.validate().is_ok()
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    MarkerSave,
    LaunchWait,
    AttemptSave,
    ObserveReady,
    ObserveWait,
    ClaimSave,
    DeliverReady,
    DeliverWait,
    Done,
}
pub(super) struct Pending {
    record: Record,
    intent: Option<wm::remote_dispatch::RemoteLaunchIntent>,
    verified: Option<VerifiedExecutionLease>,
    lease: Option<ExecutionLease>,
    assignment: Option<wm::dispatch::DispatchAssignment>,
    execution: Option<Box<runtime::remote_input::RemoteExecutionProof>>,
    generation: u64,
    request_id: u64,
    stage: Stage,
    deadline: std::time::Instant,
    next_observe: std::time::Instant,
}
impl Pending {
    pub(super) fn preparing(&self) -> bool {
        self.stage != Stage::Done && self.stage != Stage::DeliverWait
    }
    pub(super) fn pending(&self) -> bool {
        self.stage != Stage::Done
    }
}
pub(super) fn target_for(host: &CommandCenterHost, task: &work::Task) -> Option<cc::RouteTarget> {
    let project = host.projects.get(&task.workspace_id.0)?;
    if let Some((id, _)) = reconcile::idle_owner(host, task) {
        return Some(cc::RouteTarget {
            workspace_id: task.workspace_id.0.clone(),
            task_id: Some(task.id.0.clone()),
            session_id: Some(format!("remote:{id}")),
        });
    }
    let attempt = project
        .snapshot
        .attempts
        .iter()
        .find(|a| Some(&a.id) == task.current_attempt_id.as_ref())?;
    let (id, _) = host.remotes.iter().find(|(_, p)| {
        p.record.workspace == task.workspace_id
            && p.record.task_id == task.id
            && Some(&p.record.attempt_id) == task.current_attempt_id.as_ref()
            && p.record.binding.is_some()
            && p.record.binding == attempt.runtime_binding
            && p.lease.is_some()
    })?;
    Some(cc::RouteTarget {
        workspace_id: task.workspace_id.0.clone(),
        task_id: Some(task.id.0.clone()),
        session_id: Some(format!("remote:{id}")),
    })
}
pub(super) fn workspace_unknown(host: &CommandCenterHost, workspace: &str) -> bool {
    host.remotes
        .values()
        .any(|p| p.record.workspace.0 == workspace && p.record.phase == Phase::Unknown)
}
pub(super) fn workspace_preparing(host: &CommandCenterHost, workspace: &str) -> bool {
    host.remotes
        .values()
        .any(|p| p.record.workspace.0 == workspace && p.preparing())
        || host
            .launches
            .values()
            .any(|p| p.record.workspace.0 == workspace && p.preparing())
}
pub(super) fn handoff_pending(host: &CommandCenterHost, target: &cc::RouteTarget) -> bool {
    local_close::target_pending(host, target)
        || sessions::target_pending(host, target)
        || host.inputs.values().any(|p| {
            p.target.workspace_id == target.workspace_id && p.target.task_id == target.task_id
        })
        || host.remotes.values().any(|p| {
            p.pending()
                && p.record.workspace.0 == target.workspace_id
                && Some(&p.record.task_id.0) == target.task_id.as_ref()
        })
}
pub(super) fn project_pending(host: &CommandCenterHost, workspace: &str) -> bool {
    reconcile::project_pending(host, workspace)
        || local_close::project_pending(host, workspace)
        || sessions::project_pending(host, workspace)
        || host
            .inputs
            .values()
            .any(|input| input.target.workspace_id == workspace)
        || host
            .remotes
            .values()
            .any(|p| p.pending() && p.record.workspace.0 == workspace)
}
fn current_attempt(project: &Project, pending: &Pending) -> bool {
    let Some(binding) = pending.record.binding.as_ref() else {
        return false;
    };
    let Some(task) = project.snapshot.tasks.iter().find(|t| {
        t.id == pending.record.task_id
            && t.current_attempt_id.as_ref() == Some(&pending.record.attempt_id)
    }) else {
        return false;
    };
    matches!(
        task.state,
        work::WorkState::Queued
            | work::WorkState::Running
            | work::WorkState::WaitingApproval
            | work::WorkState::WaitingInput
    ) && project.snapshot.attempts.iter().any(|a| {
        a.id == pending.record.attempt_id
            && a.runtime_binding.as_ref() == Some(binding)
            && a.config.revision == pending.record.config_revision
    })
}
fn save_record(project: &mut Project, record: &Record) -> Result<(), String> {
    set_draft(
        project,
        &format!("remote:{}", record.operation_id.0),
        &serde_json::to_string(record).map_err(|e| e.to_string())?,
        matches!(
            record.phase,
            Phase::Requested | Phase::InputClaimed | Phase::Unknown
        ),
    )
}
pub(super) fn restore(host: &mut CommandCenterHost, workspace: &str) {
    let rows: Vec<_> = host.projects[workspace]
        .snapshot
        .drafts
        .iter()
        .filter(|d| d.target_key.starts_with("remote:"))
        .map(|d| serde_json::from_str::<Record>(&d.text))
        .collect();
    if rows.iter().any(Result::is_err) || rows.len() > work::WORK_DRAFT_MAX_COUNT {
        host.projects.get_mut(workspace).unwrap().storage_blocked = true;
        host.status = "이전 원격 실행 기록을 검증하지 못했습니다. 재전송하지 않았습니다.".into();
        return;
    }
    for mut record in rows.into_iter().filter_map(Result::ok) {
        if record.workspace.0 != workspace {
            host.projects.get_mut(workspace).unwrap().storage_blocked = true;
            continue;
        }
        if host.remotes.contains_key(&record.operation_id.0) {
            continue;
        }
        if !matches!(record.phase, Phase::Admitted | Phase::Rejected) {
            record.phase = Phase::Unknown;
        }
        let now = std::time::Instant::now();
        host.remotes.insert(
            record.operation_id.0.clone(),
            Pending {
                record,
                intent: None,
                verified: None,
                lease: None,
                assignment: None,
                execution: None,
                generation: 0,
                request_id: 0,
                stage: Stage::Done,
                deadline: now,
                next_observe: now,
            },
        );
    }
    sessions::restore(host, workspace);
}
fn settle(host: &mut CommandCenterHost, id: &str, unknown: bool, detail: &str) {
    let Some(mut pending) = host.remotes.remove(id) else {
        return;
    };
    if pending.stage == Stage::Done {
        host.remotes.insert(id.into(), pending);
        return;
    }
    pending.record.phase = if unknown {
        Phase::Unknown
    } else {
        Phase::Rejected
    };
    pending.stage = Stage::Done;
    if let Some(project) = host.projects.get_mut(&pending.record.workspace.0) {
        if let Some(assignment) = &pending.assignment {
            apply_delivery_outcome(
                project,
                assignment,
                if unknown {
                    ui::composer::PromptAdmissionOutcome::Unknown
                } else {
                    ui::composer::PromptAdmissionOutcome::Rejected
                },
            );
        }
        if let Err(error) = save_record(project, &pending.record) {
            project.storage_blocked = true;
            host.status = error;
        }
    }
    host.devices.finish_remote(pending.request_id);
    host.remotes.insert(id.into(), pending);
    host.finish(
        id,
        if unknown {
            cc::ReceiptStatus::Uncertain
        } else {
            cc::ReceiptStatus::Rejected
        },
        detail,
    );
}
pub(super) fn launch_pending(
    host: &mut CommandCenterHost,
    request_id: u64,
    lease: ExecutionLease,
    verified: VerifiedExecutionLease,
) {
    let id = lease.request.operation_id.0.clone();
    let Some(pending) = host.remotes.get_mut(&id) else {
        return;
    };
    if pending.stage != Stage::LaunchWait
        || pending.request_id != request_id
        || !lease_matches(&pending.record, &lease)
        || verified.lease() != &lease
    {
        return;
    }
    host.devices.finish_remote(request_id);
    pending.lease = Some(lease);
    pending.verified = Some(verified);
}
pub(super) fn launch_receipt(
    host: &mut CommandCenterHost,
    receipt: wm::remote_controller::RemoteLaunchReceipt,
) {
    let id = receipt.lease.request.operation_id.0.clone();
    let Some(pending) = host.remotes.get(&id) else {
        return;
    };
    if pending.stage != Stage::LaunchWait
        || pending.request_id != receipt.request_id
        || !lease_matches(&pending.record, &receipt.lease)
        || pending.lease.as_ref().is_some_and(|l| l != &receipt.lease)
    {
        return;
    }
    host.devices.finish_remote(receipt.request_id);
    match receipt.result {
        runtime::remote_worker::RemoteWorkResult::Launched {
            session,
            execution_generation,
        } if execution_generation > 0 && session.0 > 0 => {
            let pending = host.remotes.get_mut(&id).unwrap();
            pending.lease = Some(receipt.lease.clone());
            pending.record.binding = Some(work::RuntimeBinding {
                incarnation: receipt.lease.incarnation,
                session_id: session,
                execution_generation,
            });
            pending.record.phase = Phase::Spawned;
            pending.stage = Stage::AttemptSave;
        }
        runtime::remote_worker::RemoteWorkResult::Rejected { .. } => settle(
            host,
            &id,
            false,
            "원격 기기가 실행 생성을 거부했습니다. 작업 입력은 보내지 않았습니다.",
        ),
        _ => settle(
            host,
            &id,
            true,
            "원격 실행 생성 응답을 확인하지 못했습니다. 자동으로 다시 생성하거나 입력하지 않습니다.",
        ),
    }
}
pub(super) fn observed(
    host: &mut CommandCenterHost,
    request_id: u64,
    lease: ExecutionLease,
    execution: Option<Box<runtime::remote_input::RemoteExecutionProof>>,
    verified: Option<VerifiedExecutionLease>,
) {
    let id = lease.request.operation_id.0.clone();
    let Some(pending) = host.remotes.get_mut(&id) else {
        return;
    };
    if pending.stage != Stage::ObserveWait
        || pending.request_id != request_id
        || pending.lease.as_ref() != Some(&lease)
    {
        return;
    }
    host.devices.finish_remote(request_id);
    if let (Some(execution), Some(verified)) = (execution, verified) {
        let binding = pending.record.binding.as_ref().unwrap();
        if execution.target.identity.attempt_id != pending.record.attempt_id
            || execution.target.identity.operation_id != pending.record.operation_id
            || execution.target.identity.workspace_id != pending.record.workspace
            || execution.target.identity.device_id != pending.record.device
            || execution.target.identity.incarnation != binding.incarnation
            || execution.target.session != binding.session_id
            || execution.target.execution_generation != binding.execution_generation
            || execution.profile != pending.record.agent
            || verified.lease() != &lease
            || verified.validate_fresh().is_err()
        {
            settle(
                host,
                &id,
                true,
                "원격 실행의 실제 프로세스·기기·폴더 증명이 바뀌었습니다. 입력하지 않았습니다.",
            );
            return;
        }
        pending.execution = Some(execution);
        pending.verified = Some(verified);
        pending.stage = Stage::ClaimSave;
    } else {
        pending.stage = Stage::ObserveReady;
        pending.next_observe = std::time::Instant::now() + std::time::Duration::from_millis(500);
    }
}
pub(super) fn input_pending(
    host: &mut CommandCenterHost,
    request_id: u64,
    lease: &ExecutionLease,
    operation: &work::OperationId,
) {
    let Some(pending) = host.remotes.get(&operation.0) else {
        return;
    };
    if pending.stage == Stage::DeliverWait
        && pending.request_id == request_id
        && pending.lease.as_ref() == Some(lease)
    {
        host.devices.finish_remote(request_id);
    }
}
pub(super) fn input_receipt(
    host: &mut CommandCenterHost,
    receipt: wm::remote_controller::RemoteInputReceipt,
) {
    let id = receipt.operation_id.0.clone();
    let Some(pending) = host.remotes.get(&id) else {
        return;
    };
    if pending.stage != Stage::DeliverWait
        || pending.request_id != receipt.request_id
        || pending.lease.as_ref() != Some(&receipt.lease)
    {
        return;
    }
    host.devices.finish_remote(receipt.request_id);
    match receipt.result {
        runtime::remote_input::RemoteTaskInputResult::Admitted
            if receipt
                .accepted
                .as_ref()
                .is_some_and(|a| a.sequence > 0 && a.at_micros > 0) =>
        {
            let pending = host.remotes.get_mut(&id).unwrap();
            let project = host.projects.get_mut(&pending.record.workspace.0).unwrap();
            if let Some(assignment) = &pending.assignment {
                apply_delivery_outcome(
                    project,
                    assignment,
                    ui::composer::PromptAdmissionOutcome::Accepted,
                );
            }
            pending.record.phase = Phase::Admitted;
            pending.record.accepted_sequence = receipt.accepted.as_ref().map(|a| a.sequence);
            pending.record.accepted_at_micros = receipt.accepted.as_ref().map(|a| a.at_micros);
            pending.stage = Stage::Done;
            if let Err(error) = save_record(project, &pending.record) {
                project.storage_blocked = true;
                host.status = error;
            }
            host.finish(&id,cc::ReceiptStatus::Acknowledged,"정확한 원격 실행이 작업 입력을 승인했습니다. 제공자의 작업 시작·결과·검증은 별도로 확인합니다.");
        }
        runtime::remote_input::RemoteTaskInputResult::Rejected { .. } => settle(
            host,
            &id,
            false,
            "원격 실행이 입력을 거부했습니다. 실제 시도를 실패로 기록하고 재전송하지 않습니다.",
        ),
        _ => settle(
            host,
            &id,
            true,
            "원격 입력의 실제 승인 응답을 확인하지 못했습니다. 자동 재전송하지 않습니다.",
        ),
    }
}
pub(super) fn failed(host: &mut CommandCenterHost, request_id: u64) -> bool {
    let id = host
        .remotes
        .iter()
        .find(|(_, p)| p.request_id == request_id && p.pending())
        .map(|(id, _)| id.clone());
    if let Some(id) = id {
        settle(
            host,
            &id,
            false,
            "원격 요청이 실행 전에 거부되었습니다. 자동 재전송하지 않았습니다.",
        );
        true
    } else {
        false
    }
}
pub(super) fn unknown(
    host: &mut CommandCenterHost,
    correlation: &wm::remote_controller::RemoteDeviceCorrelation,
) -> bool {
    let id = host
        .remotes
        .iter()
        .find(|(_, p)| {
            p.request_id == correlation.request_id
                && p.pending()
                && correlation.workspace_id.as_ref() == Some(&p.record.workspace)
                && correlation.attempt_id.as_ref() == Some(&p.record.attempt_id)
                && correlation.operation_id.as_ref() == Some(&p.record.operation_id)
                && correlation.device_id.as_ref() == Some(&p.record.device)
        })
        .map(|(id, _)| id.clone());
    if let Some(id) = id {
        let effect_unknown = matches!(
            host.remotes[&id].stage,
            Stage::LaunchWait | Stage::DeliverWait
        );
        settle(
            host,
            &id,
            effect_unknown,
            "원격 워커의 정확한 응답 관측이 끊겼습니다. 자동 재연결·재생성·재입력을 하지 않습니다.",
        );
        true
    } else {
        false
    }
}
fn authority(
    project: &Project,
    pending: &Pending,
    launch: bool,
) -> Result<wm::remote_controller::RemoteSendAuthority, String> {
    let fence = project.effect_fence.clone();
    let intent = pending.intent.clone();
    let verified = pending.verified.clone();
    wm::remote_controller::RemoteSendAuthority::new(
        pending.record.operation_id.clone(),
        std::time::Instant::now() + std::time::Duration::from_secs(15),
        move |action| {
            fence.with_current(&mut || {
                let valid = if launch {
                    intent.as_ref().is_some_and(|i| i.validate_fresh().is_ok())
                } else {
                    verified
                        .as_ref()
                        .is_some_and(|v| v.validate_fresh().is_ok())
                };
                if valid {
                    action();
                }
            })
        },
    )
    .map_err(|e| format!("원격 실행 권한을 확인하지 못했습니다: {e:?}"))
}
impl App {
    pub(super) fn begin_command_center_remote(
        &mut self,
        host: &mut CommandCenterHost,
        id: &str,
        target: &cc::RouteTarget,
    ) -> Result<(), String> {
        if host.remote_permits.contains_key(id) {
            return Err("이 컴퓨터에서 새 원격 실행을 시작해 주세요.".into());
        }
        reconcile::ensure_no_idle_worker(host, target)?;
        if host.flight.is_some() {
            return Err("프로젝트 저장을 마친 뒤 실행해 주세요.".into());
        }
        if host.remotes.len() >= work::WORK_DRAFT_MAX_COUNT {
            return Err("원격 실행 기록 한도입니다. 미확인 실행 기록을 보존합니다.".into());
        }
        if host.remotes.values().filter(|p| p.preparing()).count() >= 32
            || workspace_preparing(host, &target.workspace_id)
        {
            return Err("이 프로젝트의 다른 실행 준비를 기다려 주세요.".into());
        }
        if workspace_unknown(host, &target.workspace_id)
            || launch::workspace_uncertain(&host.launches, &target.workspace_id)
        {
            return Err("확인하지 못한 이전 실행을 먼저 복구해 주세요.".into());
        }
        let project = host
            .projects
            .get_mut(&target.workspace_id)
            .ok_or("프로젝트가 없습니다.")?;
        if !project.loaded
            || project.storage_blocked
            || project.generation != project.saved_generation
        {
            return Err("프로젝트 저장 상태를 확인해 주세요.".into());
        }
        let task = project
            .snapshot
            .tasks
            .iter()
            .find(|t| Some(&t.id.0) == target.task_id.as_ref())
            .ok_or("작업이 없습니다.")?
            .clone();
        let device_id = task
            .execution_device
            .as_ref()
            .ok_or("원격 기기를 먼저 배정해 주세요.")?;
        if !host.devices.can_execute(&target.workspace_id, device_id) {
            return Err("기기의 현재 실행 용량·연결·허용 상태를 먼저 확인해 주세요.".into());
        }
        let device = host
            .devices
            .policies
            .iter()
            .find(|d| {
                d.registration().id == *device_id
                    && d.registration().workspace_id == task.workspace_id
            })
            .ok_or("현재 연결되어 실행이 허용된 기기를 확인할 수 없습니다.")?;
        let Some(assignment::Confirmed::Mixed(confirmed)) = project.confirmed_for_task(&task.id)
        else {
            return Err("변경한 기기·프로필을 포함한 전체 배정안을 먼저 확인해 주세요.".into());
        };
        let attempt = work::AttemptId::new();
        let operation = work::OperationId::from_string(id.into()).map_err(|e| e.to_string())?;
        let intent = wm::remote_dispatch::prepare_remote_launch(
            &project.state(),
            &project.ledger,
            confirmed,
            &task.id,
            attempt.clone(),
            operation.clone(),
            &project.snapshot.config,
            &project.policy(),
            device,
        )
        .map_err(|e| e.to_string())?;
        let profile = device
            .profiles()
            .iter()
            .find(|p| p.agent == task.assigned_agent)
            .ok_or("정확한 실행 프로필이 없습니다.")?;
        let record = Record {
            operation_id: operation,
            attempt_id: attempt,
            task_id: task.id,
            workspace: task.workspace_id,
            config_revision: project.snapshot.config.revision,
            device: device_id.clone(),
            profile: profile.id.clone(),
            agent: task.assigned_agent,
            folder: intent.request().folder.clone(),
            binding: None,
            phase: Phase::Reserved,
            accepted_sequence: None,
            accepted_at_micros: None,
        };
        if project
            .snapshot
            .drafts
            .iter()
            .any(|d| d.target_key == format!("remote:{id}"))
        {
            return Err("이미 사용한 원격 실행 ID입니다. 재전송하지 않았습니다.".into());
        }
        save_record(project, &record)?;
        let generation = project.generation;
        host.save(&target.workspace_id, None, None)?;
        let now = std::time::Instant::now();
        host.remotes.insert(
            id.into(),
            Pending {
                record,
                intent: Some(intent),
                verified: None,
                lease: None,
                assignment: None,
                execution: None,
                generation,
                request_id: 0,
                stage: Stage::MarkerSave,
                deadline: now + std::time::Duration::from_secs(60),
                next_observe: now,
            },
        );
        host.status = "원격 작업의 정확한 시도·작업 ID를 저장한 뒤 유휴 실행을 생성합니다.".into();
        Ok(())
    }
    pub(super) fn pump_command_center_remote(&mut self, host: &mut CommandCenterHost) {
        let ids: Vec<_> = host
            .remotes
            .iter()
            .filter(|(_, p)| p.pending())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let Some(mut pending) = host.remotes.remove(&id) else {
                continue;
            };
            let workspace = pending.record.workspace.0.clone();
            let result = (|| -> Result<(), String> {
                let project = host
                    .projects
                    .get_mut(&workspace)
                    .ok_or("원격 프로젝트가 더 이상 없습니다.")?;
                if project.storage_blocked {
                    return Err(
                        "저장 결과를 확인하지 못해 새 원격 입력을 보내지 않았습니다.".into(),
                    );
                }
                if project.snapshot.config.revision != pending.record.config_revision {
                    return Err("프로젝트 설정이 바뀌어 원격 입력을 보내지 않았습니다.".into());
                }
                if std::time::Instant::now() > pending.deadline {
                    return Err("원격 실행 또는 입력 응답 대기 시간이 만료되었습니다.".into());
                }
                if pending.assignment.is_some() && !current_attempt(project, &pending) {
                    return Err("현재 작업 시도·실행 또는 검토 상태가 바뀌어 지연된 입력을 보내지 않았습니다.".into());
                }
                if (matches!(pending.stage, Stage::MarkerSave)
                    || (pending.stage == Stage::LaunchWait && pending.request_id == 0))
                    && !project.ready_tasks()?.contains(&pending.record.task_id)
                {
                    return Err("원격 작업의 의존 관계·용량·검토 상태가 바뀌었습니다.".into());
                }
                if host.flight.is_some() {
                    return Ok(());
                }
                match pending.stage {
                    Stage::MarkerSave if project.saved_generation >= pending.generation => {
                        let intent = pending
                            .intent
                            .as_ref()
                            .ok_or("원격 실행 요청이 없습니다.")?;
                        intent.validate_fresh().map_err(|e| e.to_string())?;
                        if host.devices.has_pending() {
                            return Ok(());
                        }
                        pending.record.phase = Phase::Requested;
                        save_record(project, &pending.record)?;
                        pending.generation = project.generation;
                        host.save(&workspace, None, None)?;
                        pending.stage = Stage::LaunchWait;
                        // The actual bounded send is queued only after this second marker ACK.
                        pending.request_id = 0;
                    }
                    Stage::LaunchWait
                        if pending.request_id == 0
                            && project.saved_generation >= pending.generation =>
                    {
                        if host.devices.has_pending() {
                            return Ok(());
                        }
                        host.next_token = host.next_token.saturating_add(1);
                        let token = host.next_token;
                        let intent = pending
                            .intent
                            .as_ref()
                            .ok_or("원격 실행 요청이 없습니다.")?;
                        intent.validate_fresh().map_err(|e| e.to_string())?;
                        let job = wm::remote_controller::RemoteDeviceJob::ReserveLaunch {
                            request_id: token,
                            request: intent.request().clone(),
                            initial_prompt: None,
                            authority: authority(project, &pending, true)?,
                        };
                        host.devices.queue_remote(token, job)?;
                        pending.request_id = token;
                    }
                    Stage::AttemptSave => {
                        if pending.assignment.is_none() {
                            let Some(assignment::Confirmed::Mixed(confirmed)) =
                                project.confirmed_for_task(&pending.record.task_id)
                            else {
                                return Err("확인한 원격 배정안이 바뀌었습니다.".into());
                            };
                            let verified = pending
                                .verified
                                .as_ref()
                                .ok_or("생성된 원격 실행의 현재 권한 증명이 없습니다.")?;
                            let prepared = wm::remote_dispatch::prepare_remote_dispatch(
                                &project.state(),
                                &project.ledger,
                                confirmed,
                                wm::dispatch::DispatchRequest {
                                    task_id: pending.record.task_id.clone(),
                                    operation_id: pending.record.operation_id.clone(),
                                    folder: wm::dispatch::DispatchFolder {
                                        path: pending.record.folder.clone(),
                                        worktree_owner: None,
                                    },
                                    persistent_binding: None,
                                    runtime_binding: pending
                                        .record
                                        .binding
                                        .clone()
                                        .ok_or("원격 실행 바인딩이 없습니다.")?,
                                },
                                pending.record.attempt_id.clone(),
                                &project.snapshot.config,
                                &project.policy(),
                                verified,
                            )
                            .map_err(|e| e.to_string())?;
                            let wm::dispatch::PrepareOutcome::Ready(prepared) = prepared else {
                                return Err(
                                    "이미 사용한 원격 시도입니다. 다시 입력하지 않습니다.".into()
                                );
                            };
                            project.set_state(prepared.next_state);
                            project.snapshot.outbox.push(stored::DispatchOutbox {
                                attempt_id: prepared.assignment.attempt.id.clone(),
                                operation_id: prepared.assignment.attempt.operation_id.clone(),
                                state: stored::DispatchOutboxState::Pending,
                            });
                            pending.assignment = Some(prepared.assignment);
                            pending.record.phase = Phase::Prepared;
                            save_record(project, &pending.record)?;
                            pending.generation = project.generation;
                            host.save(&workspace, None, None)?;
                        } else if project.saved_generation >= pending.generation {
                            pending.stage = Stage::ObserveReady;
                        }
                    }
                    Stage::ObserveReady if std::time::Instant::now() >= pending.next_observe => {
                        if host.devices.has_pending() {
                            return Ok(());
                        }
                        let binding = pending
                            .record
                            .binding
                            .as_ref()
                            .ok_or("원격 실행 바인딩이 없습니다.")?;
                        host.next_token = host.next_token.saturating_add(1);
                        let token = host.next_token;
                        host.devices.queue_remote(
                            token,
                            wm::remote_controller::RemoteDeviceJob::ObserveExecution {
                                request_id: token,
                                lease: pending.lease.clone().ok_or("원격 임대가 없습니다.")?,
                                session: binding.session_id,
                                execution_generation: binding.execution_generation,
                            },
                        )?;
                        pending.request_id = token;
                        pending.stage = Stage::ObserveWait;
                    }
                    Stage::ClaimSave => {
                        let assignment = pending
                            .assignment
                            .as_ref()
                            .ok_or("원격 작업 지시가 없습니다.")?;
                        project
                            .ledger
                            .record_durable(assignment.clone())
                            .map_err(|e| e.to_string())?;
                        if !project.ledger.claim_send(&assignment.attempt.operation_id) {
                            return Err(
                                "이미 처리한 원격 입력 요청입니다. 다시 보내지 않았습니다.".into(),
                            );
                        }
                        pending.record.phase = Phase::InputClaimed;
                        save_record(project, &pending.record)?;
                        pending.generation = project.generation;
                        host.save(&workspace, None, None)?;
                        pending.stage = Stage::DeliverReady;
                    }
                    Stage::DeliverReady if project.saved_generation >= pending.generation => {
                        if host.devices.has_pending() {
                            return Ok(());
                        }
                        let verified = pending
                            .verified
                            .as_ref()
                            .ok_or("원격 입력 권한이 없습니다.")?;
                        verified
                            .validate_fresh()
                            .map_err(|_| "원격 입력 권한이 만료되었습니다.")?;
                        host.next_token = host.next_token.saturating_add(1);
                        let token = host.next_token;
                        let job = wm::remote_controller::RemoteDeviceJob::DeliverInput {
                            request_id: token,
                            lease: pending.lease.clone().ok_or("원격 임대가 없습니다.")?,
                            execution: pending
                                .execution
                                .clone()
                                .ok_or("원격 실행의 실제 프로세스를 확인하지 못했습니다.")?,
                            operation_id: pending.record.operation_id.clone(),
                            prompt: pending
                                .assignment
                                .as_ref()
                                .ok_or("원격 작업 지시가 없습니다.")?
                                .prompt
                                .clone(),
                            authority: authority(project, &pending, false)?,
                        };
                        host.devices.queue_remote(token, job)?;
                        pending.request_id = token;
                        pending.stage = Stage::DeliverWait;
                    }
                    _ => {}
                }
                Ok(())
            })();
            let unknown = matches!(pending.stage, Stage::LaunchWait | Stage::DeliverWait)
                && pending.request_id > 0;
            host.remotes.insert(id.clone(), pending);
            if let Err(detail) = result {
                settle(host, &id, unknown, &detail);
            }
        }
        if host.remotes.values().any(|p| p.pending()) {
            self.egui_ctx
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Record, ExecutionLease) {
        let request = wm::devices::LeaseRequest {
            device_id: "device-a".into(),
            workspace_id: WorkspaceId("workspace-a".into()),
            attempt_id: work::AttemptId::new(),
            operation_id: work::OperationId::new(),
            folder: "/remote/project".into(),
            agent: work::AgentSelection {
                provider: "codex".into(),
                model: Some("actual-model".into()),
                effort: Some("xhigh".into()),
            },
        };
        let record = Record {
            operation_id: request.operation_id.clone(),
            attempt_id: request.attempt_id.clone(),
            task_id: work::TaskId::new(),
            workspace: request.workspace_id.clone(),
            config_revision: work::ConfigRevision(1),
            device: request.device_id.clone(),
            profile: "actual-profile".into(),
            agent: request.agent.clone(),
            folder: request.folder.clone(),
            binding: None,
            phase: Phase::Requested,
            accepted_sequence: None,
            accepted_at_micros: None,
        };
        let lease = ExecutionLease {
            request,
            folder_anchor: "actual-anchor".into(),
            profile_id: record.profile.clone(),
            grant_epoch: 1,
            lease_epoch: 1,
            connection_epoch: 2,
            incarnation: work::RuntimeIncarnationId::new(),
        };
        (record, lease)
    }
    #[test]
    fn remote_launch_correlation_never_aliases_workspace_attempt_operation_profile_or_folder() {
        let (record, lease) = fixture();
        assert!(lease_matches(&record, &lease));
        let mut other = lease.clone();
        other.request.workspace_id = WorkspaceId("workspace-b".into());
        assert!(!lease_matches(&record, &other));
        let mut other = lease.clone();
        other.request.attempt_id = work::AttemptId::new();
        assert!(!lease_matches(&record, &other));
        let mut other = lease.clone();
        other.request.operation_id = work::OperationId::new();
        assert!(!lease_matches(&record, &other));
        let mut other = lease.clone();
        other.profile_id = "replacement-profile".into();
        assert!(!lease_matches(&record, &other));
        let mut other = lease.clone();
        other.request.folder = "/remote/replacement".into();
        assert!(!lease_matches(&record, &other));
    }
    #[test]
    fn config_effect_fence_blocks_deferred_effect_after_revision_is_retired() {
        let fence = EffectFence::new();
        let captured = fence.clone();
        let mut calls = 0;
        captured.with_current(&mut || calls += 1);
        fence.revoke();
        captured.with_current(&mut || calls += 1);
        assert_eq!(calls, 1);
    }
    pub(super) fn host_with_pending(stage: Stage) -> (CommandCenterHost, ExecutionLease, String) {
        let (mut project, mut assignment) = super::super::tests::attempted();
        let (mut record, mut lease) = fixture();
        record.operation_id = assignment.attempt.operation_id.clone();
        record.attempt_id = assignment.attempt.id.clone();
        record.task_id = assignment.task.id.clone();
        record.config_revision = project.snapshot.config.revision;
        lease.request.operation_id = record.operation_id.clone();
        lease.request.attempt_id = record.attempt_id.clone();
        let binding = work::RuntimeBinding {
            incarnation: lease.incarnation.clone(),
            session_id: runtime::SessionId(7),
            execution_generation: 19,
        };
        record.binding = Some(binding.clone());
        record.phase = if stage == Stage::DeliverWait {
            Phase::InputClaimed
        } else {
            Phase::Requested
        };
        assignment.task.execution_device = Some(record.device.clone());
        assignment.task.assigned_agent = record.agent.clone();
        assignment.attempt.runtime_binding = Some(binding.clone());
        project.snapshot.tasks[0] = assignment.task.clone();
        project.snapshot.attempts[0] = assignment.attempt.clone();
        project.ledger = Default::default();
        project.ledger.record_durable(assignment.clone()).unwrap();
        assert!(project.ledger.claim_send(&record.operation_id));
        let mut host = CommandCenterHost::new(
            std::env::temp_dir().join(format!("cc-inert-remote-{}", uuid::Uuid::new_v4())),
            &egui::Context::default(),
        );
        host.projects.insert(record.workspace.0.clone(), project);
        let id = record.operation_id.0.clone();
        let now = std::time::Instant::now();
        host.remotes.insert(
            id.clone(),
            Pending {
                record,
                intent: None,
                verified: None,
                lease: Some(lease.clone()),
                assignment: Some(assignment),
                execution: None,
                generation: 0,
                request_id: 44,
                stage,
                deadline: now + std::time::Duration::from_secs(5),
                next_observe: now,
            },
        );
        (host, lease, id)
    }
    #[test]
    fn remote_route_is_opaque_and_matches_only_current_attempt_binding() {
        let (mut host, _, id) = host_with_pending(Stage::Done);
        let task = host.projects["workspace-a"].snapshot.tasks[0].clone();
        let target = target_for(&host, &task).expect("owned remote route");
        assert_eq!(target.workspace_id, "workspace-a");
        assert_eq!(target.task_id.as_deref(), Some(task.id.0.as_str()));
        assert_eq!(target.session_id, Some(format!("remote:{id}")));
        assert!(
            host.route(&target).is_none(),
            "never a local FleetTarget alias"
        );
        host.projects
            .get_mut("workspace-a")
            .unwrap()
            .snapshot
            .attempts[0]
            .runtime_binding
            .as_mut()
            .unwrap()
            .execution_generation += 1;
        assert!(target_for(&host, &task).is_none());
    }
    #[test]
    fn launch_reply_requires_exact_lease_and_never_reapplies_after_transition() {
        let (mut host, lease, id) = host_with_pending(Stage::LaunchWait);
        let mut changed = lease.clone();
        changed.request.workspace_id = WorkspaceId("workspace-b".into());
        launch_receipt(
            &mut host,
            wm::remote_controller::RemoteLaunchReceipt {
                request_id: 44,
                lease: changed,
                result: runtime::remote_worker::RemoteWorkResult::Launched {
                    session: runtime::SessionId(99),
                    execution_generation: 2,
                },
            },
        );
        assert!(host.remotes[&id].stage == Stage::LaunchWait);
        launch_receipt(
            &mut host,
            wm::remote_controller::RemoteLaunchReceipt {
                request_id: 44,
                lease: lease.clone(),
                result: runtime::remote_worker::RemoteWorkResult::Launched {
                    session: runtime::SessionId(7),
                    execution_generation: 19,
                },
            },
        );
        assert!(host.remotes[&id].stage == Stage::AttemptSave);
        assert_eq!(
            host.remotes[&id]
                .record
                .binding
                .as_ref()
                .unwrap()
                .execution_generation,
            19
        );
        launch_receipt(
            &mut host,
            wm::remote_controller::RemoteLaunchReceipt {
                request_id: 44,
                lease,
                result: runtime::remote_worker::RemoteWorkResult::Launched {
                    session: runtime::SessionId(8),
                    execution_generation: 20,
                },
            },
        );
        assert_eq!(
            host.remotes[&id]
                .record
                .binding
                .as_ref()
                .unwrap()
                .session_id,
            runtime::SessionId(7)
        );
    }
    #[test]
    fn remote_admission_requires_actual_input_acceptance_and_never_claims_provider_running() {
        let (mut host, lease, id) = host_with_pending(Stage::DeliverWait);
        let operation = lease.request.operation_id.clone();
        let target = cc::RouteTarget {
            workspace_id: "workspace-a".into(),
            task_id: Some(host.remotes[&id].record.task_id.0.clone()),
            ..Default::default()
        };
        assert!(handoff_pending(&host, &target));
        input_receipt(
            &mut host,
            wm::remote_controller::RemoteInputReceipt {
                request_id: 44,
                lease,
                operation_id: operation,
                result: runtime::remote_input::RemoteTaskInputResult::Admitted,
                accepted: Some(wm::remote_controller::RemoteInputAcceptance {
                    sequence: 8,
                    at_micros: 1234567,
                }),
            },
        );
        assert!(!handoff_pending(&host, &target));
        assert_eq!(
            host.projects["workspace-a"].snapshot.tasks[0].state,
            work::WorkState::Queued
        );
        assert_eq!(
            host.projects["workspace-a"].snapshot.outbox[0].state,
            stored::DispatchOutboxState::Admitted
        );
        assert_eq!(
            host.receipts.back().unwrap().status,
            cc::ReceiptStatus::Acknowledged
        );
        let (mut host, lease, _) = host_with_pending(Stage::DeliverWait);
        input_receipt(
            &mut host,
            wm::remote_controller::RemoteInputReceipt {
                request_id: 44,
                operation_id: lease.request.operation_id.clone(),
                lease,
                result: runtime::remote_input::RemoteTaskInputResult::Admitted,
                accepted: None,
            },
        );
        assert_eq!(
            host.projects["workspace-a"].snapshot.tasks[0].state,
            work::WorkState::ObservationUnknown
        );
        assert_eq!(
            host.receipts.back().unwrap().status,
            cc::ReceiptStatus::Uncertain
        );
    }
    #[test]
    fn queued_remote_handoff_cannot_target_replaced_or_manually_completed_attempt() {
        let (mut host, _, id) = host_with_pending(Stage::ClaimSave);
        assert!(current_attempt(
            &host.projects["workspace-a"],
            &host.remotes[&id]
        ));
        host.projects.get_mut("workspace-a").unwrap().snapshot.tasks[0].state =
            work::WorkState::Completed;
        assert!(!current_attempt(
            &host.projects["workspace-a"],
            &host.remotes[&id]
        ));
        host.projects.get_mut("workspace-a").unwrap().snapshot.tasks[0].state =
            work::WorkState::Queued;
        host.projects.get_mut("workspace-a").unwrap().snapshot.tasks[0].current_attempt_id =
            Some(work::AttemptId::new());
        assert!(!current_attempt(
            &host.projects["workspace-a"],
            &host.remotes[&id]
        ));
    }
    #[test]
    fn known_rejection_releases_attempt_while_lost_input_receipt_retains_uncertainty() {
        let (mut host, lease, _) = host_with_pending(Stage::DeliverWait);
        let operation = lease.request.operation_id.clone();
        input_receipt(
            &mut host,
            wm::remote_controller::RemoteInputReceipt {
                request_id: 44,
                lease,
                operation_id: operation,
                result: runtime::remote_input::RemoteTaskInputResult::Rejected {
                    reason: runtime::PtyInputRejectReason::AdmissionDenied,
                },
                accepted: None,
            },
        );
        let task = &host.projects["workspace-a"].snapshot.tasks[0];
        assert_eq!(task.state, work::WorkState::Failed);
        assert!(host.projects["workspace-a"].state().can_dispatch(&task.id));
        let (mut host, lease, _) = host_with_pending(Stage::DeliverWait);
        input_receipt(
            &mut host,
            wm::remote_controller::RemoteInputReceipt {
                request_id: 44,
                operation_id: lease.request.operation_id.clone(),
                lease,
                result: runtime::remote_input::RemoteTaskInputResult::ObservationUnknown,
                accepted: None,
            },
        );
        let task = &host.projects["workspace-a"].snapshot.tasks[0];
        assert_eq!(task.state, work::WorkState::ObservationUnknown);
        assert!(!host.projects["workspace-a"].state().can_dispatch(&task.id));
    }
    #[test]
    fn restored_remote_launch_claim_never_replays_or_loses_terminal_history_after_thirty_two() {
        let (mut host, _, id) = host_with_pending(Stage::LaunchWait);
        let mut record = host.remotes[&id].record.clone();
        host.remotes.clear();
        for _ in 0..64 {
            record.operation_id = work::OperationId::new();
            record.attempt_id = work::AttemptId::new();
            record.phase = Phase::Rejected;
            save_record(host.projects.get_mut("workspace-a").unwrap(), &record).unwrap();
        }
        record.operation_id = work::OperationId::new();
        record.phase = Phase::Requested;
        let unknown_id = record.operation_id.0.clone();
        save_record(host.projects.get_mut("workspace-a").unwrap(), &record).unwrap();
        restore(&mut host, "workspace-a");
        assert_eq!(host.remotes.len(), 65);
        assert!(host.remotes[&unknown_id].record.phase == Phase::Unknown);
        assert!(!host.remotes.values().any(|p| p.pending()));
        assert!(host.pending.is_empty());
    }
}
