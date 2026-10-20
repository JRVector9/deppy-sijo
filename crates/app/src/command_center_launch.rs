//! Worker-owned launch preparation. No credential, executable or directory work runs in egui.
use super::*;
use deppy_core::work_management as work;
use runtime::remote_worker::{
    RemoteWorkLauncher, RemoteWorkerControl, RemoteWorkerHost, RemoteWorkerProfile,
};

pub(crate) enum SettingsRequest {
    ServerCopy {
        operation_id: String,
        runtime_instance: u64,
        control: RemoteWorkerControl,
        grant: super::server::Grant,
        device_id: String,
    },
    ServerInstall {
        request: Box<InstallRequest>,
        exported_workspace: WorkspaceId,
        device_id: String,
    },
    ServerRevoke {
        operation_id: String,
        runtime_instance: u64,
        control: RemoteWorkerControl,
    },
    NativeIdentity {
        operation_id: String,
        workspace: String,
        session: runtime::SessionId,
        runtime_instance: u64,
        runtime_token: String,
    },
    Catalog {
        workspace: String,
        folder: String,
        revision: work::ConfigRevision,
        prepare_root: bool,
    },
    PrepareRoot {
        operation_id: String,
        workspace: String,
        folder: String,
        revision: work::ConfigRevision,
    },
    GrantOwned {
        operation_id: String,
        runtime_instance: u64,
        control: RemoteWorkerControl,
        attempt_id: work::AttemptId,
        owned: wm::task_worktree::OwnedTaskWorktree,
    },
    Install(Box<InstallRequest>),
}
pub(crate) struct InstallRequest {
    pub operation_id: String,
    pub runtime_instance: u64,
    pub config: work::ProjectConfig,
    pub agents: Vec<work::AgentSelection>,
    pub control: RemoteWorkerControl,
    pub wake: Option<Arc<dyn Fn() + Send + Sync>>,
    pub data_dir: PathBuf,
    pub grant_epoch: u64,
    pub scrollback: usize,
    pub hooks: bool,
}
pub(crate) enum SettingsResult {
    ServerCopy {
        operation_id: String,
        runtime_instance: u64,
        grant: super::server::Grant,
        token: Result<secret::SecretString, String>,
    },
    ServerConfigured {
        operation_id: String,
        runtime_instance: u64,
        result: Result<Option<super::server::Prepared>, String>,
    },
    NativeIdentity {
        operation_id: String,
        workspace: String,
        session: runtime::SessionId,
        runtime_instance: u64,
        runtime_token: String,
        native: Option<String>,
    },
    Catalog {
        workspace: String,
        folder: String,
        revision: work::ConfigRevision,
        result: Result<Catalog, String>,
    },
    RootPrepared {
        operation_id: String,
        workspace: String,
        folder: String,
        revision: work::ConfigRevision,
        result: Result<wm::task_worktree::ProjectWorktreeRootProof, String>,
    },
    FolderGranted {
        operation_id: String,
        runtime_instance: u64,
        attempt_id: work::AttemptId,
        result: Result<runtime::remote_worker::AttemptFolderGrant, String>,
    },
    Installed {
        operation_id: String,
        runtime_instance: u64,
        config_revision: work::ConfigRevision,
        result: Result<Vec<RemoteWorkerProfile>, String>,
    },
}
#[derive(Clone)]
pub(crate) struct Catalog {
    pub choices: Vec<work::AgentSelection>,
    pub defaults: Vec<work::AgentSelection>,
    pub root: Option<wm::task_worktree::ProjectWorktreeRootProof>,
}
impl SettingsRequest {
    pub(crate) fn failed(self) -> SettingsResult {
        match self {
            Self::ServerCopy {
                operation_id,
                runtime_instance,
                grant,
                ..
            } => SettingsResult::ServerCopy {
                operation_id,
                runtime_instance,
                grant,
                token: Err("연결 정보 발급을 확인하지 못했습니다. 다시 복사해 주세요.".into()),
            },
            Self::ServerInstall { request, .. } => SettingsResult::ServerConfigured {
                operation_id: request.operation_id,
                runtime_instance: request.runtime_instance,
                result: Err("이 컴퓨터의 실행 환경을 준비하지 못했습니다.".into()),
            },
            Self::ServerRevoke {
                operation_id,
                runtime_instance,
                ..
            } => SettingsResult::ServerConfigured {
                operation_id,
                runtime_instance,
                result: Err("실행 허용 회수를 확인하지 못했습니다. 다시 회수해 주세요.".into()),
            },
            Self::NativeIdentity {
                operation_id,
                workspace,
                session,
                runtime_instance,
                runtime_token,
            } => SettingsResult::NativeIdentity {
                operation_id,
                workspace,
                session,
                runtime_instance,
                runtime_token,
                native: None,
            },
            Self::Catalog {
                workspace,
                folder,
                revision,
                ..
            } => SettingsResult::Catalog {
                workspace,
                folder,
                revision,
                result: Err("설치된 에이전트 설정을 읽지 못했습니다.".into()),
            },
            Self::PrepareRoot {
                operation_id,
                workspace,
                folder,
                revision,
            } => SettingsResult::RootPrepared {
                operation_id,
                workspace,
                folder,
                revision,
                result: Err("워크트리 루트를 준비하지 못했습니다.".into()),
            },
            Self::GrantOwned {
                operation_id,
                runtime_instance,
                attempt_id,
                ..
            } => SettingsResult::FolderGranted {
                operation_id,
                runtime_instance,
                attempt_id,
                result: Err("전용 실행 폴더를 승인하지 못했습니다.".into()),
            },
            Self::Install(request) => SettingsResult::Installed {
                operation_id: request.operation_id,
                runtime_instance: request.runtime_instance,
                config_revision: request.config.revision,
                result: Err("에이전트 실행 설정을 준비하지 못했습니다.".into()),
            },
        }
    }
}

