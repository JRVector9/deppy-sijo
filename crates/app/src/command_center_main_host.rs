//! Host authority stays live across queued intent, durable save and actual launch/input.
use super::*;
use agent_mcp::MainScope;
use main_tools::MainCall;
use serde_json::{Value, json};

pub(super) struct Guard {
    pub scope: MainScope,
    pub goal: String,
    pub tool: String,
    pub permit: runtime::InputPermit,
    retired: bool,
    local_task: Option<String>,
}
impl CommandCenterHost {
    pub(super) fn main_operation_current(&self, id: &str) -> bool {
        self.main_guards.get(id).is_none_or(|guard| {
            !guard.retired
                && guard.local_task.as_ref().is_none_or(|task| {
                    self.projects
                        .get(&guard.scope.workspace_id)
                        .is_some_and(|p| {
                            p.snapshot
                                .tasks
                                .iter()
                                .any(|t| t.id.0 == *task && t.execution_device.is_none())
                        })
                })
                && self.current_main_scopes.contains(&guard.scope)
                && same_main(self, &guard.scope)
                && self
                    .projects
                    .get(&guard.scope.workspace_id)
                    .and_then(main_goal)
                    .is_some_and(|goal| goal.id.0 == guard.goal)
        })
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.permit.revoke();
    }
}
fn main_goal(project: &Project) -> Option<&work::Goal> {
    project
        .request
        .as_ref()
        .map(|request| request.goal())
        .or_else(|| project.snapshot.goals.last())
}
fn registration_runtime_configs(
    host: &CommandCenterHost,
    workspace: &str,
    runtime_instance: u64,
) -> [Option<(String, u64, u64)>; 2] {
    // Temporary storage/load denial revokes authority separately. Keep both the current edit
    // and the exact installed revision: a failed save may reload the previous durable config.
    // Replacing the installation or runtime then makes the former template genuinely obsolete.
    [
        host.projects.get(workspace).map(|project| {
            (
                workspace.to_owned(),
                runtime_instance,
                project.snapshot.config.revision.0,
            )
        }),
        host.installed
            .get(&runtime_instance)
            .map(|installed| (workspace.to_owned(), runtime_instance, installed.revision.0)),
    ]
}
impl CommandCenterHost {
    pub(super) fn retire_main_operations(&mut self) {
        let stale = self
            .main_guards
            .keys()
            .filter(|id| !self.main_operation_current(id))
            .cloned()
            .collect::<Vec<_>>();
        for id in stale {
            if let Some(guard) = self.main_guards.get_mut(&id) {
                guard.retired = true;
                guard.permit.revoke();
            }
        }
    }
}
fn same_main(host: &CommandCenterHost, scope: &MainScope) -> bool {
    let Some(p) = host.projects.get(&scope.workspace_id).filter(|p| {
        p.loaded && !p.storage_blocked && p.snapshot.config.revision.0 == scope.config_revision
    }) else {
        return false;
    };
    let target = host.main_target(&scope.workspace_id, p);
    if target.session_id.as_deref() != Some(&scope.main_session_id) {
        return false;
    }
    let Some(live) = host.route(&target) else {
        return false;
    };
    binding(live, &scope.main_session_id).is_some_and(|b| {
        b.incarnation.0 == scope.incarnation
            && b.session_id.0 == scope.runtime_session_id
            && b.execution_generation == scope.execution_generation
    }) && live.capture.prompt.as_ref().is_some_and(|prompt| {
        prompt.runtime_instance == scope.runtime_instance && prompt.execution.is_current()
    }) && host
        .launches
        .get(&scope.launch_operation_id)
        .is_some_and(|launch| {
            launch.main_binding.as_ref().is_some_and(|b| {
                b.incarnation.0 == scope.incarnation
                    && b.session_id.0 == scope.runtime_session_id
                    && b.execution_generation == scope.execution_generation
            }) && launch.record.task_id.is_none()
                && launch.runtime_instance == scope.runtime_instance
                && launch.record.config_revision.0 == scope.config_revision
                && launch
                    .request
                    .as_ref()
                    .is_some_and(|r| r.identity.incarnation.0 == scope.incarnation)
        })
}
pub(super) fn call(
    host: &mut CommandCenterHost,
    scope: &MainScope,
    action: MainCall,
    id: Option<&str>,
) -> Result<Value, String> {
    if !same_main(host, scope) {
        return Err("main_scope_changed_no_effect".into());
    }
    let p = host
        .projects
        .get(&scope.workspace_id)
        .ok_or("project_unavailable")?;
    match action {
        MainCall::Project => {
            let tasks=p.snapshot.tasks.iter().rev().take(64).map(|t|json!({"id":t.id,"goal_id":t.goal_id,"title":bounded(&t.title,256),"state":t.state,"attempt_id":t.current_attempt_id,"result_revision":t.result.as_ref().map(|r|r.revision),"plan_confirmed":p.confirmed_for_task(&t.id).is_some()})).collect::<Vec<_>>();
            let tasks = bounded_json_entries(&tasks, 16 * 1024)?;
            let agents = bounded_json_entries(&p.allowed_agents, 4 * 1024)?;
            let roots = bounded_json_entries(&p.scopes, 8 * 1024)?;
            let mut response = json!({"workspace_id":scope.workspace_id,"config_revision":p.snapshot.config.revision.0,"goal":main_goal(p).map(|g|json!({"id":g.id,"title":bounded(&g.title,256),"prompt":bounded(&g.prompt,4096)})),"tasks":tasks,"task_count":p.snapshot.tasks.len(),"tasks_truncated":p.snapshot.tasks.len()>tasks.len(),"main_session_id":scope.main_session_id,"pending_plan_confirmation":p.pending_confirmation_goal(),"config":{"folder":p.snapshot.config.folder,"allowed_agents":agents,"allowed_scope_roots":roots,"max_workers":p.snapshot.config.max_workers},"plan_schema":p.request.as_ref().map(|r|r.response_shape()),"main_tools":"active","verification":"Explicit local user approval required; no automatic execution","final_confirmation":"User confirmation required"});
            // Keep full configured identities/paths; omit whole entries under pressure.
            response["config"]["allowed_agent_count"] = json!(p.allowed_agents.len());
            response["config"]["allowed_scope_root_count"] = json!(p.scopes.len());
            response["config"]["allowed_agents_truncated"] =
                json!(p.allowed_agents.len() > agents.len());
            response["config"]["allowed_scope_roots_truncated"] =
                json!(p.scopes.len() > roots.len());
            while serde_json::to_vec(&response)
                .map_err(|_| "main_response_too_large_no_effect")?
                .len()
                > 48 * 1024
            {
                let paths = [
                    ("config", "allowed_scope_roots"),
                    ("config", "allowed_agents"),
                    ("", "tasks"),
                ];
                let Some((parent, key)) = paths
                    .into_iter()
                    .filter(|(parent, key)| {
                        let array = if parent.is_empty() {
                            &response[*key]
                        } else {
                            &response[*parent][*key]
                        };
                        array.as_array().is_some_and(|a| !a.is_empty())
                    })
                    .max_by_key(|(parent, key)| {
                        let array = if parent.is_empty() {
                            &response[*key]
                        } else {
                            &response[*parent][*key]
                        };
                        serde_json::to_vec(array).map_or(0, |a| a.len())
                    })
                else {
                    return Err("main_response_too_large_no_effect".into());
                };
                let container = if parent.is_empty() {
                    &mut response
                } else {
                    &mut response[parent]
                };
                container[key].as_array_mut().unwrap().pop();
                container[format!("{key}_truncated")] = json!(true);
            }
            Ok(response)
        }
        MainCall::Task {
            task_id, goal_id, ..
        } => {
            let task = p
                .snapshot
                .tasks
                .iter()
                .find(|task| task.id.0 == task_id && task.goal_id.0 == goal_id)
                .ok_or("main_goal_changed_no_effect")?;
            let attempt = task
                .current_attempt_id
                .as_ref()
                .and_then(|id| p.snapshot.attempts.iter().find(|a| a.id == *id));
            Ok(
                json!({"task":task,"attempt":attempt,"plan_confirmed":p.confirmed_for_task(&task.id).is_some()}),
            )
        }
        MainCall::Operation { host_operation_id } => {
            if !host
                .main_guards
                .get(&host_operation_id)
                .is_some_and(|g| g.scope == *scope)
            {
                return Ok(json!({"status":"unknown","retry":false,"effect_state":"unknown"}));
            }
            Ok(
                match host
                    .receipts
                    .iter()
                    .find(|r| r.operation_id == host_operation_id)
                {
                    Some(receipt) => json!({"receipt":receipt,"retry":false}),
                    None => {
                        json!({"status":"pending_or_unknown","host_operation_id":host_operation_id,"retry":false})
                    }
                },
            )
        }
        MainCall::ProposePlan {
            goal_id,
            config_revision,
            json: plan,
        } => {
            if config_revision != p.snapshot.config.revision.0
                || main_goal(p).is_none_or(|g| g.id.0 != goal_id)
            {
                return Err("main_goal_changed_no_effect".into());
            }
            enqueue(
                host,
                scope,
                &goal_id,
                "cc_propose_plan",
                id,
                cc::Intent::InstallPlanJson {
                    workspace_id: scope.workspace_id.clone(),
                    json: plan,
                },
            )
        }
        MainCall::DispatchTask {
            goal_id,
            config_revision,
            task_id,
            worker_session_id,
        } => {
            if config_revision != p.snapshot.config.revision.0
                || main_goal(p).is_none_or(|g| g.id.0 != goal_id)
            {
                return Err("main_goal_changed_no_effect".into());
            }
            let task = p
                .snapshot
                .tasks
                .iter()
                .find(|t| t.id.0 == task_id && t.goal_id.0 == goal_id)
                .ok_or("main_goal_changed_no_effect")?;
            if task.execution_device.is_some() {
                return Err("main_dispatch_rejected_no_effect".into());
            }
            if p.confirmed_for_task(&task.id).is_none() || !p.state().can_dispatch(&task.id) {
                return Err("main_dispatch_rejected_no_effect".into());
            }
            let target = cc::RouteTarget {
                workspace_id: scope.workspace_id.clone(),
                session_id: worker_session_id,
                task_id: Some(task_id),
            };
            let intent = if target.session_id.is_some() {
                cc::Intent::DispatchTask { target }
            } else {
                cc::Intent::CreateWorker { target }
            };
            enqueue(host, scope, &goal_id, "cc_dispatch_task", id, intent)
        }
    }
}
fn bounded_json_entries<T: serde::Serialize>(
    items: &[T],
    budget: usize,
) -> Result<Vec<Value>, String> {
    let mut result = Vec::new();
    let mut bytes = 2;
    for item in items.iter().take(64) {
        let value = serde_json::to_value(item).map_err(|_| "main_response_too_large_no_effect")?;
        let size = serde_json::to_vec(&value)
            .map_err(|_| "main_response_too_large_no_effect")?
            .len()
            + 1;
        if bytes + size > budget {
            break;
        }
        bytes += size;
        result.push(value);
    }
    Ok(result)
}
fn enqueue(
    host: &mut CommandCenterHost,
    scope: &MainScope,
    goal: &str,
    tool: &str,
    id: Option<&str>,
    intent: cc::Intent,
) -> Result<Value, String> {
    let id = id.ok_or("main_operation_id_required")?;
    if host.main_guards.len() >= 256 || host.operations.contains_key(id) {
        return Err("main_operation_capacity_no_effect".into());
    }
    let local_task = match &intent {
        cc::Intent::DispatchTask { target } | cc::Intent::CreateWorker { target } => {
            target.task_id.clone()
        }
        _ => None,
    };
    host.enqueue(id.into(), intent);
    if !host.operations.contains_key(id) {
        return Err("busy_no_effect".into());
    }
    host.main_guards.insert(
        id.into(),
        Guard {
            scope: scope.clone(),
            goal: goal.into(),
            tool: tool.into(),
            permit: runtime::InputPermit::new(),
            retired: false,
            local_task,
        },
    );
    let target = cc::RouteTarget {
        workspace_id: scope.workspace_id.clone(),
        session_id: Some(scope.main_session_id.clone()),
        task_id: None,
    };
    conversation::record(
        host,
        &target,
        cc::ConversationEntry {
            id: format!("tool:{id}"),
            turn_id: None,
            kind: cc::ConversationEntryKind::Tool,
            text: "메인 관리 요청을 수신했습니다. 실제 실행·저장 응답을 기다립니다.".into(),
            tool_name: Some(tool.into()),
            state: Some(cc::ConversationActivityState::Running),
            observed_at: None,
        },
    );
    Ok(
        json!({"status":"accepted","host_operation_id":id,"retry":false,"user_confirmation_required":tool=="cc_propose_plan"}),
    )
}
impl App {
    pub(in crate::app) fn command_center_main_input_permit(
        &self,
        id: &str,
    ) -> Option<runtime::InputPermit> {
        self.command_center
            .as_ref()?
            .main_guards
            .get(id)
            .map(|guard| guard.permit.clone())
    }
    pub(super) fn ensure_command_center_main_tools(
        &self,
        host: &mut CommandCenterHost,
    ) -> Result<(), String> {
        if host.main_tools.is_none() {
            let ctx = self.egui_ctx.clone();
            host.main_tools = Some(
                main_tools::MainTools::new(self.command_center_redaction.clone(), move || {
                    ctx.request_repaint()
                })
                .map_err(|_| "메인 관리 도구의 로컬 연결을 준비하지 못했습니다.".to_owned())?,
            );
        }
        Ok(())
    }
    pub(super) fn pump_command_center_main_tools(&self, host: &mut CommandCenterHost) {
        let Some(mut tools) = host.main_tools.take() else {
            return;
        };
        let mut current = vec![];
        let mut pending = vec![];
        // Keep empty slots belonging to the currently installed local runtime/configuration:
        // its launch Templates intentionally share that Arc across successive MAIN executions.
        let current_runtime_configs = std::iter::once(&self.active)
            .chain(self.warm.values())
            .flat_map(|runtime| {
                registration_runtime_configs(host, &runtime.id, runtime.runtime_instance)
                    .into_iter()
                    .flatten()
            })
            .collect::<Vec<_>>();
        for (id, launch) in &host.launches {
            if launch.record.task_id.is_some() || launch.record.selection.provider != "codex" {
                continue;
            }
            let workspace = &launch.record.workspace.0;
            let Some(project) = host.projects.get(workspace).filter(|p| {
                p.loaded
                    && !p.storage_blocked
                    && p.snapshot.config.revision == launch.record.config_revision
            }) else {
                continue;
            };
            let runtime = self
                .active
                .id
                .eq(workspace)
                .then_some(&self.active)
                .or_else(|| self.warm.get(workspace))
                .filter(|r| r.runtime_instance == launch.runtime_instance);
            let Some(runtime) = runtime else {
                continue;
            };
            if !launch.finished() {
                pending.push(id.clone());
            }
            if !matches!(launch.record.phase, launch::Phase::Completed) {
                continue;
            }
            let target = host.main_target(workspace, project);
            let Some(key) = target.session_id.as_deref() else {
                continue;
            };
            let Some(live) = host.route(&target) else {
                continue;
            };
            let Some(bound) = binding(live, key) else {
                continue;
            };
            if launch.main_binding.as_ref() != Some(&bound) {
                continue;
            }
            if launch
                .request
                .as_ref()
                .is_none_or(|r| r.identity.incarnation != bound.incarnation)
            {
                continue;
            }
            let scope = MainScope {
                workspace_id: workspace.clone(),
                main_session_id: key.into(),
                runtime_instance: runtime.runtime_instance,
                runtime_token: runtime.runtime.instance_token().into(),
                incarnation: bound.incarnation.0,
                runtime_session_id: bound.session_id.0,
                execution_generation: bound.execution_generation,
                config_revision: project.snapshot.config.revision.0,
                launch_operation_id: id.clone(),
            };
            if same_main(host, &scope) && tools.activate(id, scope.clone()).is_ok() {
                current.push(scope);
            }
        }
        tools.reconcile(&current, &pending, &current_runtime_configs);
        host.current_main_scopes = current.clone();
        host.retire_main_operations();
        // Bounded work per frame. Callbacks enqueue; never launch/re-enter Auth while locked.
        for _ in 0..4 {
            let Some(request) = tools.next_request() else {
                break;
            };
            let admitted = tools.scope(&request);
            let exact = admitted.as_ref().filter(|s| current.contains(s)).cloned();
            let goal = exact
                .as_ref()
                .and_then(|s| host.projects.get(&s.workspace_id))
                .and_then(main_goal)
                .map(|g| g.id.0.clone());
            tools.handle(
                request,
                exact.as_ref(),
                goal.as_deref(),
                |scope, action, id| call(host, scope, action, id),
            );
        }
        host.main_tools = Some(tools);
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    pub(in crate::app::command_center_host) fn fixture() -> (CommandCenterHost, MainScope) {
        fixture_with_execution(
            crate::agent_detect::AgentExecutionIdentity::fixture_current(
                crate::agent_detect::AgentKind::Codex,
                std::process::id(),
            )
            .unwrap(),
        )
    }
    fn fixture_with_execution(
        execution: crate::agent_detect::AgentExecutionIdentity,
    ) -> (CommandCenterHost, MainScope) {
        let mut host = super::super::tests::host();
        let (mut project, assignment) = super::super::tests::attempted();
        let bound = assignment.attempt.runtime_binding.clone().unwrap();
        let capture = Capture {
            workspace: "workspace-a".into(),
            target: FleetTarget::Pty {
                tab: runtime::MuxTabId::new(),
                pane: runtime::MuxPaneId::new(),
                session: bound.session_id,
            },
            prompt: Some(FleetPromptTarget {
                workspace_id: "workspace-a".into(),
                runtime_instance: 19,
                session: bound.session_id,
                execution,
            }),
        };
        host.routes.refresh(vec![capture.clone()]);
        let live = LiveRoute {
            capture,
            title: "main".into(),
            agent: Some(project.snapshot.config.main_agent.clone()),
            state: cc::TaskState::Queued,
            cwd: Some(project.snapshot.config.folder.clone()),
            watch: None,
            runtime_binding: Some(bound.clone()),
        };
        project.snapshot.main_binding = persistent(&live);
        let key = host.target(&live).session_id.unwrap();
        host.live.push(live);
        let operation = work::OperationId::new();
        let request = runtime::remote_worker::RemoteWorkRequest {
            identity: runtime::remote_worker::RemoteWorkIdentity {
                device_id: "local".into(),
                workspace_id: WorkspaceId("workspace-a".into()),
                attempt_id: work::AttemptId::new(),
                operation_id: operation.clone(),
                incarnation: bound.incarnation.clone(),
                grant_epoch: 1,
                lease_epoch: 1,
            },
            folder: project.snapshot.config.folder.clone(),
            folder_anchor: project.snapshot.config.folder.clone(),
            profile_id: "main".into(),
            initial_prompt: None,
        };
        let scope = MainScope {
            workspace_id: "workspace-a".into(),
            main_session_id: key,
            runtime_instance: 19,
            runtime_token: "real-runtime".into(),
            incarnation: bound.incarnation.0.clone(),
            runtime_session_id: bound.session_id.0,
            execution_generation: bound.execution_generation,
            config_revision: project.snapshot.config.revision.0,
            launch_operation_id: operation.0.clone(),
        };
        host.launches.insert(
            operation.0.clone(),
            launch::Pending {
                record: launch::Record {
                    operation_id: operation,
                    attempt_id: work::AttemptId::new(),
                    workspace: WorkspaceId("workspace-a".into()),
                    task_id: None,
                    config_revision: project.snapshot.config.revision,
                    selection: project.snapshot.config.main_agent.clone(),
                    folder: project.snapshot.config.folder.clone(),
                    phase: launch::Phase::Completed,
                },
                runtime_instance: 19,
                main_binding: Some(bound),
                generation: 0,
                stage: launch::Stage::Done,
                request: Some(request),
                deadline: std::time::Instant::now(),
                owned: None,
                grant: None,
            },
        );
        host.projects.insert("workspace-a".into(), project);
        host.current_main_scopes.push(scope.clone());
        (host, scope)
    }
    #[test]
    fn command_center_main_host_authority_rejects_rebound_main_and_changed_goal_permanently() {
        let (mut host, scope) = fixture();
        assert!(same_main(&host, &scope));
        let goal = main_goal(&host.projects["workspace-a"])
            .unwrap()
            .id
            .0
            .clone();
        enqueue(
            &mut host,
            &scope,
            &goal,
            "cc_propose_plan",
            Some("op"),
            cc::Intent::InstallPlanJson {
                workspace_id: "workspace-a".into(),
                json: "{}".into(),
            },
        )
        .unwrap();
        assert!(host.main_operation_current("op"));
        host.projects
            .get_mut("workspace-a")
            .unwrap()
            .snapshot
            .goals
            .last_mut()
            .unwrap()
            .id = work::GoalId::new();
        assert!(!host.main_operation_current("op"));
        host.retire_main_operations();
        host.projects
            .get_mut("workspace-a")
            .unwrap()
            .snapshot
            .goals
            .last_mut()
            .unwrap()
            .id = work::GoalId::from_string(goal).unwrap();
        assert!(!host.main_operation_current("op"));
        host.live[0]
            .runtime_binding
            .as_mut()
            .unwrap()
            .execution_generation += 1;
        assert!(!same_main(&host, &scope));
    }
    fn main_spawn() -> runtime::RuntimeCommand {
        runtime::RuntimeCommand::SpawnAgent {
            agent_config_id: Some("configured-agent".into()),
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
            command: "/bin/sh".into(),
            args: vec![],
            env_plain: vec![],
            env_secrets: vec![],
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
            transient_args: vec![],
        }
    }
    #[test]
    fn command_center_main_host_temporary_storage_block_preserves_installed_slot_for_relaunch() {
        let (mut host, scope) = fixture();
        let mut tools = main_tools::MainTools::new(secret::RedactionService::new(), || {}).unwrap();
        let pending = |operation: &str| agent_mcp::PendingMainScope {
            workspace_id: scope.workspace_id.clone(),
            runtime_instance: scope.runtime_instance,
            runtime_token: scope.runtime_token.clone(),
            config_revision: scope.config_revision,
            launch_operation_id: operation.to_owned(),
        };
        tools.prepare(pending(&scope.launch_operation_id)).unwrap();
        let installed_slot = tools
            .registration_slot(
                &scope.workspace_id,
                scope.runtime_instance,
                scope.config_revision,
            )
            .unwrap();
        tools
            .activate(&scope.launch_operation_id, scope.clone())
            .unwrap();
        let project = host.projects.get_mut(&scope.workspace_id).unwrap();
        project.storage_blocked = true;
        project.loaded = false;
        assert!(
            !same_main(&host, &scope),
            "temporary denial must remove execution authority"
        );
        // This is the exact host identity collector used by the production MAIN pump.
        let current_configs =
            registration_runtime_configs(&host, &scope.workspace_id, scope.runtime_instance)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
        tools.reconcile(&[], &[], &current_configs);
        assert_eq!(
            tools.activate(&scope.launch_operation_id, scope.clone()),
            Err("main_registration_missing".into())
        );
        let project = host.projects.get_mut(&scope.workspace_id).unwrap();
        project.storage_blocked = false;
        project.loaded = true;
        tools.prepare(pending("recovered-main")).unwrap();
        let mut command = main_spawn();
        installed_slot.apply(&mut command).unwrap();
        assert!(installed_slot.apply(&mut command).is_err());
    }

    #[test]
    fn command_center_main_host_keeps_installed_slot_across_failed_config_save_and_reclaims_after_reinstall()
     {
        let (mut host, scope) = fixture();
        let old_revision = work::ConfigRevision(scope.config_revision);
        let new_revision = old_revision.next().unwrap();
        let durable = stored::StoredProject {
            revision: stored::ProjectRevision::new(1).unwrap(),
            snapshot: host.projects[&scope.workspace_id].snapshot.clone(),
        };
        host.installed.insert(
            scope.runtime_instance,
            launch::Installed {
                revision: old_revision,
                profiles: vec![],
            },
        );
        let mut tools = main_tools::MainTools::new(secret::RedactionService::new(), || {}).unwrap();
        let pending = |operation: &str| agent_mcp::PendingMainScope {
            workspace_id: scope.workspace_id.clone(),
            runtime_instance: scope.runtime_instance,
            runtime_token: scope.runtime_token.clone(),
            config_revision: scope.config_revision,
            launch_operation_id: operation.to_owned(),
        };
        tools.prepare(pending(&scope.launch_operation_id)).unwrap();
        let installed_slot = tools
            .registration_slot(
                &scope.workspace_id,
                scope.runtime_instance,
                scope.config_revision,
            )
            .unwrap();
        tools
            .activate(&scope.launch_operation_id, scope.clone())
            .unwrap();
        let project = host.projects.get_mut(&scope.workspace_id).unwrap();
        project.snapshot.config.revision = new_revision;
        project.storage_blocked = true;
        let configs =
            registration_runtime_configs(&host, &scope.workspace_id, scope.runtime_instance)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
        assert!(configs.contains(&(
            scope.workspace_id.clone(),
            scope.runtime_instance,
            new_revision.0
        )));
        assert!(configs.contains(&(
            scope.workspace_id.clone(),
            scope.runtime_instance,
            old_revision.0
        )));
        tools.reconcile(&[], &[], &configs);
        // Exercise the production repository-result reducer that restores the old durable config.
        finish_project_load(
            &mut host,
            &scope.workspace_id,
            None,
            Ok(Some(Box::new(durable))),
        );
        assert_eq!(
            host.projects[&scope.workspace_id].snapshot.config.revision,
            old_revision
        );
        assert!(host.projects[&scope.workspace_id].loaded);
        assert!(!host.projects[&scope.workspace_id].storage_blocked);
        tools.prepare(pending("recovered-durable-main")).unwrap();
        installed_slot.apply(&mut main_spawn()).unwrap();
        // Once the runtime really installs the new configuration, no owner needs the old slot.
        host.projects
            .get_mut(&scope.workspace_id)
            .unwrap()
            .snapshot
            .config
            .revision = new_revision;
        host.installed
            .get_mut(&scope.runtime_instance)
            .unwrap()
            .revision = new_revision;
        let configs =
            registration_runtime_configs(&host, &scope.workspace_id, scope.runtime_instance)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
        assert!(!configs.contains(&(
            scope.workspace_id.clone(),
            scope.runtime_instance,
            old_revision.0
        )));
        tools.reconcile(&[], &[], &configs);
        // Recreating that same old tuple now uses a distinct slot; stale Templates cannot take it.
        tools.prepare(pending("recreated-obsolete-tuple")).unwrap();
        assert!(installed_slot.apply(&mut main_spawn()).is_err());
        let fresh_slot = tools
            .registration_slot(
                &scope.workspace_id,
                scope.runtime_instance,
                scope.config_revision,
            )
            .unwrap();
        fresh_slot.apply(&mut main_spawn()).unwrap();
    }

    #[test]
    fn command_center_main_host_rejects_exited_exact_process_before_route_refresh() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("1")
            .spawn()
            .unwrap();
        let execution = crate::agent_detect::AgentExecutionIdentity::fixture_current(
            crate::agent_detect::AgentKind::Codex,
            child.id(),
        )
        .unwrap();
        let (mut host, scope) = fixture_with_execution(execution);
        assert!(same_main(&host, &scope));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!same_main(&host, &scope));
        assert!(call(&mut host, &scope, MainCall::Project, None).is_err());
    }

    #[test]
    fn command_center_main_host_cannot_activate_different_spawn_in_same_incarnation() {
        let (mut host, scope) = fixture();
        assert!(same_main(&host, &scope));
        host.launches
            .get_mut(&scope.launch_operation_id)
            .unwrap()
            .main_binding
            .as_mut()
            .unwrap()
            .session_id = runtime::SessionId(scope.runtime_session_id + 1);
        assert!(!same_main(&host, &scope));
    }
    #[test]
    fn command_center_main_host_project_read_bounds_full_scope_values() {
        let (mut host, scope) = fixture();
        let p = host.projects.get_mut("workspace-a").unwrap();
        p.scopes = (0..64)
            .map(|i| format!("/scope/{i}/{}", "한글".repeat(1000)))
            .collect();
        let expected = p.scopes.clone();
        let project = call(&mut host, &scope, MainCall::Project, None).unwrap();
        assert!(serde_json::to_vec(&project).unwrap().len() <= 48 * 1024);
        assert_eq!(project["config"]["allowed_scope_root_count"], 64);
        assert_eq!(project["config"]["allowed_scope_roots_truncated"], true);
        for value in project["config"]["allowed_scope_roots"].as_array().unwrap() {
            assert!(expected.iter().any(|root| value.as_str() == Some(root)));
        }
    }

    #[test]
    fn command_center_main_host_project_read_is_compact_and_dispatch_needs_confirmation() {
        let (mut host, scope) = fixture();
        let task = &mut host.projects.get_mut("workspace-a").unwrap().snapshot.tasks[0];
        task.instructions = "large instruction ".repeat(2048);
        let project = call(&mut host, &scope, MainCall::Project, None).unwrap();
        assert!(serde_json::to_vec(&project).unwrap().len() < 8192);
        assert!(project["tasks"][0].get("instructions").is_none());
        let goal = main_goal(&host.projects["workspace-a"])
            .unwrap()
            .id
            .0
            .clone();
        let task = host.projects["workspace-a"].snapshot.tasks[0].id.0.clone();
        assert!(
            call(
                &mut host,
                &scope,
                MainCall::DispatchTask {
                    goal_id: goal,
                    config_revision: scope.config_revision,
                    task_id: task,
                    worker_session_id: None
                },
                Some("dispatch")
            )
            .is_err()
        );
        assert!(!host.operations.contains_key("dispatch"));
    }
}
