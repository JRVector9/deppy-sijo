//! MAIN-only loopback MCP capability. No cloud sharing state or credentials are reused.
use agent_mcp::{MainScope, PendingMainScope, Request, Server};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const MAX_REGISTRATIONS: usize = 128;
const MAX_OPERATIONS: usize = 256;
const MAX_PLAN_BYTES: usize = 48 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const TOKEN_ENV: &str = "DEPPY_CC_MAIN_TOKEN";

struct LaunchCredential {
    launch_operation_id: String,
    endpoint: String,
    credential_id: String,
    #[cfg(test)]
    token: secret::SecretString,
}
/// A stable, empty slot can be installed before a MAIN exists. Each credential is consumed once.
#[derive(Clone, Default)]
pub(super) struct MainRegistration(Arc<Mutex<Option<LaunchCredential>>>);
impl MainRegistration {
    pub(super) fn apply(&self, command: &mut runtime::RuntimeCommand) -> anyhow::Result<()> {
        let runtime::RuntimeCommand::SpawnAgent {
            transient_args,
            env_plain,
            env_secrets,
            ..
        } = command
        else {
            anyhow::bail!("main_tools_requires_spawn_agent");
        };
        let credential = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("main_registration_unavailable"))?
            .take()
            .ok_or_else(|| anyhow::anyhow!("main_registration_already_consumed"))?;
        transient_args.extend([
            "-c".into(),
            format!(
                "mcp_servers.deppy_command_center={{url={},bearer_token_env_var=\"{TOKEN_ENV}\",enabled=true}}",
                serde_json::to_string(&credential.endpoint)?
            ),
        ]);
        env_plain.retain(|(name, _)| name != TOKEN_ENV);
        env_secrets.retain(|(name, _)| name != TOKEN_ENV);
        env_secrets.push((TOKEN_ENV.into(), credential.credential_id));
        Ok(())
    }
    fn clear(&self, launch_operation_id: &str) {
        if let Ok(mut slot) = self.0.lock()
            && slot
                .as_ref()
                .is_some_and(|credential| credential.launch_operation_id == launch_operation_id)
        {
            *slot = None;
        }
    }
}

