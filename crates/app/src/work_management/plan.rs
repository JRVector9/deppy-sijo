//! Planner text is untrusted data. Parsing never grants execution permission.
use super::dispatch::ExecutionPolicy;
use deppy_core::work_management::*;
use serde::{Deserialize, Serialize};

pub const PLANNER_RESPONSE_MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct PlannerRequest {
    pub(crate) goal: Goal,
    pub(crate) config: ProjectConfig,
    pub(crate) policy: ExecutionPolicy,
    pub(crate) runtime_binding: Option<RuntimeBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannerResponse {
    pub config_revision: ConfigRevision,
    pub plan: WorkPlan,
}

#[derive(Debug, Clone)]
pub struct ValidatedPlan {
    pub(crate) response: PlannerResponse,
    pub(crate) request: PlannerRequest,
}

impl PlannerRequest {
    pub fn new(
        goal: &Goal,
        config: &ProjectConfig,
        policy: &ExecutionPolicy,
    ) -> Result<Self, ValidationError> {
        goal.validate()?;
        policy.validate_config(config)?;
        if goal.workspace_id != config.workspace_id {
            return Err(ValidationError(
                "planner goal belongs to another workspace".into(),
            ));
        }
        Ok(Self {
            goal: goal.clone(),
            config: config.clone(),
            policy: policy.clone(),
            runtime_binding: None,
        })
    }
    pub fn with_runtime_binding(
        mut self,
        binding: RuntimeBinding,
    ) -> Result<Self, ValidationError> {
        binding.validate()?;
        self.runtime_binding = Some(binding);
        Ok(self)
    }
    pub fn to_prompt(&self) -> Result<String, ValidationError> {
        let schema = serde_json::json!({"config_revision":self.config.revision,"plan":{"workspace_id":self.goal.workspace_id,"goal_id":self.goal.id,"tasks":[{"workspace_id":self.goal.workspace_id,"id":"<unique task id>","goal_id":self.goal.id,"parent_task_id":null,"role":"<role appropriate to this goal>","scope":"<approved relative root; multiple roots separated by newline>","assigned_agent":self.policy.allowed_agents[0],"execution_device":null,"title":"<short task title>","instructions":"<bounded task instruction>","acceptance_criteria":"<observable completion conditions>","state":"plan_review","current_attempt_id":null,"dependencies":[],"result":null}]}});
        let agents = serde_json::to_string(&self.policy.allowed_agents)
            .map_err(|_| ValidationError("cannot encode allowed agents".into()))?;
        let roots = serde_json::to_string(&self.policy.allowed_scope_roots)
            .map_err(|_| ValidationError("cannot encode approved scopes".into()))?;
        let prompt = format!(
            "Propose a plan for the following actual goal. Choose the number and roles of tasks to fit the goal; do not execute the plan. Return one JSON object only, without markdown fences. The host will validate it and require user confirmation before dispatch.\nGoal {} (submission {}):\n{}\nAllowed exact agent/model/effort selections: {}\nApproved relative scope roots: {}\nTask scope is a newline-separated list of literal relative paths under these roots; absolute paths, traversal and globs are invalid. Execution is local only: execution_device must be null. Folder paths and executable commands are not response fields. Keep state plan_review and current_attempt_id/result null. Maximum {} tasks. Dependencies use {{\"task_id\":\"<source task id>\",\"required_result_revision\":1}} and require explicit verified results. Never invent successful results.\nResponse shape (replace placeholders; choose tasks for this goal):\n{}",
            self.goal.id,
            self.goal.submission_id,
            self.goal.prompt,
            agents,
            roots,
            WORK_PLAN_MAX_TASKS,
            schema
        );
        validate_input(&prompt)?;
        Ok(prompt)
    }
    pub fn goal(&self) -> &Goal {
        &self.goal
    }
    pub fn config_revision(&self) -> ConfigRevision {
        self.config.revision
    }
    pub(crate) fn validate_context(
        &self,
        config: &ProjectConfig,
        policy: &ExecutionPolicy,
    ) -> Result<(), ValidationError> {
        policy.validate_config(config)?;
        if config != &self.config || policy != &self.policy {
            return Err(ValidationError(
                "planner request configuration or permissions changed".into(),
            ));
        }
        Ok(())
    }
}

impl ValidatedPlan {
    pub fn plan(&self) -> &WorkPlan {
        &self.response.plan
    }
}

pub fn parse_plan_response(
    text: &str,
    request: &PlannerRequest,
    config: &ProjectConfig,
    policy: &ExecutionPolicy,
) -> Result<ValidatedPlan, ValidationError> {
    request.validate_context(config, policy)?;
    if text.len() > PLANNER_RESPONSE_MAX_BYTES {
        return Err(ValidationError("planner response exceeds 2 MiB".into()));
    }
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|_| ValidationError("planner response must be one JSON object".into()))?;
    exact_keys(&value, &["config_revision", "plan"])?;
    let plan = value
        .get("plan")
        .ok_or_else(|| ValidationError("planner response has no plan".into()))?;
    exact_keys(plan, &["workspace_id", "goal_id", "tasks"])?;
    let tasks = plan
        .get("tasks")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ValidationError("plan tasks must be an array".into()))?;
    if tasks.is_empty() || tasks.len() > WORK_PLAN_MAX_TASKS {
        return Err(ValidationError("plan must contain 1..=64 tasks".into()));
    }
    for task in tasks {
        exact_keys(
            task,
            &[
                "workspace_id",
                "id",
                "goal_id",
                "parent_task_id",
                "role",
                "scope",
                "assigned_agent",
                "execution_device",
                "title",
                "instructions",
                "acceptance_criteria",
                "state",
                "current_attempt_id",
                "dependencies",
                "result",
            ],
        )?;
        exact_keys(
            task.get("assigned_agent")
                .ok_or_else(|| ValidationError("task has no agent selection".into()))?,
            &["provider", "model", "effort"],
        )?;
        if let Some(dependencies) = task
            .get("dependencies")
            .and_then(serde_json::Value::as_array)
        {
            for dependency in dependencies {
                exact_keys(dependency, &["task_id", "required_result_revision"])?;
            }
        }
    }
    let response: PlannerResponse = serde_json::from_value(value).map_err(|_| {
        ValidationError("planner JSON does not match the bounded work schema".into())
    })?;
    if response.config_revision != request.config.revision
        || response.plan.workspace_id != request.goal.workspace_id
        || response.plan.goal_id != request.goal.id
    {
        return Err(ValidationError(
            "planner response owner or config revision is stale".into(),
        ));
    }
    response.plan.validate()?;
    for task in &response.plan.tasks {
        policy.validate_task(task)?;
        if task.state != WorkState::PlanReview
            || task.current_attempt_id.is_some()
            || task.result.is_some()
        {
            return Err(ValidationError(
                "planner cannot provide executed state or results".into(),
            ));
        }
    }
    Ok(ValidatedPlan {
        response,
        request: request.clone(),
    })
}

