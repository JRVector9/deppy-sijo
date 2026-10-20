use super::ProjectWorkState;
use super::dispatch::{ConfirmedPlan, DeliveryLedger, ExecutionPolicy};
use super::dispatch::{is_active_execution, scopes_conflict};
use deppy_core::work_management::*;

pub fn ready_tasks(
    state: &ProjectWorkState,
    confirmed: &ConfirmedPlan,
    config: &ProjectConfig,
    policy: &ExecutionPolicy,
    ledger: &DeliveryLedger,
) -> Result<Vec<TaskId>, ValidationError> {
    confirmed.validate_current(state, config, policy)?;
    let active: Vec<_> = state
        .attempts
        .iter()
        .filter(|a| is_active_execution(a.state))
        .collect();
    let capacity = usize::from(policy.max_workers).saturating_sub(active.len());
    let mut selected: Vec<&Task> = Vec::new();
    for planned in &confirmed.tasks {
        if selected.len() == capacity {
            break;
        }
        let task = state.tasks.iter().find(|t| t.id == planned.id).unwrap();
        if !state.can_dispatch(&task.id)
            || ledger.records.iter().any(|(a, s)| {
                a.task.id == task.id
                    && matches!(
                        s,
                        super::dispatch::DeliveryState::Uncertain
                            | super::dispatch::DeliveryState::Sent
                    )
            })
        {
            continue;
        }
        policy.validate_task(task)?;
        if !config.use_worktrees {
            let mut conflict = false;
            for attempt in &active {
                if let Some(other) = state.tasks.iter().find(|t| t.id == attempt.task_id)
                    && scopes_conflict(task, other)?
                {
                    conflict = true;
                    break;
                }
            }
            for other in &selected {
                if scopes_conflict(task, other)? {
                    conflict = true;
                    break;
                }
            }
            if conflict {
                continue;
            }
        }
        selected.push(task);
    }
    Ok(selected.into_iter().map(|t| t.id.clone()).collect())
}
