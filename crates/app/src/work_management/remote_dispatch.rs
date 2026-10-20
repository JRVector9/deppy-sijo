//! Explicit mixed-device confirmation. Planner JSON never expands execution authority.
use super::ProjectWorkState;
use super::devices::{
    LeaseRequest, RegisteredDevice, VerifiedExecutionDevice, VerifiedExecutionLease, WorkerProfile,
};
use super::dispatch::{
    self, ConfirmedPlan, DeliveryLedger, DeliveryState, DispatchRequest, ExecutionPolicy,
    PrepareOutcome,
};
use deppy_core::work_management::*;

#[derive(Debug, Clone)]
struct ApprovedDevice {
    registration: RegisteredDevice,
    profiles: Vec<WorkerProfile>,
}

/// Created only after the user reviews all amended task assignments and confirms the whole plan.
/// The immutable registration/profile choices are permission bounds, not a live dispatch permit.
#[derive(Debug, Clone)]
pub(crate) struct RemoteConfirmedPlan {
    confirmed: ConfirmedPlan,
    devices: Vec<ApprovedDevice>,
}

/// A pre-launch assignment, not permission to replay a launch. Keep its live fence in the
/// operation-bound RemoteSendAuthority callback through the actual controller send.
#[derive(Debug, Clone)]
pub(crate) struct RemoteLaunchIntent {
    request: LeaseRequest,
    device: VerifiedExecutionDevice,
}
impl RemoteLaunchIntent {
    pub(crate) fn request(&self) -> &LeaseRequest {
        &self.request
    }
    pub(crate) fn validate_fresh(&self) -> Result<(), ValidationError> {
        self.device
            .validate_fresh()
            .map_err(|_| error("remote launch proof is stale, changed or revoked"))
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_remote_launch(
    state: &ProjectWorkState,
    ledger: &DeliveryLedger,
    confirmed: &RemoteConfirmedPlan,
    task_id: &TaskId,
    attempt_id: AttemptId,
    operation_id: OperationId,
    config: &ProjectConfig,
    policy: &ExecutionPolicy,
    device: &VerifiedExecutionDevice,
) -> Result<RemoteLaunchIntent, ValidationError> {
    confirmed.validate_current(state, config, policy)?;
    if config.use_worktrees {
        return Err(error("remote task worktree ownership is unsupported"));
    }
    device
        .validate_fresh()
        .map_err(|_| error("remote launch device is stale, changed or revoked"))?;
    AttemptId::from_string(attempt_id.0.clone())?;
    OperationId::from_string(operation_id.0.clone())?;
    let task = state
        .tasks
        .iter()
        .find(|task| &task.id == task_id && task.goal_id == *confirmed.goal_id())
        .ok_or_else(|| error("unknown confirmed remote task"))?;
    let approved = confirmed.device(task)?;
    let profile = confirmed.profile(approved, task)?;
    let profiles: Vec<_> = device
        .profiles()
        .iter()
        .filter(|profile| profile.agent == task.assigned_agent)
        .collect();
    if device.registration() != &approved.registration
        || profiles.len() != 1
        || profiles[0] != profile
    {
        return Err(error(
            "remote launch registration or exact profile changed after confirmation",
        ));
    }
    if state
        .attempts
        .iter()
        .any(|attempt| attempt.id == attempt_id || attempt.operation_id == operation_id)
        || ledger.records.iter().any(|(assignment, _)| {
            assignment.attempt.id == attempt_id || assignment.attempt.operation_id == operation_id
        })
    {
        return Err(error(
            "launch operation or attempt already claimed; never replay",
        ));
    }
    if !ready_tasks(state, confirmed, config, policy, ledger)?.contains(task_id) {
        return Err(error(
            "remote task is not ready: dependencies, capacity or file scope",
        ));
    }
    Ok(RemoteLaunchIntent {
        request: LeaseRequest {
            device_id: approved.registration.id.clone(),
            workspace_id: state.workspace_id.clone(),
            attempt_id,
            operation_id,
            folder: approved.registration.folder.clone(),
            agent: task.assigned_agent.clone(),
        },
        device: device.clone(),
    })
}
impl RemoteConfirmedPlan {
    pub(crate) fn confirm(
        state: &mut ProjectWorkState,
        goal: &GoalId,
        config: &ProjectConfig,
        policy: &ExecutionPolicy,
        devices: &[VerifiedExecutionDevice],
    ) -> Result<Self, ValidationError> {
        policy.validate_config(config)?;
        if state.workspace_id != policy.workspace_id
            || !state
                .goals
                .iter()
                .any(|g| &g.id == goal && g.state == WorkState::PlanReview)
            || devices.len() > super::devices::EXECUTION_DEVICE_MAX
        {
            return Err(error("mixed plan is not awaiting confirmation"));
        }
        let tasks: Vec<_> = state
            .tasks
            .iter()
            .filter(|task| &task.goal_id == goal)
            .cloned()
            .collect();
        WorkPlan {
            workspace_id: state.workspace_id.clone(),
            goal_id: goal.clone(),
            tasks: tasks.clone(),
        }
        .validate()?;
        let mut approved = Vec::new();
        for task in &tasks {
            if task.state != WorkState::PlanReview
                || task.current_attempt_id.is_some()
                || task.result.is_some()
            {
                return Err(error("proposal is already started"));
            }
            validate_scope_agent(task, policy)?;
            if let Some(id) = &task.execution_device {
                if config.use_worktrees {
                    return Err(error(
                        "remote task worktrees require server-owned worktree proof; disable worktrees before reviewing a remote plan",
                    ));
                }
                if approved
                    .iter()
                    .any(|device: &ApprovedDevice| &device.registration.id == id)
                {
                    continue;
                }
                let matching: Vec<_> = devices
                    .iter()
                    .filter(|device| &device.registration().id == id)
                    .collect();
                if matching.len() != 1 {
                    return Err(error("task requires one registered execution device proof"));
                }
                let device = matching[0];
                device
                    .validate_fresh()
                    .map_err(|_| error("execution device proof is stale or revoked"))?;
                let registration = device.registration();
                if registration.workspace_id != state.workspace_id
                    || !registration.execution_granted
                {
                    return Err(error(
                        "execution device belongs to another project or has no grant",
                    ));
                }
                approved.push(ApprovedDevice {
                    registration: registration.clone(),
                    profiles: device.profiles().to_vec(),
                });
            }
        }
        let confirmed = ConfirmedPlan {
            config: config.clone(),
            policy: policy.clone(),
            goal_id: goal.clone(),
            tasks,
        };
        let token = Self {
            confirmed,
            devices: approved,
        };
        for task in &token.confirmed.tasks {
            token.validate_task(task, policy)?;
        }
        state.confirm_plan(goal)?;
        Ok(token)
    }
    /// Explicitly review pending definitions against current live device proofs.
    pub(crate) fn confirm_remaining(
        state: &ProjectWorkState,
        reviewed: &WorkPlan,
        config: &ProjectConfig,
        policy: &ExecutionPolicy,
        devices: &[VerifiedExecutionDevice],
    ) -> Result<Self, ValidationError> {
        reviewed.validate()?;
        if devices.len() > super::devices::EXECUTION_DEVICE_MAX {
            return Err(error("too many execution device proofs"));
        }
        let confirmed = ConfirmedPlan {
            config: config.clone(),
            policy: policy.clone(),
            goal_id: reviewed.goal_id.clone(),
            tasks: reviewed.tasks.clone(),
        };
        confirmed.validate_current(state, config, policy)?;
        // Historical and live attempts retain their original config and need no
        // renewed launch grant. Every future launch uses a fresh exact device proof.
        let mut approved = Vec::new();
        for task in state
            .tasks
            .iter()
            .filter(|task| task.goal_id == reviewed.goal_id && dispatch::needs_confirmation(task))
        {
            validate_scope_agent(task, policy)?;
            if let Some(id) = &task.execution_device {
                if config.use_worktrees {
                    return Err(error("remote task worktree ownership is unsupported"));
                }
                let matching: Vec<_> = devices
                    .iter()
                    .filter(|device| &device.registration().id == id)
                    .collect();
                if matching.len() != 1 {
                    return Err(error("task requires one registered execution device proof"));
                }
                let device = matching[0];
                device
                    .validate_fresh()
                    .map_err(|_| error("execution device proof is stale or revoked"))?;
                if device.registration().workspace_id != state.workspace_id
                    || !device.registration().execution_granted
                {
                    return Err(error(
                        "execution device belongs to another project or has no grant",
                    ));
                }
                if !approved
                    .iter()
                    .any(|previous: &ApprovedDevice| previous.registration.id == *id)
                {
                    approved.push(ApprovedDevice {
                        registration: device.registration().clone(),
                        profiles: device.profiles().to_vec(),
                    });
                }
            }
        }
        let token = Self {
            confirmed,
            devices: approved,
        };
        token.validate_current(state, config, policy)?;
        Ok(token)
    }
    pub(crate) fn goal_id(&self) -> &GoalId {
        self.confirmed.goal_id()
    }
    pub(crate) fn validate_current(
        &self,
        state: &ProjectWorkState,
        config: &ProjectConfig,
        policy: &ExecutionPolicy,
    ) -> Result<(), ValidationError> {
        self.confirmed.validate_current(state, config, policy)?;
        for task in state
            .tasks
            .iter()
            .filter(|task| task.goal_id == *self.goal_id() && dispatch::needs_confirmation(task))
        {
            self.validate_task(task, policy)?;
        }
        Ok(())
    }
    fn device(&self, task: &Task) -> Result<&ApprovedDevice, ValidationError> {
        self.devices
            .iter()
            .find(|device| Some(&device.registration.id) == task.execution_device.as_ref())
            .ok_or_else(|| error("task has no confirmed registered execution device"))
    }
    fn profile<'a>(
        &self,
        device: &'a ApprovedDevice,
        task: &Task,
    ) -> Result<&'a WorkerProfile, ValidationError> {
        let profiles: Vec<_> = device
            .profiles
            .iter()
            .filter(|profile| profile.agent == task.assigned_agent)
            .collect();
        if profiles.len() != 1
            || !device
                .registration
                .allowed_agents
                .contains(&task.assigned_agent)
        {
            return Err(error(
                "remote task requires one exact approved provider/model/effort profile",
            ));
        }
        Ok(profiles[0])
    }
    fn validate_task(&self, task: &Task, policy: &ExecutionPolicy) -> Result<(), ValidationError> {
        validate_scope_agent(task, policy)?;
        if task.execution_device.is_some() {
            self.profile(self.device(task)?, task)?;
        }
        Ok(())
    }
}