fn exact_keys(value: &serde_json::Value, allowed: &[&str]) -> Result<(), ValidationError> {
    let object = value
        .as_object()
        .ok_or_else(|| ValidationError("planner schema expected an object".into()))?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(ValidationError("unsupported planner response field".into()));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::work_management::dispatch::tests::fixture;

    pub(crate) fn response(
        state: &crate::work_management::ProjectWorkState,
        cfg: &ProjectConfig,
        goal: &GoalId,
    ) -> String {
        serde_json::to_string(&PlannerResponse {
            config_revision: cfg.revision,
            plan: WorkPlan {
                workspace_id: state.workspace_id.clone(),
                goal_id: goal.clone(),
                tasks: state.tasks.clone(),
            },
        })
        .unwrap()
    }

    #[test]
    fn work_management_planner_request_uses_real_goal_and_validated_response_without_fixed_roles() {
        let (state, cfg, policy, goal) = fixture();
        let request = PlannerRequest::new(&state.goals[0], &cfg, &policy).unwrap();
        let prompt = request.to_prompt().unwrap();
        assert!(prompt.contains(&goal.0));
        assert!(prompt.contains("Real goal"));
        assert!(prompt.contains("newline"));
        let proposal =
            parse_plan_response(&response(&state, &cfg, &goal), &request, &cfg, &policy).unwrap();
        assert_eq!(proposal.plan().tasks, state.tasks);
        assert_eq!(request.goal().submission_id, state.goals[0].submission_id);
        assert_eq!(request.config_revision(), cfg.revision);
    }

    #[test]
    fn work_management_planner_rejects_stale_config_and_changed_permissions() {
        let (state, mut cfg, mut policy, goal) = fixture();
        let request = PlannerRequest::new(&state.goals[0], &cfg, &policy).unwrap();
        let text = response(&state, &cfg, &goal);
        cfg.revision = ConfigRevision(2);
        assert!(parse_plan_response(&text, &request, &cfg, &policy).is_err());
        cfg.revision = ConfigRevision(1);
        policy.allowed_scope_roots = vec!["tests".into()];
        assert!(parse_plan_response(&text, &request, &cfg, &policy).is_err());
    }

    #[test]
    fn work_management_planner_rejects_path_provider_device_and_folder_escalation() {
        let (state, cfg, policy, goal) = fixture();
        let request = PlannerRequest::new(&state.goals[0], &cfg, &policy).unwrap();
        let good: serde_json::Value = serde_json::from_str(&response(&state, &cfg, &goal)).unwrap();
        for (field, value) in [
            ("scope", serde_json::json!("../outside")),
            ("scope", serde_json::json!("/etc")),
            ("scope", serde_json::json!("src/../secret")),
            ("execution_device", serde_json::json!("unregistered")),
            ("folder", serde_json::json!("/tmp/arbitrary")),
        ] {
            let mut bad = good.clone();
            bad["plan"]["tasks"][0][field] = value;
            assert!(
                parse_plan_response(&bad.to_string(), &request, &cfg, &policy).is_err(),
                "{field}"
            );
        }
        let mut bad = good;
        bad["plan"]["tasks"][0]["assigned_agent"]["provider"] = serde_json::json!("unapproved");
        assert!(parse_plan_response(&bad.to_string(), &request, &cfg, &policy).is_err());
    }

    #[test]
    fn work_management_planner_bounds_reject_without_truncating_original() {
        let (state, cfg, policy, goal) = fixture();
        let request = PlannerRequest::new(&state.goals[0], &cfg, &policy).unwrap();
        let oversized = " ".repeat(PLANNER_RESPONSE_MAX_BYTES + 1);
        let original = oversized.clone();
        assert!(parse_plan_response(&oversized, &request, &cfg, &policy).is_err());
        assert_eq!(oversized, original);
        let mut response: PlannerResponse =
            serde_json::from_str(&response(&state, &cfg, &goal)).unwrap();
        response
            .plan
            .tasks
            .resize(65, response.plan.tasks[0].clone());
        assert!(
            parse_plan_response(
                &serde_json::to_string(&response).unwrap(),
                &request,
                &cfg,
                &policy
            )
            .is_err()
        );
    }
}
