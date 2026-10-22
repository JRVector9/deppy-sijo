//! Capabilities issued only to a host-created MAIN process, never a shared cloud principal.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingMainScope {
    pub workspace_id: String,
    pub runtime_instance: u64,
    pub runtime_token: String,
    pub config_revision: u64,
    pub launch_operation_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MainScope {
    pub workspace_id: String,
    /// Replaced executions in the same pane have different host route handles.
    pub main_session_id: String,
    pub runtime_instance: u64,
    pub runtime_token: String,
    pub incarnation: String,
    pub runtime_session_id: u64,
    pub execution_generation: u64,
    pub config_revision: u64,
    pub launch_operation_id: String,
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
impl PendingMainScope {
    pub(crate) fn valid(&self) -> bool {
        valid_id(&self.workspace_id)
            && valid_id(&self.runtime_token)
            && valid_id(&self.launch_operation_id)
            && self.runtime_instance > 0
            && self.config_revision > 0
    }
    pub(crate) fn accepts(&self, scope: &MainScope) -> bool {
        self.valid()
            && scope.valid()
            && self.workspace_id == scope.workspace_id
            && self.runtime_instance == scope.runtime_instance
            && self.runtime_token == scope.runtime_token
            && self.config_revision == scope.config_revision
            && self.launch_operation_id == scope.launch_operation_id
    }
}
impl MainScope {
    pub fn valid(&self) -> bool {
        valid_id(&self.workspace_id)
            && valid_id(&self.main_session_id)
            && valid_id(&self.runtime_token)
            && valid_id(&self.incarnation)
            && valid_id(&self.launch_operation_id)
            && self.runtime_instance > 0
            && self.runtime_session_id > 0
            && self.execution_generation > 0
            && self.config_revision > 0
    }
}

pub fn is_main_tool(name: &str) -> bool {
    matches!(
        name,
        "cc_get_project"
            | "cc_get_task"
            | "cc_propose_plan"
            | "cc_dispatch_task"
            | "cc_get_operation"
    )
}

pub fn main_tools() -> Value {
    let identity = json!({"goal_id":{"type":"string","maxLength":128},"config_revision":{"type":"integer","minimum":1}});
    let operation = json!({"operation_id":{"type":"string","maxLength":128,"description":"One unique ID for this exact action. Inspect unknown outcomes with cc_get_operation; never replay them using a new ID."}});
    let mut tools = vec![
        json!({"name":"cc_get_project","description":"Read only this MAIN execution's project, current goal, exact configuration, tasks and confirmation status. No global project/session access.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true}}),
        json!({"name":"cc_get_operation","description":"Inspect the original operation without replaying it. Unknown is not permission to dispatch or propose again.","inputSchema":{"type":"object","properties":operation,"required":["operation_id"],"additionalProperties":false},"annotations":{"readOnlyHint":true}}),
    ];
    for (name, description, extra, required, read_only) in [
        (
            "cc_get_task",
            "Read one task inside this MAIN's project and exact current goal/configuration.",
            json!({"task_id":{"type":"string","maxLength":128}}),
            vec!["task_id"],
            true,
        ),
        (
            "cc_propose_plan",
            "Submit a bounded plan proposal for validation and user review. This never confirms the plan or grants execution permission.",
            json!({"plan_json":{"type":"string","maxLength":49152}}),
            vec!["operation_id", "plan_json"],
            false,
        ),
        (
            "cc_dispatch_task",
            "Request dispatch of an already user-confirmed task. Dependency, capacity, scope, execution-device and uncertain-delivery gates remain mandatory. Omit worker_session_id to request a new owned worker.",
            json!({"task_id":{"type":"string","maxLength":128},"worker_session_id":{"type":"string","maxLength":256}}),
            vec!["operation_id", "task_id"],
            false,
        ),
    ] {
        let mut properties = identity.as_object().unwrap().clone();
        if !read_only {
            properties.extend(operation.as_object().unwrap().clone());
        }
        properties.extend(extra.as_object().unwrap().clone());
        let mut req = vec!["goal_id", "config_revision"];
        req.extend(required);
        tools.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":req,"additionalProperties":false},"annotations":{"readOnlyHint":read_only}}));
    }
    json!({"tools":tools})
}
