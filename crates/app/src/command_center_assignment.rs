//! Exact assignment changes remain native and require a later whole-plan confirmation.
use super::*;

#[derive(Clone)]
pub(super) enum Confirmed {
    Local(wm::dispatch::ConfirmedPlan),
    Mixed(wm::remote_dispatch::RemoteConfirmedPlan),
}
impl Confirmed {
    pub fn goal_id(&self) -> &work::GoalId {
        match self {
            Self::Local(t) => t.goal_id(),
            Self::Mixed(t) => t.goal_id(),
        }
    }
}
impl Project {
    pub(super) fn display_goal(&self) -> Option<&work::Goal> {
        self.pending_confirmation_goal()
            .and_then(|pending| self.snapshot.goals.iter().find(|goal| goal.id == pending))
            .or_else(|| self.snapshot.goals.last())
    }
    pub(super) fn pending_confirmation_goal(&self) -> Option<work::GoalId> {
        self.proposal
            .as_ref()
            .map(|proposal| proposal.plan().goal_id.clone())
            .or_else(|| {
                self.snapshot
                    .goals
                    .iter()
                    .find(|goal| {
                        self.snapshot.tasks.iter().any(|task| {
                            task.goal_id == goal.id
                                && (task.state == work::WorkState::PlanReview
                                    || wm::dispatch::needs_confirmation(task))
                        }) && self
                            .confirmed
                            .get(&goal.id.0)
                            .is_none_or(|token| token.validate_current(self).is_err())
                    })
                    .map(|goal| goal.id.clone())
            })
    }
    pub(super) fn confirmed_for_task(&self, task: &work::TaskId) -> Option<&Confirmed> {
        let goal = &self
            .snapshot
            .tasks
            .iter()
            .find(|candidate| &candidate.id == task)?
            .goal_id;
        self.confirmed.get(&goal.0)
    }
    fn reviewed_plan(&self, goal: &work::GoalId) -> Result<work::WorkPlan, String> {
        if let Some(review) = self
            .snapshot
            .plan_confirmations
            .iter()
            .find(|review| &review.plan.goal_id == goal)
        {
            if !review.matches(
                &self.snapshot.config.workspace_id,
                goal,
                &self.snapshot.tasks,
            ) {
                return Err("저장된 확인 배정안의 작업 정의가 바뀌었습니다.".into());
            }
            return Ok(review.plan.clone());
        }
        // Legacy snapshots have no review evidence. This is data for an explicit new
        // review only; it is never used to mint a token while loading.
        Ok(work::WorkPlan {
            workspace_id: self.snapshot.config.workspace_id.clone(),
            goal_id: goal.clone(),
            tasks: self
                .snapshot
                .tasks
                .iter()
                .filter(|task| &task.goal_id == goal)
                .map(|task| {
                    let mut task = task.clone();
                    task.state = work::WorkState::PlanReview;
                    task.current_attempt_id = None;
                    task.result = None;
                    task
                })
                .collect(),
        })
    }
    pub(super) fn confirm_goal(
        &mut self,
        goal: &work::GoalId,
        devices: &[wm::devices::VerifiedExecutionDevice],
    ) -> Result<(), String> {
        if self.pending_confirmation_goal().as_ref() != Some(goal) {
            return Err("현재 화면에 표시된 배정안을 다시 확인해 주세요.".into());
        }
        let mut state = self.state();
        let policy = self.policy();
        let initial = state.goals.iter().any(|candidate| {
            &candidate.id == goal && candidate.state == work::WorkState::PlanReview
        });
        let reviewed = self.reviewed_plan(goal)?;
        let mixed = reviewed
            .tasks
            .iter()
            .any(|task| task.execution_device.is_some());
        let token = if mixed {
            Confirmed::Mixed(
                if initial {
                    wm::remote_dispatch::RemoteConfirmedPlan::confirm(
                        &mut state,
                        goal,
                        &self.snapshot.config,
                        &policy,
                        devices,
                    )
                } else {
                    wm::remote_dispatch::RemoteConfirmedPlan::confirm_remaining(
                        &state,
                        &reviewed,
                        &self.snapshot.config,
                        &policy,
                        devices,
                    )
                }
                .map_err(|error| error.to_string())?,
            )
        } else {
            Confirmed::Local(
                if initial {
                    if let Some(proposal) = self.proposal.as_ref().filter(|proposal| {
                        proposal
                            .plan()
                            .tasks
                            .iter()
                            .all(|original| state.tasks.iter().any(|task| task == original))
                    }) {
                        wm::orchestrator::confirm_proposal(
                            &mut state,
                            proposal,
                            &self.snapshot.config,
                            &policy,
                        )
                    } else {
                        wm::dispatch::ConfirmedPlan::confirm(
                            &mut state,
                            goal,
                            &self.snapshot.config,
                            &policy,
                        )
                    }
                } else {
                    wm::dispatch::ConfirmedPlan::confirm_remaining(
                        &state,
                        &reviewed,
                        &self.snapshot.config,
                        &policy,
                    )
                }
                .map_err(|error| error.to_string())?,
            )
        };
        self.snapshot
            .plan_confirmations
            .retain(|review| &review.plan.goal_id != goal);
        self.snapshot
            .plan_confirmations
            .push(stored::PlanConfirmation {
                version: 1,
                config: self.snapshot.config.clone(),
                plan: reviewed,
            });
        self.confirmed.insert(goal.0.clone(), token);
        if self
            .proposal
            .as_ref()
            .is_some_and(|proposal| &proposal.plan().goal_id == goal)
        {
            self.proposal = None;
        }
        self.set_state(state);
        Ok(())
    }
    pub(super) fn ready_tasks(&self) -> Result<Vec<work::TaskId>, String> {
        let mut tasks = Vec::new();
        if self.confirmed.is_empty() {
            return Err("현재 배정안을 먼저 확인해 주세요.".into());
        }
        for token in self.confirmed.values() {
            if token.validate_current(self).is_err() {
                continue;
            }
            let ready = match token {
                Confirmed::Local(token) => wm::queue::ready_tasks(
                    &self.state(),
                    token,
                    &self.snapshot.config,
                    &self.policy(),
                    &self.ledger,
                ),
                Confirmed::Mixed(token) => wm::remote_dispatch::ready_tasks(
                    &self.state(),
                    token,
                    &self.snapshot.config,
                    &self.policy(),
                    &self.ledger,
                ),
            }
            .map_err(|error| error.to_string())?;
            tasks.extend(ready);
        }
        Ok(tasks)
    }
    pub(super) fn prepare_existing(
        &self,
        request: wm::dispatch::DispatchRequest,
    ) -> Result<wm::dispatch::PrepareOutcome, String> {
        let token = self
            .confirmed_for_task(&request.task_id)
            .ok_or("현재 배정안을 먼저 확인해 주세요.")?;
        match token {
            Confirmed::Local(token) => wm::dispatch::prepare_dispatch(
                &self.state(),
                &self.ledger,
                token,
                request,
                &self.snapshot.config,
                &self.policy(),
            ),
            Confirmed::Mixed(token) => wm::remote_dispatch::prepare_mixed_local_dispatch(
                &self.state(),
                &self.ledger,
                token,
                request,
                work::AttemptId::new(),
                &self.snapshot.config,
                &self.policy(),
            ),
        }
        .map_err(|error| error.to_string())
    }
    pub(super) fn prepare_local(
        &self,
        request: wm::dispatch::DispatchRequest,
        attempt_id: work::AttemptId,
    ) -> Result<wm::dispatch::PrepareOutcome, String> {
        let token = self
            .confirmed_for_task(&request.task_id)
            .ok_or("현재 배정안을 먼저 확인해 주세요.")?;
        match token {
            Confirmed::Local(token) => wm::dispatch::prepare_dispatch_with_attempt_id(
                &self.state(),
                &self.ledger,
                token,
                request,
                attempt_id,
                &self.snapshot.config,
                &self.policy(),
            ),
            Confirmed::Mixed(token) => wm::remote_dispatch::prepare_mixed_local_dispatch(
                &self.state(),
                &self.ledger,
                token,
                request,
                attempt_id,
                &self.snapshot.config,
                &self.policy(),
            ),
        }
        .map_err(|error| error.to_string())
    }
}
impl Confirmed {
    fn validate_current(&self, project: &Project) -> Result<(), work::ValidationError> {
        match self {
            Self::Local(token) => token.validate_current(
                &project.state(),
                &project.snapshot.config,
                &project.policy(),
            ),
            Self::Mixed(token) => token.validate_current(
                &project.state(),
                &project.snapshot.config,
                &project.policy(),
            ),
        }
    }
}
fn local_profile(index: usize) -> String {
    format!("local:{index}")
}
pub(super) fn choices(
    project: &Project,
    policies: &[wm::devices::VerifiedExecutionDevice],
) -> Vec<cc::AssignmentChoice> {
    let workspace = &project.snapshot.config.workspace_id.0;
    let mut choices: Vec<_> = project
        .allowed_agents
        .iter()
        .enumerate()
        .filter(|(_, agent)| {
            project
                .catalog
                .as_ref()
                .is_some_and(|c| c.choices.contains(agent) || c.defaults.contains(agent))
        })
        .map(|(index, agent)| cc::AssignmentChoice {
            workspace_id: workspace.clone(),
            device_id: None,
            profile_id: local_profile(index),
            label: format!(
                "이 컴퓨터 · {} · {} · {}",
                agent.provider,
                agent.model.as_deref().unwrap_or("기본"),
                agent.effort.as_deref().unwrap_or("기본")
            ),
            provider: agent.provider.clone(),
            model: agent.model.clone(),
            effort: agent.effort.clone(),
            capability: yes(),
        })
        .collect();
    for device in policies
        .iter()
        .filter(|d| d.registration().workspace_id.0 == *workspace)
    {
        for profile in device.profiles() {
            let capability = if device.validate_fresh().is_err() {
                no("기기 연결과 권한을 다시 확인해 주세요.")
            } else if project.snapshot.config.use_worktrees {
                no("원격 기기의 작업별 워크트리 소유는 아직 확인할 수 없습니다.")
            } else if device
                .profiles()
                .iter()
                .filter(|p| p.agent == profile.agent)
                .count()
                != 1
            {
                no("같은 실행 설정의 프로필이 여러 개입니다. 기기에서 구분해 주세요.")
            } else if !project.allowed_agents.contains(&profile.agent) {
                no("프로젝트 설정에서 이 워커 공급자를 허용하고 저장해 주세요.")
            } else {
                yes()
            };
            choices.push(cc::AssignmentChoice {
                workspace_id: workspace.clone(),
                device_id: Some(device.registration().id.clone()),
                profile_id: profile.id.clone(),
                label: format!(
                    "{} · {} · {} · {}",
                    device.registration().name,
                    profile.agent.provider,
                    profile.agent.model.as_deref().unwrap_or("기본"),
                    profile.agent.effort.as_deref().unwrap_or("기본")
                ),
                provider: profile.agent.provider.clone(),
                model: profile.agent.model.clone(),
                effort: profile.agent.effort.clone(),
                capability,
            });
        }
    }
    choices
}
pub(super) fn selected(task: &work::Task, choices: &[cc::AssignmentChoice]) -> Option<String> {
    choices
        .iter()
        .find(|c| {
            c.device_id == task.execution_device
                && c.provider == task.assigned_agent.provider
                && c.model == task.assigned_agent.model
                && c.effort == task.assigned_agent.effort
        })
        .map(|c| choice_key(c.device_id.as_deref(), &c.profile_id))
}
fn choice_key(device: Option<&str>, profile: &str) -> String {
    serde_json::to_string(&(device, profile)).expect("string pair")
}
fn amend(
    project: &mut Project,
    task_id: &str,
    revision: &str,
    agent: work::AgentSelection,
    device: Option<String>,
) -> Result<(), String> {
    if revision != project.snapshot.config.revision.0.to_string() {
        return Err("프로젝트 설정이 바뀌었습니다. 배정안을 다시 확인해 주세요.".into());
    }
    if !project.loaded || project.storage_blocked || !project.allowed_agents.contains(&agent) {
        return Err("현재 프로젝트에서 허용한 실행 설정이 아닙니다.".into());
    }
    let task = project
        .snapshot
        .tasks
        .iter_mut()
        .find(|t| t.id.0 == task_id)
        .ok_or("작업이 없습니다.")?;
    if task.state != work::WorkState::PlanReview || task.current_attempt_id.is_some() {
        return Err("실행 전 검토 중인 작업만 배정을 바꿀 수 있습니다.".into());
    }
    task.assigned_agent = agent;
    task.execution_device = device;
    project.confirmed.remove(&task.goal_id.0);
    project.dirty();
    Ok(())
}
impl App {
    pub(super) fn amend_command_center_assignment(
        &mut self,
        host: &mut CommandCenterHost,
        id: &str,
        target: cc::RouteTarget,
        revision: String,
        device: Option<String>,
        profile: String,
    ) -> Result<(), String> {
        if host.flight.is_some() {
            return Err("배정안 저장이 끝난 뒤 다시 선택해 주세요.".into());
        }
        let p = host
            .projects
            .get_mut(&target.workspace_id)
            .ok_or("프로젝트가 없습니다.")?;
        let goal = p
            .proposal
            .as_ref()
            .ok_or("현재 검토 중인 배정안이 없습니다.")?
            .plan()
            .goal_id
            .clone();
        if !p
            .snapshot
            .tasks
            .iter()
            .any(|t| Some(&t.id.0) == target.task_id.as_ref() && t.goal_id == goal)
        {
            return Err("현재 배정안에 속한 작업이 아닙니다.".into());
        }
        let choice = choices(p, &host.devices.policies)
            .into_iter()
            .find(|c| c.device_id == device && c.profile_id == profile)
            .ok_or("현재 확인된 실행 프로필이 아닙니다.")?;
        if !choice.capability.enabled {
            return Err(choice.capability.reason);
        }
        amend(
            p,
            target.task_id.as_deref().ok_or("작업을 선택해 주세요.")?,
            &revision,
            work::AgentSelection {
                provider: choice.provider,
                model: choice.model,
                effort: choice.effort,
            },
            device,
        )?;
        host.save(&target.workspace_id, None, Some(id.into()))?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn planned() -> Project {
        let (mut p, _) = super::super::tests::attempted();
        p.snapshot.tasks[0].state = work::WorkState::PlanReview;
        p.snapshot.tasks[0].current_attempt_id = None;
        p.snapshot.goals[0].state = work::WorkState::PlanReview;
        p.snapshot.attempts.clear();
        p.snapshot.outbox.clear();
        p.ledger = Default::default();
        p.scopes = vec!["src".into()];
        p.allowed_agents = vec![p.snapshot.config.worker_agent.clone()];
        p
    }
    fn confirm(p: &mut Project, goal: &work::GoalId) {
        let request = wm::plan::PlannerRequest::new(
            p.snapshot
                .goals
                .iter()
                .find(|candidate| &candidate.id == goal)
                .unwrap(),
            &p.snapshot.config,
            &p.policy(),
        )
        .unwrap();
        let response = wm::plan::PlannerResponse {
            config_revision: p.snapshot.config.revision,
            plan: work::WorkPlan {
                workspace_id: p.snapshot.config.workspace_id.clone(),
                goal_id: goal.clone(),
                tasks: p
                    .snapshot
                    .tasks
                    .iter()
                    .filter(|task| &task.goal_id == goal)
                    .cloned()
                    .collect(),
            },
        };
        p.proposal = Some(
            wm::provider_adapter::accept_manual_json(
                &request,
                &serde_json::to_string(&response).unwrap(),
                &p.snapshot.config,
                &p.policy(),
            )
            .unwrap(),
        );
        p.request = Some(request);
        p.confirm_goal(goal, &[]).unwrap();
    }
    #[test]
    fn confirming_a_second_goal_keeps_the_first_goal_dispatchable() {
        let mut p = planned();
        let first = p.snapshot.tasks[0].id.clone();
        let goal = p.snapshot.goals[0].id.clone();
        confirm(&mut p, &goal);
        let mut state = p.state();
        let second_goal = state
            .submit_goal(
                work::SubmissionId::new(),
                "second goal".into(),
                "second actual request".into(),
            )
            .unwrap();
        let mut second = state.tasks[0].clone();
        second.id = work::TaskId::new();
        second.goal_id = second_goal.clone();
        second.state = work::WorkState::PlanReview;
        second.scope = "src/second".into();
        state
            .install_plan(work::WorkPlan {
                workspace_id: state.workspace_id.clone(),
                goal_id: second_goal.clone(),
                tasks: vec![second.clone()],
            })
            .unwrap();
        p.set_state(state);
        confirm(&mut p, &second_goal);
        let ready = p.ready_tasks().unwrap();
        assert!(
            ready.contains(&first),
            "confirming a second goal must not retire an earlier plan"
        );
        assert!(ready.contains(&second.id));
        p.confirmed.clear();
        assert_eq!(p.pending_confirmation_goal(), Some(goal.clone()));
        assert_eq!(
            p.display_goal().map(|display| &display.id),
            Some(&goal),
            "review button must display its own goal rather than a newer goal"
        );
    }
    #[test]
    fn sqlite_restored_queued_goal_offers_explicit_reconfirmation() {
        let mut p = planned();
        let goal = p.snapshot.goals[0].id.clone();
        confirm(&mut p, &goal);
        let dir = std::env::temp_dir().join(format!("cc-confirm-{}", uuid::Uuid::new_v4()));
        let mut store = stored::CommandCenterStore::open(&dir).unwrap();
        store.save_workspace(&p.snapshot, None).unwrap();
        let loaded = store
            .load_workspace(&p.snapshot.config.workspace_id)
            .unwrap()
            .unwrap();
        let mut restored = planned();
        restored.snapshot = loaded.snapshot;
        restored.confirmed.clear();
        assert_eq!(
            restored.pending_confirmation_goal(),
            Some(goal.clone()),
            "durable queued work must have a review path after restart"
        );
        assert!(
            restored.ready_tasks().is_err(),
            "loading data must not restore dispatch authority"
        );
        assert_eq!(restored.snapshot.plan_confirmations.len(), 1);
        restored.confirm_goal(&goal, &[]).unwrap();
        assert_eq!(
            restored.ready_tasks().unwrap(),
            vec![restored.snapshot.tasks[0].id.clone()]
        );
    }
    fn begin(p: &mut Project, task: work::TaskId) -> wm::dispatch::DispatchAssignment {
        let request = wm::dispatch::DispatchRequest {
            task_id: task,
            operation_id: work::OperationId::new(),
            folder: wm::dispatch::DispatchFolder {
                path: p.snapshot.config.folder.clone(),
                worktree_owner: None,
            },
            persistent_binding: None,
            runtime_binding: work::RuntimeBinding {
                incarnation: work::RuntimeIncarnationId::new(),
                session_id: runtime::SessionId(11),
                execution_generation: 1,
            },
        };
        let wm::dispatch::PrepareOutcome::Ready(prepared) = p.prepare_existing(request).unwrap()
        else {
            panic!("fresh dispatch expected")
        };
        let assignment = prepared.assignment;
        p.set_state(prepared.next_state);
        p.ledger.record_durable(assignment.clone()).unwrap();
        assert!(p.ledger.claim_send(&assignment.attempt.operation_id));
        p.snapshot.outbox.push(stored::DispatchOutbox {
            attempt_id: assignment.attempt.id.clone(),
            operation_id: assignment.attempt.operation_id.clone(),
            state: stored::DispatchOutboxState::Pending,
        });
        apply_delivery_outcome(
            p,
            &assignment,
            ui::composer::PromptAdmissionOutcome::Accepted,
        );
        assignment
    }
    #[test]
    fn persisted_unstarted_review_survives_lost_volatile_proposal() {
        let mut p = planned();
        let goal = p.snapshot.goals[0].id.clone();
        assert!(p.proposal.is_none());
        assert_eq!(
            p.pending_confirmation_goal(),
            Some(goal.clone()),
            "persisted validated task definitions remain reviewable"
        );
        p.confirm_goal(&goal, &[]).unwrap();
        assert_eq!(
            p.ready_tasks().unwrap(),
            vec![p.snapshot.tasks[0].id.clone()]
        );
    }
    #[test]
    fn configuration_reconfirmation_preserves_completed_and_productive_attempts() {
        let mut p = planned();
        let goal = p.snapshot.goals[0].id.clone();
        let first = p.snapshot.tasks[0].id.clone();
        let mut running = p.snapshot.tasks[0].clone();
        running.id = work::TaskId::new();
        running.scope = "src/running".into();
        p.snapshot.tasks[0].scope = "src/first".into();
        let mut waiting = running.clone();
        waiting.id = work::TaskId::new();
        waiting.scope = "src/waiting".into();
        waiting.dependencies = vec![work::TaskDependency {
            task_id: first.clone(),
            required_result_revision: work::ResultRevision(1),
        }];
        p.snapshot.tasks.extend([running.clone(), waiting.clone()]);
        confirm(&mut p, &goal);
        let first_assignment = begin(&mut p, first.clone());
        let target = cc::RouteTarget {
            workspace_id: p.snapshot.config.workspace_id.0.clone(),
            task_id: Some(first.0),
            session_id: None,
        };
        for kind in [
            work::WorkEventKind::ResultSubmitted {
                result: work::TaskResult {
                    revision: work::ResultRevision(1),
                    summary: "actual result".into(),
                    source_revision: None,
                    artifact_revision: None,
                    verification: work::VerificationState::NotRun,
                },
            },
            work::WorkEventKind::VerificationFinished {
                result_revision: work::ResultRevision(1),
                outcome: work::VerificationState::Passed,
            },
            work::WorkEventKind::UserConfirmed {
                result_revision: work::ResultRevision(1),
            },
        ] {
            apply_user_work_event(
                &mut p,
                &target,
                &first_assignment.attempt.id.0,
                kind,
                "actual operator evidence".into(),
            )
            .unwrap();
        }
        let active = begin(&mut p, running.id.clone());
        apply_user_work_event(
            &mut p,
            &cc::RouteTarget {
                task_id: Some(running.id.0),
                ..target.clone()
            },
            &active.attempt.id.0,
            work::WorkEventKind::Started,
            "actual started event".into(),
        )
        .unwrap();
        let historical = p.snapshot.attempts.clone();
        p.snapshot.config.revision = p.snapshot.config.revision.next().unwrap();
        p.snapshot.config.name = "new settings".into();
        p.confirmed.clear();
        assert_eq!(p.pending_confirmation_goal(), Some(goal.clone()));
        assert!(p.ready_tasks().is_err());
        p.confirm_goal(&goal, &[]).unwrap();
        assert_eq!(
            p.snapshot.attempts, historical,
            "reconfirmation cannot rewrite running/completed attempt configs"
        );
        assert_eq!(p.snapshot.tasks[0].state, work::WorkState::Completed);
        assert_eq!(
            p.snapshot.tasks[1].current_attempt_id.as_ref(),
            Some(&active.attempt.id)
        );
        assert_eq!(p.ready_tasks().unwrap(), vec![waiting.id.clone()]);
        let fresh = begin(&mut p, waiting.id);
        assert_eq!(fresh.attempt.config.revision, work::ConfigRevision(2));
        assert_eq!(
            p.snapshot.attempts[1].config.revision,
            work::ConfigRevision(1)
        );
    }
    #[test]
    fn reconfirmation_rejects_changed_definitions_and_current_policy_expansion() {
        let mut p = planned();
        let goal = p.snapshot.goals[0].id.clone();
        confirm(&mut p, &goal);
        p.confirmed.clear();
        let before = p.snapshot.clone();
        p.snapshot.tasks[0].instructions = "tampered persisted instructions".into();
        assert!(p.confirm_goal(&goal, &[]).is_err());
        assert!(p.snapshot.validate().is_err());
        p.snapshot = before;
        p.scopes = vec!["other".into()];
        assert!(
            p.confirm_goal(&goal, &[]).is_err(),
            "current permission bounds must still validate every remaining assignment"
        );
        assert!(p.confirmed.is_empty());
    }
    #[test]
    fn restored_unknown_outbox_stays_unreplayed_after_reconfirmation() {
        let mut p = planned();
        let goal = p.snapshot.goals[0].id.clone();
        let mut second = p.snapshot.tasks[0].clone();
        second.id = work::TaskId::new();
        second.scope = "src/second".into();
        p.snapshot.tasks[0].scope = "src/first".into();
        p.snapshot.tasks.push(second.clone());
        confirm(&mut p, &goal);
        let first_id = p.snapshot.tasks[0].id.clone();
        let assignment = begin(&mut p, first_id.clone());
        p.snapshot.outbox[0].state = stored::DispatchOutboxState::Pending;
        let dir = std::env::temp_dir().join(format!("cc-confirm-unknown-{}", uuid::Uuid::new_v4()));
        let mut store = stored::CommandCenterStore::open(&dir).unwrap();
        store.save_workspace(&p.snapshot, None).unwrap();
        p.snapshot = store
            .load_workspace(&p.snapshot.config.workspace_id)
            .unwrap()
            .unwrap()
            .snapshot;
        recover_observation(&mut p.snapshot);
        p.confirmed.clear();
        p.ledger = Default::default();
        let before = p.snapshot.attempts.clone();
        p.confirm_goal(&goal, &[]).unwrap();
        assert_eq!(
            p.snapshot.outbox[0].state,
            stored::DispatchOutboxState::Uncertain
        );
        assert_eq!(p.snapshot.attempts, before);
        assert_eq!(
            p.snapshot.tasks[0].current_attempt_id.as_ref(),
            Some(&assignment.attempt.id)
        );
        assert!(!p.ready_tasks().unwrap().contains(&first_id));
        assert!(p.ready_tasks().unwrap().contains(&second.id));
    }
    #[test]
    fn assignment_changes_only_reviewed_definition_and_requires_current_revision() {
        let (mut p, _) = super::super::tests::attempted();
        p.snapshot.tasks[0].state = work::WorkState::PlanReview;
        p.snapshot.tasks[0].current_attempt_id = None;
        p.snapshot.attempts.clear();
        let id = p.snapshot.tasks[0].id.0.clone();
        let agent = work::AgentSelection {
            provider: "claude".into(),
            model: Some("actual-profile-model".into()),
            effort: None,
        };
        p.allowed_agents.push(agent.clone());
        let original = p.snapshot.clone();
        assert!(amend(&mut p, &id, "stale", agent.clone(), None).is_err());
        assert_eq!(p.snapshot, original);
        let revision = p.snapshot.config.revision.0.to_string();
        amend(&mut p, &id, &revision, agent.clone(), None).unwrap();
        assert_eq!(p.snapshot.tasks[0].assigned_agent, agent);
        assert_eq!(
            p.snapshot.tasks[0].instructions,
            original.tasks[0].instructions
        );
        assert_eq!(p.snapshot.tasks[0].state, work::WorkState::PlanReview);
    }
    #[test]
    fn assignment_cannot_retarget_an_executing_attempt() {
        let (mut p, _) = super::super::tests::attempted();
        let before = p.snapshot.clone();
        let id = p.snapshot.tasks[0].id.0.clone();
        let revision = p.snapshot.config.revision.0.to_string();
        let agent = p.snapshot.config.worker_agent.clone();
        assert!(amend(&mut p, &id, &revision, agent, None).is_err());
        assert_eq!(p.snapshot, before);
    }
}
