use deppy_core::WorkspaceId;
use deppy_core::work_management::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    Ignored,
}

#[derive(Debug, Clone)]
pub struct ProjectWorkState {
    pub workspace_id: WorkspaceId,
    pub goals: Vec<Goal>,
    pub tasks: Vec<Task>,
    pub attempts: Vec<Attempt>,
}

impl ProjectWorkState {
    pub fn new(workspace_id: WorkspaceId) -> Self {
        Self {
            workspace_id,
            goals: Vec::new(),
            tasks: Vec::new(),
            attempts: Vec::new(),
        }
    }
    pub fn submit_goal(
        &mut self,
        submission_id: SubmissionId,
        title: String,
        prompt: String,
    ) -> Result<GoalId, ValidationError> {
        if let Some(goal) = self
            .goals
            .iter()
            .find(|goal| goal.submission_id == submission_id)
        {
            return if goal.title == title && goal.prompt == prompt {
                Ok(goal.id.clone())
            } else {
                Err(ValidationError(
                    "submission payload conflicts with existing goal".into(),
                ))
            };
        }
        let goal = Goal {
            workspace_id: self.workspace_id.clone(),
            id: GoalId::new(),
            submission_id,
            title,
            prompt,
            state: WorkState::PlanReview,
        };
        goal.validate()?;
        if self.goals.len() == WORK_PLAN_MAX_TASKS {
            return Err(ValidationError("project goal limit reached".into()));
        }
        let id = goal.id.clone();
        self.goals.push(goal);
        Ok(id)
    }
    pub fn install_plan(&mut self, plan: WorkPlan) -> Result<(), ValidationError> {
        plan.validate()?;
        if plan.workspace_id != self.workspace_id
            || !self.goals.iter().any(|g| g.id == plan.goal_id)
        {
            return Err(ValidationError("unknown goal/workspace".into()));
        }
        if self.attempts.iter().any(|a| {
            self.tasks
                .iter()
                .any(|t| t.id == a.task_id && t.goal_id == plan.goal_id)
        }) {
            return Err(ValidationError("cannot replace plan after dispatch".into()));
        }
        if plan.tasks.iter().any(|t| {
            t.state != WorkState::PlanReview
                || t.current_attempt_id.is_some()
                || t.result.is_some()
                || self
                    .tasks
                    .iter()
                    .any(|existing| existing.id == t.id && existing.goal_id != plan.goal_id)
        }) {
            return Err(ValidationError(
                "plan must contain unstarted tasks with independent identities".into(),
            ));
        }
        if self
            .tasks
            .iter()
            .filter(|t| t.goal_id != plan.goal_id)
            .count()
            + plan.tasks.len()
            > WORK_PLAN_MAX_TASKS
        {
            return Err(ValidationError("project task limit reached".into()));
        }
        self.tasks.retain(|t| t.goal_id != plan.goal_id);
        self.tasks.extend(plan.tasks);
        self.goals
            .iter_mut()
            .find(|g| g.id == plan.goal_id)
            .unwrap()
            .state = WorkState::PlanReview;
        Ok(())
    }
    pub fn confirm_plan(&mut self, goal: &GoalId) -> Result<(), ValidationError> {
        let goal = self
            .goals
            .iter_mut()
            .find(|g| &g.id == goal)
            .ok_or_else(|| ValidationError("unknown goal".into()))?;
        if !self.tasks.iter().any(|t| t.goal_id == goal.id) {
            return Err(ValidationError("goal has no plan".into()));
        }
        if goal.state != WorkState::PlanReview {
            return Ok(());
        }
        goal.state = WorkState::Queued;
        for task in self.tasks.iter_mut().filter(|t| t.goal_id == goal.id) {
            task.state = WorkState::Queued;
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub fn begin_attempt(
        &mut self,
        task_id: &TaskId,
        operation_id: OperationId,
        config: ProjectConfig,
        folder: String,
        persistent_binding: Option<PersistentBinding>,
        runtime_binding: RuntimeBinding,
    ) -> Result<AttemptId, ValidationError> {
        let attempt_id = self
            .attempts
            .iter()
            .find(|a| a.operation_id == operation_id)
            .map(|a| a.id.clone())
            .unwrap_or_else(AttemptId::new);
        self.begin_attempt_with_id(
            task_id,
            attempt_id,
            operation_id,
            config,
            folder,
            persistent_binding,
            runtime_binding,
        )
    }
    /// Preserve an identity durably reserved by the host before a runtime is spawned.
    /// The eventual exact runtime binding is still required before beginning the attempt.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_attempt_with_id(
        &mut self,
        task_id: &TaskId,
        attempt_id: AttemptId,
        operation_id: OperationId,
        config: ProjectConfig,
        folder: String,
        persistent_binding: Option<PersistentBinding>,
        runtime_binding: RuntimeBinding,
    ) -> Result<AttemptId, ValidationError> {
        if let Some(attempt) = self
            .attempts
            .iter()
            .find(|a| a.operation_id == operation_id)
        {
            return if attempt.id == attempt_id
                && &attempt.task_id == task_id
                && attempt.config == config
                && attempt.folder == folder
                && attempt.persistent_binding == persistent_binding
                && attempt.runtime_binding.as_ref() == Some(&runtime_binding)
            {
                Ok(attempt.id.clone())
            } else {
                Err(ValidationError(
                    "operation payload conflicts with existing attempt".into(),
                ))
            };
        }
        if self.attempts.iter().any(|attempt| attempt.id == attempt_id) {
            return Err(ValidationError("attempt identity already reserved".into()));
        }
        if !self.can_dispatch(task_id) {
            return Err(ValidationError(
                "task is not ready for explicit dispatch".into(),
            ));
        }
        if self.attempts.iter().any(|attempt| {
            matches!(
                attempt.state,
                WorkState::Queued
                    | WorkState::Running
                    | WorkState::WaitingApproval
                    | WorkState::WaitingInput
                    | WorkState::ObservationUnknown
            ) && attempt.runtime_binding.as_ref().is_some_and(|binding| {
                binding.incarnation == runtime_binding.incarnation
                    && binding.session_id == runtime_binding.session_id
            })
        }) {
            return Err(ValidationError(
                "runtime session already owns a live attempt".into(),
            ));
        }
        let number = self
            .attempts
            .iter()
            .filter(|a| &a.task_id == task_id)
            .map(|a| a.number)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| ValidationError("attempt number exhausted".into()))?;
        let attempt = Attempt {
            workspace_id: self.workspace_id.clone(),
            id: attempt_id,
            task_id: task_id.clone(),
            number,
            operation_id,
            config,
            folder,
            persistent_binding,
            runtime_binding: Some(runtime_binding),
            state: WorkState::Queued,
            last_event_sequence: 0,
        };
        attempt.validate()?;
        if self.attempts.len() == WORK_DRAFT_MAX_COUNT {
            return Err(ValidationError("project attempt limit reached".into()));
        }
        let id = attempt.id.clone();
        let task = self.tasks.iter_mut().find(|t| &t.id == task_id).unwrap();
        task.current_attempt_id = Some(id.clone());
        task.state = WorkState::Queued;
        if let Some(result) = &mut task.result {
            result.verification = VerificationState::NotRun;
        }
        self.attempts.push(attempt);
        self.invalidate_stale_verifications();
        self.refresh_goals();
        Ok(id)
    }
    pub fn apply_event(&mut self, event: WorkEvent) -> Result<ApplyOutcome, ValidationError> {
        event.validate()?;
        if event.workspace_id != self.workspace_id {
            return Ok(ApplyOutcome::Ignored);
        }
        let Some(ti) = self.tasks.iter().position(|t| {
            t.id == event.task_id
                && t.workspace_id == event.workspace_id
                && t.current_attempt_id.as_ref() == Some(&event.attempt_id)
        }) else {
            return Ok(ApplyOutcome::Ignored);
        };
        let Some(ai) = self.attempts.iter().position(|a| {
            a.id == event.attempt_id
                && a.task_id == event.task_id
                && a.workspace_id == event.workspace_id
                && a.runtime_binding.as_ref() == Some(&event.runtime_binding)
                && event.sequence > a.last_event_sequence
        }) else {
            return Ok(ApplyOutcome::Ignored);
        };
        let task = &self.tasks[ti];
        if task.state.is_terminal() {
            return Ok(ApplyOutcome::Ignored);
        }
        let state = task.state;
        let active = matches!(
            state,
            WorkState::Queued
                | WorkState::Running
                | WorkState::WaitingApproval
                | WorkState::WaitingInput
                | WorkState::ObservationUnknown
        );
        let next = match &event.kind {
            WorkEventKind::InputAdmitted { operation_id }
                if *operation_id == self.attempts[ai].operation_id
                    && state == WorkState::Queued =>
            {
                Some(WorkState::Queued)
            }
            WorkEventKind::Started if active => Some(WorkState::Running),
            WorkEventKind::WaitingApproval if active => Some(WorkState::WaitingApproval),
            WorkEventKind::WaitingInput if active => Some(WorkState::WaitingInput),
            WorkEventKind::ObservationLost if active => Some(WorkState::ObservationUnknown),
            WorkEventKind::ProcessExited if active => Some(WorkState::ObservationUnknown),
            WorkEventKind::ResultSubmitted { result }
                if result.verification == VerificationState::NotRun
                    && task
                        .result
                        .as_ref()
                        .is_none_or(|old| result.revision > old.revision) =>
            {
                Some(WorkState::ResultSubmitted)
            }
            WorkEventKind::VerificationFinished {
                result_revision,
                outcome,
            } if matches!(
                state,
                WorkState::ResultSubmitted
                    | WorkState::VerificationFailed
                    | WorkState::AwaitingUserConfirmation
            ) && task
                .result
                .as_ref()
                .is_some_and(|r| r.revision == *result_revision)
                && self.dependencies_satisfied(task) =>
            {
                match outcome {
                    VerificationState::Passed => Some(WorkState::AwaitingUserConfirmation),
                    VerificationState::Failed => Some(WorkState::VerificationFailed),
                    VerificationState::NotRun => None,
                }
            }
            WorkEventKind::UserConfirmed { result_revision }
                if state == WorkState::AwaitingUserConfirmation
                    && task.result.as_ref().is_some_and(|r| {
                        r.revision == *result_revision
                            && r.verification == VerificationState::Passed
                    })
                    && self.dependencies_satisfied(task) =>
            {
                Some(WorkState::Completed)
            }
            WorkEventKind::Failed { .. } => Some(WorkState::Failed),
            WorkEventKind::Cancelled => Some(WorkState::Cancelled),
            _ => None,
        };
        let Some(next) = next else {
            return Ok(ApplyOutcome::Ignored);
        };
        let task = &mut self.tasks[ti];
        match event.kind {
            WorkEventKind::ResultSubmitted { result } => task.result = Some(result),
            WorkEventKind::VerificationFinished { outcome, .. } => {
                task.result.as_mut().unwrap().verification = outcome
            }
            _ => {}
        }
        task.state = next;
        self.attempts[ai].state = next;
        self.attempts[ai].last_event_sequence = event.sequence;
        self.invalidate_stale_verifications();
        self.refresh_goals();
        Ok(ApplyOutcome::Applied)
    }
    pub fn can_dispatch(&self, id: &TaskId) -> bool {
        let Some(task) = self
            .tasks
            .iter()
            .find(|t| &t.id == id && t.workspace_id == self.workspace_id)
        else {
            return false;
        };
        if !matches!(
            task.state,
            WorkState::Queued
                | WorkState::Failed
                | WorkState::Cancelled
                | WorkState::VerificationFailed
        ) {
            return false;
        }
        if task.current_attempt_id.as_ref().is_some_and(|id| {
            self.attempts.iter().any(|a| {
                &a.id == id
                    && !matches!(
                        a.state,
                        WorkState::Failed | WorkState::Cancelled | WorkState::VerificationFailed
                    )
            })
        }) {
            return false;
        }
        self.dependencies_satisfied(task)
    }
    pub fn goal_is_complete(&self, goal: &GoalId) -> bool {
        let mut tasks = self.tasks.iter().filter(|t| &t.goal_id == goal);
        tasks.next().is_some_and(|first| {
            first.state == WorkState::Completed && tasks.all(|t| t.state == WorkState::Completed)
        })
    }
    fn dependencies_satisfied(&self, task: &Task) -> bool {
        task.dependencies.iter().all(|dependency| {
            self.tasks.iter().any(|source| {
                source.id == dependency.task_id
                    && source.workspace_id == task.workspace_id
                    && source.goal_id == task.goal_id
                    && source.result.as_ref().is_some_and(|r| {
                        r.revision == dependency.required_result_revision
                            && r.verification == VerificationState::Passed
                    })
            })
        })
    }
    fn invalidate_stale_verifications(&mut self) {
        loop {
            let stale: Vec<_> = self
                .tasks
                .iter()
                .enumerate()
                .filter(|(_, t)| {
                    t.result
                        .as_ref()
                        .is_some_and(|r| r.verification == VerificationState::Passed)
                        && !self.dependencies_satisfied(t)
                })
                .map(|(i, _)| i)
                .collect();
            if stale.is_empty() {
                break;
            }
            for i in stale {
                let task = &mut self.tasks[i];
                task.result.as_mut().unwrap().verification = VerificationState::NotRun;
                task.state = WorkState::VerificationFailed;
                if let Some(id) = &task.current_attempt_id {
                    if let Some(attempt) = self.attempts.iter_mut().find(|a| &a.id == id) {
                        attempt.state = WorkState::VerificationFailed;
                    }
                }
            }
        }
    }
    fn refresh_goals(&mut self) {
        for goal in &mut self.goals {
            let states: Vec<_> = self
                .tasks
                .iter()
                .filter(|t| t.goal_id == goal.id)
                .map(|t| t.state)
                .collect();
            if states.is_empty() {
                continue;
            }
            goal.state = if states.iter().all(|s| *s == WorkState::Completed) {
                WorkState::Completed
            } else {
                [
                    WorkState::PlanReview,
                    WorkState::ObservationUnknown,
                    WorkState::WaitingApproval,
                    WorkState::WaitingInput,
                    WorkState::Running,
                    WorkState::VerificationFailed,
                    WorkState::Failed,
                    WorkState::Cancelled,
                    WorkState::Queued,
                    WorkState::ResultSubmitted,
                    WorkState::AwaitingUserConfirmation,
                ]
                .into_iter()
                .find(|s| states.contains(s))
                .unwrap_or(WorkState::ObservationUnknown)
            };
        }
    }
}
