//! Read-only remote viewports and explicit, durably identified native input/close effects.
use super::*;
use wm::remote_controller::{
    RemoteCloseReceipt, RemoteDeviceJob, RemoteInputReceipt, RemoteSessionObservation,
};

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum Kind {
    Input,
    Close,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum Outcome {
    Pending,
    Requested,
    Admitted,
    Closed,
    Rejected,
    Unknown,
}
#[derive(Clone, Serialize, Deserialize)]
struct ActionRecord {
    operation: work::OperationId,
    source: String,
    target: cc::RouteTarget,
    attempt: work::AttemptId,
    binding: work::RuntimeBinding,
    config_revision: work::ConfigRevision,
    kind: Kind,
    outcome: Outcome,
    accepted_sequence: Option<u64>,
    accepted_at_micros: Option<i64>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionStage {
    Save,
    Observe,
    ObserveWait,
    ClaimSave,
    Send,
    Await,
    Done,
}
struct Action {
    record: ActionRecord,
    draft: String,
    lease: ExecutionLease,
    execution: Option<Box<runtime::remote_input::RemoteExecutionProof>>,
    verified: Option<VerifiedExecutionLease>,
    stage: ActionStage,
    generation: u64,
    token: u64,
    deadline: std::time::Instant,
    next_observe: std::time::Instant,
}
struct View {
    observation: RemoteSessionObservation,
    text: Option<String>,
    cache: terminal::renderer_egui::TerminalRenderCache,
    generation: u64,
}
struct Watch {
    permit: Arc<web_remote::cloud::CloudControlPermit>,
    expires: std::time::Instant,
}
#[derive(Default)]
pub(in super::super) struct Sessions {
    actions: BTreeMap<String, Action>,
    views: BTreeMap<String, View>,
    observing: Option<(u64, String)>,
    next_view: Option<std::time::Instant>,
    watches: BTreeMap<cc::RouteTarget, Watch>,
    watch_cursor: usize,
}
pub(in super::super) fn has_watches(host: &CommandCenterHost) -> bool {
    !host.remote_sessions.watches.is_empty()
}
pub(in super::super) fn request_view(
    host: &mut CommandCenterHost,
    id: &str,
    target: &cc::RouteTarget,
) -> Result<(), String> {
    if !is_target(host, target) {
        return Err("현재 원격 시도·실행 대상이 바뀌었습니다.".into());
    }
    let permit = host
        .remote_permits
        .get(id)
        .cloned()
        .ok_or("현재 원격 화면 읽기 권한을 확인할 수 없습니다.")?;
    if !permit.is_current() {
        return Err("원격 화면 읽기 권한이 만료되었습니다.".into());
    }
    if host.remote_sessions.watches.len() >= 8 && !host.remote_sessions.watches.contains_key(target)
    {
        return Err("동시에 조회할 수 있는 원격 화면 한도입니다.".into());
    }
    host.remote_sessions.watches.insert(
        target.clone(),
        Watch {
            permit,
            expires: std::time::Instant::now() + std::time::Duration::from_secs(5),
        },
    );
    host.finish(
        id,
        cc::ReceiptStatus::Acknowledged,
        "정확한 원격 실행의 읽기 전용 화면 조회를 요청했습니다.",
    );
    Ok(())
}
pub(in super::super) fn public_terminal(
    host: &CommandCenterHost,
    target: &cc::RouteTarget,
    budget: &mut usize,
) -> Option<cc::RemoteTerminal> {
    let (key, _) = source(host, target)?;
    let view = host.remote_sessions.views.get(key)?;
    let snapshot = view.observation.terminal.as_ref()?;
    let text = bounded(
        view.text.as_deref().unwrap_or_default(),
        (*budget).min(32 * 1024),
    );
    *budget = budget.saturating_sub(text.len());
    Some(cc::RemoteTerminal {
        text,
        cols: snapshot.cols,
        rows: snapshot.rows,
        connected: view.observation.connected,
        closed: closed(host, key),
        status: status(host, target).unwrap_or_default(),
    })
}
fn source<'a>(
    host: &'a CommandCenterHost,
    target: &cc::RouteTarget,
) -> Option<(&'a str, &'a Pending)> {
    let key = target.session_id.as_deref()?.strip_prefix("remote:")?;
    let pending = host.remotes.get(key)?;
    let project = host.projects.get(&target.workspace_id)?;
    let task = project
        .snapshot
        .tasks
        .iter()
        .find(|t| Some(&t.id.0) == target.task_id.as_ref())?;
    if target_for(host, task).as_ref() != Some(target) {
        return None;
    }
    Some((host.remotes.get_key_value(key)?.0.as_str(), pending))
}
pub(in super::super) fn is_target(host: &CommandCenterHost, target: &cc::RouteTarget) -> bool {
    source(host, target).is_some()
}
fn action_current(project: &Project, record: &ActionRecord) -> bool {
    (record.kind == Kind::Close || project.snapshot.config.revision == record.config_revision)
        && project.snapshot.tasks.iter().any(|t| {
            Some(&t.id.0) == record.target.task_id.as_ref()
                && t.current_attempt_id.as_ref() == Some(&record.attempt)
                && (record.kind == Kind::Close
                    || matches!(
                        t.state,
                        work::WorkState::Queued
                            | work::WorkState::Running
                            | work::WorkState::WaitingApproval
                            | work::WorkState::WaitingInput
                    ))
        })
        && project.snapshot.attempts.iter().any(|a| {
            a.id == record.attempt
                && a.runtime_binding.as_ref() == Some(&record.binding)
                && a.config.revision == record.config_revision
        })
}
fn store(project: &mut Project, record: &ActionRecord) -> Result<(), String> {
    set_draft(
        project,
        &format!("remote-action:{}", record.operation.0),
        &serde_json::to_string(record).map_err(|e| e.to_string())?,
        matches!(record.outcome, Outcome::Requested | Outcome::Unknown),
    )
}
pub(in super::super) fn restore(host: &mut CommandCenterHost, workspace: &str) {
    let Some(project) = host.projects.get_mut(workspace) else {
        return;
    };
    // No restored action can reconstruct a private TLS lease or authorize another send.
    for draft in &mut project.snapshot.drafts {
        if !draft.target_key.starts_with("remote-action:") {
            continue;
        }
        match serde_json::from_str::<ActionRecord>(&draft.text) {
            Ok(mut record)
                if record.target.workspace_id == workspace
                    && draft.target_key == format!("remote-action:{}", record.operation.0) =>
            {
                if matches!(record.outcome, Outcome::Pending | Outcome::Requested) {
                    record.outcome = Outcome::Unknown;
                    draft.text = serde_json::to_string(&record).unwrap_or_default();
                    draft.delivery_uncertain = true;
                }
            }
            _ => {
                project.storage_blocked = true;
            }
        }
    }
}
pub(in super::super) fn pending(host: &CommandCenterHost) -> bool {
    host.remote_sessions
        .actions
        .values()
        .any(|a| a.stage != ActionStage::Done)
        || host.remote_sessions.observing.is_some()
}
pub(in super::super) fn target_pending(host: &CommandCenterHost, target: &cc::RouteTarget) -> bool {
    host.remote_sessions
        .actions
        .values()
        .any(|a| a.record.target == *target && a.stage != ActionStage::Done)
}
pub(in super::super) fn project_pending(host: &CommandCenterHost, workspace: &str) -> bool {
    host.remote_sessions
        .actions
        .values()
        .any(|a| a.record.target.workspace_id == workspace && a.stage != ActionStage::Done)
}
fn blocked(host: &CommandCenterHost, target: &cc::RouteTarget) -> bool {
    host.projects.get(&target.workspace_id).is_some_and(|p| {
        p.snapshot.drafts.iter().any(|d| {
            d.target_key.starts_with("remote-action:")
                && serde_json::from_str::<ActionRecord>(&d.text)
                    .is_ok_and(|r| r.target == *target && r.outcome == Outcome::Unknown)
        })
    })
}
fn closed(host: &CommandCenterHost, key: &str) -> bool {
    host.remote_sessions
        .views
        .get(key)
        .is_some_and(|v| v.observation.exit_code.is_some())
        || host
            .remote_sessions
            .actions
            .values()
            .any(|a| a.record.source == key && a.record.outcome == Outcome::Closed)
}
pub(in super::super) fn input_cap(
    host: &CommandCenterHost,
    target: &cc::RouteTarget,
) -> cc::Capability {
    let Some((key, pending)) = source(host, target) else {
        return no("현재 원격 실행을 확인할 수 없습니다.");
    };
    let project = &host.projects[&target.workspace_id];
    if project.snapshot.config.revision != pending.record.config_revision
        || pending.record.phase != Phase::Admitted
        || !current_attempt(project, pending)
        || closed(host, key)
        || blocked(host, target)
    {
        no("현재 입력 가능한 원격 시도와 실제 승인 상태를 먼저 확인해 주세요.")
    } else if target_pending(host, target) || pending.pending() {
        no("이 실행의 실제 응답을 기다리고 있습니다.")
    } else {
        yes()
    }
}
pub(in super::super) fn close_cap(
    host: &CommandCenterHost,
    target: &cc::RouteTarget,
) -> cc::Capability {
    let Some((key, pending)) = source(host, target) else {
        return no("소유한 원격 실행이 없습니다.");
    };
    if closed(host, key) {
        no("이미 종료된 원격 실행입니다.")
    } else if target_pending(host, target) || pending.pending() || blocked(host, target) {
        no("실제 원격 응답 또는 미확인 종료를 먼저 확인해 주세요.")
    } else if pending.execution.is_some() {
        yes()
    } else {
        no("현재 원격 실행의 프로세스·권한 증명이 필요합니다.")
    }
}
pub(in super::super) fn status(
    host: &CommandCenterHost,
    target: &cc::RouteTarget,
) -> Option<String> {
    let (key, _) = source(host, target)?;
    let view = host.remote_sessions.views.get(key)?;
    Some(if view.observation.exit_code.is_some() {
        "원격 실행 종료 확인 · 작업 결과와 검증은 별도".into()
    } else if !view.observation.connected {
        "원격 실행 연결 끊김 · 작업 관측 불명".into()
    } else {
        format!(
            "원격 실행 관측: {:?} · 작업 시작·완료 판정과 별도",
            view.observation.detected_status
        )
    })
}
pub(in super::super) fn begin(
    host: &mut CommandCenterHost,
    id: &str,
    target: &cc::RouteTarget,
    text: Option<String>,
    attempt_id: Option<&str>,
) -> Result<(), String> {
    if host.remote_permits.contains_key(id) {
        return Err("이 컴퓨터에서 원격 실행을 조작해 주세요.".into());
    }
    let cap = if text.is_some() {
        input_cap(host, target)
    } else {
        close_cap(host, target)
    };
    if !cap.enabled {
        return Err(cap.reason);
    }
    if host.flight.is_some()
        || host
            .remote_sessions
            .actions
            .values()
            .filter(|a| a.stage != ActionStage::Done)
            .count()
            >= 32
    {
        return Err("저장 또는 원격 요청 응답을 기다려 주세요.".into());
    }
    let (key, pending) = source(host, target).ok_or("원격 실행이 바뀌었습니다.")?;
    if attempt_id.is_some_and(|id| id != pending.record.attempt_id.0) {
        return Err("종료하려는 시도가 바뀌었습니다.".into());
    }
    let record = ActionRecord {
        operation: work::OperationId::from_string(id.into()).map_err(|e| e.to_string())?,
        source: key.into(),
        target: target.clone(),
        attempt: pending.record.attempt_id.clone(),
        binding: pending
            .record
            .binding
            .clone()
            .ok_or("실행 바인딩이 없습니다.")?,
        config_revision: pending.record.config_revision,
        kind: if text.is_some() {
            Kind::Input
        } else {
            Kind::Close
        },
        outcome: Outcome::Pending,
        accepted_sequence: None,
        accepted_at_micros: None,
    };
    let lease = pending.lease.clone().ok_or("현재 임대가 없습니다.")?;
    let execution = pending.execution.clone();
    let verified = pending.verified.clone();
    let draft = text.unwrap_or_default();
    if record.kind == Kind::Input {
        work::validate_input(&draft).map_err(|e| e.to_string())?;
    }
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
        .any(|d| d.target_key == format!("remote-action:{id}"))
    {
        return Err("이미 사용한 입력·종료 ID입니다. 다시 보내지 않았습니다.".into());
    }
    store(project, &record)?;
    let generation = project.generation;
    host.save(&target.workspace_id, None, None)?;
    let now = std::time::Instant::now();
    host.remote_sessions.actions.insert(
        id.into(),
        Action {
            record,
            draft,
            lease,
            execution,
            verified,
            stage: ActionStage::Save,
            generation,
            token: 0,
            deadline: now + std::time::Duration::from_secs(30),
            next_observe: now,
        },
    );
    Ok(())
}
fn settle(host: &mut CommandCenterHost, id: &str, outcome: Outcome, detail: &str) {
    let Some(mut action) = host.remote_sessions.actions.remove(id) else {
        return;
    };
    if action.stage == ActionStage::Done {
        host.remote_sessions.actions.insert(id.into(), action);
        return;
    }
    action.record.outcome = outcome;
    action.stage = ActionStage::Done;
    host.devices.finish_remote(action.token);
    if let Some(project) = host.projects.get_mut(&action.record.target.workspace_id) {
        if let Err(error) = store(project, &action.record) {
            project.storage_blocked = true;
            host.status = error;
        }
    }
    if outcome == Outcome::Admitted
        && clear_admitted_draft(&mut host.ui, &action.record.target, &action.draft)
        && let Ok(key) = draft_key(host, &action.record.target)
        && let Some(project) = host.projects.get_mut(&action.record.target.workspace_id)
    {
        let _ = set_draft(project, &key, "", false);
    }
    let status = if matches!(outcome, Outcome::Admitted | Outcome::Closed) {
        cc::ReceiptStatus::Acknowledged
    } else if outcome == Outcome::Unknown {
        cc::ReceiptStatus::Uncertain
    } else {
        cc::ReceiptStatus::Rejected
    };
    host.remote_sessions.actions.insert(id.into(), action);
    host.finish(id, status, detail);
    host.publication_dirty = true;
}
fn exact_execution(action: &Action, proof: &runtime::remote_input::RemoteExecutionProof) -> bool {
    proof.target.identity.attempt_id == action.record.attempt
        && proof.target.identity.workspace_id.0 == action.record.target.workspace_id
        && proof.target.identity.operation_id == action.lease.request.operation_id
        && proof.target.identity.device_id == action.lease.request.device_id
        && proof.target.identity.incarnation == action.record.binding.incarnation
        && proof.target.session == action.record.binding.session_id
        && proof.target.execution_generation == action.record.binding.execution_generation
        && proof.profile == action.lease.request.agent
}
pub(in super::super) fn observed(
    host: &mut CommandCenterHost,
    token: u64,
    lease: &ExecutionLease,
    execution: Option<Box<runtime::remote_input::RemoteExecutionProof>>,
    verified: Option<VerifiedExecutionLease>,
) -> bool {
    let Some((id, action)) = host.remote_sessions.actions.iter_mut().find(|(_, a)| {
        a.token == token && a.stage == ActionStage::ObserveWait && a.lease == *lease
    }) else {
        return false;
    };
    let id = id.clone();
    host.devices.finish_remote(token);
    if let (Some(proof), Some(verified)) = (execution, verified) {
        if !exact_execution(action, &proof)
            || verified.lease() != lease
            || verified.validate_fresh().is_err()
        {
            settle(
                host,
                &id,
                Outcome::Rejected,
                "원격 실행의 실제 프로세스·권한이 바뀌어 입력하지 않았습니다.",
            );
        } else {
            action.execution = Some(proof);
            action.verified = Some(verified);
            action.stage = ActionStage::ClaimSave;
        }
    } else {
        action.stage = ActionStage::Observe;
        action.next_observe = std::time::Instant::now() + std::time::Duration::from_millis(500);
    }
    true
}
pub(in super::super) fn input_pending(
    host: &mut CommandCenterHost,
    token: u64,
    lease: &ExecutionLease,
    operation: &work::OperationId,
) -> bool {
    let matches = host
        .remote_sessions
        .actions
        .get(&operation.0)
        .is_some_and(|a| {
            a.stage == ActionStage::Await
                && a.token == token
                && a.lease == *lease
                && a.record.kind == Kind::Input
        });
    if matches {
        host.devices.finish_remote(token);
    }
    matches
}
pub(in super::super) fn input_receipt(
    host: &mut CommandCenterHost,
    receipt: &RemoteInputReceipt,
) -> bool {
    let id = &receipt.operation_id.0;
    if !host.remote_sessions.actions.get(id).is_some_and(|a| {
        a.token == receipt.request_id
            && a.stage == ActionStage::Await
            && a.lease == receipt.lease
            && a.record.kind == Kind::Input
    }) {
        return false;
    }
    match &receipt.result {
        runtime::remote_input::RemoteTaskInputResult::Admitted
            if receipt
                .accepted
                .as_ref()
                .is_some_and(|a| a.sequence > 0 && a.at_micros > 0) =>
        {
            let action = host.remote_sessions.actions.get_mut(id).unwrap();
            action.record.accepted_sequence = receipt.accepted.as_ref().map(|a| a.sequence);
            action.record.accepted_at_micros = receipt.accepted.as_ref().map(|a| a.at_micros);
            settle(
                host,
                id,
                Outcome::Admitted,
                "정확한 원격 실행의 입력 승인을 확인했습니다.",
            );
        }
        runtime::remote_input::RemoteTaskInputResult::Rejected { .. } => settle(
            host,
            id,
            Outcome::Rejected,
            "원격 실행이 입력을 거부했습니다. 초안을 보존합니다.",
        ),
        _ => settle(
            host,
            id,
            Outcome::Unknown,
            "원격 입력 승인 여부가 불명확합니다. 자동 재전송하지 않습니다.",
        ),
    }
    true
}
pub(in super::super) fn close_receipt(host: &mut CommandCenterHost, receipt: RemoteCloseReceipt) {
    let id = &receipt.operation_id.0;
    if !host.remote_sessions.actions.get(id).is_some_and(|a| {
        a.token == receipt.request_id
            && a.stage == ActionStage::Await
            && a.lease == receipt.lease
            && a.record.kind == Kind::Close
    }) {
        return;
    }
    match receipt.result {
        runtime::remote_input::RemoteSessionCloseResult::Closed => {
            let source = host.remote_sessions.actions[id].record.source.clone();
            let record = host.remote_sessions.actions[id].record.clone();
            if let Some(project) = host.projects.get_mut(&record.target.workspace_id)
                && project.snapshot.tasks.iter().any(|t| {
                    Some(&t.id.0) == record.target.task_id.as_ref()
                        && t.current_attempt_id.as_ref() == Some(&record.attempt)
                        && matches!(
                            t.state,
                            work::WorkState::Queued
                                | work::WorkState::Running
                                | work::WorkState::WaitingApproval
                                | work::WorkState::WaitingInput
                        )
                })
            {
                let _=apply_user_work_event(project,&record.target,&record.attempt.0,work::WorkEventKind::ObservationLost,"사용자가 정확한 원격 워커 실행을 종료했습니다. 작업 완료나 검증 통과를 뜻하지 않습니다.".into());
            }
            if let Some(view) = host.remote_sessions.views.get_mut(&source) {
                view.observation.exit_code = Some(None);
            }
            settle(
                host,
                id,
                Outcome::Closed,
                "정확한 원격 워커의 프로세스 종료와 실행 용량 반환을 확인했습니다. 작업 결과·검증 기록은 보존합니다.",
            );
        }
        runtime::remote_input::RemoteSessionCloseResult::Rejected { .. } => settle(
            host,
            id,
            Outcome::Rejected,
            "현재 원격 실행과 소유권이 일치하지 않아 종료하지 않았습니다.",
        ),
        _ => settle(
            host,
            id,
            Outcome::Unknown,
            "원격 워커 종료 여부를 확인하지 못했습니다. 자동 종료·재실행하지 않습니다.",
        ),
    }
}
pub(in super::super) fn failed(host: &mut CommandCenterHost, token: u64, unknown: bool) -> bool {
    let id = host
        .remote_sessions
        .actions
        .iter()
        .find(|(_, a)| a.token == token && a.stage != ActionStage::Done)
        .map(|(id, _)| id.clone());
    if let Some(id) = id {
        let unknown = unknown && host.remote_sessions.actions[&id].stage == ActionStage::Await;
        settle(
            host,
            &id,
            if unknown {
                Outcome::Unknown
            } else {
                Outcome::Rejected
            },
            "원격 입력·종료 응답을 확인하지 못했습니다. 자동 재전송하지 않습니다.",
        );
        true
    } else if host
        .remote_sessions
        .observing
        .as_ref()
        .is_some_and(|(id, _)| *id == token)
    {
        host.remote_sessions.observing = None;
        host.devices.finish_remote(token);
        true
    } else {
        false
    }
}
pub(in super::super) fn session_observed(
    host: &mut CommandCenterHost,
    token: Option<u64>,
    observation: RemoteSessionObservation,
) {
    let key = host
        .remotes
        .iter()
        .find(|(_, p)| {
            p.lease.as_ref() == Some(&observation.lease)
                && p.record.binding.as_ref().is_some_and(|b| {
                    b.session_id == observation.session
                        && b.execution_generation == observation.execution_generation
                        && b.incarnation == observation.lease.incarnation
                })
        })
        .map(|(id, _)| id.clone());
    let Some(key) = key else {
        return;
    };
    if let Some(token) = token {
        if !host
            .remote_sessions
            .observing
            .as_ref()
            .is_some_and(|(id, expected)| *id == token && expected == &key)
        {
            return;
        }
        host.devices.finish_remote(token);
        host.remote_sessions.observing = None;
    }
    let changed = host.remote_sessions.views.get(&key).is_none_or(|v| {
        v.observation.detected_status != observation.detected_status
            || v.observation.exit_code != observation.exit_code
            || v.observation.connected != observation.connected
    });
    if changed {
        host.publication_dirty = true;
    }
    if let Some(view) = host.remote_sessions.views.get_mut(&key) {
        if view.observation.terminal != observation.terminal {
            view.generation = view.generation.saturating_add(1);
            view.text = observation.terminal.as_ref().map(|s| {
                bounded(
                    &crate::cloud_agent::screen_text_with_truncation(s).0,
                    32 * 1024,
                )
            });
            host.publication_dirty = true;
        }
        view.observation = observation;
    } else {
        if host.remote_sessions.views.len() >= 32
            && let Some(oldest) = host.remote_sessions.views.keys().next().cloned()
        {
            host.remote_sessions.views.remove(&oldest);
        }
        host.remote_sessions.views.insert(
            key,
            View {
                text: observation.terminal.as_ref().map(|s| {
                    bounded(
                        &crate::cloud_agent::screen_text_with_truncation(s).0,
                        32 * 1024,
                    )
                }),
                observation,
                cache: Default::default(),
                generation: 1,
            },
        );
    }
}
pub(in super::super) fn render(
    host: &mut CommandCenterHost,
    ui: &mut egui::Ui,
    rect: egui::Rect,
    target: &cc::RouteTarget,
    config: &crate::config::TerminalConfig,
    scale: f32,
) -> bool {
    let Some((key, _)) = source(host, target) else {
        return false;
    };
    let key = key.to_owned();
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .id_salt(("remote-readonly-terminal", &key)),
    );
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    child
        .painter()
        .rect_filled(rect, 0.0, terminal::renderer_egui::TERMINAL_SURFACE_BG);
    if let Some(view) = host.remote_sessions.views.get_mut(&key)
        && let Some(snapshot) = &view.observation.terminal
    {
        egui::ScrollArea::both()
            .id_salt(("remote-viewport-scroll", &key))
            .show(&mut child, |ui| {
                let metrics = terminal::renderer_egui::CellMetrics {
                    font_size: config.font_size / scale,
                    line_height: config.line_height,
                };
                let cell = terminal::renderer_egui::cell_size(ui.ctx(), metrics);
                ui.set_min_size(egui::vec2(
                    cell.x * f32::from(snapshot.cols),
                    cell.y * f32::from(snapshot.rows),
                ));
                terminal::renderer_egui::draw(
                    ui,
                    snapshot,
                    metrics,
                    &mut view.cache,
                    None,
                    false,
                    None,
                    view.generation,
                );
            });
    } else {
        child.colored_label(
            egui::Color32::LIGHT_GRAY,
            "정확한 원격 실행의 읽기 전용 화면을 조회합니다. 입력은 아래 작성란을 사용합니다.",
        );
    }
    true
}
impl App {
    pub(in super::super) fn pump_command_center_remote_sessions(
        &mut self,
        host: &mut CommandCenterHost,
        visible: bool,
    ) {
        let now = std::time::Instant::now();
        host.remote_sessions
            .watches
            .retain(|_, watch| now < watch.expires && watch.permit.is_current());
        if !host.remote_sessions.watches.is_empty() {
            self.egui_ctx
                .request_repaint_after(std::time::Duration::from_millis(500));
        }
        let ids: Vec<_> = host
            .remote_sessions
            .actions
            .iter()
            .filter(|(_, a)| a.stage != ActionStage::Done)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let mut action = host.remote_sessions.actions.remove(&id).unwrap();
            let result = (|| -> Result<(), String> {
                let project = host
                    .projects
                    .get_mut(&action.record.target.workspace_id)
                    .ok_or("프로젝트가 없습니다.")?;
                if !action_current(project, &action.record) || project.storage_blocked {
                    return Err(
                        "현재 원격 시도·실행·저장 상태가 바뀌어 새 요청을 보내지 않았습니다."
                            .into(),
                    );
                }
                if std::time::Instant::now() > action.deadline {
                    return Err("원격 입력·종료 응답 시간이 만료되었습니다.".into());
                }
                if host.flight.is_some() || host.devices.has_pending() {
                    return Ok(());
                }
                match action.stage {
                    ActionStage::Save if project.saved_generation >= action.generation => {
                        action.stage = if action.record.kind == Kind::Close
                            && action.execution.is_some()
                            && action
                                .verified
                                .as_ref()
                                .is_some_and(|v| v.validate_fresh().is_ok())
                        {
                            ActionStage::ClaimSave
                        } else {
                            ActionStage::Observe
                        };
                    }
                    ActionStage::Observe if std::time::Instant::now() >= action.next_observe => {
                        host.next_token = host.next_token.saturating_add(1);
                        action.token = host.next_token;
                        host.devices.queue_remote(
                            action.token,
                            RemoteDeviceJob::ObserveExecution {
                                request_id: action.token,
                                lease: action.lease.clone(),
                                session: action.record.binding.session_id,
                                execution_generation: action.record.binding.execution_generation,
                            },
                        )?;
                        action.stage = ActionStage::ObserveWait;
                    }
                    ActionStage::ClaimSave => {
                        action.record.outcome = Outcome::Requested;
                        store(project, &action.record)?;
                        action.generation = project.generation;
                        host.save(&action.record.target.workspace_id, None, None)?;
                        action.stage = ActionStage::Send;
                    }
                    ActionStage::Send if project.saved_generation >= action.generation => {
                        let verified = action
                            .verified
                            .clone()
                            .ok_or("원격 실행 권한이 없습니다.")?;
                        verified
                            .validate_fresh()
                            .map_err(|_| "원격 실행 권한이 만료되었습니다.")?;
                        let fence = project.effect_fence.clone();
                        let authority = wm::remote_controller::RemoteSendAuthority::new(
                            action.record.operation.clone(),
                            std::time::Instant::now() + std::time::Duration::from_secs(15),
                            move |effect| {
                                fence.with_current(&mut || {
                                    if verified.validate_fresh().is_ok() {
                                        effect();
                                    }
                                })
                            },
                        )
                        .map_err(|e| format!("권한 오류: {e:?}"))?;
                        let execution = action
                            .execution
                            .clone()
                            .ok_or("실제 원격 프로세스 증명이 없습니다.")?;
                        if !exact_execution(&action, &execution) {
                            return Err("실제 원격 실행 대상이 바뀌었습니다.".into());
                        }
                        host.next_token = host.next_token.saturating_add(1);
                        action.token = host.next_token;
                        let job = match action.record.kind {
                            Kind::Input => RemoteDeviceJob::DeliverInput {
                                request_id: action.token,
                                lease: action.lease.clone(),
                                execution,
                                operation_id: action.record.operation.clone(),
                                prompt: action.draft.clone(),
                                authority,
                            },
                            Kind::Close => RemoteDeviceJob::CloseSession {
                                request_id: action.token,
                                lease: action.lease.clone(),
                                execution,
                                operation_id: action.record.operation.clone(),
                                authority,
                            },
                        };
                        host.devices.queue_remote(action.token, job)?;
                        action.stage = ActionStage::Await;
                    }
                    _ => {}
                }
                Ok(())
            })();
            let unknown = action.stage == ActionStage::Await;
            host.remote_sessions.actions.insert(id.clone(), action);
            if let Err(error) = result {
                settle(
                    host,
                    &id,
                    if unknown {
                        Outcome::Unknown
                    } else {
                        Outcome::Rejected
                    },
                    &error,
                );
            }
        }
        if pending(host) {
            self.egui_ctx
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
        if host.devices.has_pending() {
            return;
        }
        if host
            .remote_sessions
            .next_view
            .is_some_and(|at| std::time::Instant::now() < at)
        {
            return;
        }
        let mut targets: Vec<_> = host.remote_sessions.watches.keys().cloned().collect();
        if let Some(native) = visible
            .then(|| host.ui.terminal_target.clone())
            .flatten()
            .filter(|t| is_target(host, t))
            && !targets.contains(&native)
        {
            targets.push(native);
        }
        if targets.is_empty() {
            return;
        }
        let target = targets
            .get(host.remote_sessions.watch_cursor % targets.len())
            .cloned();
        host.remote_sessions.watch_cursor = (host.remote_sessions.watch_cursor + 1) % targets.len();
        let Some(target) = target else {
            return;
        };
        let Some((key, pending)) = source(host, &target) else {
            return;
        };
        if closed(host, key) {
            return;
        }
        let key = key.to_owned();
        let Some(lease) = pending.lease.clone() else {
            return;
        };
        let binding = pending.record.binding.as_ref().unwrap().clone();
        host.next_token = host.next_token.saturating_add(1);
        let token = host.next_token;
        let job = RemoteDeviceJob::ObserveSession {
            request_id: token,
            lease,
            session: binding.session_id,
            execution_generation: binding.execution_generation,
        };
        let mut job = Some(job);
        let mut admitted = false;
        let permit = host
            .remote_sessions
            .watches
            .get(&target)
            .map(|w| w.permit.clone());
        let mut queue = || {
            if let Some(job) = job.take() {
                admitted = host.devices.queue_remote(token, job).is_ok();
            }
        };
        if let Some(permit) = permit {
            permit.with_current(&mut queue);
        } else {
            queue();
        }
        if admitted {
            host.remote_sessions.observing = Some((token, key));
            host.remote_sessions.next_view =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(500));
        }
        self.egui_ctx
            .request_repaint_after(std::time::Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn host_action(
        stage: ActionStage,
        kind: Kind,
    ) -> (CommandCenterHost, String, ExecutionLease, cc::RouteTarget) {
        let (mut host, lease, source) = super::super::tests::host_with_pending(Stage::Done);
        host.remotes.get_mut(&source).unwrap().record.phase = Phase::Admitted;
        let task = host.projects["workspace-a"].snapshot.tasks[0].clone();
        let target = target_for(&host, &task).unwrap();
        let pending = &host.remotes[&source];
        let now = std::time::Instant::now();
        let id = work::OperationId::new();
        let record = ActionRecord {
            operation: id.clone(),
            source,
            target: target.clone(),
            attempt: pending.record.attempt_id.clone(),
            binding: pending.record.binding.clone().unwrap(),
            config_revision: pending.record.config_revision,
            kind,
            outcome: Outcome::Requested,
            accepted_sequence: None,
            accepted_at_micros: None,
        };
        host.remote_sessions.actions.insert(
            id.0.clone(),
            Action {
                record,
                draft: "exact input".into(),
                lease: lease.clone(),
                execution: None,
                verified: None,
                stage,
                generation: 0,
                token: 77,
                deadline: now + std::time::Duration::from_secs(30),
                next_observe: now,
            },
        );
        (host, id.0, lease, target)
    }
    #[test]
    fn viewport_wrong_lease_never_consumes_another_exact_query() {
        let (mut host, _, lease, _) = host_action(ActionStage::Done, Kind::Input);
        let source = host.remotes.keys().next().unwrap().clone();
        host.remote_sessions.observing = Some((93, source));
        let mut wrong = lease;
        wrong.request.workspace_id = WorkspaceId("other-project".into());
        session_observed(
            &mut host,
            Some(93),
            RemoteSessionObservation {
                lease: wrong,
                session: runtime::SessionId(7),
                execution_generation: 19,
                terminal: None,
                detected_status: None,
                exit_code: None,
                connected: true,
            },
        );
        assert!(host.remote_sessions.observing.is_some());
        assert!(host.remote_sessions.views.is_empty());
    }
    #[test]
    fn read_only_failure_does_not_claim_an_unsent_input_is_uncertain() {
        let (mut host, id, _, _) = host_action(ActionStage::ObserveWait, Kind::Input);
        assert!(failed(&mut host, 77, true));
        assert!(host.remote_sessions.actions[&id].record.outcome == Outcome::Rejected);
    }
    #[test]
    fn actual_followup_admission_preserves_newer_draft_and_never_starts_task() {
        let (mut host, id, lease, target) = host_action(ActionStage::Await, Kind::Input);
        host.ui.set_draft(target.clone(), "newer draft".into());
        let state = host.projects["workspace-a"].snapshot.tasks[0].state;
        let receipt = RemoteInputReceipt {
            request_id: 77,
            lease,
            operation_id: work::OperationId::from_string(id.clone()).unwrap(),
            result: runtime::remote_input::RemoteTaskInputResult::Admitted,
            accepted: Some(wm::remote_controller::RemoteInputAcceptance {
                sequence: 2,
                at_micros: 123,
            }),
        };
        assert!(input_receipt(&mut host, &receipt));
        assert_eq!(host.ui.draft(&target), "newer draft");
        assert_eq!(host.projects["workspace-a"].snapshot.tasks[0].state, state);
        assert!(host.remote_sessions.actions[&id].record.outcome == Outcome::Admitted);
        assert!(!input_receipt(&mut host, &receipt));
    }
    #[test]
    fn exact_close_cannot_terminate_replacement_attempt_and_preserves_verified_result() {
        let (mut host, id, lease, target) = host_action(ActionStage::Await, Kind::Close);
        host.projects.get_mut("workspace-a").unwrap().snapshot.tasks[0].state =
            work::WorkState::Completed;
        let mut wrong = lease.clone();
        wrong.request.attempt_id = work::AttemptId::new();
        close_receipt(
            &mut host,
            RemoteCloseReceipt {
                request_id: 77,
                lease: wrong,
                operation_id: work::OperationId::from_string(id.clone()).unwrap(),
                result: runtime::remote_input::RemoteSessionCloseResult::Closed,
            },
        );
        assert!(host.remote_sessions.actions[&id].stage == ActionStage::Await);
        close_receipt(
            &mut host,
            RemoteCloseReceipt {
                request_id: 77,
                lease,
                operation_id: work::OperationId::from_string(id.clone()).unwrap(),
                result: runtime::remote_input::RemoteSessionCloseResult::Closed,
            },
        );
        assert!(host.remote_sessions.actions[&id].record.outcome == Outcome::Closed);
        assert!(!input_cap(&host, &target).enabled);
        assert!(!close_cap(&host, &target).enabled);
        assert_eq!(
            host.projects["workspace-a"].snapshot.tasks[0].state,
            work::WorkState::Completed
        );
    }
    #[test]
    fn close_of_active_execution_records_observation_lost_without_claiming_completion() {
        let (mut host, id, lease, _) = host_action(ActionStage::Await, Kind::Close);
        close_receipt(
            &mut host,
            RemoteCloseReceipt {
                request_id: 77,
                lease,
                operation_id: work::OperationId::from_string(id).unwrap(),
                result: runtime::remote_input::RemoteSessionCloseResult::Closed,
            },
        );
        assert_eq!(
            host.projects["workspace-a"].snapshot.tasks[0].state,
            work::WorkState::ObservationUnknown
        );
    }
    #[test]
    fn config_change_blocks_old_input_but_keeps_exact_owned_worker_close_available() {
        let (mut host, id, _, _) = host_action(ActionStage::Save, Kind::Close);
        let record = host.remote_sessions.actions[&id].record.clone();
        let project = host.projects.get_mut("workspace-a").unwrap();
        project.snapshot.config.revision = work::ConfigRevision(99);
        assert!(action_current(project, &record));
        let mut input = record;
        input.kind = Kind::Input;
        assert!(!action_current(project, &input));
    }
    #[test]
    fn public_remote_view_is_actual_scoped_text_and_utf8_budgeted() {
        use terminal::TerminalBackend;
        let (mut host, _, lease, target) = host_action(ActionStage::Done, Kind::Input);
        let mut backend = terminal::AlacrittyBackend::new(40, 3, 10);
        backend
            .feed("실제 원격 한글 a\u{301}\u{308}".as_bytes())
            .unwrap();
        session_observed(
            &mut host,
            None,
            RemoteSessionObservation {
                lease,
                session: runtime::SessionId(7),
                execution_generation: 19,
                terminal: Some(Arc::new(backend.viewport_snapshot().unwrap())),
                detected_status: None,
                exit_code: None,
                connected: true,
            },
        );
        let mut budget = 32 * 1024;
        let view = public_terminal(&host, &target, &mut budget).unwrap();
        assert!(view.text.contains("실제 원격 한글"));
        assert!(view.text.contains("a\u{301}\u{308}"));
        assert_eq!((view.cols, view.rows), (40, 3));
        assert!(view.connected);
        assert!(!view.closed);
        let mut tiny = 4;
        let view = public_terminal(&host, &target, &mut tiny).unwrap();
        assert!(view.text.len() <= 4);
        assert!(std::str::from_utf8(view.text.as_bytes()).is_ok());
        let mut wrong = target;
        wrong.workspace_id = "another-workspace".into();
        assert!(public_terminal(&host, &wrong, &mut budget).is_none());
    }
    #[test]
    fn readonly_watch_requires_original_view_permit_and_never_changes_native_navigation() {
        let (mut host, _, _, target) = host_action(ActionStage::Done, Kind::Input);
        let view = host.ui.view;
        assert!(request_view(&mut host, &uuid::Uuid::new_v4().to_string(), &target).is_err());
        assert!(host.remote_sessions.watches.is_empty());
        assert_eq!(host.ui.view, view);
        let intent = cc::Intent::ObserveRemoteSession { target };
        let value = serde_json::to_value(&intent).unwrap();
        assert_eq!(value["type"], "observe_remote_session");
        assert_eq!(serde_json::from_value::<cc::Intent>(value).unwrap(), intent);
    }
}
