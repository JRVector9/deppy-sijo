//! Command Center adapter for exact, admitted Codex turns. Generic PTY status is never evidence.
use super::*;
use crate::cloud_agent::completion::{LocalCompletionState, WatchTarget};
use wm::observation::{CodexTracker, FinalResponseWorker, ObservationIdentity, ObservedTurn};

struct Watch {
    identity: ObservationIdentity,
    target: WatchTarget,
    route: cc::RouteTarget,
    config_revision: work::ConfigRevision,
    capture: Capture,
    started: bool,
    reading: bool,
}
struct Delayed {
    target: FleetPromptTarget,
    prompt: String,
    runtime_token: String,
    deadline: std::time::Instant,
}
pub(super) struct Observations {
    tracker: CodexTracker,
    worker: FinalResponseWorker,
    native: BTreeMap<String, storage::NativeTurnProof>,
    sequences: std::collections::HashMap<(u64, runtime::SessionId), u64>,
    watches: BTreeMap<String, Watch>,
    delayed: BTreeMap<String, Delayed>,
    reads_inflight: std::collections::BTreeSet<String>,
}
impl Observations {
    pub fn new(ctx: &egui::Context) -> Self {
        let wake = ctx.clone();
        Self {
            tracker: CodexTracker::default(),
            worker: FinalResponseWorker::new(move || wake.request_repaint()),
            native: BTreeMap::new(),
            sequences: std::collections::HashMap::new(),
            watches: BTreeMap::new(),
            delayed: BTreeMap::new(),
            reads_inflight: std::collections::BTreeSet::new(),
        }
    }
    pub(super) fn has_pending(&self) -> bool {
        !self.watches.is_empty() || !self.delayed.is_empty() || !self.reads_inflight.is_empty()
    }
    fn begin(
        &mut self,
        host_input: &Input,
        live: &LiveRoute,
        id: &str,
        prompt: &str,
        runtime_token: &str,
        native: &str,
        config_revision: work::ConfigRevision,
        attempt_id: Option<work::AttemptId>,
    ) -> Result<(), &'static str> {
        if self.watches.len() >= 64 {
            return Err("observation_capacity");
        }
        let source = live
            .capture
            .prompt
            .as_ref()
            .ok_or("observation_execution_missing")?;
        let binding = binding(
            live,
            host_input
                .target
                .session_id
                .as_deref()
                .ok_or("observation_route_missing")?,
        )
        .ok_or("observation_binding_missing")?;
        let identity = ObservationIdentity {
            workspace_id: WorkspaceId(source.workspace_id.clone()),
            operation_id: work::OperationId::from_string(id.into())
                .map_err(|_| "observation_operation_invalid")?,
            attempt_id,
            binding,
        };
        let target = WatchTarget {
            workspace: source.workspace_id.clone(),
            session_id: host_input.target.session_id.clone().unwrap(),
            generation: format!(
                "{}:{}",
                source.runtime_instance, identity.binding.execution_generation
            ),
            runtime: source.runtime_instance,
            session: source.session,
            execution: source.execution,
            runtime_token: runtime_token.into(),
            native_session_id: native.into(),
        };
        let key = format!("{}:{}", source.workspace_id, source.session.0);
        let sequence = self
            .sequences
            .get(&(source.runtime_instance, source.session))
            .copied();
        let digest = storage::native_prompt_digest(prompt);
        if let (Some(baseline), Some(sequence)) = (self.native.get(&key), sequence) {
            self.tracker.begin(
                identity.clone(),
                target.clone(),
                baseline,
                sequence,
                digest,
                micros(),
            )?;
        } else if live.runtime_binding.is_some() {
            // Only an exact idle-launch result grants the initial sequence-zero watermark.
            self.tracker.begin_fresh(
                identity.clone(),
                target.clone(),
                sequence.unwrap_or(0),
                digest,
                micros(),
            )?;
        } else {
            return Err("observation_baseline_missing");
        }
        self.watches.insert(
            id.into(),
            Watch {
                identity,
                target,
                route: host_input.target.clone(),
                config_revision,
                capture: live.capture.clone(),
                started: false,
                reading: false,
            },
        );
        Ok(())
    }
}
fn micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_micros().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}
impl App {
    pub(in crate::app) fn command_center_native_keys(
        &self,
        mut base: Vec<String>,
        completion: bool,
    ) -> Vec<String> {
        if let Some(host) = &self.command_center {
            let visible = self.command_center_layout.mode == layout::LayoutMode::CommandCenter;
            let extra = if completion || !visible {
                host.observations.tracker.native_turn_keys()
            } else {
                host.live
                    .iter()
                    .filter(|l| l.agent.as_ref().is_some_and(|a| a.provider == "codex"))
                    .filter_map(|l| l.capture.prompt.as_ref())
                    .map(|p| format!("{}:{}", p.workspace_id, p.session.0))
                    .take(64)
                    .collect()
            };
            for key in extra {
                if !base.contains(&key) && base.len() < if completion { 64 } else { 256 } {
                    base.push(key);
                }
            }
        }
        base
    }
    pub(in crate::app) fn command_center_native_turns(
        &mut self,
        proofs: &[storage::NativeTurnProof],
    ) {
        if let Some(host) = self.command_center.as_mut() {
            host.observations.native = proofs
                .iter()
                .take(256)
                .map(|p| (p.session_key.clone(), p.clone()))
                .collect();
        }
    }
    pub(super) fn stage_command_center_observed_prompt(
        &mut self,
        target: FleetPromptTarget,
        prompt: String,
        id: String,
    ) {
        let Some(mut host) = self.command_center.take() else {
            return;
        };
        let live = host
            .inputs
            .get(&id)
            .and_then(|input| host.route(&input.target))
            .cloned();
        let runtime_token = self
            .active
            .id
            .eq(&target.workspace_id)
            .then_some(&self.active)
            .or_else(|| self.warm.get(&target.workspace_id))
            .filter(|r| r.runtime_instance == target.runtime_instance)
            .map(|r| r.runtime.instance_token().to_owned());
        let codex = live
            .as_ref()
            .is_some_and(|l| l.agent.as_ref().is_some_and(|a| a.provider == "codex"));
        let mut delayed = false;
        if codex && host.observations.delayed.len() < 64 {
            if let Some(token) = runtime_token {
                let request = launch::SettingsRequest::NativeIdentity {
                    operation_id: id.clone(),
                    workspace: target.workspace_id.clone(),
                    session: target.session,
                    runtime_instance: target.runtime_instance,
                    runtime_token: token.clone(),
                };
                if self.queue_global_settings_action(
                    &target.workspace_id,
                    SettingsJobAction::CommandCenter(request),
                ) {
                    host.observations.delayed.insert(
                        id.clone(),
                        Delayed {
                            target: target.clone(),
                            prompt: prompt.clone(),
                            runtime_token: token,
                            deadline: std::time::Instant::now()
                                + std::time::Duration::from_secs(20),
                        },
                    );
                    self.egui_ctx
                        .request_repaint_after(std::time::Duration::from_secs(20));
                    delayed = true;
                }
            }
        }
        self.command_center = Some(host);
        if !delayed {
            self.send_command_center_observed_prompt(target, prompt, id);
        }
    }
    pub(super) fn command_center_native_identity_result(
        &mut self,
        id: String,
        workspace: String,
        session: runtime::SessionId,
        runtime_instance: u64,
        runtime_token: String,
        native: Option<String>,
    ) {
        let Some(mut host) = self.command_center.take() else {
            return;
        };
        let delayed = host.observations.delayed.remove(&id);
        let mut send = None;
        if let Some(delayed) = delayed {
            let current = delayed.target.workspace_id == workspace
                && delayed.target.session == session
                && delayed.target.runtime_instance == runtime_instance
                && delayed.runtime_token == runtime_token
                && std::time::Instant::now() < delayed.deadline;
            let live = host
                .inputs
                .get(&id)
                .and_then(|input| host.route(&input.target))
                .cloned()
                .filter(|l| l.capture.prompt.as_ref() == Some(&delayed.target));
            if current && let Some(live) = live {
                if let (Some(native), Some(input), Some(p)) =
                    (native, host.inputs.get(&id), host.projects.get(&workspace))
                {
                    if host
                        .observations
                        .begin(
                            input,
                            &live,
                            &id,
                            &delayed.prompt,
                            &runtime_token,
                            &native,
                            p.snapshot.config.revision,
                            input
                                .dispatch
                                .as_ref()
                                .map(|a| a.attempt.id.clone())
                                .or_else(|| {
                                    p.snapshot
                                        .tasks
                                        .iter()
                                        .find(|t| Some(&t.id.0) == input.target.task_id.as_ref())
                                        .and_then(|t| t.current_attempt_id.clone())
                                }),
                        )
                        .is_err()
                    {
                        host.status="이 실행의 자동 턴 관측을 증명하지 못했습니다. 실제 대화와 수동 계획·결과 보고를 사용할 수 있습니다.".into();
                    }
                }
                send = Some((delayed.target, delayed.prompt, id.clone()));
            } else {
                reject_delayed(
                    &mut host,
                    &id,
                    "입력 전 실행 대상이나 현재 관측 요청이 바뀌었습니다. 입력을 보내지 않았습니다.",
                );
            }
        }
        self.command_center = Some(host);
        if let Some((target, prompt, id)) = send {
            self.send_command_center_observed_prompt(target, prompt, id);
        }
    }
    fn send_command_center_observed_prompt(
        &mut self,
        target: FleetPromptTarget,
        prompt: String,
        id: String,
    ) {
        let authorized = self.command_center.as_ref().is_some_and(|host| {
            host.inputs.contains_key(&id)
                && host
                    .remote_permits
                    .get(&id)
                    .is_none_or(|permit| permit.is_current())
        });
        if !authorized {
            self.command_center_input_outcome(&id, ui::composer::PromptAdmissionOutcome::Rejected);
            return;
        }
        let automatic = self
            .command_center
            .as_ref()
            .and_then(|h| h.inputs.get(&id))
            .is_some_and(|input| input.dispatch.is_some());
        self.broadcast_prompt_to(
            &target,
            &prompt,
            PromptDeliveryOrigin::CommandCenter {
                automatic,
                operation_id: id,
            },
        );
    }
    pub(super) fn observe_command_center_turn_events(
        &mut self,
        runtime: u64,
        events: &[runtime::RuntimeEvent],
    ) {
        let Some(mut host) = self.command_center.take() else {
            return;
        };
        let mut terminal = vec![];
        for event in events {
            match event {
                runtime::RuntimeEvent::SessionInputAccepted {
                    session,
                    sequence,
                    operation_id,
                    at_micros,
                } => {
                    if host.observations.sequences.len() < 256
                        || host
                            .observations
                            .sequences
                            .contains_key(&(runtime, *session))
                    {
                        host.observations
                            .sequences
                            .entry((runtime, *session))
                            .and_modify(|s| *s = (*s).max(*sequence))
                            .or_insert(*sequence);
                    }
                    terminal.extend(host.observations.tracker.accepted_input(
                        runtime,
                        *session,
                        *sequence,
                        operation_id.as_deref(),
                        *at_micros,
                    ));
                }
                runtime::RuntimeEvent::InputAdmitted {
                    session,
                    operation_id,
                    result,
                } => {
                    if let Some(watch) = host
                        .observations
                        .watches
                        .get(operation_id)
                        .filter(|w| w.target.runtime == runtime && w.target.session == *session)
                    {
                        if let Some(turn) =
                            host.observations.tracker.admit(&watch.identity, *result)
                        {
                            terminal.push(turn);
                        }
                    }
                }
                _ => {}
            }
        }
        for turn in terminal {
            install_turn(&mut host, turn);
        }
        self.command_center = Some(host);
    }
    pub(in crate::app) fn command_center_observation_lost(&mut self, runtime: u64) {
        let Some(mut host) = self.command_center.take() else {
            return;
        };
        let identities: Vec<_> = host
            .observations
            .watches
            .values()
            .filter(|w| w.target.runtime == runtime)
            .map(|w| w.identity.clone())
            .collect();
        for identity in identities {
            if let Some(turn) = host.observations.tracker.revoke(&identity) {
                install_turn(&mut host, turn);
            }
        }
        host.observations
            .sequences
            .retain(|(source, _), _| *source != runtime);
        self.command_center = Some(host);
    }
    pub(super) fn pump_command_center_observations(&mut self, host: &mut CommandCenterHost) {
        let now = micros();
        let expired: Vec<_> = host
            .observations
            .delayed
            .iter()
            .filter(|(_, p)| std::time::Instant::now() >= p.deadline)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            host.observations.delayed.remove(&id);
            reject_delayed(
                host,
                &id,
                "실행 식별 조회 응답이 끊겼습니다. 저장된 작업 입력은 보내지 않았습니다.",
            );
        }
        if let Some(response) = host.observations.worker.try_recv() {
            let id = response.job.identity.operation_id.0.clone();
            host.observations.reads_inflight.remove(&id);
            if let Some(watch) = host.observations.watches.get_mut(&id) {
                watch.reading = false;
            }
            if let Some(watch) = host.observations.watches.get(&id) {
                if let Some(proof) = host.observations.native.get(&format!(
                    "{}:{}",
                    watch.target.workspace, watch.target.session.0
                )) {
                    if let Some(current) = current_identity(host, watch)
                        && let Some(turn) = host.observations.tracker.complete(
                            response,
                            &current,
                            &watch.target,
                            proof,
                            now,
                        )
                    {
                        install_turn(host, turn);
                    }
                }
            }
        }
        let ids: Vec<_> = host.observations.watches.keys().cloned().collect();
        for id in ids {
            let Some(mut watch) = host.observations.watches.remove(&id) else {
                continue;
            };
            let current = current_identity(host, &watch).as_ref() == Some(&watch.identity);
            if !current {
                let turn = host.observations.tracker.revoke(&watch.identity);
                host.observations.watches.insert(id, watch);
                if let Some(turn) = turn {
                    install_turn(host, turn);
                }
                continue;
            }
            let proof = host.observations.native.get(&format!(
                "{}:{}",
                watch.target.workspace, watch.target.session.0
            ));
            let mut terminal = None;
            if let Some(proof) = proof {
                let poll =
                    host.observations
                        .tracker
                        .observe(&watch.identity, &watch.target, proof, now);
                let was_waiting = host
                    .projects
                    .get(&watch.route.workspace_id)
                    .and_then(|p| {
                        p.snapshot
                            .tasks
                            .iter()
                            .find(|t| Some(&t.id.0) == watch.route.task_id.as_ref())
                    })
                    .is_some_and(|t| t.state == work::WorkState::WaitingInput);
                if poll.state == LocalCompletionState::Working
                    && !poll.waiting
                    && (!watch.started || was_waiting)
                {
                    if let Some(attempt) = &watch.identity.attempt_id {
                        if let Some(p) = host.projects.get_mut(&watch.route.workspace_id) {
                            let _ = apply_user_work_event(
                                p,
                                &watch.route,
                                &attempt.0,
                                work::WorkEventKind::Started,
                                "정확한 입력 승인과 새 공급자 턴 시작을 확인했습니다.".into(),
                            );
                        }
                    }
                    watch.started = true;
                }
                if poll.waiting
                    && let Some(attempt) = &watch.identity.attempt_id
                {
                    if let Some(p) = host.projects.get_mut(&watch.route.workspace_id) {
                        let current = p
                            .snapshot
                            .tasks
                            .iter()
                            .find(|t| Some(&t.id.0) == watch.route.task_id.as_ref())
                            .map(|t| t.state);
                        if current != Some(work::WorkState::WaitingInput) {
                            let _=apply_user_work_event(p,&watch.route,&attempt.0,work::WorkEventKind::WaitingInput,"정확한 공급자 턴이 입력을 기다립니다. 승인 요청의 소유권은 별도로 확인합니다.".into());
                        }
                    }
                }
                if !watch.reading
                    && let Some(job) = poll.job
                {
                    match host.observations.worker.try_request(job) {
                        Ok(()) => {
                            watch.reading = true;
                            host.observations.reads_inflight.insert(id.clone());
                        }
                        Err(error) => {
                            let _ = error.into_job();
                        }
                    }
                }
                terminal = poll.terminal;
            }
            host.observations.watches.insert(id, watch);
            if let Some(turn) = terminal {
                install_turn(host, turn);
            }
        }
        for turn in host.observations.tracker.expire(now) {
            install_turn(host, turn);
        }
    }
}
fn reject_delayed(host: &mut CommandCenterHost, id: &str, message: &str) {
    if let Some(input) = host.inputs.remove(id) {
        if let Some(p) = host.projects.get_mut(&input.target.workspace_id) {
            if let Some(assignment) = &input.dispatch {
                apply_delivery_outcome(
                    p,
                    assignment,
                    ui::composer::PromptAdmissionOutcome::Rejected,
                );
            }
            if let Some(marker) = p
                .snapshot
                .drafts
                .iter_mut()
                .find(|d| d.target_key == format!("operation:{id}"))
            {
                marker.delivery_uncertain = false;
                p.dirty();
            }
        }
    }
    host.reject(id, message);
}
fn current_identity(host: &CommandCenterHost, watch: &Watch) -> Option<ObservationIdentity> {
    let live = host
        .route(&watch.route)
        .filter(|live| live.capture == watch.capture)?;
    if !live.capture.prompt.as_ref()?.execution.is_current() {
        return None;
    }
    let p = host
        .projects
        .get(&watch.route.workspace_id)
        .filter(|p| !p.storage_blocked && p.snapshot.config.revision == watch.config_revision)?;
    let binding = binding(live, watch.route.session_id.as_deref()?)?;
    if let Some(id) = &watch.identity.attempt_id {
        let task = p.snapshot.tasks.iter().find(|t| {
            Some(&t.id.0) == watch.route.task_id.as_ref()
                && t.current_attempt_id.as_ref() == Some(id)
        })?;
        let attempt = p.snapshot.attempts.iter().find(|a| {
            &a.id == id && a.task_id == task.id && a.runtime_binding.as_ref() == Some(&binding)
        })?;
        if matches!(
            attempt.state,
            work::WorkState::Failed | work::WorkState::Cancelled | work::WorkState::Completed
        ) {
            return None;
        }
    } else {
        let request = p.request.as_ref()?;
        if request.runtime_binding.as_ref() != Some(&binding)
            || request.goal().submission_id.0 != watch.identity.operation_id.0
        {
            return None;
        }
    }
    Some(ObservationIdentity {
        binding,
        ..watch.identity.clone()
    })
}
fn install_turn(host: &mut CommandCenterHost, turn: ObservedTurn) {
    let id = turn.identity.operation_id.0.clone();
    let Some(watch) = host.observations.watches.remove(&id) else {
        return;
    };
    let exact = current_identity(host, &watch).as_ref() == Some(&turn.identity)
        && watch.identity == turn.identity;
    if !exact {
        if let Some(attempt) = &watch.identity.attempt_id {
            if let Some(p) = host.projects.get_mut(&watch.route.workspace_id) {
                // The old completed payload is discarded; only the still-current exact attempt
                // loses observation. A replacement attempt is never changed by this response.
                let _ = apply_user_work_event(
                    p,
                    &watch.route,
                    &attempt.0,
                    work::WorkEventKind::ObservationLost,
                    "실행 캡처나 설정이 바뀌어 이전 턴의 관측을 종료했습니다.".into(),
                );
            }
        }
        host.publication_dirty = true;
        host.status = "이전 실행의 턴 응답을 현재 작업에 적용하지 않았습니다.".into();
        return;
    }
    let Some(p) = host.projects.get_mut(&watch.route.workspace_id) else {
        return;
    };
    if turn.state == LocalCompletionState::Completed {
        let Some(text) = turn.text else { return };
        if let Some(attempt) = &turn.identity.attempt_id {
            let revision = p
                .snapshot
                .tasks
                .iter()
                .find(|t| Some(&t.id.0) == watch.route.task_id.as_ref())
                .and_then(|t| t.result.as_ref())
                .map(|r| r.revision.next())
                .unwrap_or(Some(work::ResultRevision(1)));
            if let Some(revision) = revision {
                let result = work::TaskResult {
                    revision,
                    summary: bounded(&text, work::WORK_EVENT_MAX_BYTES),
                    source_revision: turn.turn_id,
                    artifact_revision: None,
                    verification: work::VerificationState::NotRun,
                };
                if apply_user_work_event(p,&watch.route,&attempt.0,work::WorkEventKind::ResultSubmitted {result},format!("정확한 공급자 최종 응답 · 입력 순서 {:?} · 관측 리비전 {:?}. 검증은 아직 실행하지 않았습니다.",turn.sequence,turn.proof.as_ref().map(|p|p.completion_revision))).is_ok() {host.status="작업 결과를 수신했습니다. 실제 검증과 사용자 확인이 필요합니다.".into();}
            }
        } else if let Some(request) = &p.request {
            let policy = p.policy();
            match wm::provider_adapter::accept_final_response(request,&turn.identity.binding,&text,wm::provider_adapter::PlannerSupport {structured_final_response:true,tool_execution_restricted:false},&p.snapshot.config,&policy) {
                Ok(proposal)=>{let mut state=p.state();if wm::orchestrator::install_proposal(&mut state,&proposal,&p.snapshot.config,&policy).is_ok() {p.proposal=Some(proposal);p.set_state(state);host.status="메인의 정확한 최종 응답에서 배정안을 검증했습니다. 확인 후 실행해 주세요.".into();}},
                Err(_)=>host.status="메인 응답의 배정 JSON이 스키마와 일치하지 않습니다. 계획 JSON 편집기에서 확인해 주세요.".into(),
            }
        }
    } else if let Some(attempt) = &turn.identity.attempt_id {
        let _ = apply_user_work_event(
            p,
            &watch.route,
            &attempt.0,
            work::WorkEventKind::ObservationLost,
            format!("현재 턴의 관측을 확인하지 못했습니다: {}", turn.reason),
        );
    }
    host.publication_dirty = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (CommandCenterHost, Watch, wm::dispatch::DispatchAssignment) {
        let mut host = super::super::tests::host();
        let (p, assignment) = super::super::tests::attempted();
        let binding = assignment.attempt.runtime_binding.clone().unwrap();
        let execution = crate::agent_detect::AgentExecutionIdentity::fixture_current(
            crate::agent_detect::AgentKind::Codex,
            std::process::id(),
        )
        .unwrap();
        let capture = Capture {
            workspace: "workspace-a".into(),
            target: FleetTarget::Pty {
                tab: runtime::MuxTabId::new(),
                pane: runtime::MuxPaneId::new(),
                session: binding.session_id,
            },
            prompt: Some(FleetPromptTarget {
                workspace_id: "workspace-a".into(),
                runtime_instance: 19,
                session: binding.session_id,
                execution,
            }),
        };
        host.routes.refresh(vec![capture.clone()]);
        host.live.push(LiveRoute {
            capture: capture.clone(),
            title: "actual CLI".into(),
            agent: Some(assignment.task.assigned_agent.clone()),
            state: cc::TaskState::Running,
            cwd: Some(p.snapshot.config.folder.clone()),
            watch: None,
            runtime_binding: Some(binding.clone()),
        });
        let mut route = host.target(&host.live[0]);
        route.task_id = Some(assignment.task.id.0.clone());
        let watch = Watch {
            identity: ObservationIdentity {
                workspace_id: assignment.task.workspace_id.clone(),
                operation_id: assignment.attempt.operation_id.clone(),
                attempt_id: Some(assignment.attempt.id.clone()),
                binding,
            },
            target: WatchTarget {
                workspace: "workspace-a".into(),
                session_id: route.session_id.clone().unwrap(),
                generation: "actual-host-generation".into(),
                runtime: 19,
                session: assignment
                    .attempt
                    .runtime_binding
                    .as_ref()
                    .unwrap()
                    .session_id,
                execution,
                runtime_token: "actual-runtime-token".into(),
                native_session_id: uuid::Uuid::new_v4().to_string(),
            },
            route,
            config_revision: p.snapshot.config.revision,
            capture,
            started: false,
            reading: false,
        };
        host.projects.insert("workspace-a".into(), p);
        (host, watch, assignment)
    }
    #[test]
    fn final_response_current_binding_is_derived_from_host_not_the_job_copy() {
        let (mut host, watch, _) = fixture();
        assert_eq!(
            current_identity(&host, &watch),
            Some(watch.identity.clone())
        );
        host.live[0]
            .runtime_binding
            .as_mut()
            .unwrap()
            .execution_generation += 1;
        assert!(
            current_identity(&host, &watch).is_none(),
            "stale job identity cannot authorize a new generation"
        );
    }
    #[test]
    fn same_pane_replacement_or_new_current_attempt_cannot_receive_old_final_text() {
        let (mut host, watch, _) = fixture();
        host.live[0].capture.prompt.as_mut().unwrap().execution =
            crate::agent_detect::AgentExecutionIdentity::fixture(
                crate::agent_detect::AgentKind::Codex,
                99,
            );
        host.routes.refresh(vec![host.live[0].capture.clone()]);
        assert!(current_identity(&host, &watch).is_none());
        let (mut host, watch, _) = fixture();
        host.projects.get_mut("workspace-a").unwrap().snapshot.tasks[0].current_attempt_id =
            Some(work::AttemptId::new());
        assert!(current_identity(&host, &watch).is_none());
    }
    #[test]
    fn rollback_keeps_pending_observation_or_final_reader_reaping_active() {
        let (mut host, watch, _) = fixture();
        assert!(!host.observations.has_pending());
        let id = watch.identity.operation_id.0.clone();
        host.observations.watches.insert(id.clone(), watch);
        assert!(host.observations.has_pending());
        host.observations.watches.clear();
        host.observations.reads_inflight.insert(id);
        assert!(host.observations.has_pending());
    }
    #[test]
    fn stale_completed_text_only_loses_observation_for_the_same_current_attempt() {
        let (mut host, watch, _) = fixture();
        let identity = watch.identity.clone();
        let id = identity.operation_id.0.clone();
        host.projects
            .get_mut("workspace-a")
            .unwrap()
            .snapshot
            .config
            .revision = work::ConfigRevision(2);
        host.observations.watches.insert(id, watch);
        install_turn(
            &mut host,
            ObservedTurn {
                identity,
                state: LocalCompletionState::Completed,
                reason: "fixture stale final",
                turn_id: Some("actual-old-turn".into()),
                sequence: Some(1),
                text: Some("must not become result".into()),
                proof: None,
            },
        );
        assert_eq!(
            host.projects["workspace-a"].snapshot.tasks[0].state,
            work::WorkState::ObservationUnknown
        );
        assert!(
            host.projects["workspace-a"].snapshot.tasks[0]
                .result
                .is_none()
        );
        let (mut host, watch, _) = fixture();
        let identity = watch.identity.clone();
        host.projects.get_mut("workspace-a").unwrap().snapshot.tasks[0].current_attempt_id =
            Some(work::AttemptId::new());
        host.observations
            .watches
            .insert(identity.operation_id.0.clone(), watch);
        install_turn(
            &mut host,
            ObservedTurn {
                identity,
                state: LocalCompletionState::Completed,
                reason: "fixture old attempt",
                turn_id: None,
                sequence: Some(1),
                text: Some("stale".into()),
                proof: None,
            },
        );
        assert_eq!(
            host.projects["workspace-a"].snapshot.tasks[0].state,
            work::WorkState::Queued
        );
        assert!(
            host.projects["workspace-a"].snapshot.tasks[0]
                .result
                .is_none()
        );
    }
    #[test]
    fn native_identity_timeout_is_known_unsent_and_frees_dispatch_capacity() {
        let (mut host, watch, assignment) = fixture();
        let id = assignment.attempt.operation_id.0.clone();
        host.inputs.insert(
            id.clone(),
            Input {
                target: watch.route,
                draft: String::new(),
                dispatch: Some(assignment),
            },
        );
        reject_delayed(
            &mut host,
            &id,
            "native identity response timed out before enqueue",
        );
        assert!(host.inputs.is_empty());
        assert_eq!(
            host.projects["workspace-a"].snapshot.tasks[0].state,
            work::WorkState::Failed
        );
        assert_eq!(
            host.projects["workspace-a"].snapshot.outbox[0].state,
            stored::DispatchOutboxState::Acknowledged
        );
        assert_eq!(
            host.receipts.back().unwrap().status,
            cc::ReceiptStatus::Rejected
        );
    }
}
