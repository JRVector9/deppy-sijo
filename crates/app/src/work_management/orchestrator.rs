//! Proposal installation/confirmation and truthful aggregation; no automatic process work.
use super::{
    ProjectWorkState,
    dispatch::{ConfirmedPlan, ExecutionPolicy},
    plan::ValidatedPlan,
};
use deppy_core::work_management::*;

fn validate_proposal_context(
    state: &ProjectWorkState,
    proposal: &ValidatedPlan,
    config: &ProjectConfig,
    policy: &ExecutionPolicy,
) -> Result<(), ValidationError> {
    proposal.request.validate_context(config, policy)?;
    let expected = &proposal.request.goal;
    if state.workspace_id != expected.workspace_id
        || !state.goals.iter().any(|goal| {
            goal.id == expected.id
                && goal.submission_id == expected.submission_id
                && goal.title == expected.title
                && goal.prompt == expected.prompt
        })
    {
        return Err(ValidationError("planner goal submission changed".into()));
    }
    Ok(())
}
pub fn install_proposal(
    state: &mut ProjectWorkState,
    proposal: &ValidatedPlan,
    config: &ProjectConfig,
    policy: &ExecutionPolicy,
) -> Result<(), ValidationError> {
    validate_proposal_context(state, proposal, config, policy)?;
    state.install_plan(proposal.plan().clone())
}
pub fn confirm_proposal(
    state: &mut ProjectWorkState,
    proposal: &ValidatedPlan,
    config: &ProjectConfig,
    policy: &ExecutionPolicy,
) -> Result<ConfirmedPlan, ValidationError> {
    validate_proposal_context(state, proposal, config, policy)?;
    let plan = proposal.plan();
    let current: Vec<_> = state
        .tasks
        .iter()
        .filter(|t| t.goal_id == plan.goal_id)
        .cloned()
        .collect();
    if current != plan.tasks {
        return Err(ValidationError(
            "displayed proposal changed; review it again".into(),
        ));
    }
    ConfirmedPlan::confirm(state, &plan.goal_id, config, policy)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalAggregate {
    pub total: usize,
    pub queued: usize,
    pub running: usize,
    pub waiting: usize,
    pub unknown: usize,
    pub results_submitted: usize,
    pub verification_passed: usize,
    pub verification_failed: usize,
    pub user_confirmed: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub complete: bool,
}

pub fn aggregate_goal(state: &ProjectWorkState, goal: &GoalId) -> GoalAggregate {
    let mut aggregate = GoalAggregate::default();
    for task in state
        .tasks
        .iter()
        .filter(|t| &t.goal_id == goal && t.workspace_id == state.workspace_id)
    {
        aggregate.total += 1;
        match task.state {
            WorkState::Queued => aggregate.queued += 1,
            WorkState::Running => aggregate.running += 1,
            WorkState::WaitingApproval | WorkState::WaitingInput => aggregate.waiting += 1,
            WorkState::ObservationUnknown => aggregate.unknown += 1,
            WorkState::VerificationFailed => aggregate.verification_failed += 1,
            WorkState::Completed => aggregate.user_confirmed += 1,
            WorkState::Failed => aggregate.failed += 1,
            WorkState::Cancelled => aggregate.cancelled += 1,
            _ => {}
        }
        if matches!(
            task.state,
            WorkState::ResultSubmitted
                | WorkState::VerificationFailed
                | WorkState::AwaitingUserConfirmation
                | WorkState::Completed
        ) && task.result.is_some()
        {
            aggregate.results_submitted += 1;
        }
        if matches!(
            task.state,
            WorkState::AwaitingUserConfirmation | WorkState::Completed
        ) && task
            .result
            .as_ref()
            .is_some_and(|r| r.verification == VerificationState::Passed)
        {
            aggregate.verification_passed += 1;
        }
    }
    aggregate.complete = aggregate.total > 0
        && aggregate.user_confirmed == aggregate.total
        && aggregate.verification_passed == aggregate.total;
    aggregate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_management::{
        dispatch::tests::fixture,
        plan::{PlannerRequest, parse_plan_response, tests::response},
    };
    #[test]
    fn work_management_proposal_requires_displayed_exact_plan_and_explicit_confirmation() {
        let (mut state, cfg, policy, goal) = fixture();
        let request = PlannerRequest::new(&state.goals[0], &cfg, &policy).unwrap();
        let proposal =
            parse_plan_response(&response(&state, &cfg, &goal), &request, &cfg, &policy).unwrap();
        install_proposal(&mut state, &proposal, &cfg, &policy).unwrap();
        assert!(state.tasks.iter().all(|t| t.state == WorkState::PlanReview));
        state.tasks[0]
            .instructions
            .push_str(" changed after preview");
        assert!(confirm_proposal(&mut state, &proposal, &cfg, &policy).is_err());
        install_proposal(&mut state, &proposal, &cfg, &policy).unwrap();
        confirm_proposal(&mut state, &proposal, &cfg, &policy).unwrap();
        assert!(state.tasks.iter().all(|t| t.state == WorkState::Queued));
    }
    #[test]
    fn work_management_aggregate_parent_result_never_completes_live_child_or_unverified_result() {
        let (mut state, _cfg, _policy, goal) = fixture();
        state.tasks[1].parent_task_id = Some(state.tasks[0].id.clone());
        state.tasks[0].state = WorkState::Completed;
        state.tasks[0].result = Some(TaskResult {
            revision: ResultRevision(1),
            summary: "Actual result".into(),
            source_revision: None,
            artifact_revision: None,
            verification: VerificationState::Passed,
        });
        state.tasks[1].state = WorkState::Running;
        state.tasks[2].state = WorkState::ObservationUnknown;
        let summary = aggregate_goal(&state, &goal);
        assert_eq!(summary.total, 3);
        assert_eq!(summary.running, 1);
        assert_eq!(summary.unknown, 1);
        assert!(!summary.complete);
        state.tasks[1].state = WorkState::Completed;
        state.tasks[2].state = WorkState::Completed;
        assert!(!aggregate_goal(&state, &goal).complete);
    }
}
