//! Pure dispatch preparation. The host persists the returned state/outbox before sending.
use super::ProjectWorkState;
use deppy_core::WorkspaceId;
use deppy_core::work_management::*;
use std::path::{Component, Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionPolicy {
    pub workspace_id: WorkspaceId,
    pub config_revision: ConfigRevision,
    /// Host-resolved canonical paths, never paths supplied by a planner response.
    pub project_folder: String,
    pub worktree_root: Option<String>,
    pub allowed_agents: Vec<AgentSelection>,
    pub allowed_scope_roots: Vec<String>,
    pub max_workers: u8,
}

#[derive(Debug, Clone)]
pub struct ConfirmedPlan {
    pub(crate) config: ProjectConfig,
    pub(crate) policy: ExecutionPolicy,
    pub(crate) goal_id: GoalId,
    pub(crate) tasks: Vec<Task>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchFolder {
    pub path: String,
    pub worktree_owner: Option<TaskId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchRequest {
    pub task_id: TaskId,
    pub operation_id: OperationId,
    pub folder: DispatchFolder,
    pub persistent_binding: Option<PersistentBinding>,
    pub runtime_binding: RuntimeBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyResultReference {
    pub task_id: TaskId,
    pub result: TaskResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchAssignment {
    pub task: Task,
    pub attempt: Attempt,
    pub source_results: Vec<DependencyResultReference>,
    pub prompt: String,
}

#[derive(Debug, Clone)]
pub struct PreparedDispatch {
    pub next_state: ProjectWorkState,
    pub assignment: DispatchAssignment,
}

#[derive(Debug, Clone)]
pub enum PrepareOutcome {
    Ready(Box<PreparedDispatch>),
    Existing {
        attempt_id: AttemptId,
        delivery: DeliveryState,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState {
    Durable,
    Sent,
    Admitted,
    Rejected,
    Uncertain,
}

#[derive(Debug, Clone, Default)]
pub struct DeliveryLedger {
    pub(crate) records: Vec<(DispatchAssignment, DeliveryState)>,
}

impl DeliveryLedger {
    /// Call only after the sidecar outbox transaction has committed.
    pub fn record_durable(
        &mut self,
        assignment: DispatchAssignment,
    ) -> Result<(), ValidationError> {
        assignment.attempt.validate()?;
        assignment.task.validate()?;
        validate_input(&assignment.prompt)?;
        if assignment.task.workspace_id != assignment.attempt.workspace_id
            || assignment.task.id != assignment.attempt.task_id
            || assignment.task.current_attempt_id.as_ref() != Some(&assignment.attempt.id)
            || assignment.source_results.len() > WORK_PLAN_MAX_TASKS
        {
            return Err(ValidationError("invalid durable assignment owner".into()));
        }
        if let Some((existing, _)) = self
            .records
            .iter()
            .find(|(a, _)| a.attempt.operation_id == assignment.attempt.operation_id)
        {
            return if existing == &assignment {
                Ok(())
            } else {
                Err(ValidationError("operation payload conflict".into()))
            };
        }
        if self.records.len() >= WORK_DRAFT_MAX_COUNT {
            return Err(ValidationError("delivery ledger is full".into()));
        }
        self.records.push((assignment, DeliveryState::Durable));
        Ok(())
    }
    /// An unacknowledged recovered record is observation-unknown, never sendable.
    pub fn restore(
        &mut self,
        assignment: DispatchAssignment,
        stage: DeliveryState,
    ) -> Result<(), ValidationError> {
        let operation = assignment.attempt.operation_id.clone();
        if self.stage(&operation).is_some() {
            return Err(ValidationError("delivery already tracked".into()));
        }
        self.record_durable(assignment)?;
        self.records
            .iter_mut()
            .find(|(a, _)| a.attempt.operation_id == operation)
            .unwrap()
            .1 = match stage {
            DeliveryState::Durable | DeliveryState::Sent | DeliveryState::Uncertain => {
                DeliveryState::Uncertain
            }
            other => other,
        };
        Ok(())
    }
    /// The host persists this claim as uncertain/sent before touching the runtime.
    pub fn claim_send(&mut self, operation: &OperationId) -> bool {
        let Some((_, stage)) = self
            .records
            .iter_mut()
            .find(|(a, _)| &a.attempt.operation_id == operation)
        else {
            return false;
        };
        if *stage != DeliveryState::Durable {
            return false;
        }
        *stage = DeliveryState::Sent;
        true
    }
    pub fn mark_uncertain(&mut self, operation: &OperationId) {
        if let Some((_, stage)) = self
            .records
            .iter_mut()
            .find(|(a, _)| &a.attempt.operation_id == operation)
            && matches!(stage, DeliveryState::Durable | DeliveryState::Sent)
        {
            *stage = DeliveryState::Uncertain;
        }
    }
    pub fn note_admitted(
        &mut self,
        operation: &OperationId,
        attempt: &AttemptId,
        binding: &RuntimeBinding,
    ) -> bool {
        let Some((assignment, stage)) = self
            .records
            .iter_mut()
            .find(|(a, _)| &a.attempt.operation_id == operation)
        else {
            return false;
        };
        if &assignment.attempt.id != attempt
            || assignment.attempt.runtime_binding.as_ref() != Some(binding)
            || !matches!(stage, DeliveryState::Sent | DeliveryState::Uncertain)
        {
            return false;
        }
        *stage = DeliveryState::Admitted;
        true
    }
    pub fn note_rejected(
        &mut self,
        operation: &OperationId,
        attempt: &AttemptId,
        binding: &RuntimeBinding,
    ) -> bool {
        let Some((assignment, stage)) = self
            .records
            .iter_mut()
            .find(|(a, _)| &a.attempt.operation_id == operation)
        else {
            return false;
        };
        if &assignment.attempt.id != attempt
            || assignment.attempt.runtime_binding.as_ref() != Some(binding)
            || !matches!(stage, DeliveryState::Sent | DeliveryState::Uncertain)
        {
            return false;
        }
        *stage = DeliveryState::Rejected;
        true
    }
    pub fn stage(&self, operation: &OperationId) -> Option<DeliveryState> {
        self.records
            .iter()
            .find(|(a, _)| &a.attempt.operation_id == operation)
            .map(|(_, stage)| *stage)
    }
}

impl ConfirmedPlan {
    /// Invoke from an explicit user confirmation, after displaying the proposal.
    pub fn confirm(
        state: &mut ProjectWorkState,
        goal: &GoalId,
        config: &ProjectConfig,
        policy: &ExecutionPolicy,
    ) -> Result<Self, ValidationError> {
        policy.validate_config(config)?;
        if !state
            .goals
            .iter()
            .any(|g| &g.id == goal && g.state == WorkState::PlanReview)
            || state.workspace_id != policy.workspace_id
        {
            return Err(ValidationError(
                "goal is not awaiting plan confirmation".into(),
            ));
        }
        let tasks: Vec<_> = state
            .tasks
            .iter()
            .filter(|t| &t.goal_id == goal)
            .cloned()
            .collect();
        WorkPlan {
            workspace_id: state.workspace_id.clone(),
            goal_id: goal.clone(),
            tasks: tasks.clone(),
        }
        .validate()?;
        for task in &tasks {
            policy.validate_task(task)?;
            if task.state != WorkState::PlanReview
                || task.current_attempt_id.is_some()
                || task.result.is_some()
            {
                return Err(ValidationError("proposal is already started".into()));
            }
        }
        state.confirm_plan(goal)?;
        Ok(Self {
            config: config.clone(),
            policy: policy.clone(),
            goal_id: goal.clone(),
            tasks,
        })
    }
    pub fn goal_id(&self) -> &GoalId {
        &self.goal_id
    }
    pub fn validate_current(
        &self,
        state: &ProjectWorkState,
        config: &ProjectConfig,
        policy: &ExecutionPolicy,
    ) -> Result<(), ValidationError> {
        policy.validate_config(config)?;
        if config != &self.config
            || policy != &self.policy
            || state.workspace_id != policy.workspace_id
            || !state
                .goals
                .iter()
                .any(|g| g.id == self.goal_id && g.state != WorkState::PlanReview)
        {
            return Err(ValidationError("confirmation is stale".into()));
        }
        let current: Vec<_> = state
            .tasks
            .iter()
            .filter(|t| t.goal_id == self.goal_id)
            .collect();
        if current.len() != self.tasks.len()
            || self
                .tasks
                .iter()
                .any(|expected| !current.iter().any(|t| same_task_definition(expected, t)))
        {
            return Err(ValidationError("confirmed plan changed".into()));
        }
        Ok(())
    }
}

impl ExecutionPolicy {
    pub fn validate_config(&self, config: &ProjectConfig) -> Result<(), ValidationError> {
        config.validate()?;
        validate_folder(&self.project_folder)?;
        if let Some(root) = &self.worktree_root {
            validate_folder(root)?;
        }
        if self.workspace_id != config.workspace_id
            || self.config_revision != config.revision
            || self.project_folder != config.folder
            || self.max_workers != config.max_workers
            || self.allowed_agents.is_empty()
            || self.allowed_agents.len() > WORK_PLAN_MAX_TASKS
            || self.allowed_scope_roots.is_empty()
            || self.allowed_scope_roots.len() > WORK_PLAN_MAX_TASKS
            || config.use_worktrees && self.worktree_root.is_none()
        {
            return Err(ValidationError(
                "execution policy does not match project config".into(),
            ));
        }
        for agent in &self.allowed_agents {
            agent.validate()?;
        }
        for root in &self.allowed_scope_roots {
            relative_scope_paths(root)?;
            if root.contains('\n') {
                return Err(ValidationError(
                    "policy root must be one relative path".into(),
                ));
            }
        }
        Ok(())
    }
    pub fn validate_task(&self, task: &Task) -> Result<(), ValidationError> {
        task.validate()?;
        if task.workspace_id != self.workspace_id
            || task.execution_device.is_some()
            || !self.allowed_agents.contains(&task.assigned_agent)
        {
            return Err(ValidationError(
                "planner attempted to expand agent/device permissions".into(),
            ));
        }
        for path in relative_scope_paths(&task.scope)? {
            if !self
                .allowed_scope_roots
                .iter()
                .any(|root| scope_contains(root, &path))
            {
                return Err(ValidationError(
                    "task scope is outside approved roots".into(),
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn same_task_definition(a: &Task, b: &Task) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    for task in [&mut a, &mut b] {
        task.state = WorkState::PlanReview;
        task.current_attempt_id = None;
        task.result = None;
    }
    a == b
}

pub(crate) fn is_active_execution(state: WorkState) -> bool {
    matches!(
        state,
        WorkState::Queued
            | WorkState::Running
            | WorkState::WaitingApproval
            | WorkState::WaitingInput
            | WorkState::ObservationUnknown
    )
}

pub fn relative_scope_paths(scope: &str) -> Result<Vec<String>, ValidationError> {
    if scope.is_empty() || scope.len() > WORK_EVENT_MAX_BYTES {
        return Err(ValidationError(
            "scope must contain newline-separated relative path roots".into(),
        ));
    }
    let mut paths = Vec::new();
    for path in scope.lines() {
        if path.is_empty()
            || path != path.trim()
            || path.contains(['\0', '\\', '*', '?', '[', ']'])
            || Path::new(path).is_absolute()
            || path != "."
                && path
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
            || path.chars().any(char::is_control)
        {
            return Err(ValidationError(
                "scope must use literal relative paths, without traversal or globs".into(),
            ));
        }
        if paths.len() == WORK_PLAN_MAX_TASKS {
            return Err(ValidationError("scope root limit reached".into()));
        }
        paths.push(path.to_string());
    }
    if paths.is_empty() {
        return Err(ValidationError("scope has no path roots".into()));
    }
    Ok(paths)
}

pub(crate) fn scope_contains(root: &str, path: &str) -> bool {
    root == "." || root == path || Path::new(path).starts_with(root)
}

pub(crate) fn scopes_conflict(a: &Task, b: &Task) -> Result<bool, ValidationError> {
    let a = relative_scope_paths(&a.scope)?;
    let b = relative_scope_paths(&b.scope)?;
    Ok(a.iter().any(|a| {
        b.iter()
            .any(|b| scope_contains(a, b) || scope_contains(b, a))
    }))
}

fn validate_folder(folder: &str) -> Result<(), ValidationError> {
    if folder.is_empty()
        || folder.len() > WORK_FOLDER_MAX_BYTES
        || folder.contains(['\0', '\\'])
        || !Path::new(folder).is_absolute()
        || folder.split('/').any(|part| part == "." || part == "..")
        || Path::new(folder).components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        return Err(ValidationError(
            "host folder must be an absolute resolved path".into(),
        ));
    }
    Ok(())
}

pub fn prepare_dispatch(
    state: &ProjectWorkState,
    ledger: &DeliveryLedger,
    confirmed: &ConfirmedPlan,
    request: DispatchRequest,
    config: &ProjectConfig,
    policy: &ExecutionPolicy,
) -> Result<PrepareOutcome, ValidationError> {
    let attempt_id = ledger
        .records
        .iter()
        .find(|(a, _)| a.attempt.operation_id == request.operation_id)
        .map(|(a, _)| a.attempt.id.clone())
        .or_else(|| {
            state
                .attempts
                .iter()
                .find(|a| a.operation_id == request.operation_id)
                .map(|a| a.id.clone())
        })
        .unwrap_or_else(AttemptId::new);
    prepare_dispatch_with_attempt_id(
        state, ledger, confirmed, request, attempt_id, config, policy,
    )
}

/// Use a host-reserved launch identity after observing the exact new runtime binding.
/// This preparation never spawns or mutates its source state and never retries a send.
pub fn prepare_dispatch_with_attempt_id(
    state: &ProjectWorkState,
    ledger: &DeliveryLedger,
    confirmed: &ConfirmedPlan,
    request: DispatchRequest,
    attempt_id: AttemptId,
    config: &ProjectConfig,
    policy: &ExecutionPolicy,
) -> Result<PrepareOutcome, ValidationError> {
    confirmed.validate_current(state, config, policy)?;
    if let Some(existing) = existing_dispatch(state, ledger, &request, &attempt_id, config)? {
        return Ok(existing);
    }
    if !super::queue::ready_tasks(state, confirmed, config, policy, ledger)?
        .contains(&request.task_id)
    {
        return Err(ValidationError(
            "task is not ready: confirmation, dependencies, capacity or file scope".into(),
        ));
    }
    validate_dispatch_folder(state, &request, config, policy)?;
    Ok(PrepareOutcome::Ready(Box::new(prepare_assignment(
        state, request, attempt_id, config,
    )?)))
}

pub(crate) fn existing_dispatch(
    state: &ProjectWorkState,
    ledger: &DeliveryLedger,
    request: &DispatchRequest,
    attempt_id: &AttemptId,
    config: &ProjectConfig,
) -> Result<Option<PrepareOutcome>, ValidationError> {
    if state
        .attempts
        .iter()
        .any(|a| &a.id == attempt_id && a.operation_id != request.operation_id)
        || ledger.records.iter().any(|(a, _)| {
            &a.attempt.id == attempt_id && a.attempt.operation_id != request.operation_id
        })
    {
        return Err(ValidationError("attempt identity already reserved".into()));
    }
    if let Some((existing, delivery)) = ledger
        .records
        .iter()
        .find(|(a, _)| a.attempt.operation_id == request.operation_id)
    {
        let a = &existing.attempt;
        if &a.id != attempt_id
            || a.task_id != request.task_id
            || a.config != *config
            || a.folder != request.folder.path
            || a.persistent_binding != request.persistent_binding
            || a.runtime_binding.as_ref() != Some(&request.runtime_binding)
        {
            return Err(ValidationError("operation assignment changed".into()));
        }
        return Ok(Some(PrepareOutcome::Existing {
            attempt_id: a.id.clone(),
            delivery: *delivery,
        }));
    }
    if let Some(a) = state
        .attempts
        .iter()
        .find(|a| a.operation_id == request.operation_id)
    {
        if &a.id != attempt_id
            || a.task_id != request.task_id
            || a.config != *config
            || a.folder != request.folder.path
            || a.persistent_binding != request.persistent_binding
            || a.runtime_binding.as_ref() != Some(&request.runtime_binding)
        {
            return Err(ValidationError(
                "restored operation assignment changed".into(),
            ));
        }
        return Ok(Some(PrepareOutcome::Existing {
            attempt_id: a.id.clone(),
            delivery: DeliveryState::Uncertain,
        }));
    }
    Ok(None)
}

pub(crate) fn validate_dispatch_folder(
    state: &ProjectWorkState,
    request: &DispatchRequest,
    config: &ProjectConfig,
    policy: &ExecutionPolicy,
) -> Result<(), ValidationError> {
    validate_folder(&request.folder.path)?;
    if config.use_worktrees {
        let root = policy
            .worktree_root
            .as_ref()
            .ok_or_else(|| ValidationError("no approved worktree root".into()))?;
        if request.folder.worktree_owner.as_ref() != Some(&request.task_id)
            || request.folder.path == *root
            || !Path::new(&request.folder.path).starts_with(root)
            || request.folder.path == policy.project_folder
        {
            return Err(ValidationError(
                "worktree is outside its approved task ownership".into(),
            ));
        }
    } else if request.folder.worktree_owner.is_some()
        || request.folder.path != policy.project_folder
    {
        return Err(ValidationError(
            "shared-folder dispatch must use the approved project folder".into(),
        ));
    }
    if state.attempts.iter().any(|a| {
        a.task_id != request.task_id
            && a.folder == request.folder.path
            && (config.use_worktrees || is_active_execution(a.state) && a.config.use_worktrees)
    }) {
        return Err(ValidationError(
            "folder belongs to another worktree task".into(),
        ));
    }
    Ok(())
}

pub(crate) fn prepare_assignment(
    state: &ProjectWorkState,
    request: DispatchRequest,
    attempt_id: AttemptId,
    config: &ProjectConfig,
) -> Result<PreparedDispatch, ValidationError> {
    let mut next_state = state.clone();
    let id = next_state.begin_attempt_with_id(
        &request.task_id,
        attempt_id,
        request.operation_id,
        config.clone(),
        request.folder.path,
        request.persistent_binding,
        request.runtime_binding,
    )?;
    let task = next_state
        .tasks
        .iter()
        .find(|t| t.id == request.task_id)
        .unwrap()
        .clone();
    let attempt = next_state
        .attempts
        .iter()
        .find(|a| a.id == id)
        .unwrap()
        .clone();
    let source_results: Vec<_> = task
        .dependencies
        .iter()
        .map(|d| DependencyResultReference {
            task_id: d.task_id.clone(),
            result: state
                .tasks
                .iter()
                .find(|t| t.id == d.task_id)
                .unwrap()
                .result
                .clone()
                .unwrap(),
        })
        .collect();
    let mut prompt = format!(
        "Task {}\nGoal {}\nRole: {}\nAllowed relative scope roots:\n{}\nInstructions:\n{}\nAcceptance criteria:\n{}\n",
        task.id, task.goal_id, task.role, task.scope, task.instructions, task.acceptance_criteria
    );
    for source in &source_results {
        prompt.push_str(&format!(
            "\nDependency {} result revision {} source {:?} artifact {:?}:\n{}\n",
            source.task_id,
            source.result.revision.get(),
            source.result.source_revision,
            source.result.artifact_revision,
            source.result.summary
        ));
    }
    validate_input(&prompt)?;
    Ok(PreparedDispatch {
        next_state,
        assignment: DispatchAssignment {
            task,
            attempt,
            source_results,
            prompt,
        },
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::work_management::queue;
    use deppy_core::SessionId;

    pub(crate) fn agent() -> AgentSelection {
        AgentSelection {
            provider: "codex".into(),
            model: Some("model".into()),
            effort: Some("xhigh".into()),
        }
    }
    pub(crate) fn fixture() -> (ProjectWorkState, ProjectConfig, ExecutionPolicy, GoalId) {
        let mut state = ProjectWorkState::new(WorkspaceId::new());
        let cfg = ProjectConfig {
            workspace_id: state.workspace_id.clone(),
            revision: ConfigRevision(1),
            name: "Fixture".into(),
            folder: "/fixture/project".into(),
            main_agent: agent(),
            worker_agent: agent(),
            max_workers: 2,
            confirm_plan: true,
            use_worktrees: false,
        };
        let policy = ExecutionPolicy {
            workspace_id: state.workspace_id.clone(),
            config_revision: cfg.revision,
            project_folder: cfg.folder.clone(),
            worktree_root: Some("/fixture/worktrees".into()),
            allowed_agents: vec![agent()],
            allowed_scope_roots: vec!["src".into(), "tests".into()],
            max_workers: 2,
        };
        let goal = state
            .submit_goal(SubmissionId::new(), "Goal".into(), "Real goal".into())
            .unwrap();
        let tasks = ["src/a", "src/b", "tests"]
            .into_iter()
            .map(|scope| Task {
                workspace_id: state.workspace_id.clone(),
                id: TaskId::new(),
                goal_id: goal.clone(),
                parent_task_id: None,
                role: "worker".into(),
                scope: scope.into(),
                assigned_agent: agent(),
                execution_device: None,
                title: scope.into(),
                instructions: format!("Implement {scope}"),
                acceptance_criteria: "Verify result".into(),
                state: WorkState::PlanReview,
                current_attempt_id: None,
                dependencies: Vec::new(),
                result: None,
            })
            .collect();
        state
            .install_plan(WorkPlan {
                workspace_id: state.workspace_id.clone(),
                goal_id: goal.clone(),
                tasks,
            })
            .unwrap();
        (state, cfg, policy, goal)
    }
    pub(crate) fn request(task: &Task, policy: &ExecutionPolicy) -> DispatchRequest {
        DispatchRequest {
            task_id: task.id.clone(),
            operation_id: OperationId::new(),
            folder: DispatchFolder {
                path: policy.project_folder.clone(),
                worktree_owner: None,
            },
            persistent_binding: None,
            runtime_binding: RuntimeBinding {
                incarnation: RuntimeIncarnationId::new(),
                session_id: SessionId(1),
                execution_generation: 1,
            },
        }
    }
    fn ready(outcome: PrepareOutcome) -> Box<PreparedDispatch> {
        match outcome {
            PrepareOutcome::Ready(next) => next,
            _ => panic!("expected new dispatch"),
        }
    }

    #[test]
    fn work_management_dispatch_preallocated_attempt_matches_durable_launch_identity() {
        let (mut state, cfg, policy, goal) = fixture();
        let token = ConfirmedPlan::confirm(&mut state, &goal, &cfg, &policy).unwrap();
        let mut ledger = DeliveryLedger::default();
        let req = request(&state.tasks[0], &policy);
        let reserved = AttemptId::new();
        let before = format!("{state:?}");
        let prepared = ready(
            prepare_dispatch_with_attempt_id(
                &state,
                &ledger,
                &token,
                req.clone(),
                reserved.clone(),
                &cfg,
                &policy,
            )
            .unwrap(),
        );
        assert_eq!(format!("{state:?}"), before);
        assert_eq!(prepared.assignment.attempt.id, reserved);
        assert_eq!(
            prepared.assignment.task.current_attempt_id.as_ref(),
            Some(&reserved)
        );
        state = prepared.next_state;
        assert!(
            prepare_dispatch_with_attempt_id(
                &state,
                &ledger,
                &token,
                req.clone(),
                AttemptId::new(),
                &cfg,
                &policy
            )
            .is_err()
        );
        ledger.record_durable(prepared.assignment).unwrap();
        assert!(
            matches!(prepare_dispatch_with_attempt_id(&state, &ledger, &token, req.clone(), reserved.clone(), &cfg, &policy).unwrap(), PrepareOutcome::Existing{attempt_id,..} if attempt_id == reserved)
        );
        assert!(
            prepare_dispatch_with_attempt_id(
                &state,
                &ledger,
                &token,
                req.clone(),
                AttemptId::new(),
                &cfg,
                &policy
            )
            .is_err()
        );
        assert!(
            matches!(prepare_dispatch(&state, &ledger, &token, req, &cfg, &policy).unwrap(), PrepareOutcome::Existing{attempt_id,..} if attempt_id == reserved)
        );
    }

    #[test]
    fn work_management_dispatch_preallocated_collision_is_rejected_before_mutation() {
        let (mut state, cfg, policy, goal) = fixture();
        let token = ConfirmedPlan::confirm(&mut state, &goal, &cfg, &policy).unwrap();
        let mut ledger = DeliveryLedger::default();
        let reserved = AttemptId::new();
        let prepared = ready(
            prepare_dispatch_with_attempt_id(
                &state,
                &ledger,
                &token,
                request(&state.tasks[0], &policy),
                reserved.clone(),
                &cfg,
                &policy,
            )
            .unwrap(),
        );
        // Even a durable ledger without the swapped live state owns its reserved identity.
        ledger.record_durable(prepared.assignment.clone()).unwrap();
        let before = format!("{state:?}");
        assert!(
            prepare_dispatch_with_attempt_id(
                &state,
                &ledger,
                &token,
                request(&state.tasks[1], &policy),
                reserved.clone(),
                &cfg,
                &policy
            )
            .is_err()
        );
        assert_eq!(format!("{state:?}"), before);
        state = prepared.next_state;
        let before = format!("{state:?}");
        assert!(
            prepare_dispatch_with_attempt_id(
                &state,
                &DeliveryLedger::default(),
                &token,
                request(&state.tasks[1], &policy),
                reserved,
                &cfg,
                &policy
            )
            .is_err()
        );
        let mut invalid = request(&state.tasks[1], &policy);
        invalid.folder.path = "/outside/project".into();
        assert!(
            prepare_dispatch_with_attempt_id(
                &state,
                &ledger,
                &token,
                invalid,
                AttemptId::new(),
                &cfg,
                &policy
            )
            .is_err()
        );
        assert_eq!(format!("{state:?}"), before);
    }

    #[test]
    fn work_management_dispatch_requires_explicit_confirmation_then_two_independent_workers() {
        let (mut state, cfg, policy, goal) = fixture();
        let token = ConfirmedPlan::confirm(&mut state, &goal, &cfg, &policy).unwrap();
        let mut ledger = DeliveryLedger::default();
        let first = ready(
            prepare_dispatch(
                &state,
                &ledger,
                &token,
                request(&state.tasks[0], &policy),
                &cfg,
                &policy,
            )
            .unwrap(),
        );
        assert!(state.attempts.is_empty());
        state = first.next_state;
        ledger.record_durable(first.assignment).unwrap();
        let second = ready(
            prepare_dispatch(
                &state,
                &ledger,
                &token,
                request(&state.tasks[1], &policy),
                &cfg,
                &policy,
            )
            .unwrap(),
        );
        assert_ne!(state.attempts[0].id, second.assignment.attempt.id);
        state = second.next_state;
        assert_eq!(state.attempts.len(), 2);
        assert!(
            queue::ready_tasks(&state, &token, &cfg, &policy, &ledger)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn work_management_dispatch_same_operation_and_uncertainty_never_resend() {
        let (mut state, cfg, policy, goal) = fixture();
        let token = ConfirmedPlan::confirm(&mut state, &goal, &cfg, &policy).unwrap();
        let mut ledger = DeliveryLedger::default();
        let req = request(&state.tasks[0], &policy);
        let prepared =
            ready(prepare_dispatch(&state, &ledger, &token, req.clone(), &cfg, &policy).unwrap());
        state = prepared.next_state;
        ledger.record_durable(prepared.assignment.clone()).unwrap();
        assert!(ledger.claim_send(&req.operation_id));
        assert!(!ledger.claim_send(&req.operation_id));
        ledger.mark_uncertain(&req.operation_id);
        assert!(matches!(
            prepare_dispatch(&state, &ledger, &token, req.clone(), &cfg, &policy).unwrap(),
            PrepareOutcome::Existing {
                delivery: DeliveryState::Uncertain,
                ..
            }
        ));
        assert!(!ledger.claim_send(&req.operation_id));
        let mut altered = req;
        altered.folder.path = "/fixture/other".into();
        assert!(prepare_dispatch(&state, &ledger, &token, altered, &cfg, &policy).is_err());
    }

    #[test]
    fn work_management_dispatch_stale_plan_scope_conflicts_and_unowned_worktrees_are_blocked() {
        let (mut state, mut cfg, policy, goal) = fixture();
        state.tasks[1].scope = "src".into();
        let token = ConfirmedPlan::confirm(&mut state, &goal, &cfg, &policy).unwrap();
        let prepared = ready(
            prepare_dispatch(
                &state,
                &DeliveryLedger::default(),
                &token,
                request(&state.tasks[0], &policy),
                &cfg,
                &policy,
            )
            .unwrap(),
        );
        state = prepared.next_state;
        let choices =
            queue::ready_tasks(&state, &token, &cfg, &policy, &DeliveryLedger::default()).unwrap();
        assert!(!choices.contains(&state.tasks[1].id));
        cfg.revision = ConfigRevision(2);
        assert!(
            queue::ready_tasks(&state, &token, &cfg, &policy, &DeliveryLedger::default()).is_err()
        );
        let (mut state, mut cfg, policy, goal) = fixture();
        cfg.use_worktrees = true;
        let token = ConfirmedPlan::confirm(&mut state, &goal, &cfg, &policy).unwrap();
        let mut req = request(&state.tasks[0], &policy);
        req.folder.path = "/fixture/worktrees/task-a".into();
        req.folder.worktree_owner = Some(state.tasks[1].id.clone());
        assert!(
            prepare_dispatch(
                &state,
                &DeliveryLedger::default(),
                &token,
                req.clone(),
                &cfg,
                &policy
            )
            .is_err()
        );
        req.folder.worktree_owner = Some(state.tasks[0].id.clone());
        assert!(
            prepare_dispatch(
                &state,
                &DeliveryLedger::default(),
                &token,
                req,
                &cfg,
                &policy
            )
            .is_ok()
        );
    }

    #[test]
    fn work_management_dispatch_dependency_gate_and_restored_receipts_keep_exact_execution() {
        let (mut state, cfg, policy, goal) = fixture();
        let source = state.tasks[0].id.clone();
        let child = state.tasks[1].id.clone();
        state.tasks[1].dependencies = vec![TaskDependency {
            task_id: source.clone(),
            required_result_revision: ResultRevision(1),
        }];
        let token = ConfirmedPlan::confirm(&mut state, &goal, &cfg, &policy).unwrap();
        assert_eq!(token.goal_id(), &goal);
        let empty = DeliveryLedger::default();
        assert!(
            !queue::ready_tasks(&state, &token, &cfg, &policy, &empty)
                .unwrap()
                .contains(&child)
        );
        let req = request(&state.tasks[0], &policy);
        let prepared = ready(prepare_dispatch(&state, &empty, &token, req, &cfg, &policy).unwrap());
        state = prepared.next_state;
        let assignment = prepared.assignment;
        let mut ledger = DeliveryLedger::default();
        ledger
            .restore(assignment.clone(), DeliveryState::Sent)
            .unwrap();
        let op = &assignment.attempt.operation_id;
        assert_eq!(ledger.stage(op), Some(DeliveryState::Uncertain));
        assert!(!ledger.claim_send(op));
        assert!(!ledger.note_admitted(
            op,
            &assignment.attempt.id,
            &RuntimeBinding {
                incarnation: RuntimeIncarnationId::new(),
                session_id: deppy_core::SessionId(1),
                execution_generation: 1
            }
        ));
        let binding = assignment.attempt.runtime_binding.clone().unwrap();
        assert!(ledger.note_admitted(op, &assignment.attempt.id, &binding));
        assert_eq!(ledger.stage(op), Some(DeliveryState::Admitted));
        let mut event = WorkEvent {
            id: EventId::new(),
            workspace_id: state.workspace_id.clone(),
            task_id: source,
            attempt_id: assignment.attempt.id.clone(),
            runtime_binding: binding.clone(),
            sequence: 1,
            kind: WorkEventKind::ResultSubmitted {
                result: TaskResult {
                    revision: ResultRevision(1),
                    summary: "Actual source result".into(),
                    source_revision: Some("commit".into()),
                    artifact_revision: Some("artifact".into()),
                    verification: VerificationState::NotRun,
                },
            },
        };
        state.apply_event(event.clone()).unwrap();
        assert!(
            !queue::ready_tasks(&state, &token, &cfg, &policy, &ledger)
                .unwrap()
                .contains(&child)
        );
        event.id = EventId::new();
        event.sequence = 2;
        event.kind = WorkEventKind::VerificationFinished {
            result_revision: ResultRevision(1),
            outcome: VerificationState::Passed,
        };
        state.apply_event(event).unwrap();
        assert!(
            queue::ready_tasks(&state, &token, &cfg, &policy, &ledger)
                .unwrap()
                .contains(&child)
        );
        let next = ready(
            prepare_dispatch(
                &state,
                &ledger,
                &token,
                request(&state.tasks[1], &policy),
                &cfg,
                &policy,
            )
            .unwrap(),
        );
        assert_eq!(next.assignment.source_results.len(), 1);
        assert_eq!(
            next.assignment.source_results[0]
                .result
                .artifact_revision
                .as_deref(),
            Some("artifact")
        );
        let mut rejected = DeliveryLedger::default();
        rejected.record_durable(next.assignment.clone()).unwrap();
        let op = &next.assignment.attempt.operation_id;
        assert!(rejected.claim_send(op));
        assert!(rejected.note_rejected(
            op,
            &next.assignment.attempt.id,
            next.assignment.attempt.runtime_binding.as_ref().unwrap()
        ));
        assert!(!rejected.claim_send(op));
        assert_eq!(rejected.stage(op), Some(DeliveryState::Rejected));
    }
}