struct Registration {
    principal_id: String,
    pending: PendingMainScope,
    active: Option<MainScope>,
    revoked: bool,
    credential: Option<crate::app::EphemeralMainCredential>,
}
struct Operation {
    tool: String,
    args: Value,
    host_id: String,
    reply: Result<Value, String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum MainCall {
    Project,
    Task {
        task_id: String,
        goal_id: String,
        config_revision: u64,
    },
    ProposePlan {
        goal_id: String,
        config_revision: u64,
        json: String,
    },
    DispatchTask {
        goal_id: String,
        config_revision: u64,
        task_id: String,
        worker_session_id: Option<String>,
    },
    Operation {
        host_operation_id: String,
    },
}

pub(super) struct MainTools {
    server: Server,
    slots: BTreeMap<(String, u64, u64), MainRegistration>,
    registrations: BTreeMap<String, Registration>,
    operations: BTreeMap<(String, String), Operation>,
    redaction: secret::RedactionService,
}
impl MainTools {
    pub(super) fn new(
        redaction: secret::RedactionService,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            server: Server::start_with_redaction(0, "", redaction.clone(), wake)?,
            slots: BTreeMap::new(),
            registrations: BTreeMap::new(),
            operations: BTreeMap::new(),
            redaction,
        })
    }
    pub(super) fn registration_slot(
        &mut self,
        workspace_id: &str,
        runtime_instance: u64,
        config_revision: u64,
    ) -> Result<MainRegistration, String> {
        let key = (workspace_id.to_owned(), runtime_instance, config_revision);
        if !self.slots.contains_key(&key) && self.slots.len() >= 32 {
            return Err("main_registration_capacity_no_effect".into());
        }
        Ok(self.slots.entry(key).or_default().clone())
    }
    pub(super) fn prepare(&mut self, pending: PendingMainScope) -> Result<(), String> {
        if self
            .registrations
            .contains_key(&pending.launch_operation_id)
        {
            return Err("main_registration_operation_already_used".into());
        }
        if self.registrations.len() >= MAX_REGISTRATIONS {
            return Err("main_registration_capacity_no_effect".into());
        }
        let slot = self.registration_slot(
            &pending.workspace_id,
            pending.runtime_instance,
            pending.config_revision,
        )?;
        let mut cell = slot.0.lock().map_err(|_| "main_registration_unavailable")?;
        if cell.is_some() {
            return Err("main_registration_pending_no_effect".into());
        }
        let (principal, token) = self
            .server
            .auth
            .mint_pending_main_connection(pending.clone(), agent_mcp::now())
            .ok_or("main_registration_unavailable")?;
        let credential = match crate::app::EphemeralMainCredential::register(
            secret::SecretString::new(token.to_string()),
        ) {
            Ok(credential) => credential,
            Err(_) => {
                self.server.auth.revoke_connection(&principal.id);
                return Err("main_registration_unavailable_no_effect".into());
            }
        };
        *cell = Some(LaunchCredential {
            launch_operation_id: pending.launch_operation_id.clone(),
            endpoint: format!("http://{}/mcp", self.server.addr),
            credential_id: credential.id().to_owned(),
            #[cfg(test)]
            token: secret::SecretString::new(token.to_string()),
        });
        self.registrations.insert(
            pending.launch_operation_id.clone(),
            Registration {
                principal_id: principal.id,
                pending,
                active: None,
                revoked: false,
                credential: Some(credential),
            },
        );
        Ok(())
    }
    pub(super) fn activate(
        &mut self,
        launch_operation_id: &str,
        scope: MainScope,
    ) -> Result<(), String> {
        let registration = self
            .registrations
            .get_mut(launch_operation_id)
            .ok_or("main_registration_missing")?;
        if registration.revoked
            || registration.pending.launch_operation_id != scope.launch_operation_id
            || !self
                .server
                .auth
                .activate_main_connection(&registration.principal_id, scope.clone())
        {
            return Err("main_scope_changed_no_effect".into());
        }
        registration.active = Some(scope);
        Ok(())
    }
    /// Called with current host-observed tuples and durable launches still awaiting their first binding.
    pub(super) fn reconcile(&mut self, current: &[MainScope], pending_launch_ids: &[String]) {
        for registration in self
            .registrations
            .values_mut()
            .filter(|registration| !registration.revoked)
        {
            let keep = match &registration.active {
                Some(scope) => current.contains(scope),
                None => pending_launch_ids.contains(&registration.pending.launch_operation_id),
            };
            if !keep {
                self.server
                    .auth
                    .revoke_connection(&registration.principal_id);
                registration.revoked = true;
                registration.credential = None;
                let key = (
                    registration.pending.workspace_id.clone(),
                    registration.pending.runtime_instance,
                    registration.pending.config_revision,
                );
                if let Some(slot) = self.slots.get(&key) {
                    slot.clear(&registration.pending.launch_operation_id);
                }
            }
        }
    }
    pub(super) fn scope(&self, request: &Request) -> Option<MainScope> {
        self.server.auth.main_scope(request)
    }
    pub(super) fn next_request(&self) -> Option<Request> {
        self.server.requests.try_recv().ok()
    }
    /// The callback must use existing host plan/dispatch gates and must not call Auth again.
    pub(super) fn handle(
        &mut self,
        request: Request,
        current_scope: Option<&MainScope>,
        current_goal_id: Option<&str>,
        mut callback: impl FnMut(&MainScope, MainCall, Option<&str>) -> Result<Value, String>,
    ) {
        let result = self.handle_inner(&request, current_scope, current_goal_id, &mut callback);
        let _ = request.reply.try_send(result);
    }
    fn handle_inner(
        &mut self,
        request: &Request,
        current_scope: Option<&MainScope>,
        current_goal_id: Option<&str>,
        callback: &mut impl FnMut(&MainScope, MainCall, Option<&str>) -> Result<Value, String>,
    ) -> Result<Value, String> {
        let scope = self
            .scope(request)
            .filter(|scope| Some(scope) == current_scope)
            .ok_or("main_scope_changed_no_effect")?;
        let call = parse_call(&request.tool, &request.args, &scope, current_goal_id)?;
        if let MainCall::Operation {
            host_operation_id: operation_id,
        } = &call
        {
            let key = (request.principal.id.clone(), operation_id.clone());
            let Some(operation) = self.operations.get(&key) else {
                return Ok(
                    json!({"status":"unknown","retry":false,"retryable":false,"effect_state":"unknown"}),
                );
            };
            return bounded_response(
                callback(
                    &scope,
                    MainCall::Operation {
                        host_operation_id: operation.host_id.clone(),
                    },
                    None,
                )?,
                &self.redaction,
            );
        }
        let mutation = matches!(
            call,
            MainCall::ProposePlan { .. } | MainCall::DispatchTask { .. }
        );
        if !mutation {
            return bounded_response(callback(&scope, call, None)?, &self.redaction);
        }
        let operation_id = field(&request.args, "operation_id", 128)?;
        let key = (request.principal.id.clone(), operation_id);
        if let Some(operation) = self.operations.get(&key) {
            return if operation.tool == request.tool && operation.args == request.args {
                operation.reply.clone()
            } else {
                Err("main_operation_conflict_no_effect".into())
            };
        }
        if self.operations.len() >= MAX_OPERATIONS {
            return Err("main_operation_capacity_no_effect".into());
        }
        let host_id = format!("main-tool:{}", uuid::Uuid::new_v4());
        // Consume the identity before the effect. A dropped/oversized reply cannot replay it.
        self.operations.insert(
            key.clone(),
            Operation {
                tool: request.tool.clone(),
                args: request.args.clone(),
                host_id: host_id.clone(),
                reply: Err("main_operation_unknown_do_not_retry".into()),
            },
        );
        let mut reply = Err("expired_or_revoked_request_no_effect".into());
        self.server.auth.admit_current_access(
            request.epoch,
            request.access_key.as_deref(),
            &mut || {
                reply = callback(&scope, call.clone(), Some(&host_id));
            },
        );
        reply = reply.and_then(|value| {
            bounded_response(value, &self.redaction)
                .map_err(|_| "main_response_too_large_unknown".into())
        });
        self.operations.get_mut(&key).unwrap().reply = reply.clone();
        reply
    }
}
fn bounded_response(
    mut value: Value,
    redaction: &secret::RedactionService,
) -> Result<Value, String> {
    if serde_json::to_vec(&value)
        .map_err(|_| "main_response_too_large_no_effect")?
        .len()
        > MAX_RESPONSE_BYTES
    {
        return Err("main_response_too_large_no_effect".into());
    }
    fn redact(value: &mut Value, redaction: &secret::RedactionService, fragment: bool) {
        match value {
            Value::String(text) => {
                *text = if fragment {
                    redaction.redact_wrapped_fragment(text)
                } else {
                    redaction.redact_wrapped_text(text)
                };
            }
            Value::Array(values) => {
                for value in values {
                    redact(value, redaction, fragment);
                }
            }
            Value::Object(values) => {
                for (key, value) in values {
                    // These host display fields may already be clipped. Protocol identities and
                    // statuses must retain exact semantics, including one-character overlaps.
                    redact(
                        value,
                        redaction,
                        matches!(key.as_str(), "title" | "prompt" | "detail"),
                    );
                }
            }
            _ => {}
        }
    }
    redact(&mut value, redaction, false);
    if serde_json::to_vec(&value)
        .map_err(|_| "main_response_too_large_no_effect")?
        .len()
        > MAX_RESPONSE_BYTES
    {
        return Err("main_response_too_large_no_effect".into());
    }
    Ok(value)
}
fn field(args: &Value, name: &str, max: usize) -> Result<String, String> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
        })
        .map(str::to_owned)
        .ok_or_else(|| "invalid_arguments".into())
}
fn parse_call(
    tool: &str,
    args: &Value,
    scope: &MainScope,
    current_goal_id: Option<&str>,
) -> Result<MainCall, String> {
    let allowed: &[&str] = match tool {
        "cc_get_project" => &[],
        "cc_get_operation" => &["operation_id"],
        "cc_get_task" => &["goal_id", "config_revision", "task_id"],
        "cc_propose_plan" => &["goal_id", "config_revision", "operation_id", "plan_json"],
        "cc_dispatch_task" => &[
            "goal_id",
            "config_revision",
            "operation_id",
            "task_id",
            "worker_session_id",
        ],
        _ => return Err("main_tool_not_allowed_no_effect".into()),
    };
    if args
        .as_object()
        .is_none_or(|args| args.keys().any(|key| !allowed.contains(&key.as_str())))
    {
        return Err("invalid_arguments".into());
    }
    if tool == "cc_get_project" {
        return Ok(MainCall::Project);
    }
    if tool == "cc_get_operation" {
        return Ok(MainCall::Operation {
            host_operation_id: field(args, "operation_id", 128)?,
        });
    }
    let goal_id = field(args, "goal_id", 128)?;
    let config_revision = args["config_revision"]
        .as_u64()
        .filter(|revision| *revision > 0)
        .ok_or("invalid_arguments")?;
    if Some(goal_id.as_str()) != current_goal_id || config_revision != scope.config_revision {
        return Err("main_goal_changed_no_effect".into());
    }
    match tool {
        "cc_get_task" => Ok(MainCall::Task {
            goal_id,
            config_revision,
            task_id: field(args, "task_id", 128)?,
        }),
        "cc_propose_plan" => {
            field(args, "operation_id", 128)?;
            let json = args["plan_json"]
                .as_str()
                .filter(|text| !text.trim().is_empty() && text.len() <= MAX_PLAN_BYTES)
                .ok_or("invalid_arguments")?
                .to_owned();
            Ok(MainCall::ProposePlan {
                goal_id,
                config_revision,
                json,
            })
        }
        "cc_dispatch_task" => {
            field(args, "operation_id", 128)?;
            let worker_session_id = args
                .get("worker_session_id")
                .map(|_| field(args, "worker_session_id", 256))
                .transpose()?;
            Ok(MainCall::DispatchTask {
                goal_id,
                config_revision,
                task_id: field(args, "task_id", 128)?,
                worker_session_id,
            })
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    fn pending(operation: &str) -> PendingMainScope {
        PendingMainScope {
            workspace_id: "project-a".into(),
            runtime_instance: 7,
            runtime_token: "runtime-a".into(),
            config_revision: 3,
            launch_operation_id: operation.into(),
        }
    }
    fn scope(operation: &str) -> MainScope {
        MainScope {
            workspace_id: "project-a".into(),
            main_session_id: "host:route-a".into(),
            runtime_instance: 7,
            runtime_token: "runtime-a".into(),
            incarnation: "incarnation-a".into(),
            runtime_session_id: 11,
            execution_generation: 13,
            config_revision: 3,
            launch_operation_id: operation.into(),
        }
    }
    fn spawn() -> runtime::RuntimeCommand {
        runtime::RuntimeCommand::SpawnAgent {
            agent_config_id: Some("configured-agent".into()),
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
            command: "/bin/sh".into(),
            args: vec!["--model".into(), "configured-model".into()],
            env_plain: vec![
                (TOKEN_ENV.into(), "old-plain".into()),
                ("KEEP".into(), "configured".into()),
            ],
            env_secrets: vec![(TOKEN_ENV.into(), "old-secret".into())],
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
            transient_args: vec![],
        }
    }
    fn prepared(operation: &str) -> (MainTools, MainScope, String) {
        let mut tools = MainTools::new(secret::RedactionService::new(), || {}).unwrap();
        tools.prepare(pending(operation)).unwrap();
        let slot = tools.registration_slot("project-a", 7, 3).unwrap();
        let token = slot
            .0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .token
            .expose()
            .to_owned();
        let current = scope(operation);
        tools.activate(operation, current.clone()).unwrap();
        (tools, current, token)
    }
    fn invoke(
        tools: &mut MainTools,
        token: &str,
        current: &MainScope,
        goal: &str,
        tool: &str,
        args: Value,
        callback: impl FnMut(&MainScope, MainCall, Option<&str>) -> Result<Value, String>,
    ) -> Value {
        let addr = tools.server.addr;
        let token = token.to_owned();
        let tool = tool.to_owned();
        let caller = std::thread::spawn(move || {
            let body=json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":args}}).to_string();
            let mut stream = std::net::TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            write!(stream,"POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len()).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            let envelope: Value =
                serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
            serde_json::from_str::<Value>(
                envelope["result"]["content"][0]["text"].as_str().unwrap(),
            )
            .unwrap()
        });
        let request = tools
            .server
            .requests
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        tools.handle(request, Some(current), Some(goal), callback);
        caller.join().unwrap()
    }
    #[test]
    fn command_center_main_real_http_uses_actual_host_scope_queue_and_receipts() {
        use super::super::main_host;
        let (mut host, current) = main_host::tests::fixture();
        let goal = host.projects[&current.workspace_id]
            .snapshot
            .goals
            .last()
            .unwrap()
            .id
            .0
            .clone();
        let mut tools = MainTools::new(secret::RedactionService::new(), || {}).unwrap();
        tools
            .prepare(PendingMainScope {
                workspace_id: current.workspace_id.clone(),
                runtime_instance: current.runtime_instance,
                runtime_token: current.runtime_token.clone(),
                config_revision: current.config_revision,
                launch_operation_id: current.launch_operation_id.clone(),
            })
            .unwrap();
        let token = tools
            .registration_slot(
                &current.workspace_id,
                current.runtime_instance,
                current.config_revision,
            )
            .unwrap()
            .0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .token
            .expose()
            .to_owned();
        tools
            .activate(&current.launch_operation_id, current.clone())
            .unwrap();
        let read = invoke(
            &mut tools,
            &token,
            &current,
            &goal,
            "cc_get_project",
            json!({}),
            |scope, call, id| main_host::call(&mut host, scope, call, id),
        );
        assert_eq!(read["workspace_id"], current.workspace_id);
        let args = json!({"goal_id":goal,"config_revision":current.config_revision,"operation_id":"actual-plan-op","plan_json":"{}"});
        let accepted = invoke(
            &mut tools,
            &token,
            &current,
            &goal,
            "cc_propose_plan",
            args.clone(),
            |scope, call, id| main_host::call(&mut host, scope, call, id),
        );
        assert_eq!(accepted["status"], "accepted");
        assert_eq!(host.pending.len(), 1);
        let duplicate = invoke(
            &mut tools,
            &token,
            &current,
            &goal,
            "cc_propose_plan",
            args,
            |_, _, _| panic!("same operation replayed into host"),
        );
        assert_eq!(duplicate, accepted);
        assert_eq!(host.pending.len(), 1);
        let op = accepted["host_operation_id"].as_str().unwrap();
        host.reject(
            op,
            "배정안 스키마를 확인하지 못했습니다. 실행하지 않았습니다.",
        );
        let receipt = invoke(
            &mut tools,
            &token,
            &current,
            &goal,
            "cc_get_operation",
            json!({"operation_id":"actual-plan-op"}),
            |scope, call, id| main_host::call(&mut host, scope, call, id),
        );
        assert_eq!(receipt["receipt"]["status"], "rejected");
        assert!(host.projects[&current.workspace_id].confirmed.is_empty());
    }
    #[test]
    fn command_center_main_registration_consumes_only_one_secret_spawn() {
        let mut tools = MainTools::new(secret::RedactionService::new(), || {}).unwrap();
        let slot = tools.registration_slot("project-a", 7, 3).unwrap();
        assert!(slot.apply(&mut spawn()).is_err());
        tools.prepare(pending("main-a")).unwrap();
        let mut command = spawn();
        slot.apply(&mut command).unwrap();
        assert!(slot.apply(&mut spawn()).is_err());
        let runtime::RuntimeCommand::SpawnAgent {
            args,
            transient_args,
            env_plain,
            env_secrets,
            ..
        } = command
        else {
            unreachable!()
        };
        assert_eq!(args, ["--model", "configured-model"]);
        assert_eq!(transient_args.len(), 2);
        assert_eq!(transient_args[0], "-c");
        assert!(
            transient_args[1]
                .starts_with("mcp_servers.deppy_command_center={url=\"http://127.0.0.1:")
        );
        assert_eq!(env_plain, vec![("KEEP".into(), "configured".into())]);
        assert_eq!(env_secrets.len(), 1);
        assert_eq!(env_secrets[0].0, TOKEN_ENV);
        assert!(
            env_secrets[0]
                .1
                .starts_with(crate::app::EPHEMERAL_MAIN_CREDENTIAL_PREFIX)
        );
        assert!(!args.iter().any(|arg| arg.contains(&env_secrets[0].1)));
    }
    #[test]
    fn command_center_main_real_http_one_time_operation_replay_and_lookup() {
        let (mut tools, current, token) = prepared("main-a");
        let args = json!({"goal_id":"goal-a","config_revision":3,"operation_id":"op-a","task_id":"task-a"});
        let mut calls = 0;
        let mut host_id = String::new();
        let accepted = invoke(
            &mut tools,
            &token,
            &current,
            "goal-a",
            "cc_dispatch_task",
            args.clone(),
            |scope, call, id| {
                assert_eq!(scope, &current);
                assert!(matches!(call, MainCall::DispatchTask { .. }));
                calls += 1;
                host_id = id.unwrap().to_owned();
                Ok(json!({"status":"accepted","dispatch_complete":false}))
            },
        );
        assert_eq!(calls, 1);
        assert_eq!(accepted["dispatch_complete"], false);
        let replay = invoke(
            &mut tools,
            &token,
            &current,
            "goal-a",
            "cc_dispatch_task",
            args.clone(),
            |_, _, _| panic!("same operation must never execute again"),
        );
        assert_eq!(replay, accepted);
        let mut changed = args;
        changed["task_id"] = json!("other-task");
        let conflict = invoke(
            &mut tools,
            &token,
            &current,
            "goal-a",
            "cc_dispatch_task",
            changed,
            |_, _, _| panic!("conflicting action must not execute"),
        );
        assert_eq!(conflict["code"], "main_operation_conflict_no_effect");
        let lookup = invoke(
            &mut tools,
            &token,
            &current,
            "goal-a",
            "cc_get_operation",
            json!({"operation_id":"op-a"}),
            |_, call, id| {
                assert!(id.is_none());
                assert_eq!(
                    call,
                    MainCall::Operation {
                        host_operation_id: host_id.clone()
                    }
                );
                Ok(json!({"status":"pending","completed":false}))
            },
        );
        assert_eq!(lookup["status"], "pending");
        let unknown = invoke(
            &mut tools,
            &token,
            &current,
            "goal-a",
            "cc_get_operation",
            json!({"operation_id":"missing"}),
            |_, _, _| panic!("unknown lookup has no effect"),
        );
        assert_eq!(unknown["status"], "unknown");
        assert_eq!(unknown["retry"], false);
    }
    #[test]
    fn command_center_main_rejects_stale_goal_config_cross_project_and_large_plans() {
        let (mut tools, current, token) = prepared("main-a");
        for (args, expected) in [
            (
                json!({"goal_id":"old-goal","config_revision":3,"task_id":"task-a"}),
                "main_goal_changed_no_effect",
            ),
            (
                json!({"goal_id":"goal-a","config_revision":4,"task_id":"task-a"}),
                "main_goal_changed_no_effect",
            ),
            (
                json!({"goal_id":"goal-a","config_revision":3,"task_id":"task-a","workspace_id":"foreign"}),
                "invalid_arguments",
            ),
        ] {
            let response = invoke(
                &mut tools,
                &token,
                &current,
                "goal-a",
                "cc_get_task",
                args,
                |_, _, _| panic!("stale or foreign context must not reach host"),
            );
            assert_eq!(response["code"], expected);
        }
        assert!(parse_call("cc_propose_plan",&json!({"goal_id":"goal-a","config_revision":3,"operation_id":"large","plan_json":"x".repeat(MAX_PLAN_BYTES+1)}),&current,Some("goal-a")).is_err());
        tools.reconcile(&[], &[]);
        assert!(
            tools
                .server
                .auth
                .authenticate_principal(&format!("Bearer {token}"), agent_mcp::now())
                .is_none()
        );
    }
    #[test]
    fn command_center_main_retiring_old_execution_preserves_new_pending_registration() {
        let (mut tools, _, _) = prepared("main-a");
        let slot = tools.registration_slot("project-a", 7, 3).unwrap();
        slot.apply(&mut spawn()).unwrap();
        tools.prepare(pending("main-b")).unwrap();
        tools.reconcile(&[], &["main-b".into()]);
        assert!(
            slot.apply(&mut spawn()).is_ok(),
            "retiring old execution must not erase a distinct new pending credential in the same slot"
        );
    }

    #[test]
    fn command_center_main_oversized_mutation_reply_remains_unknown_without_replay() {
        let (mut tools, current, token) = prepared("main-a");
        let args = json!({"goal_id":"goal-a","config_revision":3,"operation_id":"large-reply","task_id":"task-a"});
        let first = invoke(
            &mut tools,
            &token,
            &current,
            "goal-a",
            "cc_dispatch_task",
            args.clone(),
            |_, _, _| Ok(json!({"text":"x".repeat(MAX_RESPONSE_BYTES + 1)})),
        );
        assert_eq!(first["code"], "main_response_too_large_unknown");
        assert_eq!(first["retryable"], false);
        assert_eq!(first["effect_state"], "unknown");
        let replay = invoke(
            &mut tools,
            &token,
            &current,
            "goal-a",
            "cc_dispatch_task",
            args,
            |_, _, _| panic!("a dropped result must not repeat dispatch"),
        );
        assert_eq!(first, replay);
    }

    #[test]
    fn command_center_main_response_limit_applies_after_redaction_expansion() {
        let redaction = secret::RedactionService::new();
        redaction.register(&secret::SecretString::new("abcdef".into()));
        let response = json!({"text":"abcdef ".repeat(8000)});
        assert!(serde_json::to_vec(&response).unwrap().len() < MAX_RESPONSE_BYTES);
        assert!(
            matches!(bounded_response(response, &redaction), Err(code) if code == "main_response_too_large_no_effect")
        );
    }

    #[test]
    fn command_center_main_read_scope_and_secret_redaction_are_enforced() {
        let (mut tools, current, token) = prepared("main-a");
        let mut replacement = current.clone();
        replacement.execution_generation += 1;
        let rejected = invoke(
            &mut tools,
            &token,
            &replacement,
            "goal-a",
            "cc_get_project",
            json!({}),
            |_, _, _| panic!("stale main cannot read a replacement execution"),
        );
        assert_eq!(rejected["code"], "main_scope_changed_no_effect");
        let response = invoke(
            &mut tools,
            &token,
            &current,
            "goal-a",
            "cc_get_project",
            json!({}),
            |_, _, _| Ok(json!({"text":format!("credential={token}")})),
        );
        assert!(!response.to_string().contains(&token));
    }

    #[test]
    fn command_center_main_response_masks_secrets_clipped_by_host_metadata_caps() {
        let redaction = secret::RedactionService::new();
        let credential = "secret-unfinished-crossing-metadata-boundary";
        redaction.register(&secret::SecretString::new(credential.into()));
        for cap in [512, 4096] {
            let prefix = "description ".repeat(cap / 12 + 1);
            let prefix = &prefix[..cap - 17];
            let clipped = format!("{prefix}{credential}")[..cap].to_owned();
            assert!(clipped.ends_with("secret-unfinished"));
            let result = bounded_response(
                json!({"status":"success","host_operation_id":"yes","nested":[{"detail":clipped}]}),
                &redaction,
            )
            .unwrap();
            assert_eq!(result["status"], "success");
            assert_eq!(result["host_operation_id"], "yes");
            assert!(
                !result.to_string().contains("secret-unfinished"),
                "host metadata leaked a credential prefix at cap {cap}"
            );
        }
    }
    #[test]
    fn command_center_main_registered_launch_uses_production_resolver_and_owned_child_env() {
        use runtime::{RuntimeCommandSink, RuntimeEventStream, RuntimeSecretResolver};
        use sha2::Digest;
        let root =
            std::env::temp_dir().join(format!("deppy-main-ephemeral-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let redaction = secret::RedactionService::new();
        let mut tools = MainTools::new(redaction.clone(), || {}).unwrap();
        tools.prepare(pending("owned-env")).unwrap();
        let registration = tools.registration_slot("project-a", 7, 3).unwrap();
        let token = registration
            .0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .token
            .expose()
            .to_owned();
        let output = root.join("child-env.sha256");
        let script = format!(
            "test -n \"${TOKEN_ENV}\" && printf '%s' \"${TOKEN_ENV}\" | /usr/bin/shasum -a 256 > '{}'",
            output.to_string_lossy().replace('\'', "'\\''")
        );
        let mut command = spawn();
        if let runtime::RuntimeCommand::SpawnAgent {
            args,
            env_plain,
            env_secrets,
            ..
        } = &mut command
        {
            *args = vec!["-c".into(), script];
            env_plain.clear();
            env_secrets.clear();
        }
        registration.apply(&mut command).unwrap();
        let serialized = serde_json::to_string(&command).unwrap();
        let credential_id = match &command {
            runtime::RuntimeCommand::SpawnAgent { env_secrets, .. } => env_secrets[0].1.clone(),
            _ => unreachable!(),
        };
        let db_path = root.join("must-not-create.db");
        let resolver = Arc::new(crate::app::AppRuntimeSecretResolver::new(db_path.clone()));
        let client = runtime::InProcessRuntimeClient::try_new_with_resolver(
            5,
            resolver.clone(),
            root.join("logs"),
            redaction,
            None,
            Some(root.clone()),
            vec![],
        )
        .unwrap();
        let events = client.subscribe();
        client.send_command(command).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let spawned = loop {
            if let Some(event) = events.drain().into_iter().find(|event| {
                matches!(
                    event,
                    runtime::RuntimeEvent::AgentSpawned { .. }
                        | runtime::RuntimeEvent::SpawnFailed { .. }
                )
            }) {
                break event;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "owned fixture did not report spawn outcome"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        if let runtime::RuntimeEvent::SpawnFailed { message, .. } = spawned {
            panic!(
                "registered MAIN could not resolve its credential through the production runtime resolver: {message:?}"
            );
        }
        while !output.exists()
            || std::fs::read_to_string(&output)
                .unwrap_or_default()
                .trim()
                .is_empty()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "owned child did not capture its environment digest"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let actual = std::fs::read_to_string(&output).unwrap();
        let expected = sha2::Sha256::digest(token.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(actual.split_whitespace().next(), Some(expected.as_str()));
        assert!(
            !serialized.contains(&token),
            "raw bearer entered command serialization"
        );
        assert!(
            resolver.resolve(&credential_id).is_err(),
            "one-use credential resolved twice"
        );
        assert!(
            resolver.db.lock().unwrap().is_none(),
            "ephemeral reference opened the repository"
        );
        assert!(
            !db_path.exists(),
            "ephemeral credential resolution touched SQLite"
        );
        drop(client);
        drop(tools);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn command_center_main_ephemeral_credential_retires_without_repository_fallback() {
        use runtime::RuntimeSecretResolver;
        let db_path =
            std::env::temp_dir().join(format!("must-not-create-{}.db", uuid::Uuid::new_v4()));
        let resolver = crate::app::AppRuntimeSecretResolver::new(db_path.clone());
        assert!(
            resolver
                .resolve(&format!(
                    "{}unknown",
                    crate::app::EPHEMERAL_MAIN_CREDENTIAL_PREFIX
                ))
                .is_err()
        );
        for retire_scope in [true, false] {
            let mut tools = MainTools::new(secret::RedactionService::new(), || {}).unwrap();
            tools.prepare(pending("retired-credential")).unwrap();
            let slot = tools.registration_slot("project-a", 7, 3).unwrap();
            let mut command = spawn();
            slot.apply(&mut command).unwrap();
            let runtime::RuntimeCommand::SpawnAgent { env_secrets, .. } = command else {
                unreachable!()
            };
            let credential_id = &env_secrets[0].1;
            if retire_scope {
                tools.reconcile(&[], &[]);
                assert!(resolver.resolve(credential_id).is_err());
            }
            drop(tools);
            assert!(resolver.resolve(credential_id).is_err());
        }
        assert!(resolver.db.lock().unwrap().is_none());
        assert!(!db_path.exists());
    }
}
