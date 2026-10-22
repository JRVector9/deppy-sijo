//! Explicit native grants for the dedicated TLS worker. Listening alone grants no execution.
use super::*;
use runtime::remote_worker::{RemoteWorkerControl, RemoteWorkerProfile};

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Grant {
    pub workspace: String,
    pub exported_workspace: String,
    pub revision: work::ConfigRevision,
    pub folder: String,
    pub agents: Vec<work::AgentSelection>,
    pub epoch: u64,
}
struct Pending {
    id: String,
    grant: Option<Grant>,
    deadline: std::time::Instant,
}
pub(in crate::app) struct ExecutionServer {
    pub control: RemoteWorkerControl,
    pub wake: Option<Arc<dyn Fn() + Send + Sync>>,
    epoch: u64,
    grant: Option<Grant>,
    pending: Option<Pending>,
    error: Option<String>,
    copy_pending: Option<(String, Grant)>,
}
impl ExecutionServer {
    pub fn new(control: RemoteWorkerControl, wake: Option<Arc<dyn Fn() + Send + Sync>>) -> Self {
        Self {
            control,
            wake,
            epoch: 0,
            grant: None,
            pending: None,
            error: None,
            copy_pending: None,
        }
    }
    fn can_request(&self) -> bool {
        self.pending
            .as_ref()
            .is_none_or(|p| std::time::Instant::now() >= p.deadline)
    }
    fn copy_grant(&self) -> Result<&Grant, String> {
        if self.pending.is_some() || self.error.is_some() {
            return Err("실행 허용 변경을 확인하거나 회수한 뒤 다시 복사해 주세요.".into());
        }
        self.grant
            .as_ref()
            .ok_or_else(|| "먼저 이 컴퓨터의 프로젝트 실행을 허용해 주세요.".into())
    }
    fn finish_copy(&mut self, id: &str, grant: &Grant) -> Option<Result<(), String>> {
        if !self
            .copy_pending
            .as_ref()
            .is_some_and(|(pending_id, pending_grant)| pending_id == id && pending_grant == grant)
        {
            return None;
        }
        self.copy_pending = None;
        Some(self.copy_grant().and_then(|current| {
            if current == grant {
                Ok(())
            } else {
                Err("실행 허용이 변경되어 연결 정보를 복사하지 않았습니다.".into())
            }
        }))
    }
    fn finish(&mut self, id: &str, result: Result<Option<Grant>, String>) -> bool {
        if self.pending.as_ref().is_none_or(|p| p.id != id) {
            return false;
        }
        let pending = self.pending.take().unwrap();
        match result {
            Ok(grant)
                if pending
                    .grant
                    .as_ref()
                    .map(|g| (&g.workspace, &g.exported_workspace, g.revision))
                    == grant
                        .as_ref()
                        .map(|g| (&g.workspace, &g.exported_workspace, g.revision)) =>
            {
                self.grant = grant;
                self.error = None;
            }
            Ok(_) => {
                self.error = Some(
                    "기기 실행 응답의 프로젝트가 일치하지 않습니다. 실행 허용을 회수해 주세요."
                        .into(),
                );
            }
            Err(error) => {
                self.error = Some(error);
            }
        }
        true
    }
}

pub(super) fn next_server_grant_epoch(data_dir: &Path) -> anyhow::Result<u64> {
    std::fs::create_dir_all(data_dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(data_dir.join("command-center-server-grant.lock"))?;
    lock.try_lock()
        .map_err(|_| anyhow::anyhow!("server_grant_epoch_busy"))?;
    let path = data_dir.join("command-center-server-grant.epoch");
    let previous = match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            anyhow::ensure!(
                meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= 20,
                "server_grant_epoch_invalid"
            );
            std::fs::read_to_string(&path)?.parse::<u64>()?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error.into()),
    };
    let next = previous
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("server_grant_epoch_exhausted"))?;
    deppy_core::fs::atomic_write(&path, next.to_string().as_bytes())?;
    Ok(next)
}

pub(crate) struct Prepared {
    pub profiles: Vec<RemoteWorkerProfile>,
    pub folder: String,
    pub epoch: u64,
}