pub(crate) fn execute(db: &Db, request: SettingsRequest) -> SettingsResult {
    match request {
        SettingsRequest::ServerCopy {
            operation_id,
            runtime_instance,
            control,
            grant,
            device_id,
        } => {
            let token = control
                .issue_scoped_token(
                    &device_id,
                    &WorkspaceId(grant.exported_workspace.clone()),
                    grant.epoch,
                )
                .map_err(|_| {
                    "현재 프로젝트 실행 허용이 바뀌어 연결 정보를 복사하지 않았습니다.".into()
                });
            SettingsResult::ServerCopy {
                operation_id,
                runtime_instance,
                grant,
                token,
            }
        }
        SettingsRequest::ServerInstall {
            request,
            exported_workspace,
            device_id,
        } => {
            let mut request = *request;
            let result = (|| -> anyhow::Result<_> {
                request.grant_epoch = super::server::next_server_grant_epoch(&request.data_dir)?;
                let host = prepare_host(db, &request, device_id, exported_workspace)?;
                let folder = std::fs::canonicalize(&request.config.folder)?;
                let folder = folder.to_str().context("server_folder_encoding")?.to_owned();
                request.control.configure(Some(host))?;
                Ok(Some(super::server::Prepared { profiles: host_profiles(&request.config, &request.agents), folder, epoch: request.grant_epoch }))
            })().map_err(|_| "선택한 프로젝트의 실행 환경을 허용하지 못했습니다. 실행 중인 작업과 에이전트 설정을 확인해 주세요.".into());
            SettingsResult::ServerConfigured {
                operation_id: request.operation_id,
                runtime_instance: request.runtime_instance,
                result,
            }
        }
        SettingsRequest::ServerRevoke {
            operation_id,
            runtime_instance,
            control,
        } => {
            let result = control
                .configure(None)
                .map(|()| None)
                .map_err(|_| "실행 허용을 회수하지 못했습니다. 다시 회수해 주세요.".into());
            SettingsResult::ServerConfigured {
                operation_id,
                runtime_instance,
                result,
            }
        }
        SettingsRequest::NativeIdentity {
            operation_id,
            workspace,
            session,
            runtime_instance,
            runtime_token,
        } => {
            let native = db
                .agent_attention_native_for_runtime(
                    &format!("{workspace}:{}", session.0),
                    &runtime_token,
                )
                .ok()
                .flatten();
            SettingsResult::NativeIdentity {
                operation_id,
                workspace,
                session,
                runtime_instance,
                runtime_token,
                native,
            }
        }
        SettingsRequest::Catalog {
            workspace,
            folder,
            revision,
            prepare_root,
        } => SettingsResult::Catalog {
            workspace,
            folder: folder.clone(),
            revision,
            result: (|| {
                let mut catalog = catalog(&crate::agent_launcher::detect_installed_agents(Some(
                    Path::new(&folder),
                )));
                if prepare_root {
                    catalog.root = Some(
                        wm::task_worktree::prepare_project_worktree_root(&folder)
                            .map_err(|e| e.as_str().to_owned())?,
                    );
                }
                Ok(catalog)
            })(),
        },
        SettingsRequest::PrepareRoot {
            operation_id,
            workspace,
            folder,
            revision,
        } => SettingsResult::RootPrepared {
            operation_id,
            workspace,
            revision,
            result: wm::task_worktree::prepare_project_worktree_root(&folder)
                .map_err(|e| e.as_str().into()),
            folder,
        },
        SettingsRequest::GrantOwned {
            operation_id,
            runtime_instance,
            control,
            attempt_id,
            owned,
        } => {
            // Only a worker-created private Git ownership proof reaches this branch.
            let result = control
                .grant_attempt_folder(&attempt_id, &owned.path)
                .map_err(|_| "소유가 검증된 워크트리 실행 폴더를 승인하지 못했습니다.".into());
            SettingsResult::FolderGranted {
                operation_id,
                runtime_instance,
                attempt_id,
                result,
            }
        }
        SettingsRequest::Install(request) => {
            let result = install(db,&request).map_err(|_| "현재 공급자·모델·강도 또는 실행 폴더를 준비할 수 없습니다. 기존 실행 설정을 확인해 주세요.".into());
            SettingsResult::Installed {
                operation_id: request.operation_id,
                runtime_instance: request.runtime_instance,
                config_revision: request.config.revision,
                result,
            }
        }
    }
}
fn selection(
    agent: &crate::agent_launcher::DetectedAgent,
    model: &str,
    effort: Option<crate::agent_launcher::ReasoningEffort>,
) -> work::AgentSelection {
    work::AgentSelection {
        provider: format!("{:?}", agent.kind()).to_ascii_lowercase(),
        model: (!model.is_empty()).then(|| model.into()),
        effort: effort.map(|e| e.value().into()),
    }
}
fn catalog(detected: &crate::agent_launcher::DetectionSnapshot) -> Catalog {
    let mut choices = vec![];
    let mut defaults = vec![];
    for agent in detected.agents() {
        let default = selection(
            agent,
            agent.initial_model(),
            agent.initial_effort(agent.initial_model()),
        );
        defaults.push(default.clone());
        choices.push(default);
        for model in agent.models() {
            choices.push(selection(agent, model.value(), None));
            for effort in model.efforts() {
                choices.push(selection(agent, model.value(), Some(*effort)));
            }
        }
    }
    choices.sort_by(|a, b| {
        (&a.provider, &a.model, &a.effort).cmp(&(&b.provider, &b.model, &b.effort))
    });
    choices.dedup();
    choices.truncate(256);
    Catalog {
        choices,
        defaults,
        root: None,
    }
}
struct Templates(Vec<(RemoteWorkerProfile, runtime::RuntimeCommand)>);
impl RemoteWorkLauncher for Templates {
    fn prepare(
        &self,
        profile: &RemoteWorkerProfile,
        initial_prompt: Option<&str>,
    ) -> anyhow::Result<runtime::RuntimeCommand> {
        anyhow::ensure!(
            initial_prompt.is_none(),
            "command_center_requires_idle_launch"
        );
        self.0
            .iter()
            .find(|(approved, _)| approved == profile)
            .map(|(_, command)| command.clone())
            .ok_or_else(|| anyhow::anyhow!("command_center_profile_changed"))
    }
}
fn install(db: &Db, request: &InstallRequest) -> anyhow::Result<Vec<RemoteWorkerProfile>> {
    let host = prepare_host(
        db,
        request,
        format!("local-{}", request.runtime_instance),
        request.config.workspace_id.clone(),
    )?;
    request.control.configure(Some(host))?;
    Ok(host_profiles(&request.config, &request.agents))
}
fn prepare_host(
    db: &Db,
    request: &InstallRequest,
    device_id: String,
    exported_workspace: WorkspaceId,
) -> anyhow::Result<RemoteWorkerHost> {
    request.config.validate()?;
    anyhow::ensure!(
        !request.agents.is_empty() && request.agents.len() <= 32,
        "command_center_profile_capacity"
    );
    let detected =
        crate::agent_launcher::detect_installed_agents(Some(Path::new(&request.config.folder)));
    let mut templates = vec![];
    for (index, approved) in request.agents.iter().enumerate() {
        approved.validate()?;
        let agent = detected
            .agents()
            .iter()
            .find(|a| format!("{:?}", a.kind()).to_ascii_lowercase() == approved.provider)
            .context("command_center_provider_unavailable")?;
        let effort = approved
            .effort
            .as_deref()
            .map(|value| {
                agent
                    .supported_efforts(approved.model.as_deref().unwrap_or_default())
                    .iter()
                    .copied()
                    .find(|e| e.value() == value)
                    .context("command_center_effort_unavailable")
            })
            .transpose()?;
        let shim = (request.hooks && agent.kind().supports_deppy_shim())
            .then(crate::agent_shim::shim_dir)
            .flatten()
            .map(|dir| dir.join(agent.kind().id()));
        let spec = crate::agent_launcher::build_launch_spec(
            agent,
            crate::agent_launcher::LaunchOptions {
                model: approved.model.clone().unwrap_or_default(),
                effort,
                yolo: false,
            },
            shim.as_deref(),
        )
        .map_err(|_| anyhow::anyhow!("command_center_selection_unavailable"))?;
        let prepared = prepare_quick_agent_launch(
            db,
            &request.config.workspace_id.0,
            spec,
            request.config.workspace_id.0.clone(),
        )?;
        // Proxy launches need the existing explicit approval setup; they cannot inherit consent.
        anyhow::ensure!(
            prepared.proxy.is_none() && prepared.approval_ticket.is_none(),
            "command_center_proxy_requires_setup"
        );
        let profile = RemoteWorkerProfile {
            id: format!("cc-{}-{index}", request.config.revision.0),
            agent: approved.clone(),
        };
        let command = runtime::RuntimeCommand::SpawnAgent {
            agent_config_id: Some(prepared.agent_config_id),
            cols: 80,
            rows: 24,
            scrollback_lines: request.scrollback,
            command: prepared.command,
            args: prepared.args,
            env_plain: prepared.env_plain,
            env_secrets: prepared.env_secrets,
            waiting_regex: prepared.waiting_regex,
            approval_regex: prepared.approval_regex,
            error_regex: prepared.error_regex,
            done_regex: prepared.done_regex,
            transient_args: prepared.transient_args,
        };
        templates.push((profile, command));
    }
    let profiles = templates
        .iter()
        .map(|(profile, _)| profile.clone())
        .collect();
    let wake = request.wake.clone();
    let journal = Arc::new(
        crate::work_management::remote_execution::AsyncRemoteJournal::new(
            request.data_dir.clone(),
            move || {
                if let Some(wake) = &wake {
                    wake();
                }
            },
        ),
    );
    let host = RemoteWorkerHost::new(
        runtime::remote_worker::RemoteWorkerConfig {
            device_id,
            workspace_id: exported_workspace,
            folder: PathBuf::from(&request.config.folder),
            grant_epoch: request.grant_epoch,
            max_workers: request.config.max_workers,
            profiles,
        },
        Arc::new(Templates(templates)),
        journal,
    )?;
    Ok(host)
}
fn host_profiles(
    config: &work::ProjectConfig,
    agents: &[work::AgentSelection],
) -> Vec<RemoteWorkerProfile> {
    agents
        .iter()
        .enumerate()
        .map(|(index, agent)| RemoteWorkerProfile {
            id: format!("cc-{}-{index}", config.revision.0),
            agent: agent.clone(),
        })
        .collect()
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Record {
    pub operation_id: work::OperationId,
    pub attempt_id: work::AttemptId,
    pub workspace: deppy_core::WorkspaceId,
    pub task_id: Option<work::TaskId>,
    pub config_revision: work::ConfigRevision,
    pub selection: work::AgentSelection,
    pub folder: String,
    pub phase: Phase,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Phase {
    Preparing,
    Inspecting,
    SpawnRequested,
    Spawned {
        session: runtime::SessionId,
        execution_generation: u64,
    },
    Unknown,
    Rejected,
    Completed,
}

pub(super) struct Installed {
    pub revision: work::ConfigRevision,
    pub profiles: Vec<RemoteWorkerProfile>,
}
pub(super) struct Pending {
    pub record: Record,
    pub runtime_instance: u64,
    pub generation: u64,
    pub stage: Stage,
    pub request: Option<runtime::remote_worker::RemoteWorkRequest>,
    pub deadline: std::time::Instant,
    pub owned: Option<wm::task_worktree::OwnedTaskWorktree>,
    pub grant: Option<runtime::remote_worker::AttemptFolderGrant>,
}
#[derive(Clone, Copy)]
pub(super) enum Stage {
    Durable,
    Settings,
    Worktree,
    Grant,
    Inspect(u64),
    Spawn,
    Binding,
    MainSave(u64),
    Input,
    Done,
}
impl Pending {
    pub fn finished(&self) -> bool {
        matches!(self.record.phase, Phase::Completed | Phase::Rejected)
    }
    pub fn preparing(&self) -> bool {
        !self.finished() && !matches!(self.stage, Stage::Input | Stage::Done)
    }
}
pub(super) fn save_record(p: &mut Project, record: &Record) -> Result<(), String> {
    let key = format!("launch:{}", record.operation_id.0);
    let text = serde_json::to_string(record).map_err(|_| "실행 기록을 저장하지 못했습니다.")?;
    let uncertain = matches!(record.phase, Phase::Unknown | Phase::SpawnRequested);
    if p.snapshot
        .drafts
        .iter()
        .any(|d| d.target_key == key && d.text == text && d.delivery_uncertain == uncertain)
    {
        return Ok(());
    }
    set_draft(p, &key, &text, uncertain)
}

fn reject_worktree_preparation(
    pending: &mut Pending,
    error: wm::task_worktree::TaskWorktreeError,
) -> String {
    pending.record.phase = Phase::Rejected;
    pending.stage = Stage::Done;
    format!(
        "전용 워크트리 소유권을 확인하지 못했습니다: {}. 에이전트는 생성하지 않았습니다. 같은 작업 폴더는 자동 재생성하지 않습니다.",
        error.as_str()
    )
}
fn accept_spawn_result(
    pending: &mut Pending,
    workspace: &str,
    runtime_instance: u64,
    identity: &runtime::remote_worker::RemoteWorkIdentity,
    result: &runtime::remote_worker::RemoteWorkResult,
) -> bool {
    if pending.runtime_instance != runtime_instance
        || pending.record.workspace.0 != workspace
        || !pending
            .request
            .as_ref()
            .is_some_and(|request| request.identity == *identity)
        || !matches!(pending.stage, Stage::Spawn)
    {
        return false;
    }
    match result {
        runtime::remote_worker::RemoteWorkResult::Launched {
            session,
            execution_generation,
        } if *execution_generation > 0 => {
            pending.record.phase = Phase::Spawned {
                session: *session,
                execution_generation: *execution_generation,
            };
            pending.stage = Stage::Binding;
        }
        runtime::remote_worker::RemoteWorkResult::Rejected { .. } => {
            pending.record.phase = Phase::Rejected;
            pending.stage = Stage::Done;
        }
        _ => {
            pending.record.phase = Phase::Unknown;
            pending.stage = Stage::Done;
        }
    }
    true
}
fn worktree_job_matches(record: &Record, job: &wm::task_worktree::EnsureTaskWorktree) -> bool {
    record.workspace == job.workspace_id
        && record.task_id.as_ref() == Some(&job.task_id)
        && record.attempt_id == job.attempt_id
        && record.operation_id == job.operation_id
        && record.config_revision == job.config_revision
        && same_folder(&record.folder, &job.project_folder)
}
pub(super) fn workspace_uncertain(launches: &BTreeMap<String, Pending>, workspace: &str) -> bool {
    launches.values().any(|pending| {
        pending.record.workspace.0 == workspace && pending.record.phase == Phase::Unknown
    })
}
pub(super) fn restore_records(host: &mut CommandCenterHost, workspace: &str) {
    let records: Vec<_> = host.projects[workspace]
        .snapshot
        .drafts
        .iter()
        .filter(|d| d.target_key.starts_with("launch:"))
        .map(|d| serde_json::from_str::<Record>(&d.text))
        .collect();
    if records.iter().any(Result::is_err) || records.len() > work::WORK_DRAFT_MAX_COUNT {
        host.projects.get_mut(workspace).unwrap().storage_blocked = true;
        host.status =
            "이전 실행 기록을 검증하지 못했습니다. 프로젝트 저장 상태를 복구해 주세요.".into();
        return;
    }
    let records = records.into_iter().filter_map(Result::ok);
    for mut record in records.take(work::WORK_DRAFT_MAX_COUNT) {
        if record.workspace.0 != workspace || host.launches.contains_key(&record.operation_id.0) {
            continue;
        }
        if !matches!(record.phase, Phase::Completed | Phase::Rejected) {
            record.phase = Phase::Unknown;
        }
        let id = record.operation_id.0.clone();
        host.launches.insert(
            id.clone(),
            Pending {
                record,
                runtime_instance: 0,
                generation: 0,
                stage: Stage::Done,
                request: None,
                owned: None,
                grant: None,
                deadline: std::time::Instant::now(),
            },
        );
        if host.launches[&id].record.phase == Phase::Unknown {
            host.status =
                "이전 실행의 관측이 끊겼습니다. 자동으로 새 워커를 생성하거나 재전송하지 않습니다."
                    .into();
        }
    }
}
impl App {
    pub(super) fn begin_command_center_launch(
        &mut self,
        host: &mut CommandCenterHost,
        id: &str,
        workspace: &str,
        task_id: Option<&str>,
    ) -> Result<(), String> {
        if host.flight.is_some() {
            return Err("상태 저장을 마친 뒤 실행해 주세요.".into());
        }
        if remote::workspace_preparing(host, workspace) {
            return Err("이 프로젝트의 다른 실행 준비를 기다려 주세요.".into());
        }
        if remote::workspace_unknown(host, workspace) {
            return Err("관측하지 못한 이전 원격 실행을 먼저 복구해 주세요.".into());
        }
        if host.launches.len() >= work::WORK_DRAFT_MAX_COUNT {
            return Err(
                "저장된 실행 기록 한도입니다. 실행 기록을 보존한 채 새 요청을 거부했습니다.".into(),
            );
        }
        if host.launches.values().filter(|p| p.preparing()).count() >= 32 {
            return Err(
                "동시에 준비할 수 있는 새 실행은 32개입니다. 현재 응답을 기다려 주세요.".into(),
            );
        }
        let runtime = self
            .active
            .id
            .eq(workspace)
            .then_some(&self.active)
            .or_else(|| self.warm.get(workspace))
            .ok_or("프로젝트 실행 환경을 먼저 열어 주세요.")?;
        let p = host
            .projects
            .get_mut(workspace)
            .ok_or("프로젝트가 없습니다.")?;
        if !p.loaded || p.storage_blocked {
            return Err("프로젝트 저장 상태를 먼저 확인해 주세요.".into());
        }
        if workspace_uncertain(&host.launches, workspace) {
            return Err(
                "확인되지 않은 이전 실행이 있습니다. 실행 상태를 먼저 복구해 주세요.".into(),
            );
        }
        let policy = p.policy();
        policy
            .validate_config(&p.snapshot.config)
            .map_err(|e| e.to_string())?;
        if p.snapshot
            .drafts
            .iter()
            .any(|d| d.target_key == format!("launch:{id}"))
        {
            return Err("이미 저장된 실행 ID입니다. 자동 재전송하지 않았습니다.".into());
        }
        let task = task_id
            .map(|id| {
                p.snapshot
                    .tasks
                    .iter()
                    .find(|t| t.id.0 == id)
                    .cloned()
                    .ok_or("작업이 없습니다.")
            })
            .transpose()?;
        let selection = if let Some(task) = &task {
            let ready = p.ready_tasks()?;
            if !ready.contains(&task.id) {
                return Err("의존 작업·실행 용량·수정 범위를 먼저 확인해 주세요.".into());
            }
            task.assigned_agent.clone()
        } else {
            p.snapshot.config.main_agent.clone()
        };
        if host.launches.values().any(|launch| {
            !launch.finished()
                && launch.record.workspace.0 == workspace
                && launch.record.task_id == task.as_ref().map(|t| t.id.clone())
        }) {
            return Err(
                "이 대상의 실행 확인을 기다리고 있습니다. 다시 시작하지 않았습니다.".into(),
            );
        }
        let reservations = host
            .launches
            .values()
            .filter(|launch| launch.record.workspace.0 == workspace && launch.preparing())
            .count();
        if reservations >= p.snapshot.config.max_workers as usize {
            return Err("자동 실행 준비 용량에 도달했습니다.".into());
        }
        let record = Record {
            operation_id: work::OperationId::from_string(id.into()).map_err(|e| e.to_string())?,
            attempt_id: work::AttemptId::new(),
            workspace: p.snapshot.config.workspace_id.clone(),
            task_id: task.map(|t| t.id),
            config_revision: p.snapshot.config.revision,
            selection,
            folder: p.snapshot.config.folder.clone(),
            phase: Phase::Preparing,
        };
        save_record(p, &record)?;
        let generation = p.generation;
        host.save(workspace, None, None)?;
        host.launches.insert(
            id.into(),
            Pending {
                record,
                runtime_instance: runtime.runtime_instance,
                generation,
                stage: Stage::Durable,
                request: None,
                owned: None,
                grant: None,
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(45),
            },
        );
        host.status =
            "실행 의도를 저장한 뒤 정확한 공급자 설정으로 새 에이전트를 준비합니다.".into();
        host.publication_dirty = true;
        self.egui_ctx
            .request_repaint_after(std::time::Duration::from_secs(45));
        Ok(())
    }
    pub(in crate::app) fn command_center_settings_result(&mut self, result: SettingsResult) {
        let Some(mut host) = self.command_center.take() else {
            return;
        };
        match result {
            SettingsResult::ServerCopy {
                operation_id,
                runtime_instance,
                grant,
                token,
            } => {
                self.command_center = Some(host);
                self.command_center_server_copy_result(
                    operation_id,
                    runtime_instance,
                    grant,
                    token,
                );
                return;
            }
            SettingsResult::ServerConfigured {
                operation_id,
                runtime_instance,
                result,
            } => {
                self.command_center = Some(host);
                self.command_center_server_result(operation_id, runtime_instance, result);
                return;
            }
            SettingsResult::NativeIdentity {
                operation_id,
                workspace,
                session,
                runtime_instance,
                runtime_token,
                native,
            } => {
                self.command_center = Some(host);
                self.command_center_native_identity_result(
                    operation_id,
                    workspace,
                    session,
                    runtime_instance,
                    runtime_token,
                    native,
                );
                return;
            }
            SettingsResult::Catalog {
                workspace,
                folder,
                revision,
                result,
            } => {
                if let Some(p) = host.projects.get_mut(&workspace).filter(|p| {
                    p.snapshot.config.revision == revision
                        && same_folder(&p.snapshot.config.folder, &folder)
                }) {
                    match result {
                        Ok(catalog) => {
                            if catalog.root.is_some() {
                                p.worktree_root = catalog.root.clone();
                            }
                            p.catalog = Some(catalog);
                            if p.request.is_none()
                                && let Some(goal) = p
                                    .snapshot
                                    .goals
                                    .iter()
                                    .rev()
                                    .find(|g| g.state == work::WorkState::PlanReview)
                            {
                                p.request = wm::plan::PlannerRequest::new(
                                    goal,
                                    &p.snapshot.config,
                                    &p.policy(),
                                )
                                .ok();
                            }
                        }
                        Err(error) => host.status = error,
                    };
                    host.publication_dirty = true;
                }
            }
            SettingsResult::RootPrepared {
                operation_id,
                workspace,
                folder,
                revision,
                result,
            } => {
                let continuation = host.pending_configs.remove(&workspace);
                if let Some((id, config)) = continuation.filter(|(id, _)| id == &operation_id) {
                    let current = host.projects.get_mut(&workspace).filter(|p| {
                        p.snapshot.config.revision == revision
                            && same_folder(&p.snapshot.config.folder, &folder)
                    });
                    match (current, result) {
                        (Some(p), Ok(proof)) if same_folder(proof.project_folder(), &folder) => {
                            p.worktree_root = Some(proof);
                            host.pending.push_back((
                                id,
                                cc::Intent::SaveProject {
                                    workspace_id: workspace,
                                    config,
                                },
                            ));
                        }
                        (_, Err(error)) => host.reject(&id, error),
                        _ => host.reject(&id, "워크트리 준비 중 프로젝트 설정이 바뀌었습니다."),
                    }
                }
            }
            SettingsResult::FolderGranted {
                operation_id,
                runtime_instance,
                attempt_id,
                result,
            } => {
                if let Some(pending) = host.launches.get_mut(&operation_id).filter(|p| {
                    p.runtime_instance == runtime_instance
                        && p.record.attempt_id == attempt_id
                        && matches!(p.stage, Stage::Grant)
                }) {
                    match result {
                        Ok(grant)
                            if grant.workspace_id == pending.record.workspace
                                && grant.attempt_id == attempt_id
                                && pending
                                    .owned
                                    .as_ref()
                                    .is_some_and(|o| same_folder(&o.path, &grant.folder)) =>
                        {
                            pending.grant = Some(grant);
                            pending.stage = Stage::Durable;
                        }
                        _ => {
                            pending.record.phase = Phase::Rejected;
                            pending.stage = Stage::Done;
                            host.reject(
                                &operation_id,
                                "전용 실행 폴더의 현재 승인 증거를 확인하지 못했습니다.",
                            );
                        }
                    }
                }
            }
            SettingsResult::Installed {
                operation_id,
                runtime_instance,
                config_revision,
                result,
            } => {
                if let Some(pending) = host.launches.get_mut(&operation_id).filter(|l| {
                    l.runtime_instance == runtime_instance
                        && l.record.config_revision == config_revision
                        && matches!(l.stage, Stage::Settings)
                }) {
                    match result {
                        Ok(profiles) => {
                            host.installed.insert(
                                runtime_instance,
                                Installed {
                                    revision: config_revision,
                                    profiles,
                                },
                            );
                            pending.stage = Stage::Durable;
                        }
                        Err(error) => {
                            pending.record.phase = Phase::Rejected;
                            pending.stage = Stage::Done;
                            host.reject(&operation_id, error);
                        }
                    }
                }
            }
        }
        self.command_center = Some(host);
        self.egui_ctx.request_repaint();
    }
    pub(super) fn pump_command_center_launches(&mut self, host: &mut CommandCenterHost) {
        if let Some(result) = host.worktrees.try_recv() {
            let id = result.job.operation_id.0.clone();
            if let Some(pending) = host.launches.get_mut(&id).filter(|p| {
                matches!(p.stage, Stage::Worktree) && worktree_job_matches(&p.record, &result.job)
            }) {
                match result.result {
                    Ok(owned)
                        if pending.record.task_id.as_ref() == Some(&owned.owner)
                            && host
                                .projects
                                .get(&pending.record.workspace.0)
                                .and_then(|p| p.worktree_root.as_ref())
                                .is_some_and(|root| root.root() == owned.root) =>
                    {
                        pending.record.folder = owned.path.clone();
                        pending.owned = Some(owned);
                        pending.stage = Stage::Durable;
                    }
                    Err(error) => {
                        let message = reject_worktree_preparation(pending, error);
                        host.reject(&id, message);
                    }
                    _ => {
                        pending.record.phase = Phase::Rejected;
                        pending.stage = Stage::Done;
                        host.finish(
                            &id,
                            cc::ReceiptStatus::Rejected,
                            "전용 워크트리의 소유 작업·루트가 바뀌었습니다.",
                        );
                    }
                }
            }
        }
        // A catalog is loaded once on the existing lazy settings worker, only for the open project.
        if let Some(p) = host
            .projects
            .get_mut(&self.active.id)
            .filter(|p| p.loaded && !p.catalog_requested)
            && self.queue_global_settings_action(
                &self.active.id.clone(),
                SettingsJobAction::CommandCenter(SettingsRequest::Catalog {
                    workspace: self.active.id.clone(),
                    folder: p.snapshot.config.folder.clone(),
                    revision: p.snapshot.config.revision,
                    prepare_root: p.snapshot.config.use_worktrees,
                }),
            )
        {
            p.catalog_requested = true;
        }
        let ids: Vec<_> = host.launches.keys().cloned().collect();
        for id in ids {
            let Some(mut pending) = host.launches.remove(&id) else {
                continue;
            };
            if pending.finished() || matches!(pending.stage, Stage::Done | Stage::Input) {
                host.launches.insert(id, pending);
                continue;
            }
            let workspace = pending.record.workspace.0.clone();
            let current = host.projects.get(&workspace).is_some_and(|p| {
                !p.storage_blocked && p.snapshot.config.revision == pending.record.config_revision
            });
            let authorized = host
                .remote_permits
                .get(&id)
                .is_none_or(|permit| permit.is_current());
            if !current || !authorized || std::time::Instant::now() >= pending.deadline {
                let unknown = matches!(
                    pending.stage,
                    Stage::Spawn | Stage::Binding | Stage::MainSave(_)
                );
                pending.record.phase = if unknown {
                    Phase::Unknown
                } else {
                    Phase::Rejected
                };
                pending.stage = Stage::Done;
                host.finish(
                    &id,
                    if unknown {
                        cc::ReceiptStatus::Uncertain
                    } else {
                        cc::ReceiptStatus::Rejected
                    },
                    "실행의 현재 권한·설정·응답을 확인하지 못했습니다. 자동 재실행하지 않습니다.",
                );
            } else if matches!(pending.stage, Stage::Durable) && host.flight.is_none() {
                let p = &host.projects[&workspace];
                if p.saved_generation >= pending.generation {
                    let runtime = self
                        .active
                        .id
                        .eq(&workspace)
                        .then_some(&self.active)
                        .or_else(|| self.warm.get(&workspace))
                        .filter(|r| r.runtime_instance == pending.runtime_instance);
                    if let Some(runtime) = runtime {
                        if host
                            .installed
                            .get(&runtime.runtime_instance)
                            .is_some_and(|i| i.revision == pending.record.config_revision)
                        {
                            if p.snapshot.config.use_worktrees
                                && pending.record.task_id.is_some()
                                && pending.owned.is_none()
                            {
                                let job = wm::task_worktree::EnsureTaskWorktree {
                                    workspace_id: pending.record.workspace.clone(),
                                    task_id: pending.record.task_id.clone().unwrap(),
                                    attempt_id: pending.record.attempt_id.clone(),
                                    operation_id: pending.record.operation_id.clone(),
                                    project_folder: p.snapshot.config.folder.clone(),
                                    config_revision: pending.record.config_revision,
                                };
                                match host.worktrees.try_request(job) {
                                    Ok(()) => pending.stage = Stage::Worktree,
                                    Err(error) => {
                                        let reason=match &error {wm::task_worktree::TaskWorktreeSubmitError::Invalid {error,..}=>error.as_str(),wm::task_worktree::TaskWorktreeSubmitError::Full(_)=>"worktree_queue_full",wm::task_worktree::TaskWorktreeSubmitError::Unavailable(_)=>"worktree_worker_unavailable"};
                                        let job = error.into_job();
                                        debug_assert_eq!(
                                            job.operation_id,
                                            pending.record.operation_id
                                        );
                                        pending.record.phase = Phase::Rejected;
                                        pending.stage = Stage::Done;
                                        host.reject(&id,format!("워크트리 준비 요청을 접수하지 못했습니다: {reason}. 실행은 시작하지 않았습니다."));
                                    }
                                }
                            } else if let Some(owned) =
                                pending.owned.as_ref().filter(|_| pending.grant.is_none())
                            {
                                let request = SettingsRequest::GrantOwned {
                                    operation_id: id.clone(),
                                    runtime_instance: pending.runtime_instance,
                                    control: runtime.runtime.remote_worker_control(),
                                    attempt_id: pending.record.attempt_id.clone(),
                                    owned: owned.clone(),
                                };
                                if self.queue_global_settings_action(
                                    &workspace,
                                    SettingsJobAction::CommandCenter(request),
                                ) {
                                    pending.stage = Stage::Grant;
                                }
                            } else {
                                host.next_token = host.next_token.saturating_add(1);
                                let token = host.next_token;
                                if runtime
                                    .runtime
                                    .send_command(runtime::RuntimeCommand::InspectRemoteWorker {
                                        request_id: token,
                                    })
                                    .is_ok()
                                {
                                    pending.stage = Stage::Inspect(token);
                                    pending.record.phase = Phase::Inspecting;
                                } else {
                                    pending.record.phase = Phase::Rejected;
                                    pending.stage = Stage::Done;
                                    host.reject(&id, "실행 환경에 준비 요청을 보내지 못했습니다.");
                                }
                            }
                        } else {
                            let mut agents = p.allowed_agents.clone();
                            if !agents.contains(&p.snapshot.config.main_agent) {
                                agents.push(p.snapshot.config.main_agent.clone());
                            }
                            host.next_grant_epoch = host.next_grant_epoch.saturating_add(1).max(1);
                            let request = InstallRequest {
                                operation_id: id.clone(),
                                runtime_instance: runtime.runtime_instance,
                                config: p.snapshot.config.clone(),
                                agents,
                                control: runtime.runtime.remote_worker_control(),
                                wake: runtime.runtime.remote_worker_wake(),
                                data_dir: self.data_dir().to_path_buf(),
                                grant_epoch: host.next_grant_epoch,
                                scrollback: self.config.terminal.scrollback_lines as usize,
                                hooks: self.config.ui.agent_status_hooks,
                            };
                            if self.queue_global_settings_action(
                                &workspace,
                                SettingsJobAction::CommandCenter(SettingsRequest::Install(
                                    Box::new(request),
                                )),
                            ) {
                                pending.stage = Stage::Settings;
                            }
                        }
                    } else {
                        pending.record.phase = Phase::Rejected;
                        pending.stage = Stage::Done;
                        host.reject(&id, "프로젝트 실행 세대가 바뀌었습니다.");
                    }
                }
            } else if matches!(pending.stage, Stage::Binding) && host.flight.is_none() {
                if let (
                    Phase::Spawned {
                        session,
                        execution_generation,
                    },
                    Some(request),
                ) = (&pending.record.phase, &pending.request)
                {
                    let found = host.live.iter().position(|live| {
                        live.capture.workspace == workspace
                            && live.capture.prompt.as_ref().is_some_and(|p| {
                                p.runtime_instance == pending.runtime_instance
                                    && p.session == *session
                            })
                            && live
                                .agent
                                .as_ref()
                                .is_some_and(|a| a.provider == pending.record.selection.provider)
                    });
                    if let Some(index) = found {
                        let binding = work::RuntimeBinding {
                            incarnation: request.identity.incarnation.clone(),
                            session_id: *session,
                            execution_generation: *execution_generation,
                        };
                        let live = &mut host.live[index];
                        if host.execution_bindings.len() >= 256 {
                            pending.record.phase = Phase::Unknown;
                            pending.stage = Stage::Done;
                            host.finish(
                                &id,
                                cc::ReceiptStatus::Uncertain,
                                "실행 바인딩 기록 한도입니다. 생성된 세션에 입력하지 않았습니다.",
                            );
                        } else {
                            host.execution_bindings.push((
                                live.capture.clone(),
                                binding.clone(),
                                pending.record.selection.clone(),
                            ));
                            host.observations
                                .register_first_input(live.capture.clone(), binding.clone());
                            live.runtime_binding = Some(binding.clone());
                            live.agent = Some(pending.record.selection.clone());
                            let live = live.clone();
                            let mut target = host.target(&live);
                            target.task_id = pending.record.task_id.as_ref().map(|id| id.0.clone());
                            let before = host.projects.get(&workspace).cloned();
                            let result = (|| -> Result<(), String> {
                                let p = host.projects.get_mut(&workspace).unwrap();
                                if let Some(task_id) = &pending.record.task_id {
                                    let dispatch = p
                                        .prepare_local(
                                            wm::dispatch::DispatchRequest {
                                                task_id: task_id.clone(),
                                                operation_id: pending.record.operation_id.clone(),
                                                folder: wm::dispatch::DispatchFolder {
                                                    path: pending.record.folder.clone(),
                                                    worktree_owner: pending
                                                        .owned
                                                        .as_ref()
                                                        .map(|owned| owned.owner.clone()),
                                                },
                                                persistent_binding: persistent(&live),
                                                runtime_binding: binding,
                                            },
                                            pending.record.attempt_id.clone(),
                                        )
                                        .map_err(|e| e.to_string())?;
                                    let wm::dispatch::PrepareOutcome::Ready(prepared) = dispatch
                                    else {
                                        return Err("이미 처리된 실행 요청입니다. 다시 입력하지 않았습니다.".into());
                                    };
                                    p.set_state(prepared.next_state);
                                    p.snapshot.outbox.push(stored::DispatchOutbox {
                                        attempt_id: prepared.assignment.attempt.id.clone(),
                                        operation_id: prepared
                                            .assignment
                                            .attempt
                                            .operation_id
                                            .clone(),
                                        state: stored::DispatchOutboxState::Pending,
                                    });
                                    p.dirty();
                                    let after = AfterSave {
                                        id: id.clone(),
                                        input: Input {
                                            target,
                                            draft: String::new(),
                                            dispatch: Some(prepared.assignment.clone()),
                                        },
                                        prompt: prepared.assignment.prompt,
                                        config_revision: p.snapshot.config.revision,
                                    };
                                    host.save(&workspace, Some(after), None)?;
                                    pending.stage = Stage::Input;
                                } else {
                                    p.snapshot.main_binding = Some(persistent(&live).ok_or(
                                        "생성된 메인 세션의 영구 바인딩을 확인할 수 없습니다.",
                                    )?);
                                    p.dirty();
                                    let generation = p.generation;
                                    host.save(&workspace, None, None)?;
                                    pending.stage = Stage::MainSave(generation);
                                }
                                Ok(())
                            })();
                            if let Err(error) = result {
                                if let Some(before) = before {
                                    host.projects.insert(workspace.clone(), before);
                                }
                                pending.record.phase = Phase::Unknown;
                                pending.stage = Stage::Done;
                                host.finish(
                                    &id,
                                    cc::ReceiptStatus::Uncertain,
                                    format!(
                                        "세션은 생성되었으나 작업 입력을 보내지 않았습니다. {error}"
                                    ),
                                );
                            }
                        }
                    }
                }
            } else if let Stage::MainSave(generation) = pending.stage
                && host.projects[&workspace].saved_generation >= generation
            {
                pending.record.phase = Phase::Completed;
                pending.stage = Stage::Done;
                host.finish(
                    &id,
                    cc::ReceiptStatus::Acknowledged,
                    "새 메인 세션을 만들고 정확한 바인딩을 저장했습니다.",
                );
            }
            if let Some(p) = host.projects.get_mut(&workspace) {
                let _ = save_record(p, &pending.record);
            }
            host.launches.insert(id, pending);
        }
    }
    pub(in crate::app) fn observe_command_center_runtime(
        &mut self,
        workspace: &str,
        runtime_instance: u64,
        events: &[runtime::RuntimeEvent],
    ) {
        self.observe_command_center_turn_events(runtime_instance, events);
        let Some(mut host) = self.command_center.take() else {
            return;
        };
        for event in events {
            match event {
                runtime::RuntimeEvent::RemoteWorkerObserved {
                    request_id,
                    capabilities,
                } => {
                    let id = host
                        .launches
                        .iter()
                        .find(|(_, p)| {
                            p.runtime_instance == runtime_instance
                                && p.record.workspace.0 == workspace
                                && matches!(p.stage,Stage::Inspect(id) if id==*request_id)
                        })
                        .map(|(id, _)| id.clone());
                    if let Some(id) = id {
                        let pending = host.launches.get_mut(&id).unwrap();
                        let current = host.projects.get(workspace).is_some_and(|p| {
                            p.snapshot.config.revision == pending.record.config_revision
                                && !p.storage_blocked
                        });
                        let authorized = host
                            .remote_permits
                            .get(&id)
                            .is_none_or(|permit| permit.is_current());
                        if let Some(cap) = capabilities.as_ref().filter(|cap| {
                            current
                                && authorized
                                && cap.workspace_id.0 == workspace
                                && host.projects.get(workspace).is_some_and(|p| {
                                    same_folder(&cap.folder, &p.snapshot.config.folder)
                                })
                                && host.installed.get(&runtime_instance).is_some_and(|i| {
                                    i.revision == pending.record.config_revision
                                        && i.profiles == cap.profiles
                                })
                                && pending.grant.as_ref().is_none_or(|g| {
                                    g.device_id == cap.device_id
                                        && g.workspace_id == cap.workspace_id
                                        && g.attempt_id == pending.record.attempt_id
                                        && g.grant_epoch == cap.grant_epoch
                                        && same_folder(&g.folder, &pending.record.folder)
                                })
                                && cap
                                    .profiles
                                    .iter()
                                    .any(|p| p.agent == pending.record.selection)
                        }) {
                            let profile = cap
                                .profiles
                                .iter()
                                .find(|p| p.agent == pending.record.selection)
                                .unwrap();
                            let request = runtime::remote_worker::RemoteWorkRequest {
                                identity: runtime::remote_worker::RemoteWorkIdentity {
                                    device_id: cap.device_id.clone(),
                                    workspace_id: pending.record.workspace.clone(),
                                    attempt_id: pending.record.attempt_id.clone(),
                                    operation_id: pending.record.operation_id.clone(),
                                    incarnation: cap.incarnation.clone(),
                                    grant_epoch: cap.grant_epoch,
                                    lease_epoch: cap.lease_epoch,
                                },
                                folder: pending
                                    .grant
                                    .as_ref()
                                    .map(|g| g.folder.clone())
                                    .unwrap_or_else(|| cap.folder.clone()),
                                folder_anchor: pending
                                    .grant
                                    .as_ref()
                                    .map(|g| g.folder_anchor.clone())
                                    .unwrap_or_else(|| cap.folder_anchor.clone()),
                                profile_id: profile.id.clone(),
                                initial_prompt: None,
                            };
                            pending.request = Some(request.clone());
                            pending.stage = Stage::Spawn;
                            pending.record.phase = Phase::SpawnRequested;
                            let sent = self
                                .runtime_by_instance_mut(runtime_instance)
                                .filter(|r| r.id == workspace)
                                .is_some_and(|r| {
                                    r.runtime
                                        .send_command(runtime::RuntimeCommand::SpawnRemoteWork {
                                            request,
                                        })
                                        .is_ok()
                                });
                            if !sent {
                                pending.record.phase = Phase::Rejected;
                                pending.stage = Stage::Done;
                                host.reject(&id, "실행 요청을 런타임에 전달하지 못했습니다.");
                            }
                        } else {
                            pending.record.phase = Phase::Rejected;
                            pending.stage = Stage::Done;
                            host.reject(&id, "현재 실행 권한·프로필·폴더를 확인하지 못했습니다.");
                        }
                    }
                }
                runtime::RuntimeEvent::RemoteWorkResolved { identity, result } => {
                    let id = identity.operation_id.0.clone();
                    if let Some(pending) = host.launches.get_mut(&id)
                        && accept_spawn_result(
                            pending,
                            workspace,
                            runtime_instance,
                            identity,
                            result,
                        )
                    {
                        let phase = pending.record.phase;
                        if phase == Phase::Rejected {
                            host.reject(
                                &id,
                                "런타임이 실행을 거부했습니다. 세션은 시작하지 않았습니다.",
                            );
                        } else if phase == Phase::Unknown {
                            host.finish(
                                &id,
                                cc::ReceiptStatus::Uncertain,
                                "세션 생성 여부를 확인하지 못했습니다. 자동 재실행하지 않습니다.",
                            );
                        }
                    }
                }
                runtime::RuntimeEvent::SessionExited { session, .. } => {
                    let bindings: Vec<_> = host
                        .execution_bindings
                        .iter()
                        .filter(|(capture, _, _)| {
                            capture.workspace == workspace
                                && capture.prompt.as_ref().is_some_and(|p| {
                                    p.runtime_instance == runtime_instance && p.session == *session
                                })
                        })
                        .map(|(_, binding, _)| binding.clone())
                        .collect();
                    if let Some(p) = host.projects.get_mut(workspace) {
                        let attempts: Vec<_> = p
                            .snapshot
                            .attempts
                            .iter()
                            .filter(|a| {
                                a.runtime_binding
                                    .as_ref()
                                    .is_some_and(|b| bindings.contains(b))
                                    && matches!(
                                        a.state,
                                        work::WorkState::Queued
                                            | work::WorkState::Running
                                            | work::WorkState::WaitingApproval
                                            | work::WorkState::WaitingInput
                                    )
                            })
                            .cloned()
                            .collect();
                        for attempt in attempts {
                            let Some(binding) = attempt.runtime_binding else {
                                continue;
                            };
                            let mut state = p.state();
                            let event = work::WorkEvent {
                                id: work::EventId::new(),
                                workspace_id: attempt.workspace_id,
                                task_id: attempt.task_id,
                                attempt_id: attempt.id,
                                runtime_binding: binding,
                                sequence: attempt.last_event_sequence.saturating_add(1),
                                kind: work::WorkEventKind::ProcessExited,
                            };
                            if matches!(state.apply_event(event), Ok(wm::ApplyOutcome::Applied)) {
                                p.set_state(state);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        self.command_center = Some(host);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pending() -> Pending {
        let workspace = WorkspaceId("workspace-a".into());
        let operation_id = work::OperationId::new();
        let attempt_id = work::AttemptId::new();
        let request = runtime::remote_worker::RemoteWorkRequest {
            identity: runtime::remote_worker::RemoteWorkIdentity {
                device_id: "actual-device".into(),
                workspace_id: workspace.clone(),
                attempt_id: attempt_id.clone(),
                operation_id: operation_id.clone(),
                incarnation: work::RuntimeIncarnationId::new(),
                grant_epoch: 7,
                lease_epoch: 3,
            },
            folder: "/project".into(),
            folder_anchor: "pinned-anchor".into(),
            profile_id: "config-1-profile-0".into(),
            initial_prompt: None,
        };
        Pending {
            record: Record {
                operation_id,
                attempt_id,
                workspace,
                task_id: Some(work::TaskId::new()),
                config_revision: work::ConfigRevision(1),
                selection: work::AgentSelection {
                    provider: "codex".into(),
                    model: Some("actual-model".into()),
                    effort: Some("xhigh".into()),
                },
                folder: "/project".into(),
                phase: Phase::SpawnRequested,
            },
            runtime_instance: 19,
            generation: 2,
            stage: Stage::Spawn,
            request: Some(request),
            deadline: std::time::Instant::now(),
            owned: None,
            grant: None,
        }
    }
    #[test]
    fn launch_ack_requires_exact_runtime_operation_attempt_and_grant() {
        let mut p = pending();
        let identity = p.request.as_ref().unwrap().identity.clone();
        let launched = runtime::remote_worker::RemoteWorkResult::Launched {
            session: runtime::SessionId(42),
            execution_generation: 91,
        };
        assert!(!accept_spawn_result(
            &mut p,
            "other-workspace",
            19,
            &identity,
            &launched
        ));
        assert!(!accept_spawn_result(
            &mut p,
            "workspace-a",
            20,
            &identity,
            &launched
        ));
        for changed in 0..4 {
            let mut wrong = identity.clone();
            match changed {
                0 => wrong.attempt_id = work::AttemptId::new(),
                1 => wrong.operation_id = work::OperationId::new(),
                2 => wrong.grant_epoch += 1,
                _ => wrong.incarnation = work::RuntimeIncarnationId::new(),
            };
            assert!(!accept_spawn_result(
                &mut p,
                "workspace-a",
                19,
                &wrong,
                &launched
            ));
        }
        assert_eq!(p.record.phase, Phase::SpawnRequested);
        assert!(accept_spawn_result(
            &mut p,
            "workspace-a",
            19,
            &identity,
            &launched
        ));
        assert_eq!(
            p.record.phase,
            Phase::Spawned {
                session: runtime::SessionId(42),
                execution_generation: 91
            }
        );
        assert!(matches!(p.stage, Stage::Binding));
        assert!(
            !accept_spawn_result(&mut p, "workspace-a", 19, &identity, &launched),
            "duplicate reply cannot start a second binding/input"
        );
    }
    #[test]
    fn zero_generation_or_lost_launch_reply_stays_unknown_and_blocks_new_launches() {
        let mut p = pending();
        let identity = p.request.as_ref().unwrap().identity.clone();
        assert!(accept_spawn_result(
            &mut p,
            "workspace-a",
            19,
            &identity,
            &runtime::remote_worker::RemoteWorkResult::Launched {
                session: runtime::SessionId(42),
                execution_generation: 0
            }
        ));
        assert_eq!(p.record.phase, Phase::Unknown);
        let mut launches = BTreeMap::new();
        launches.insert(identity.operation_id.0, p);
        assert!(workspace_uncertain(&launches, "workspace-a"));
        assert!(!workspace_uncertain(&launches, "workspace-b"));
    }
    #[test]
    fn worktree_failure_never_claims_unknown_cli_or_blocks_independent_work() {
        use wm::task_worktree::TaskWorktreeError as E;
        for error in [
            E::InvalidJob,
            E::InvalidFolder,
            E::OwnershipConflict,
            E::CreationUncertain,
            E::Unavailable,
        ] {
            let mut p = pending();
            p.stage = Stage::Worktree;
            let message = reject_worktree_preparation(&mut p, error);
            assert!(message.contains("에이전트는 생성하지 않았습니다"));
            assert_eq!(p.record.phase, Phase::Rejected);
            let mut launches = BTreeMap::new();
            launches.insert(p.record.operation_id.0.clone(), p);
            assert!(!workspace_uncertain(&launches, "workspace-a"));
        }
    }
    #[test]
    fn worktree_callback_cannot_substitute_task_attempt_config_or_project() {
        let p = pending();
        let job = wm::task_worktree::EnsureTaskWorktree {
            workspace_id: p.record.workspace.clone(),
            task_id: p.record.task_id.clone().unwrap(),
            attempt_id: p.record.attempt_id.clone(),
            operation_id: p.record.operation_id.clone(),
            project_folder: p.record.folder.clone(),
            config_revision: p.record.config_revision,
        };
        assert!(worktree_job_matches(&p.record, &job));
        for changed in 0..4 {
            let mut wrong = job.clone();
            match changed {
                0 => wrong.task_id = work::TaskId::new(),
                1 => wrong.attempt_id = work::AttemptId::new(),
                2 => wrong.config_revision = work::ConfigRevision(2),
                _ => wrong.project_folder = "/other-project".into(),
            };
            assert!(!worktree_job_matches(&p.record, &wrong));
        }
    }
    #[test]
    fn immutable_launch_template_preserves_configuration_and_never_accepts_task_text() {
        let agent = work::AgentSelection {
            provider: "codex".into(),
            model: Some("configured-model".into()),
            effort: Some("xhigh".into()),
        };
        let profile = RemoteWorkerProfile {
            id: "config-7-profile-1".into(),
            agent,
        };
        let templates = Templates(vec![(
            profile.clone(),
            runtime::RuntimeCommand::SpawnAgent {
                agent_config_id: Some("configured-agent".into()),
                cols: 80,
                rows: 24,
                scrollback_lines: 1000,
                command: "/bin/sh".into(),
                args: vec!["--model".into(), "configured-model".into()],
                env_plain: vec![("CONFIG".into(), "configured-env".into())],
                env_secrets: vec![("FIXTURE_SECRET".into(), "fixture-value".into())],
                waiting_regex: None,
                approval_regex: None,
                error_regex: None,
                done_regex: None,
                transient_args: vec![],
            },
        )]);
        assert!(
            templates
                .prepare(&profile, Some("task text must wait for durable attempt"))
                .is_err()
        );
        let mut stale = profile.clone();
        stale.agent.model = Some("other-model".into());
        assert!(templates.prepare(&stale, None).is_err());
        let runtime::RuntimeCommand::SpawnAgent {
            args,
            env_plain,
            env_secrets,
            transient_args,
            ..
        } = templates.prepare(&profile, None).unwrap()
        else {
            panic!("must produce approved SpawnAgent");
        };
        assert_eq!(args, vec!["--model", "configured-model"]);
        assert_eq!(env_plain[0].1, "configured-env");
        assert_eq!(env_secrets.len(), 1);
        assert!(transient_args.is_empty());
    }
}