fn error(message: &str) -> ValidationError {
    ValidationError(message.into())
}
fn validate_scope_agent(task: &Task, policy: &ExecutionPolicy) -> Result<(), ValidationError> {
    // Validate the same project/provider/scope bounds without granting the local policy remote authority.
    let mut local = task.clone();
    local.execution_device = None;
    policy.validate_task(&local)
}

/// Suggestions only. Each chosen launch is checked again after its exact runtime ACK.
pub(crate) fn ready_tasks(
    state: &ProjectWorkState,
    confirmed: &RemoteConfirmedPlan,
    config: &ProjectConfig,
    policy: &ExecutionPolicy,
    ledger: &DeliveryLedger,
) -> Result<Vec<TaskId>, ValidationError> {
    confirmed.validate_current(state, config, policy)?;
    let active: Vec<_> = state
        .attempts
        .iter()
        .filter(|attempt| dispatch::is_active_execution(attempt.state))
        .collect();
    let capacity = usize::from(policy.max_workers).saturating_sub(active.len());
    let mut selected: Vec<&Task> = Vec::new();
    for planned in &confirmed.confirmed.tasks {
        if selected.len() == capacity {
            break;
        }
        let task = state
            .tasks
            .iter()
            .find(|task| task.id == planned.id)
            .ok_or_else(|| error("confirmed task missing"))?;
        if !state.can_dispatch(&task.id)
            || ledger.records.iter().any(|(assignment, stage)| {
                assignment.task.id == task.id
                    && matches!(stage, DeliveryState::Sent | DeliveryState::Uncertain)
            })
        {
            continue;
        }
        confirmed.validate_task(task, policy)?;
        if task.execution_device.is_some() {
            let device = confirmed.device(task)?;
            let active_on_device = active
                .iter()
                .filter(|attempt| {
                    state.tasks.iter().any(|other| {
                        other.id == attempt.task_id
                            && other.execution_device == task.execution_device
                    })
                })
                .count();
            let selected_on_device = selected
                .iter()
                .filter(|other| other.execution_device == task.execution_device)
                .count();
            if active_on_device + selected_on_device >= usize::from(device.registration.max_workers)
            {
                continue;
            }
        }
        if !config.use_worktrees {
            let active_tasks: Result<Vec<_>, _> = active
                .iter()
                .map(|attempt| {
                    state
                        .tasks
                        .iter()
                        .find(|task| task.id == attempt.task_id)
                        .ok_or_else(|| error("active attempt owner missing"))
                })
                .collect();
            let mut conflict = false;
            for other in active_tasks?.into_iter().chain(selected.iter().copied()) {
                if task.execution_device == other.execution_device
                    && dispatch::scopes_conflict(task, other)?
                {
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
    Ok(selected.into_iter().map(|task| task.id.clone()).collect())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_remote_dispatch(
    state: &ProjectWorkState,
    ledger: &DeliveryLedger,
    confirmed: &RemoteConfirmedPlan,
    request: DispatchRequest,
    attempt_id: AttemptId,
    config: &ProjectConfig,
    policy: &ExecutionPolicy,
    verified: &VerifiedExecutionLease,
) -> Result<PrepareOutcome, ValidationError> {
    confirmed.validate_current(state, config, policy)?;
    if config.use_worktrees {
        return Err(error("remote task worktree ownership is unsupported"));
    }
    verified
        .validate_fresh()
        .map_err(|_| error("remote lease is stale, disconnected or revoked"))?;
    let lease = verified.lease();
    lease
        .validate()
        .map_err(|_| error("remote lease identity is invalid"))?;
    let task = state
        .tasks
        .iter()
        .find(|task| task.id == request.task_id)
        .ok_or_else(|| error("unknown remote task"))?;
    if task.goal_id != *confirmed.goal_id() {
        return Err(error("task belongs to another confirmed plan"));
    }
    let device = confirmed.device(task)?;
    let profile = confirmed.profile(device, task)?;
    let registration = &device.registration;
    if verified.registration() != registration
        || lease.request.device_id != registration.id
        || lease.request.workspace_id != state.workspace_id
        || lease.request.attempt_id != attempt_id
        || lease.request.operation_id != request.operation_id
        || lease.request.agent != task.assigned_agent
        || lease.profile_id != profile.id
        || lease.request.folder != registration.folder
        || lease.folder_anchor != registration.folder_anchor
        || lease.grant_epoch != registration.grant_epoch
        || request.folder.path != lease.request.folder
        || request.folder.worktree_owner.is_some()
        || request.runtime_binding.incarnation != lease.incarnation
    {
        return Err(error(
            "remote dispatch does not match the confirmed device/profile/folder and exact lease",
        ));
    }
    request.runtime_binding.validate()?;
    if let Some(existing) =
        dispatch::existing_dispatch(state, ledger, &request, &attempt_id, config)?
    {
        return Ok(existing);
    }
    if !ready_tasks(state, confirmed, config, policy, ledger)?.contains(&request.task_id) {
        return Err(error(
            "remote task is not ready: dependencies, capacity or file scope",
        ));
    }
    Ok(PrepareOutcome::Ready(Box::new(
        dispatch::prepare_assignment(state, request, attempt_id, config)?,
    )))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_mixed_local_dispatch(
    state: &ProjectWorkState,
    ledger: &DeliveryLedger,
    confirmed: &RemoteConfirmedPlan,
    request: DispatchRequest,
    attempt_id: AttemptId,
    config: &ProjectConfig,
    policy: &ExecutionPolicy,
) -> Result<PrepareOutcome, ValidationError> {
    confirmed.validate_current(state, config, policy)?;
    let task = state
        .tasks
        .iter()
        .find(|task| task.id == request.task_id)
        .ok_or_else(|| error("unknown local task"))?;
    if task.goal_id != *confirmed.goal_id() || task.execution_device.is_some() {
        return Err(error("local dispatch cannot execute a remote task"));
    }
    policy.validate_task(task)?;
    if let Some(existing) =
        dispatch::existing_dispatch(state, ledger, &request, &attempt_id, config)?
    {
        return Ok(existing);
    }
    if !ready_tasks(state, confirmed, config, policy, ledger)?.contains(&request.task_id) {
        return Err(error(
            "local task is not ready: dependencies, capacity or file scope",
        ));
    }
    dispatch::validate_dispatch_folder(state, &request, config, policy)?;
    Ok(PrepareOutcome::Ready(Box::new(
        dispatch::prepare_assignment(state, request, attempt_id, config)?,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_management::devices::{
        DeviceRegistry, LeaseRequest, RegisteredDevice, WorkerObservation, WorkerProfile,
    };
    use crate::work_management::dispatch::{self, DeliveryState};
    use deppy_core::{SessionId, WorkspaceId};

    fn registry(
        workspace: WorkspaceId,
        folder: &str,
        grant: u64,
        profile: &str,
    ) -> (DeviceRegistry, RegisteredDevice, WorkerObservation) {
        let registration = RegisteredDevice {
            id: "worker-a".into(),
            name: "Actual registered worker".into(),
            endpoint: "127.0.0.1:9000".parse().unwrap(),
            pinned_fingerprint: vec!["aa"; 32].join(":"),
            workspace_id: workspace,
            folder: folder.into(),
            folder_anchor: "remote-folder-anchor".into(),
            allowed_agents: vec![dispatch::tests::agent()],
            max_workers: 2,
            grant_epoch: grant,
            execution_granted: true,
        };
        let observation = WorkerObservation {
            device_id: registration.id.clone(),
            pinned_fingerprint: registration.pinned_fingerprint.clone(),
            connection_epoch: 1,
            incarnation: RuntimeIncarnationId("remote-incarnation".into()),
            workspace_id: registration.workspace_id.clone(),
            folder: registration.folder.clone(),
            folder_anchor: registration.folder_anchor.clone(),
            grant_epoch: grant,
            lease_epoch: 1,
            profiles: vec![WorkerProfile {
                id: profile.into(),
                agent: dispatch::tests::agent(),
            }],
            max_workers: 2,
            active_workers: 0,
            active_operation_ids: Vec::new(),
        };
        let mut registry = DeviceRegistry::default();
        registry.register(registration.clone()).unwrap();
        registry.observe(observation.clone(), 10).unwrap();
        (registry, registration, observation)
    }
    struct Fixture {
        state: ProjectWorkState,
        config: ProjectConfig,
        policy: ExecutionPolicy,
        goal: GoalId,
        registry: DeviceRegistry,
        device: RegisteredDevice,
    }
    impl Fixture {
        fn new() -> Self {
            let (mut state, config, policy, goal) = dispatch::tests::fixture();
            let (registry, device, _) = registry(
                state.workspace_id.clone(),
                "/remote/project",
                1,
                "approved-profile",
            );
            state.tasks[0].execution_device = Some(device.id.clone());
            Self {
                state,
                config,
                policy,
                goal,
                registry,
                device,
            }
        }
        fn confirm(&mut self) -> RemoteConfirmedPlan {
            let devices = self.registry.verified_devices(&self.state.workspace_id, 10);
            RemoteConfirmedPlan::confirm(
                &mut self.state,
                &self.goal,
                &self.config,
                &self.policy,
                &devices,
            )
            .unwrap()
        }
        fn lease(&mut self, index: usize) -> VerifiedExecutionLease {
            let task = &self.state.tasks[index];
            let raw = self
                .registry
                .reserve(
                    LeaseRequest {
                        device_id: self.device.id.clone(),
                        workspace_id: self.state.workspace_id.clone(),
                        attempt_id: AttemptId::new(),
                        operation_id: OperationId::new(),
                        folder: self.device.folder.clone(),
                        agent: task.assigned_agent.clone(),
                    },
                    10,
                )
                .unwrap();
            self.registry.verified_lease(&raw, 10).unwrap()
        }
        fn request(&self, lease: &VerifiedExecutionLease, index: usize) -> DispatchRequest {
            let raw = lease.lease();
            DispatchRequest {
                task_id: self.state.tasks[index].id.clone(),
                operation_id: raw.request.operation_id.clone(),
                folder: dispatch::DispatchFolder {
                    path: raw.request.folder.clone(),
                    worktree_owner: None,
                },
                persistent_binding: None,
                runtime_binding: RuntimeBinding {
                    incarnation: raw.incarnation.clone(),
                    session_id: SessionId(7),
                    execution_generation: 1,
                },
            }
        }
    }
    fn ready(outcome: PrepareOutcome) -> Box<dispatch::PreparedDispatch> {
        match outcome {
            PrepareOutcome::Ready(ready) => ready,
            _ => panic!("expected first dispatch"),
        }
    }
    #[test]
    fn mixed_reconfirmation_requires_fresh_exact_device_proofs_without_replaying_attempts() {
        let mut fixture = Fixture::new();
        let initial = fixture.confirm();
        let reviewed = WorkPlan {
            workspace_id: fixture.state.workspace_id.clone(),
            goal_id: fixture.goal.clone(),
            tasks: initial.confirmed.tasks.clone(),
        };
        fixture.config.revision = fixture.config.revision.next().unwrap();
        fixture.policy.config_revision = fixture.config.revision;
        let before = format!("{:?}", fixture.state);
        assert!(
            RemoteConfirmedPlan::confirm_remaining(
                &fixture.state,
                &reviewed,
                &fixture.config,
                &fixture.policy,
                &[]
            )
            .is_err()
        );
        let devices = fixture
            .registry
            .verified_devices(&fixture.state.workspace_id, 10);
        let restored = RemoteConfirmedPlan::confirm_remaining(
            &fixture.state,
            &reviewed,
            &fixture.config,
            &fixture.policy,
            &devices,
        )
        .unwrap();
        assert_eq!(format!("{:?}", fixture.state), before);
        let fresh_attempt = AttemptId::new();
        let operation = OperationId::new();
        let intent = prepare_remote_launch(
            &fixture.state,
            &DeliveryLedger::default(),
            &restored,
            &fixture.state.tasks[0].id,
            fresh_attempt.clone(),
            operation.clone(),
            &fixture.config,
            &fixture.policy,
            &devices[0],
        )
        .unwrap();
        assert_eq!(intent.request().attempt_id, fresh_attempt);
        assert_eq!(intent.request().operation_id, operation);
        assert!(
            fixture.state.attempts.is_empty(),
            "review and preparation must not launch or create an attempt"
        );
        let mut changed = reviewed.clone();
        changed.tasks[0].instructions.push_str("changed");
        assert!(
            RemoteConfirmedPlan::confirm_remaining(
                &fixture.state,
                &changed,
                &fixture.config,
                &fixture.policy,
                &devices
            )
            .is_err()
        );
    }
    #[test]
    fn verified_mixed_plan_prepares_exact_remote_attempt_without_sending_or_mutating_source() {
        let mut fixture = Fixture::new();
        assert!(
            dispatch::ConfirmedPlan::confirm(
                &mut fixture.state,
                &fixture.goal,
                &fixture.config,
                &fixture.policy
            )
            .is_err()
        );
        let confirmed = fixture.confirm();
        let lease = fixture.lease(0);
        let before = format!("{:?}", fixture.state);
        let prepared = ready(
            prepare_remote_dispatch(
                &fixture.state,
                &DeliveryLedger::default(),
                &confirmed,
                fixture.request(&lease, 0),
                lease.lease().request.attempt_id.clone(),
                &fixture.config,
                &fixture.policy,
                &lease,
            )
            .unwrap(),
        );
        assert_eq!(format!("{:?}", fixture.state), before);
        assert_eq!(prepared.assignment.attempt.folder, "/remote/project");
        assert_eq!(
            prepared.assignment.attempt.id,
            lease.lease().request.attempt_id
        );
        assert_eq!(
            prepared.assignment.task.execution_device.as_deref(),
            Some("worker-a")
        );
        assert_eq!(prepared.assignment.task.state, WorkState::Queued);
        assert!(
            prepared
                .assignment
                .prompt
                .contains("Allowed relative scope roots")
        );
    }
    #[test]
    fn remote_retry_and_uncertainty_never_create_a_second_send_or_rebind_operation() {
        let mut fixture = Fixture::new();
        let confirmed = fixture.confirm();
        let lease = fixture.lease(0);
        let req = fixture.request(&lease, 0);
        let id = lease.lease().request.attempt_id.clone();
        let mut ledger = DeliveryLedger::default();
        let prepared = ready(
            prepare_remote_dispatch(
                &fixture.state,
                &ledger,
                &confirmed,
                req.clone(),
                id.clone(),
                &fixture.config,
                &fixture.policy,
                &lease,
            )
            .unwrap(),
        );
        fixture.state = prepared.next_state;
        ledger.record_durable(prepared.assignment).unwrap();
        assert!(ledger.claim_send(&req.operation_id));
        ledger.mark_uncertain(&req.operation_id);
        assert!(matches!(
            prepare_remote_dispatch(
                &fixture.state,
                &ledger,
                &confirmed,
                req.clone(),
                id,
                &fixture.config,
                &fixture.policy,
                &lease
            )
            .unwrap(),
            PrepareOutcome::Existing {
                delivery: DeliveryState::Uncertain,
                ..
            }
        ));
        assert!(!ledger.claim_send(&req.operation_id));
        let mut wrong = req;
        wrong.runtime_binding.execution_generation += 1;
        assert!(
            prepare_remote_dispatch(
                &fixture.state,
                &ledger,
                &confirmed,
                wrong,
                lease.lease().request.attempt_id.clone(),
                &fixture.config,
                &fixture.policy,
                &lease
            )
            .is_err()
        );
    }
    #[test]
    fn remote_dispatch_rejects_wrong_task_operation_attempt_folder_and_incarnation() {
        let mut fixture = Fixture::new();
        let confirmed = fixture.confirm();
        let lease = fixture.lease(0);
        for change in 0..5 {
            let mut req = fixture.request(&lease, 0);
            let mut attempt = lease.lease().request.attempt_id.clone();
            match change {
                0 => req.task_id = fixture.state.tasks[1].id.clone(),
                1 => req.operation_id = OperationId::new(),
                2 => attempt = AttemptId::new(),
                3 => req.folder.path = "/remote/other".into(),
                _ => req.runtime_binding.incarnation = RuntimeIncarnationId::new(),
            }
            assert!(
                prepare_remote_dispatch(
                    &fixture.state,
                    &DeliveryLedger::default(),
                    &confirmed,
                    req,
                    attempt,
                    &fixture.config,
                    &fixture.policy,
                    &lease
                )
                .is_err()
            );
        }
    }
    #[test]
    fn remote_confirmation_rejects_unregistered_scope_agent_and_owned_worktree_fiction() {
        for change in 0..4 {
            let mut fixture = Fixture::new();
            match change {
                0 => fixture.state.tasks[0].execution_device = Some("unregistered".into()),
                1 => fixture.state.tasks[0].scope = "../outside".into(),
                2 => fixture.state.tasks[0].assigned_agent.provider = "unapproved".into(),
                _ => fixture.config.use_worktrees = true,
            }
            let before = format!("{:?}", fixture.state);
            let devices = fixture
                .registry
                .verified_devices(&fixture.state.workspace_id, 10);
            assert!(
                RemoteConfirmedPlan::confirm(
                    &mut fixture.state,
                    &fixture.goal,
                    &fixture.config,
                    &fixture.policy,
                    &devices
                )
                .is_err()
            );
            assert_eq!(format!("{:?}", fixture.state), before);
        }
    }
    #[test]
    fn remote_dispatch_rejects_changed_registered_grant_folder_profile_and_plan() {
        for change in 0..4 {
            let mut fixture = Fixture::new();
            let confirmed = fixture.confirm();
            if change < 3 {
                let folder = if change == 1 {
                    "/remote/rebound"
                } else {
                    "/remote/project"
                };
                let grant = if change == 0 { 2 } else { 1 };
                let profile = if change == 2 {
                    "replacement-profile"
                } else {
                    "approved-profile"
                };
                let (registry, device, _) =
                    registry(fixture.state.workspace_id.clone(), folder, grant, profile);
                fixture.registry = registry;
                fixture.device = device;
            } else {
                fixture.state.tasks[0]
                    .instructions
                    .push_str(" changed after confirm");
            }
            let lease = fixture.lease(0);
            assert!(
                prepare_remote_dispatch(
                    &fixture.state,
                    &DeliveryLedger::default(),
                    &confirmed,
                    fixture.request(&lease, 0),
                    lease.lease().request.attempt_id.clone(),
                    &fixture.config,
                    &fixture.policy,
                    &lease
                )
                .is_err()
            );
        }
    }
    #[test]
    fn raw_unreserved_or_revoked_registry_state_never_grants_dispatch() {
        let mut fixture = Fixture::new();
        let confirmed = fixture.confirm();
        let lease = fixture.lease(0);
        let mut forged = lease.lease().clone();
        forged.request.operation_id = OperationId::new();
        assert!(fixture.registry.verified_lease(&forged, 10).is_err());
        fixture.registry.revoke("worker-a");
        assert!(
            prepare_remote_dispatch(
                &fixture.state,
                &DeliveryLedger::default(),
                &confirmed,
                fixture.request(&lease, 0),
                lease.lease().request.attempt_id.clone(),
                &fixture.config,
                &fixture.policy,
                &lease
            )
            .is_err()
        );
    }

    #[test]
    fn mixed_queue_allows_two_devices_same_scope_but_keeps_global_capacity() {
        let mut fixture = Fixture::new();
        fixture.state.tasks[0].scope = "src/shared".into();
        fixture.state.tasks[1].scope = "src/shared".into();
        let confirmed = fixture.confirm();
        let mut ledger = DeliveryLedger::default();
        assert_eq!(
            ready_tasks(
                &fixture.state,
                &confirmed,
                &fixture.config,
                &fixture.policy,
                &ledger
            )
            .unwrap(),
            vec![
                fixture.state.tasks[0].id.clone(),
                fixture.state.tasks[1].id.clone()
            ]
        );
        let lease = fixture.lease(0);
        let prepared = ready(
            prepare_remote_dispatch(
                &fixture.state,
                &ledger,
                &confirmed,
                fixture.request(&lease, 0),
                lease.lease().request.attempt_id.clone(),
                &fixture.config,
                &fixture.policy,
                &lease,
            )
            .unwrap(),
        );
        fixture.state = prepared.next_state;
        ledger.record_durable(prepared.assignment).unwrap();
        let local = dispatch::tests::request(&fixture.state.tasks[1], &fixture.policy);
        let prepared = ready(
            prepare_mixed_local_dispatch(
                &fixture.state,
                &ledger,
                &confirmed,
                local,
                AttemptId::new(),
                &fixture.config,
                &fixture.policy,
            )
            .unwrap(),
        );
        fixture.state = prepared.next_state;
        ledger.record_durable(prepared.assignment).unwrap();
        assert_eq!(fixture.state.attempts.len(), 2);
        assert!(
            ready_tasks(
                &fixture.state,
                &confirmed,
                &fixture.config,
                &fixture.policy,
                &ledger
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            prepare_mixed_local_dispatch(
                &fixture.state,
                &ledger,
                &confirmed,
                dispatch::tests::request(&fixture.state.tasks[2], &fixture.policy),
                AttemptId::new(),
                &fixture.config,
                &fixture.policy
            )
            .is_err()
        );
    }

    #[test]
    fn mixed_dependencies_require_exact_verified_remote_result_revision() {
        let mut fixture = Fixture::new();
        let source = fixture.state.tasks[0].id.clone();
        let child = fixture.state.tasks[1].id.clone();
        fixture.state.tasks[1].dependencies = vec![TaskDependency {
            task_id: source.clone(),
            required_result_revision: ResultRevision(1),
        }];
        let confirmed = fixture.confirm();
        let ledger = DeliveryLedger::default();
        assert!(
            !ready_tasks(
                &fixture.state,
                &confirmed,
                &fixture.config,
                &fixture.policy,
                &ledger
            )
            .unwrap()
            .contains(&child)
        );
        let lease = fixture.lease(0);
        let prepared = ready(
            prepare_remote_dispatch(
                &fixture.state,
                &ledger,
                &confirmed,
                fixture.request(&lease, 0),
                lease.lease().request.attempt_id.clone(),
                &fixture.config,
                &fixture.policy,
                &lease,
            )
            .unwrap(),
        );
        fixture.state = prepared.next_state;
        let mut event = WorkEvent {
            id: EventId::new(),
            workspace_id: fixture.state.workspace_id.clone(),
            task_id: source,
            attempt_id: prepared.assignment.attempt.id,
            runtime_binding: prepared.assignment.attempt.runtime_binding.unwrap(),
            sequence: 1,
            kind: WorkEventKind::ResultSubmitted {
                result: TaskResult {
                    revision: ResultRevision(1),
                    summary: "Actual remote result".into(),
                    source_revision: Some("source-commit".into()),
                    artifact_revision: None,
                    verification: VerificationState::NotRun,
                },
            },
        };
        fixture.state.apply_event(event.clone()).unwrap();
        assert!(
            !ready_tasks(
                &fixture.state,
                &confirmed,
                &fixture.config,
                &fixture.policy,
                &ledger
            )
            .unwrap()
            .contains(&child)
        );
        event.id = EventId::new();
        event.sequence = 2;
        event.kind = WorkEventKind::VerificationFinished {
            result_revision: ResultRevision(1),
            outcome: VerificationState::Passed,
        };
        fixture.state.apply_event(event).unwrap();
        let prepared = ready(
            prepare_mixed_local_dispatch(
                &fixture.state,
                &ledger,
                &confirmed,
                dispatch::tests::request(&fixture.state.tasks[1], &fixture.policy),
                AttemptId::new(),
                &fixture.config,
                &fixture.policy,
            )
            .unwrap(),
        );
        assert_eq!(
            prepared.assignment.source_results[0].result.revision,
            ResultRevision(1)
        );
        assert!(prepared.assignment.prompt.contains("Actual remote result"));
        assert_ne!(fixture.state.tasks[0].state, WorkState::Completed);
    }

    #[test]
    fn same_device_scope_conflicts_and_ambiguous_profile_selection_are_blocked() {
        let mut fixture = Fixture::new();
        fixture.state.tasks[1].execution_device = Some(fixture.device.id.clone());
        fixture.state.tasks[1].scope = fixture.state.tasks[0].scope.clone();
        let confirmed = fixture.confirm();
        let tasks = ready_tasks(
            &fixture.state,
            &confirmed,
            &fixture.config,
            &fixture.policy,
            &DeliveryLedger::default(),
        )
        .unwrap();
        assert!(tasks.contains(&fixture.state.tasks[0].id));
        assert!(!tasks.contains(&fixture.state.tasks[1].id));
        let mut fixture = Fixture::new();
        let (_, _, mut observation) = registry(
            fixture.state.workspace_id.clone(),
            "/remote/project",
            1,
            "approved-profile",
        );
        observation.profiles.push(WorkerProfile {
            id: "ambiguous-same-agent".into(),
            agent: dispatch::tests::agent(),
        });
        fixture.registry.observe(observation, 11).unwrap();
        let devices = fixture
            .registry
            .verified_devices(&fixture.state.workspace_id, 11);
        assert!(
            RemoteConfirmedPlan::confirm(
                &mut fixture.state,
                &fixture.goal,
                &fixture.config,
                &fixture.policy,
                &devices
            )
            .is_err()
        );
    }

    #[test]
    fn replacement_registered_pin_endpoint_or_device_requires_new_confirmation() {
        for change in 0..3 {
            let mut fixture = Fixture::new();
            let confirmed = fixture.confirm();
            let (_, mut device, mut observation) = registry(
                fixture.state.workspace_id.clone(),
                "/remote/project",
                1,
                "approved-profile",
            );
            match change {
                0 => {
                    device.pinned_fingerprint = vec!["bb"; 32].join(":");
                    observation.pinned_fingerprint = device.pinned_fingerprint.clone();
                }
                1 => device.endpoint = "127.0.0.1:9001".parse().unwrap(),
                _ => {
                    device.id = "worker-b".into();
                    observation.device_id = device.id.clone();
                }
            }
            fixture.registry = DeviceRegistry::default();
            fixture.registry.register(device.clone()).unwrap();
            fixture.registry.observe(observation, 10).unwrap();
            fixture.device = device;
            let lease = fixture.lease(0);
            assert!(
                prepare_remote_dispatch(
                    &fixture.state,
                    &DeliveryLedger::default(),
                    &confirmed,
                    fixture.request(&lease, 0),
                    lease.lease().request.attempt_id.clone(),
                    &fixture.config,
                    &fixture.policy,
                    &lease
                )
                .is_err(),
                "replacement case {change}"
            );
        }
    }

    #[test]
    fn changed_connection_or_grant_epoch_invalidates_an_inflight_proof() {
        for connection_change in [false, true] {
            let mut fixture = Fixture::new();
            let confirmed = fixture.confirm();
            let lease = fixture.lease(0);
            let (_, _, mut observation) = registry(
                fixture.state.workspace_id.clone(),
                "/remote/project",
                1,
                "approved-profile",
            );
            if connection_change {
                observation.connection_epoch += 1;
            } else {
                fixture.device.grant_epoch += 1;
                fixture.registry.register(fixture.device.clone()).unwrap();
                observation.grant_epoch = fixture.device.grant_epoch;
            }
            fixture.registry.observe(observation, 11).unwrap();
            assert!(lease.validate_fresh().is_err());
            assert!(
                fixture
                    .registry
                    .verified_execution_lease(lease.lease(), 11)
                    .is_err()
            );
            assert!(
                prepare_remote_dispatch(
                    &fixture.state,
                    &DeliveryLedger::default(),
                    &confirmed,
                    fixture.request(&lease, 0),
                    lease.lease().request.attempt_id.clone(),
                    &fixture.config,
                    &fixture.policy,
                    &lease
                )
                .is_err()
            );
        }
    }

    #[test]
    fn capacity_epoch_expires_initial_launch_lease_but_preserves_exact_existing_execution() {
        let mut fixture = Fixture::new();
        let confirmed = fixture.confirm();
        let lease = fixture.lease(0);
        let (_, _, mut observation) = registry(
            fixture.state.workspace_id.clone(),
            "/remote/project",
            1,
            "approved-profile",
        );
        observation.lease_epoch += 1;
        observation.active_workers = 1;
        observation.active_operation_ids = vec![lease.lease().request.operation_id.clone()];
        fixture.registry.observe(observation, 11).unwrap();

        // A capacity update cannot authorize a new launch with the old reservation.
        assert_eq!(
            fixture.registry.validate_lease(lease.lease(), 11),
            Err(super::super::devices::DeviceUnavailable::Conflict)
        );
        assert!(fixture.registry.verified_lease(lease.lease(), 11).is_err());
        // The exact already-launched execution still owns its operation and input target.
        let execution = fixture
            .registry
            .verified_execution_lease(lease.lease(), 11)
            .unwrap();
        assert_eq!(execution.lease(), lease.lease());
        assert!(lease.validate_fresh().is_ok());
        let before = format!("{:?}", fixture.state);
        let prepared = ready(
            prepare_remote_dispatch(
                &fixture.state,
                &DeliveryLedger::default(),
                &confirmed,
                fixture.request(&execution, 0),
                execution.lease().request.attempt_id.clone(),
                &fixture.config,
                &fixture.policy,
                &execution,
            )
            .unwrap(),
        );
        assert_eq!(format!("{:?}", fixture.state), before);
        assert_eq!(
            prepared.assignment.attempt.id,
            lease.lease().request.attempt_id
        );
        assert_eq!(
            prepared.assignment.attempt.operation_id,
            lease.lease().request.operation_id
        );
        assert_eq!(
            prepared.assignment.attempt.runtime_binding,
            Some(fixture.request(&execution, 0).runtime_binding)
        );
    }

    #[test]
    fn prelaunch_intent_has_exact_assignment_and_revokes_before_any_empty_spawn() {
        let mut fixture = Fixture::new();
        let confirmed = fixture.confirm();
        let devices = fixture
            .registry
            .verified_devices(&fixture.state.workspace_id, 10);
        let attempt = AttemptId::new();
        let operation = OperationId::new();
        let before = format!("{:?}", fixture.state);
        let intent = prepare_remote_launch(
            &fixture.state,
            &DeliveryLedger::default(),
            &confirmed,
            &fixture.state.tasks[0].id,
            attempt.clone(),
            operation.clone(),
            &fixture.config,
            &fixture.policy,
            &devices[0],
        )
        .unwrap();
        assert_eq!(intent.request().attempt_id, attempt);
        assert_eq!(intent.request().operation_id, operation);
        assert_eq!(intent.request().device_id, "worker-a");
        assert_eq!(intent.request().folder, "/remote/project");
        assert_eq!(
            intent.request().agent,
            fixture.state.tasks[0].assigned_agent
        );
        assert_eq!(format!("{:?}", fixture.state), before);
        assert!(intent.validate_fresh().is_ok());
        fixture.registry.revoke("worker-a");
        assert!(intent.validate_fresh().is_err());
    }

    #[test]
    fn mixed_queue_respects_the_confirmed_per_device_worker_limit() {
        let mut fixture = Fixture::new();
        fixture.device.max_workers = 1;
        fixture.device.grant_epoch = 2;
        fixture.registry.register(fixture.device.clone()).unwrap();
        let (_, _, observation) = registry(
            fixture.state.workspace_id.clone(),
            "/remote/project",
            2,
            "approved-profile",
        );
        fixture.registry.observe(observation, 10).unwrap();
        fixture.state.tasks[1].execution_device = Some(fixture.device.id.clone());
        let confirmed = fixture.confirm();
        let tasks = ready_tasks(
            &fixture.state,
            &confirmed,
            &fixture.config,
            &fixture.policy,
            &DeliveryLedger::default(),
        )
        .unwrap();
        assert!(tasks.contains(&fixture.state.tasks[0].id));
        assert!(!tasks.contains(&fixture.state.tasks[1].id));
        assert!(tasks.contains(&fixture.state.tasks[2].id));
    }
}