impl App {
    pub(super) fn command_center_server_snapshot(
        &self,
        host: &CommandCenterHost,
    ) -> cc::ServerExecutionSetup {
        let Some(remote) = &self.remote else {
            return cc::ServerExecutionSetup::default();
        };
        let state = &remote.execution;
        let p = host.projects.get(&self.active.id);
        let ready =
            p.is_some_and(|p| p.loaded && !p.storage_blocked && p.generation == p.saved_generation);
        let pending = !state.can_request();
        let current = state.grant.as_ref();
        cc::ServerExecutionSetup {
            running: true,
            busy: pending,
            endpoint: remote.server.local_addr().to_string(),
            fingerprint: remote.fingerprint.clone(),
            workspace_id: current.map(|g| g.workspace.clone()),
            exported_workspace_id: current.map(|g| g.exported_workspace.clone()),
            folder: current.map(|g| g.folder.clone()).unwrap_or_default(),
            requested_agents: p
                .map(|p| {
                    let mut agents = p.allowed_agents.clone();
                    if !agents.contains(&p.snapshot.config.main_agent) {
                        agents.push(p.snapshot.config.main_agent.clone());
                    }
                    agents
                        .into_iter()
                        .map(|a| {
                            format!(
                                "{} · {} · {}",
                                a.provider,
                                a.model.as_deref().unwrap_or("기본"),
                                a.effort.as_deref().unwrap_or("기본")
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
            agents: current
                .map(|g| {
                    g.agents
                        .iter()
                        .map(|a| {
                            format!(
                                "{} · {} · {}",
                                a.provider,
                                a.model.as_deref().unwrap_or("기본"),
                                a.effort.as_deref().unwrap_or("기본")
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
            status: state.error.clone().unwrap_or_else(|| {
                if pending {
                    "실행 허용 변경의 실제 응답을 기다립니다.".into()
                } else if state.pending.is_some() {
                    "설정 응답을 확인하지 못했습니다. 실행 허용 회수 후 다시 연결해 주세요.".into()
                } else if current.is_some() {
                    "이 컴퓨터의 선택한 프로젝트 실행을 허용했습니다.".into()
                } else {
                    "TLS 연결만 켜져 있습니다. 프로젝트 실행은 아직 허용하지 않았습니다.".into()
                }
            }),
            allow: if ready && !pending && state.pending.is_none() {
                yes()
            } else {
                no("프로젝트 설정 저장과 현재 실행 설정 응답을 먼저 확인해 주세요.")
            },
            revoke: if !pending && (current.is_some() || state.pending.is_some()) {
                yes()
            } else {
                no("실행 허용이 없거나 변경 중입니다.")
            },
            copy: if state.copy_grant().is_ok() && state.copy_pending.is_none() {
                yes()
            } else {
                no("알려진 실행 허용을 확인한 뒤 복사해 주세요.")
            },
        }
    }
    pub(super) fn allow_command_center_server(
        &mut self,
        host: &mut CommandCenterHost,
        id: &str,
        workspace: &str,
        revision: &str,
        exported: &str,
    ) -> Result<(), String> {
        if workspace != self.active.id
            || exported.is_empty()
            || exported.len() > 128
            || exported.chars().any(char::is_control)
        {
            return Err("현재 프로젝트와 연결할 프로젝트 ID를 확인해 주세요.".into());
        }
        let p = host
            .projects
            .get(workspace)
            .filter(|p| {
                p.loaded
                    && !p.storage_blocked
                    && p.generation == p.saved_generation
                    && p.snapshot.config.revision.0.to_string() == revision
            })
            .ok_or("먼저 현재 프로젝트 설정을 저장해 주세요.")?;
        let mut agents = p.allowed_agents.clone();
        if !agents.contains(&p.snapshot.config.main_agent) {
            agents.push(p.snapshot.config.main_agent.clone());
        }
        if agents.is_empty() || agents.len() > 32 {
            return Err("허용할 에이전트 프로필을 1~32개로 설정해 주세요.".into());
        }
        let remote = self
            .remote
            .as_ref()
            .ok_or("먼저 TLS 연결 설정에서 서버를 켜 주세요.")?;
        if !remote.execution.can_request() || remote.execution.pending.is_some() {
            return Err(
                "현재 기기 실행 설정의 응답을 기다리거나 실행 허용을 회수해 주세요.".into(),
            );
        }
        let epoch = remote
            .execution
            .epoch
            .checked_add(1)
            .ok_or("실행 허용 번호를 더 만들 수 없습니다.")?;
        let request = launch::InstallRequest {
            main_tools: None,
            operation_id: id.into(),
            runtime_instance: remote.policy.runtime_instance,
            config: p.snapshot.config.clone(),
            agents: agents.clone(),
            control: remote.execution.control.clone(),
            wake: remote.execution.wake.clone(),
            data_dir: self.data_dir().to_path_buf(),
            grant_epoch: epoch,
            scrollback: self.config.terminal.scrollback_lines as usize,
            hooks: self.config.ui.agent_status_hooks,
        };
        let grant = Grant {
            workspace: workspace.into(),
            exported_workspace: exported.into(),
            revision: p.snapshot.config.revision,
            folder: p.snapshot.config.folder.clone(),
            agents,
            epoch,
        };
        let device_id = server_device_id(&remote.fingerprint);
        if !self.queue_global_settings_action(
            workspace,
            SettingsJobAction::CommandCenter(launch::SettingsRequest::ServerInstall {
                request: Box::new(request),
                exported_workspace: WorkspaceId(exported.into()),
                device_id,
            }),
        ) {
            return Err("설정 워커가 사용 중입니다. 잠시 뒤 다시 실행 허용해 주세요.".into());
        }
        let state = &mut self.remote.as_mut().unwrap().execution;
        state.epoch = epoch;
        state.pending = Some(Pending {
            id: id.into(),
            grant: Some(grant),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(30),
        });
        state.error = None;
        host.publication_dirty = true;
        self.egui_ctx
            .request_repaint_after(std::time::Duration::from_secs(30));
        Ok(())
    }
    pub(super) fn revoke_command_center_server(
        &mut self,
        host: &mut CommandCenterHost,
        id: &str,
    ) -> Result<(), String> {
        let remote = self.remote.as_ref().ok_or("TLS 서버가 꺼져 있습니다.")?;
        if !remote.execution.can_request() {
            return Err("기기 설정 응답을 기다려 주세요.".into());
        }
        let workspace = remote
            .execution
            .grant
            .as_ref()
            .map(|g| g.workspace.clone())
            .unwrap_or_else(|| self.active.id.clone());
        let request = launch::SettingsRequest::ServerRevoke {
            operation_id: id.into(),
            runtime_instance: remote.policy.runtime_instance,
            control: remote.execution.control.clone(),
        };
        if !self.queue_global_settings_action(&workspace, SettingsJobAction::CommandCenter(request))
        {
            return Err("설정 워커가 사용 중입니다. 실행 허용 회수를 다시 눌러 주세요.".into());
        }
        self.remote.as_mut().unwrap().execution.pending = Some(Pending {
            id: id.into(),
            grant: None,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(30),
        });
        host.publication_dirty = true;
        self.egui_ctx
            .request_repaint_after(std::time::Duration::from_secs(30));
        Ok(())
    }
    pub(super) fn command_center_server_result(
        &mut self,
        id: String,
        runtime_instance: u64,
        result: Result<Option<Prepared>, String>,
    ) {
        let Some(remote) = self
            .remote
            .as_mut()
            .filter(|r| r.policy.runtime_instance == runtime_instance)
        else {
            return;
        };
        let pending = remote.execution.pending.as_ref().filter(|p| p.id == id);
        let result =
            result.and_then(
                |prepared| match (pending.and_then(|p| p.grant.clone()), prepared) {
                    (Some(mut grant), Some(prepared))
                        if prepared.profiles.len() == grant.agents.len()
                            && prepared
                                .profiles
                                .iter()
                                .zip(&grant.agents)
                                .all(|(p, a)| &p.agent == a) =>
                    {
                        grant.folder = prepared.folder;
                        grant.epoch = prepared.epoch;
                        Ok(Some(grant))
                    }
                    (None, None) => Ok(None),
                    _ => Err(
                        "실제 실행 설정이 승인 대상과 다릅니다. 실행 허용을 회수해 주세요.".into(),
                    ),
                },
            );
        if remote.execution.finish(&id, result) {
            let ok = remote.execution.error.is_none();
            if let Some(host) = self.command_center.as_mut() {
                if ok {
                    host.finish(
                        &id,
                        cc::ReceiptStatus::Acknowledged,
                        "이 컴퓨터의 프로젝트 실행 허용 변경을 적용했습니다.",
                    );
                } else {
                    host.reject(
                        &id,
                        remote
                            .execution
                            .error
                            .clone()
                            .unwrap_or_else(|| "기기 실행 설정을 적용하지 못했습니다.".into()),
                    );
                }
                host.publication_dirty = true;
            }
            self.egui_ctx.request_repaint();
        }
    }
    pub(super) fn copy_command_center_server_connection(
        &mut self,
        host: &mut CommandCenterHost,
        id: &str,
    ) -> Result<(), String> {
        let remote = self.remote.as_ref().ok_or("TLS 서버가 꺼져 있습니다.")?;
        let grant = remote.execution.copy_grant()?.clone();
        if remote.execution.copy_pending.is_some() {
            return Err("연결 정보 복사 응답을 기다려 주세요.".into());
        }
        let request = launch::SettingsRequest::ServerCopy {
            operation_id: id.into(),
            runtime_instance: remote.policy.runtime_instance,
            control: remote.execution.control.clone(),
            grant: grant.clone(),
            device_id: server_device_id(&remote.fingerprint),
        };
        if !self.queue_global_settings_action(
            &grant.workspace,
            SettingsJobAction::CommandCenter(request),
        ) {
            return Err("설정 워커가 사용 중입니다. 잠시 뒤 다시 복사해 주세요.".into());
        }
        self.remote.as_mut().unwrap().execution.copy_pending = Some((id.into(), grant));
        host.publication_dirty = true;
        Ok(())
    }
    pub(super) fn command_center_server_copy_result(
        &mut self,
        id: String,
        runtime_instance: u64,
        grant: Grant,
        token: Result<secret::SecretString, String>,
    ) {
        let Some(remote) = self
            .remote
            .as_mut()
            .filter(|r| r.policy.runtime_instance == runtime_instance)
        else {
            return;
        };
        let Some(valid) = remote.execution.finish_copy(&id, &grant) else {
            return;
        };
        let result = valid.and(token).map(|token| {
            self.egui_ctx.copy_text(serde_json::json!({"endpoint":remote.server.local_addr().to_string(),"pinned_fingerprint":remote.fingerprint,"workspace_id":grant.exported_workspace,"folder":grant.folder,"auth_token":token.expose()}).to_string());
        });
        if let Some(host) = self.command_center.as_mut() {
            match result {
                Ok(()) => host.finish(
                    &id,
                    cc::ReceiptStatus::Acknowledged,
                    "프로젝트 전용 연결 정보를 복사했습니다.",
                ),
                Err(error) => host.reject(&id, error),
            }
            host.publication_dirty = true;
        }
        self.egui_ctx.request_repaint();
    }
}

pub(super) fn server_device_id(fingerprint: &str) -> String {
    format!("tls-{}", fingerprint.replace(':', "").to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (ExecutionServer, runtime::InProcessRuntimeClient, PathBuf) {
        let dir = std::env::temp_dir().join(format!("deppy-cc-server-{}", uuid::Uuid::new_v4()));
        let factory = runtime::InProcessRuntimeHostFactory::new(
            Arc::new(AppRuntimeSecretResolver::new(dir.join("unused.db"))),
            secret::RedactionService::new(),
        );
        let worker = factory
            .create_client(runtime::RuntimeHostConfig {
                output_batch_ms: 10,
                scrollback_policy: None,
                logs_root: dir.join("logs"),
                persist: None,
                cwd: None,
                extra_env: vec![],
            })
            .unwrap();
        (
            ExecutionServer::new(worker.remote_worker_control(), worker.remote_worker_wake()),
            worker,
            dir,
        )
    }
    fn grant() -> Grant {
        Grant {
            workspace: "server-local-project".into(),
            exported_workspace: "controller-project".into(),
            revision: work::ConfigRevision(3),
            folder: "/fixture/local-project".into(),
            agents: vec![work::AgentSelection {
                provider: "fixture".into(),
                model: None,
                effort: None,
            }],
            epoch: 1,
        }
    }
    #[test]
    fn server_grant_completion_requires_exact_pending_operation_and_mapping() {
        let (mut state, worker, dir) = fixture();
        assert!(
            state.grant.is_none(),
            "starting a runtime never grants project execution"
        );
        state.pending = Some(Pending {
            id: "operation-a".into(),
            grant: Some(grant()),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(30),
        });
        assert!(!state.finish("foreign-operation", Ok(Some(grant()))));
        assert!(state.pending.is_some());
        assert!(state.grant.is_none());
        let mut wrong = grant();
        wrong.exported_workspace = "other-controller".into();
        assert!(state.finish("operation-a", Ok(Some(wrong))));
        assert!(state.grant.is_none());
        assert!(state.error.is_some());
        state.pending = Some(Pending {
            id: "operation-b".into(),
            grant: Some(grant()),
            deadline: std::time::Instant::now(),
        });
        assert!(state.finish("operation-b", Ok(Some(grant()))));
        assert_eq!(
            state.grant.as_ref().unwrap().workspace,
            "server-local-project"
        );
        assert_eq!(
            state.grant.as_ref().unwrap().exported_workspace,
            "controller-project"
        );
        state.pending = Some(Pending {
            id: "revoke".into(),
            grant: None,
            deadline: std::time::Instant::now(),
        });
        assert!(state.finish("revoke", Ok(None)));
        assert!(state.grant.is_none());
        drop(state);
        drop(worker);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn server_connection_copy_requires_known_current_grant_without_pending_change() {
        let (mut state, worker, dir) = fixture();
        assert!(state.copy_grant().is_err());
        state.grant = Some(grant());
        let original = grant();
        state.copy_pending = Some(("copy-a".into(), original.clone()));
        assert!(state.finish_copy("foreign", &original).is_none());
        state.grant.as_mut().unwrap().epoch += 1;
        assert!(state.finish_copy("copy-a", &original).unwrap().is_err());
        assert!(state.finish_copy("copy-a", &original).is_none());
        let current = state.grant.clone().unwrap();
        state.copy_pending = Some(("copy-b".into(), current.clone()));
        assert!(state.finish_copy("copy-b", &current).unwrap().is_ok());
        state.grant = Some(grant());
        assert!(state.copy_grant().is_ok());
        state.pending = Some(Pending {
            id: "replacement".into(),
            grant: Some(grant()),
            deadline: std::time::Instant::now(),
        });
        assert!(
            state.copy_grant().is_err(),
            "expired changes remain uncertain"
        );
        state.pending = None;
        state.error = Some("unknown side effect".into());
        assert!(state.copy_grant().is_err());
        drop(state);
        drop(worker);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn server_grant_epoch_survives_restarts_and_rejects_corruption() {
        let dir =
            std::env::temp_dir().join(format!("deppy-cc-server-epoch-{}", uuid::Uuid::new_v4()));
        assert_eq!(next_server_grant_epoch(&dir).unwrap(), 1);
        assert_eq!(next_server_grant_epoch(&dir).unwrap(), 2);
        assert_eq!(next_server_grant_epoch(&dir).unwrap(), 3);
        let path = dir.join("command-center-server-grant.epoch");
        std::fs::write(&path, b"invalid").unwrap();
        assert!(next_server_grant_epoch(&dir).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"invalid");
        std::fs::write(&path, u64::MAX.to_string()).unwrap();
        assert!(next_server_grant_epoch(&dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn native_server_configuration_and_secret_copy_controls_never_cross_web_wire() {
        let snapshot = cc::Snapshot {
            server_execution: cc::ServerExecutionSetup {
                running: true,
                workspace_id: Some("native-only".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let value = serde_json::to_value(snapshot).unwrap();
        assert!(value.get("server_execution").is_none());
        for intent in [
            cc::Intent::AllowThisComputer {
                workspace_id: "local".into(),
                config_revision: "1".into(),
                exported_workspace_id: "mapped".into(),
            },
            cc::Intent::RevokeThisComputer,
            cc::Intent::CopyThisComputerConnection,
            cc::Intent::OpenThisComputerSettings,
        ] {
            assert!(serde_json::to_value(&intent).is_err());
        }
        assert!(
            serde_json::from_value::<cc::Intent>(
                serde_json::json!({"type":"copy_this_computer_connection"})
            )
            .is_err()
        );
    }
}
