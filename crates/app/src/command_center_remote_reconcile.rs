//! A user-requested query can reconcile a retained receipt; it never re-arms an effect.
use super::*;
use wm::remote_controller::{RemoteDeviceJob, RemoteInputReceipt, RemoteLaunchReceipt};
#[cfg(test)]
#[path = "command_center_remote_reconcile_storage_tests.rs"]
mod storage_tests;
enum Source {
    Launch(String),
    Dispatch(String),
    Input(String),
}
struct Query {
    click: String,
    source: Source,
    lease: ExecutionLease,
    deadline: std::time::Instant,
}
#[derive(Default)]
pub(in super::super) struct Queries {
    pending: BTreeMap<u64, Query>,
}
pub(in super::super) fn pending(host: &CommandCenterHost) -> bool {
    !host.reconciles.pending.is_empty()
}
pub(in super::super) fn project_pending(host: &CommandCenterHost, workspace: &str) -> bool {
    host.reconciles
        .pending
        .values()
        .any(|query| query.lease.request.workspace_id.0 == workspace)
}
fn storage_ready(host: &CommandCenterHost, workspace: &str) -> bool {
    host.projects
        .get(workspace)
        .is_some_and(|project| project.loaded && !project.storage_blocked)
        && !host.requested_loads.contains_key(workspace)
        && !matches!(&host.flight, Some(Flight::Load { workspace: loading, .. }) if loading == workspace)
}
fn candidate(
    host: &CommandCenterHost,
    target: &cc::RouteTarget,
) -> Option<(Source, ExecutionLease)> {
    let project = host.projects.get(&target.workspace_id)?;
    let task = project
        .snapshot
        .tasks
        .iter()
        .find(|t| Some(&t.id.0) == target.task_id.as_ref())?;
    if let Some((id, p)) = host.remotes.iter().find(|(_, p)| {
        p.record.workspace.0 == target.workspace_id
            && Some(&p.record.task_id.0) == target.task_id.as_ref()
            && p.record.phase == Phase::Unknown
            && p.stage == Stage::Done
            && p.lease.is_some()
            && (task.current_attempt_id.as_ref() == Some(&p.record.attempt_id)
                || p.assignment.is_none() && task.current_attempt_id.is_none())
    }) {
        return Some((
            if p.assignment.is_some() {
                Source::Dispatch(id.clone())
            } else {
                Source::Launch(id.clone())
            },
            p.lease.clone()?,
        ));
    }
    sessions::uncertain_input(host, target).map(|(id, lease)| (Source::Input(id), lease))
}
pub(in super::super) fn capability(
    host: &CommandCenterHost,
    target: &cc::RouteTarget,
) -> cc::Capability {
    if !storage_ready(host, &target.workspace_id) {
        no("프로젝트 저장 복구와 불러오기를 마친 뒤 원래 요청의 상태를 확인해 주세요.")
    } else if pending(host) {
        no("현재 원래 요청의 상태를 조회하고 있습니다.")
    } else if candidate(host, target).is_some() {
        yes()
    } else {
        no(
            "원래 요청·임대가 이 연결에 남아 있어야 읽기 전용 상태 조회를 할 수 있습니다. 자료가 없는 재시작 후에는 관측 불명 상태를 보존합니다.",
        )
    }
}
pub(in super::super) fn idle_attempt(
    host: &CommandCenterHost,
    task: &work::Task,
) -> Option<String> {
    idle_owner(host, task).map(|(_, p)| p.record.attempt_id.0.clone())
}
pub(in super::super) fn idle_owner<'a>(
    host: &'a CommandCenterHost,
    task: &work::Task,
) -> Option<(&'a str, &'a Pending)> {
    if task.current_attempt_id.is_some() {
        return None;
    }
    host.remotes
        .iter()
        .find(|(id, p)| {
            p.record.task_id == task.id
                && p.record.workspace == task.workspace_id
                && p.record.phase == Phase::ObservedIdle
                && p.record.binding.is_some()
                && p.lease.is_some()
                && !sessions::closed(host, id)
        })
        .map(|(id, p)| (id.as_str(), p))
}
pub(in super::super) fn has_idle(host: &CommandCenterHost, task: &work::Task) -> bool {
    idle_owner(host, task).is_some()
}
pub(in super::super) fn ensure_no_idle_worker(
    host: &CommandCenterHost,
    target: &cc::RouteTarget,
) -> Result<(), String> {
    if host
        .projects
        .get(&target.workspace_id)
        .and_then(|project| {
            project
                .snapshot
                .tasks
                .iter()
                .find(|task| Some(&task.id.0) == target.task_id.as_ref())
        })
        .is_some_and(|task| has_idle(host, task))
    {
        return Err("이 작업의 복구된 원격 워커를 먼저 명시적으로 종료해 주세요. 새 실행을 만들지 않았습니다.".into());
    }
    Ok(())
}
pub(in super::super) fn activity(host: &CommandCenterHost, task: &work::Task) -> Option<String> {
    if has_idle(host, task) {
        Some("원래 원격 실행 생성 확인 · 작업 지시 미전달 · 필요하면 실행을 명시적으로 종료해 주세요.".into())
    } else if host.remotes.values().any(|p| {
        p.record.task_id == task.id
            && p.record.workspace == task.workspace_id
            && p.record.phase == Phase::Unknown
    }) {
        Some("원래 원격 실행·입력의 관측 불명 · 자동 재전송 없이 상태 확인이 필요합니다.".into())
    } else {
        None
    }
}
pub(in super::super) fn begin(
    host: &mut CommandCenterHost,
    id: &str,
    target: &cc::RouteTarget,
) -> Result<(), String> {
    if host.remote_permits.contains_key(id) {
        return Err("이 컴퓨터에서 원래 원격 요청의 상태를 확인해 주세요.".into());
    }
    if !storage_ready(host, &target.workspace_id) {
        return Err(
            "프로젝트 저장 복구와 불러오기를 마친 뒤 원래 요청의 상태를 확인해 주세요.".into(),
        );
    }
    if pending(host) || host.devices.has_pending() {
        return Err("현재 기기 조회 응답을 기다려 주세요.".into());
    }
    let (source, lease) = candidate(host, target).ok_or(
        "조회할 원래 요청·임대가 현재 연결에 남아 있지 않습니다. 자동 재실행하지 않습니다.",
    )?;
    host.next_token = host.next_token.saturating_add(1);
    let token = host.next_token;
    let job = match &source {
        Source::Launch(_) => RemoteDeviceJob::ReconcileLaunch {
            request_id: token,
            lease: lease.clone(),
        },
        Source::Dispatch(op) | Source::Input(op) => RemoteDeviceJob::ReconcileInput {
            request_id: token,
            lease: lease.clone(),
            operation_id: work::OperationId::from_string(op.clone()).map_err(|e| e.to_string())?,
        },
    };
    host.devices.queue_remote(token, job)?;
    host.reconciles.pending.insert(
        token,
        Query {
            click: id.into(),
            source,
            lease,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(15),
        },
    );
    Ok(())
}
fn finish(host: &mut CommandCenterHost, token: u64, detail: &str) {
    if let Some(query) = host.reconciles.pending.remove(&token) {
        host.devices.finish_remote(token);
        host.finish(&query.click, cc::ReceiptStatus::Acknowledged, detail);
        host.publication_dirty = true;
    }
}
pub(in super::super) fn launch_receipt(
    host: &mut CommandCenterHost,
    receipt: &RemoteLaunchReceipt,
) -> bool {
    let Some(query) = host
        .reconciles
        .pending
        .get(&receipt.request_id)
        .filter(|q| q.lease == receipt.lease)
    else {
        return false;
    };
    let Source::Launch(id) = &query.source else {
        return false;
    };
    let id = id.clone();
    let Some(p) = host.remotes.get_mut(&id).filter(|p| {
        p.record.phase == Phase::Unknown
            && p.stage == Stage::Done
            && p.lease.as_ref() == Some(&receipt.lease)
    }) else {
        finish(
            host,
            receipt.request_id,
            "원래 실행 대상이 바뀌어 조회 결과를 적용하지 않았습니다.",
        );
        return true;
    };
    match &receipt.result {
        runtime::remote_worker::RemoteWorkResult::Launched {
            session,
            execution_generation,
        } if *execution_generation > 0 => {
            p.record.binding = Some(work::RuntimeBinding {
                incarnation: receipt.lease.incarnation.clone(),
                session_id: *session,
                execution_generation: *execution_generation,
            });
            p.record.phase = Phase::ObservedIdle;
            // Deliberately keep Stage::Done: no prepare_dispatch, prompt reconstruction or send.
        }
        runtime::remote_worker::RemoteWorkResult::Rejected { .. } => {
            p.record.phase = Phase::Rejected
        }
        _ => {}
    }
    if let Some(project) = host.projects.get_mut(&p.record.workspace.0)
        && let Err(error) = save_record(project, &p.record)
    {
        project.storage_blocked = true;
        host.status = error;
    }
    finish(
        host,
        receipt.request_id,
        "원래 실행 요청의 실제 상태를 조회했습니다. 새 워커나 작업 입력을 보내지 않았습니다.",
    );
    true
}
pub(in super::super) fn input_receipt(
    host: &mut CommandCenterHost,
    receipt: &RemoteInputReceipt,
) -> bool {
    let Some(query) = host
        .reconciles
        .pending
        .get(&receipt.request_id)
        .filter(|q| q.lease == receipt.lease)
    else {
        return false;
    };
    let (source, id) = match &query.source {
        Source::Dispatch(id) => (false, id.clone()),
        Source::Input(id) => (true, id.clone()),
        _ => return false,
    };
    if id != receipt.operation_id.0 {
        return false;
    }
    if source {
        sessions::reconciled_input(host, &id, receipt);
    } else if let Some(p) = host.remotes.get_mut(&id).filter(|p| {
        p.record.phase == Phase::Unknown
            && p.stage == Stage::Done
            && p.lease.as_ref() == Some(&receipt.lease)
    }) {
        // Re-enter only the receipt validator within this call, never an effect pump stage.
        p.stage = Stage::DeliverWait;
        p.request_id = receipt.request_id;
        super::input_receipt(host, receipt.clone());
    }
    finish(
        host,
        receipt.request_id,
        "원래 입력 요청의 실제 승인 영수증을 조회했습니다. 입력을 재전송하지 않았습니다.",
    );
    true
}
pub(in super::super) fn failed(host: &mut CommandCenterHost, token: u64) -> bool {
    if host.reconciles.pending.contains_key(&token) {
        finish(
            host,
            token,
            "원래 요청의 실제 상태를 아직 확인할 수 없습니다. 관측 불명 기록을 보존하고 재전송하지 않습니다.",
        );
        true
    } else {
        false
    }
}
impl App {
    pub(in super::super) fn pump_command_center_reconcile(&mut self, host: &mut CommandCenterHost) {
        let expired: Vec<_> = host
            .reconciles
            .pending
            .iter()
            .filter(|(_, q)| std::time::Instant::now() > q.deadline)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            failed(host, id);
        }
        if pending(host) {
            self.egui_ctx
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(launch: bool) -> (CommandCenterHost, ExecutionLease, String, u64) {
        let (mut host, lease, id) = super::super::tests::host_with_pending(Stage::Done);
        let p = host.remotes.get_mut(&id).unwrap();
        p.record.phase = Phase::Unknown;
        if launch {
            p.assignment = None;
            p.record.binding = None;
            let project = host.projects.get_mut("workspace-a").unwrap();
            project.snapshot.tasks[0].current_attempt_id = None;
            project.snapshot.attempts.clear();
        } else {
            let assignment = p.assignment.as_ref().unwrap().clone();
            apply_delivery_outcome(
                host.projects.get_mut("workspace-a").unwrap(),
                &assignment,
                ui::composer::PromptAdmissionOutcome::Unknown,
            );
        }
        let token = 101;
        host.reconciles.pending.insert(
            token,
            Query {
                click: uuid::Uuid::new_v4().to_string(),
                source: if launch {
                    Source::Launch(id.clone())
                } else {
                    Source::Dispatch(id.clone())
                },
                lease: lease.clone(),
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(5),
            },
        );
        (host, lease, id, token)
    }
    #[test]
    fn recovered_idle_blocks_a_stale_native_new_launch_at_the_host_guard() {
        let (mut host, lease, _, token) = fixture(true);
        launch_receipt(
            &mut host,
            &RemoteLaunchReceipt {
                request_id: token,
                lease,
                result: runtime::remote_worker::RemoteWorkResult::Launched {
                    session: runtime::SessionId(7),
                    execution_generation: 19,
                },
            },
        );
        let task = &host.projects["workspace-a"].snapshot.tasks[0];
        let target = target_for(&host, task).unwrap();
        assert!(ensure_no_idle_worker(&host, &target).is_err());
    }
    #[test]
    fn closed_recovered_idle_does_not_hide_the_next_recovered_worker() {
        let (mut host, lease, old, token) = fixture(true);
        launch_receipt(
            &mut host,
            &RemoteLaunchReceipt {
                request_id: token,
                lease: lease.clone(),
                result: runtime::remote_worker::RemoteWorkResult::Launched {
                    session: runtime::SessionId(7),
                    execution_generation: 19,
                },
            },
        );
        sessions::session_observed(
            &mut host,
            None,
            wm::remote_controller::RemoteSessionObservation {
                lease,
                session: runtime::SessionId(7),
                execution_generation: 19,
                terminal: None,
                detected_status: None,
                exit_code: Some(Some(0)),
                connected: true,
            },
        );
        assert!(sessions::closed(&host, &old));
        let task = host.projects["workspace-a"].snapshot.tasks[0].clone();
        assert!(!has_idle(&host, &task));

        // The newer operation sorts after the closed one; both historical records remain.
        let (mut next, mut lease, next_id, _) = fixture(true);
        let new = "ffffffff-ffff-4fff-bfff-ffffffffffff";
        let mut worker = next.remotes.remove(&next_id).unwrap();
        worker.record.operation_id = work::OperationId::from_string(new.into()).unwrap();
        worker.record.task_id = task.id.clone();
        lease.request.operation_id = worker.record.operation_id.clone();
        worker.lease = Some(lease.clone());
        let attempt = worker.record.attempt_id.0.clone();
        host.remotes.insert(new.into(), worker);
        host.reconciles.pending.insert(
            102,
            Query {
                click: uuid::Uuid::new_v4().to_string(),
                source: Source::Launch(new.into()),
                lease: lease.clone(),
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(5),
            },
        );
        launch_receipt(
            &mut host,
            &RemoteLaunchReceipt {
                request_id: 102,
                lease,
                result: runtime::remote_worker::RemoteWorkResult::Launched {
                    session: runtime::SessionId(8),
                    execution_generation: 20,
                },
            },
        );
        let target = target_for(&host, &task).unwrap();
        assert_eq!(target.session_id, Some(format!("remote:{new}")));
        assert_eq!(idle_attempt(&host, &task), Some(attempt));
        assert!(sessions::close_cap(&host, &target).enabled);
        assert!(has_idle(&host, &task));
    }
    #[test]
    fn recovered_launch_only_installs_owned_idle_binding_and_never_rearms_initial_prompt() {
        let (mut host, lease, id, token) = fixture(true);
        assert!(launch_receipt(
            &mut host,
            &RemoteLaunchReceipt {
                request_id: token,
                lease,
                result: runtime::remote_worker::RemoteWorkResult::Launched {
                    session: runtime::SessionId(7),
                    execution_generation: 19
                }
            }
        ));
        let p = &host.remotes[&id];
        assert!(p.record.phase == Phase::ObservedIdle);
        assert!(p.stage == Stage::Done);
        assert!(p.assignment.is_none());
        assert!(p.execution.is_none());
        assert!(host.inputs.is_empty());
        let task = &host.projects["workspace-a"].snapshot.tasks[0];
        assert!(task.current_attempt_id.is_none());
        assert!(host.projects["workspace-a"].snapshot.attempts.is_empty());
        let target = target_for(&host, task).unwrap();
        assert!(sessions::close_cap(&host, &target).enabled);
        assert!(!sessions::input_cap(&host, &target).enabled);
        assert!(!pending(&host));
        assert!(has_idle(&host, task));
    }
    #[test]
    fn query_requires_original_lease_and_unknown_reply_preserves_unknown_without_replay() {
        let (mut host, lease, id, token) = fixture(true);
        let mut wrong = lease.clone();
        wrong.request.operation_id = work::OperationId::new();
        assert!(!launch_receipt(
            &mut host,
            &RemoteLaunchReceipt {
                request_id: token,
                lease: wrong,
                result: runtime::remote_worker::RemoteWorkResult::ObservationUnknown
            }
        ));
        assert!(pending(&host));
        assert!(launch_receipt(
            &mut host,
            &RemoteLaunchReceipt {
                request_id: token,
                lease,
                result: runtime::remote_worker::RemoteWorkResult::ObservationUnknown
            }
        ));
        assert!(host.remotes[&id].record.phase == Phase::Unknown);
        assert!(host.remotes[&id].stage == Stage::Done);
        assert!(host.remotes[&id].record.binding.is_none());
        assert!(host.inputs.is_empty());
    }
    #[test]
    fn recovered_input_requires_original_operation_and_actual_sequence_without_claiming_started() {
        let (mut host, lease, id, token) = fixture(false);
        let operation = lease.request.operation_id.clone();
        assert!(input_receipt(
            &mut host,
            &RemoteInputReceipt {
                request_id: token,
                lease,
                operation_id: operation.clone(),
                result: runtime::remote_input::RemoteTaskInputResult::Admitted,
                accepted: Some(wm::remote_controller::RemoteInputAcceptance {
                    sequence: 4,
                    at_micros: 1234
                })
            }
        ));
        assert!(host.remotes[&id].record.phase == Phase::Admitted);
        assert!(host.remotes[&id].stage == Stage::Done);
        assert_eq!(
            host.projects["workspace-a"].ledger.stage(&operation),
            Some(wm::dispatch::DeliveryState::Admitted)
        );
        assert_eq!(
            host.projects["workspace-a"].snapshot.tasks[0].state,
            work::WorkState::ObservationUnknown
        );
        assert!(host.inputs.is_empty());
        let (mut host, lease, id, token) = fixture(false);
        assert!(input_receipt(
            &mut host,
            &RemoteInputReceipt {
                request_id: token,
                operation_id: lease.request.operation_id.clone(),
                lease,
                result: runtime::remote_input::RemoteTaskInputResult::Admitted,
                accepted: None
            }
        ));
        assert!(host.remotes[&id].record.phase == Phase::Unknown);
    }
    #[test]
    fn restart_without_retained_original_request_does_not_fabricate_recovery_authority() {
        let (mut host, _, _, _) = fixture(true);
        let task = host.projects["workspace-a"].snapshot.tasks[0].clone();
        let target = cc::RouteTarget {
            workspace_id: task.workspace_id.0,
            task_id: Some(task.id.0),
            session_id: None,
        };
        host.reconciles.pending.clear();
        for p in host.remotes.values_mut() {
            p.lease = None;
            p.intent = None;
        }
        assert!(!capability(&host, &target).enabled);
        assert!(candidate(&host, &target).is_none());
    }
}
