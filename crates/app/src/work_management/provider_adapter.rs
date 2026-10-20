//! Only host-observed capabilities select structured planning; terminals keep dialogue/manual JSON.
use super::dispatch::ExecutionPolicy;
use super::plan::{PlannerRequest, ValidatedPlan};
use deppy_core::work_management::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannerSupport {
    pub structured_final_response: bool,
    pub tool_execution_restricted: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlannerMode {
    StructuredResponse,
    ManualJson,
}

pub fn planner_mode(support: PlannerSupport) -> PlannerMode {
    if support.structured_final_response {
        PlannerMode::StructuredResponse
    } else {
        PlannerMode::ManualJson
    }
}
pub fn accept_final_response(
    request: &PlannerRequest,
    binding: &RuntimeBinding,
    text: &str,
    support: PlannerSupport,
    config: &ProjectConfig,
    policy: &ExecutionPolicy,
) -> Result<ValidatedPlan, ValidationError> {
    if planner_mode(support) != PlannerMode::StructuredResponse {
        return Err(ValidationError(
            "provider requires manual JSON plan editing".into(),
        ));
    }
    binding.validate()?;
    if request.runtime_binding.as_ref() != Some(binding) {
        return Err(ValidationError(
            "planner response belongs to another main execution".into(),
        ));
    }
    super::plan::parse_plan_response(text, request, config, policy)
}
pub fn accept_manual_json(
    request: &PlannerRequest,
    text: &str,
    config: &ProjectConfig,
    policy: &ExecutionPolicy,
) -> Result<ValidatedPlan, ValidationError> {
    super::plan::parse_plan_response(text, request, config, policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_management::{
        dispatch::tests::{fixture, request},
        plan::tests::response,
    };

    #[test]
    fn work_management_unsupported_provider_keeps_manual_json_without_fake_automation() {
        let (state, cfg, policy, goal) = fixture();
        let binding = request(&state.tasks[0], &policy).runtime_binding;
        let planner = PlannerRequest::new(&state.goals[0], &cfg, &policy)
            .unwrap()
            .with_runtime_binding(binding.clone())
            .unwrap();
        let text = response(&state, &cfg, &goal);
        for _provider in ["claude-pty", "kimi-pty", "grok-pty", "custom-cli"] {
            let support = PlannerSupport {
                structured_final_response: false,
                tool_execution_restricted: false,
            };
            assert_eq!(planner_mode(support), PlannerMode::ManualJson);
            assert!(
                accept_final_response(&planner, &binding, &text, support, &cfg, &policy).is_err()
            );
            assert!(accept_manual_json(&planner, &text, &cfg, &policy).is_ok());
        }
    }
    #[test]
    fn work_management_structured_plan_only_accepts_exact_bound_main_execution() {
        let (state, cfg, policy, goal) = fixture();
        let binding = request(&state.tasks[0], &policy).runtime_binding;
        let planner = PlannerRequest::new(&state.goals[0], &cfg, &policy)
            .unwrap()
            .with_runtime_binding(binding.clone())
            .unwrap();
        let text = response(&state, &cfg, &goal);
        let support = PlannerSupport {
            structured_final_response: true,
            tool_execution_restricted: false,
        };
        assert_eq!(planner_mode(support), PlannerMode::StructuredResponse);
        let mut stale = binding.clone();
        stale.execution_generation += 1;
        assert!(accept_final_response(&planner, &stale, &text, support, &cfg, &policy).is_err());
        assert!(accept_final_response(&planner, &binding, &text, support, &cfg, &policy).is_ok());
    }
}
