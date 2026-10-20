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
    pub(super) fn ready_tasks(&self) -> Result<Vec<work::TaskId>, String> {
        let token = self
            .confirmed
            .as_ref()
            .ok_or("현재 배정안을 먼저 확인해 주세요.")?;
        match token {
            Confirmed::Local(t) => wm::queue::ready_tasks(
                &self.state(),
                t,
                &self.snapshot.config,
                &self.policy(),
                &self.ledger,
            ),
            Confirmed::Mixed(t) => wm::remote_dispatch::ready_tasks(
                &self.state(),
                t,
                &self.snapshot.config,
                &self.policy(),
                &self.ledger,
            ),
        }
        .map_err(|e| e.to_string())
    }
    pub(super) fn prepare_local(
        &self,
        request: wm::dispatch::DispatchRequest,
        attempt_id: work::AttemptId,
    ) -> Result<wm::dispatch::PrepareOutcome, String> {
        let token = self
            .confirmed
            .as_ref()
            .ok_or("현재 배정안을 먼저 확인해 주세요.")?;
        match token {
            Confirmed::Local(t) => wm::dispatch::prepare_dispatch_with_attempt_id(
                &self.state(),
                &self.ledger,
                t,
                request,
                attempt_id,
                &self.snapshot.config,
                &self.policy(),
            ),
            Confirmed::Mixed(t) => wm::remote_dispatch::prepare_mixed_local_dispatch(
                &self.state(),
                &self.ledger,
                t,
                request,
                attempt_id,
                &self.snapshot.config,
                &self.policy(),
            ),
        }
        .map_err(|e| e.to_string())
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
    project.confirmed = None;
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
