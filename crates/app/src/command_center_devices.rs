//! Native trust actions and bounded projections. TLS, keychain and sidecar work stay in the worker.
use super::*;
use wm::devices::{DeviceView, VerifiedExecutionDevice};
use wm::remote_controller::{
    RemoteDeviceCandidate, RemoteDeviceJob, RemoteDeviceOutcome, RemoteDeviceSetup,
    RemoteDeviceWorker,
};

struct Request {
    id: String,
    workspace: String,
    token: u64,
    device: Option<String>,
    candidate: Option<String>,
}
pub(super) struct Devices {
    pub worker: RemoteDeviceWorker,
    pub policies: Vec<VerifiedExecutionDevice>,
    views: BTreeMap<String, Vec<DeviceView>>,
    candidates: Vec<(RemoteDeviceCandidate, std::time::Instant)>,
    loaded: std::collections::BTreeSet<String>,
    request: Option<Request>,
    poll_workspace: Option<String>,
    last_poll: std::time::Instant,
    poll_cursor: usize,
    remote_request: Option<u64>,
}
impl Devices {
    pub fn new(data_dir: PathBuf, ctx: &egui::Context) -> Self {
        Self {
            worker: create_command_center_device_worker(data_dir, ctx),
            policies: vec![],
            views: BTreeMap::new(),
            candidates: vec![],
            loaded: Default::default(),
            request: None,
            poll_workspace: None,
            last_poll: std::time::Instant::now(),
            poll_cursor: 0,
            remote_request: None,
        }
    }
    fn next_connected_workspace(&mut self) -> Option<String> {
        let count = self
            .views
            .values()
            .filter(|rows| rows.iter().any(|d| d.connected))
            .count();
        if count == 0 {
            return None;
        }
        let workspace = self
            .views
            .iter()
            .filter(|(_, rows)| rows.iter().any(|d| d.connected))
            .nth(self.poll_cursor % count)
            .map(|(workspace, _)| workspace.clone());
        self.poll_cursor = (self.poll_cursor + 1) % count;
        workspace
    }
    pub fn has_pending(&self) -> bool {
        self.request.is_some() || self.poll_workspace.is_some() || self.remote_request.is_some()
    }
    pub(super) fn can_execute(&self, workspace: &str, device: &str) -> bool {
        self.views
            .get(workspace)
            .is_some_and(|views| views.iter().any(|d| d.id == device && d.can_execute))
    }
    pub(super) fn queue_remote(&mut self, token: u64, job: RemoteDeviceJob) -> Result<(), String> {
        if self.has_pending() {
            return Err("기기 워커의 현재 응답을 기다려 주세요.".into());
        }
        self.worker.try_submit(job).map_err(|_| {
            "원격 요청을 접수하지 못했습니다. 새 실행·입력을 보내지 않았습니다.".to_owned()
        })?;
        self.remote_request = Some(token);
        Ok(())
    }
    pub(super) fn finish_remote(&mut self, token: u64) {
        if self.remote_request == Some(token) {
            self.remote_request = None;
        }
    }
    fn queue(
        &mut self,
        id: &str,
        workspace: &str,
        token: u64,
        job: RemoteDeviceJob,
    ) -> Result<(), String> {
        if self.has_pending() {
            return Err("기기 연결 워커가 사용 중입니다. 현재 응답을 기다려 주세요.".into());
        }
        let (device, candidate) = match &job {
            RemoteDeviceJob::Grant { candidate_id, .. } => (
                self.candidates
                    .iter()
                    .find(|(c, _)| &c.candidate_id == candidate_id)
                    .map(|(c, _)| c.device_id.clone()),
                Some(candidate_id.clone()),
            ),
            RemoteDeviceJob::Revoke { device_id, .. } => (Some(device_id.clone()), None),
            _ => (None, None),
        };
        self.worker.try_submit(job).map_err(|_| {
            "기기 연결 요청을 접수하지 못했습니다. 실행을 시작하지 않았습니다.".to_owned()
        })?;
        self.request = Some(Request {
            id: id.into(),
            workspace: workspace.into(),
            token,
            device,
            candidate,
        });
        Ok(())
    }
    pub(super) fn project(&self, snapshot: &mut cc::Snapshot, workspace: &str) {
        snapshot.execution_setup = cc::ExecutionSetup {
            enabled: true,
            reason: String::new(),
            connecting: self.has_pending(),
            assignments: vec![],
            candidates: self
                .candidates
                .iter()
                .filter(|(c, at)| {
                    c.workspace_id.0 == workspace
                        && at.elapsed().as_millis() < u128::from(c.expires_in_ms)
                })
                .map(|(c, _)| cc::ExecutionCandidate {
                    id: c.candidate_id.clone(),
                    workspace_id: c.workspace_id.0.clone(),
                    device_id: c.device_id.clone(),
                    name: c.name.clone(),
                    endpoint: c.endpoint.to_string(),
                    pinned_fingerprint: c.pinned_fingerprint.clone(),
                    folder: c.folder.clone(),
                    profiles: c
                        .profiles
                        .iter()
                        .map(|p| cc::ExecutionProfile {
                            id: p.id.clone(),
                            provider: p.agent.provider.clone(),
                            model: p.agent.model.clone(),
                            effort: p.agent.effort.clone(),
                        })
                        .collect(),
                    max_workers: c.max_workers,
                    active_workers: c.active_workers,
                })
                .collect(),
        };
        if let Some(devices) = self.views.get(workspace) {
            for d in devices {
                snapshot.devices.push(cc::Device {
                    id: d.id.clone(),
                    name: d.name.clone(),
                    description: "직접 등록한 인증서 고정 TLS 실행 기기".into(),
                    capabilities: d
                        .profiles
                        .iter()
                        .map(|p| {
                            format!(
                                "{} · {} · {}",
                                p.agent.provider,
                                p.agent.model.as_deref().unwrap_or("기본"),
                                p.agent.effort.as_deref().unwrap_or("기본")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" / "),
                    connection: if d.can_execute {
                        "실행 허용 · 연결됨".into()
                    } else {
                        format!("실행 불가 · {:?}", d.unavailable)
                    },
                    connected: d.connected,
                    can_execute: d.can_execute,
                    revoke: if d.unavailable == Some(wm::devices::DeviceUnavailable::Revoked) {
                        no("이미 실행 허용을 회수했습니다.")
                    } else {
                        yes()
                    },
                });
            }
        }
    }
}
fn native_correlation_matches(
    request: &Request,
    correlation: &wm::remote_controller::RemoteDeviceCorrelation,
) -> bool {
    request.token == correlation.request_id
        && correlation
            .workspace_id
            .as_ref()
            .is_none_or(|workspace| workspace.0 == request.workspace)
        && correlation.attempt_id.is_none()
        && correlation.operation_id.is_none()
        && correlation
            .device_id
            .as_ref()
            .is_none_or(|device| request.device.as_ref() == Some(device))
        && correlation
            .candidate_id
            .as_ref()
            .is_none_or(|candidate| request.candidate.as_ref() == Some(candidate))
}
fn take_request(devices: &mut Devices, token: u64, workspace: Option<&str>) -> Option<Request> {
    devices.request.as_ref().filter(|r| {
        r.token == token && workspace.is_none_or(|workspace| r.workspace == workspace)
    })?;
    devices.request.take()
}
impl App {
    pub(super) fn connect_command_center_device(
        &mut self,
        host: &mut CommandCenterHost,
        id: &str,
        workspace: &str,
        request_id: u64,
    ) -> Result<(), String> {
        let setup: cc::NativeDeviceSetupRequest = host
            .ui
            .take_device_setup_request(request_id)
            .ok_or("이미 소비되었거나 만료된 기기 연결 요청입니다.")?;
        if setup.workspace_id != workspace || !host.projects.contains_key(workspace) {
            return Err("기기 연결 대상 프로젝트가 바뀌었습니다.".into());
        }
        let endpoint = setup
            .endpoint
            .parse()
            .map_err(|_| "기기 IP:포트가 유효하지 않습니다.")?;
        host.next_token = host.next_token.saturating_add(1);
        let token = host.next_token;
        let job = RemoteDeviceJob::Inspect {
            setup: RemoteDeviceSetup {
                request_id: token,
                name: setup.name,
                endpoint,
                pinned_fingerprint: setup.pinned_fingerprint,
                workspace_id: WorkspaceId(workspace.into()),
                expected_folder: setup.expected_folder,
                auth_token: secret::SecretString::new(setup.auth_token.into_inner()),
            },
        };
        host.devices.queue(id, workspace, token, job)?;
        host.status = "인증서가 고정된 TLS 연결에서 실제 기기·폴더·프로필을 조회합니다.".into();
        Ok(())
    }
    pub(super) fn grant_command_center_device(
        &mut self,
        host: &mut CommandCenterHost,
        id: &str,
        workspace: &str,
        candidate: &str,
        profiles: Vec<String>,
        max_workers: u8,
    ) -> Result<(), String> {
        if !host
            .devices
            .candidates
            .iter()
            .any(|(c, _)| c.candidate_id == candidate && c.workspace_id.0 == workspace)
        {
            return Err("현재 프로젝트에서 확인한 기기 후보가 아닙니다.".into());
        }
        host.next_token = host.next_token.saturating_add(1);
        let token = host.next_token;
        host.devices.queue(
            id,
            workspace,
            token,
            RemoteDeviceJob::Grant {
                request_id: token,
                candidate_id: candidate.into(),
                allowed_profile_ids: profiles,
                max_workers,
            },
        )
    }
    pub(super) fn revoke_command_center_device(
        &mut self,
        host: &mut CommandCenterHost,
        id: &str,
        workspace: &str,
        device: &str,
    ) -> Result<(), String> {
        if !host
            .devices
            .views
            .get(workspace)
            .is_some_and(|v| v.iter().any(|d| d.id == device))
        {
            return Err("현재 프로젝트의 등록 기기가 아닙니다.".into());
        }
        host.next_token = host.next_token.saturating_add(1);
        let token = host.next_token;
        host.devices.queue(
            id,
            workspace,
            token,
            RemoteDeviceJob::Revoke {
                request_id: token,
                workspace_id: WorkspaceId(workspace.into()),
                device_id: device.into(),
            },
        )
    }
    pub(super) fn pump_command_center_devices(
        &mut self,
        host: &mut CommandCenterHost,
        visible: bool,
    ) {
        if let Some(outcome) = host.devices.worker.try_recv() {
            let background_snapshot = matches!(&outcome, RemoteDeviceOutcome::Snapshot { .. });
            let previous_views = if background_snapshot {
                Some(host.devices.views.clone())
            } else {
                None
            };
            match outcome {
                RemoteDeviceOutcome::Candidate {
                    request_id,
                    candidate,
                } => {
                    if let Some(request) = take_request(
                        &mut host.devices,
                        request_id,
                        Some(&candidate.workspace_id.0),
                    ) {
                        if host.devices.candidates.len() == 32 {
                            host.devices.candidates.remove(0);
                        }
                        host.devices
                            .candidates
                            .retain(|(c, _)| c.candidate_id != candidate.candidate_id);
                        host.devices
                            .candidates
                            .push((candidate, std::time::Instant::now()));
                        host.finish(&request.id,cc::ReceiptStatus::Acknowledged,"실제 기기 정보를 확인했습니다. 폴더와 허용 프로필을 검토한 뒤 실행을 허용해 주세요.");
                    }
                }
                RemoteDeviceOutcome::Granted {
                    request_id,
                    device,
                    devices,
                    policies,
                } => {
                    if let Some(request) = take_request(
                        &mut host.devices,
                        request_id,
                        Some(&device.registration.workspace_id.0),
                    ) {
                        host.devices
                            .views
                            .insert(request.workspace.clone(), devices);
                        host.devices
                            .policies
                            .retain(|p| p.registration().workspace_id.0 != request.workspace);
                        host.devices.policies.extend(policies);
                        host.devices.candidates.retain(|(c, _)| {
                            c.device_id != device.registration.id
                                || c.workspace_id.0 != request.workspace
                        });
                        host.finish(
                            &request.id,
                            cc::ReceiptStatus::Acknowledged,
                            "선택한 폴더·프로필·실행 한도를 기기에 허용하고 저장했습니다.",
                        );
                    }
                }
                RemoteDeviceOutcome::Revoked { request_id, device } => {
                    if let Some(request) = take_request(
                        &mut host.devices,
                        request_id,
                        Some(&device.registration.workspace_id.0),
                    ) {
                        host.devices.policies.retain(|p| {
                            p.registration().id != device.registration.id
                                || p.registration().workspace_id.0 != request.workspace
                        });
                        if let Some(views) = host.devices.views.get_mut(&request.workspace)
                            && let Some(view) =
                                views.iter_mut().find(|d| d.id == device.registration.id)
                        {
                            view.can_execute = false;
                            view.connected = false;
                            view.unavailable = Some(wm::devices::DeviceUnavailable::Revoked);
                        }
                        host.finish(&request.id,cc::ReceiptStatus::Acknowledged,"기기의 실행 허용을 회수하고 저장했습니다. 이전 미확인 실행은 재전송하지 않습니다.");
                    }
                }
                RemoteDeviceOutcome::Loaded {
                    workspace_id,
                    devices,
                } => {
                    if host.devices.poll_workspace.as_deref() == Some(&workspace_id.0) {
                        host.devices.poll_workspace = None;
                        host.devices.loaded.insert(workspace_id.0.clone());
                        host.devices.views.insert(workspace_id.0, devices);
                    }
                }
                RemoteDeviceOutcome::Snapshot {
                    workspace_id,
                    devices,
                    receipts,
                    policies,
                    inputs,
                    sessions,
                } => {
                    if host.devices.poll_workspace.as_deref() == Some(&workspace_id.0) {
                        host.devices.poll_workspace = None;
                        host.devices.views.insert(workspace_id.0.clone(), devices);
                        host.devices
                            .policies
                            .retain(|p| p.registration().workspace_id != workspace_id);
                        host.devices.policies.extend(policies);
                    }
                    for receipt in receipts {
                        if !remote::reconcile::launch_receipt(host, &receipt) {
                            remote::launch_receipt(host, receipt);
                        }
                    }
                    for receipt in inputs {
                        if !remote::reconcile::input_receipt(host, &receipt)
                            && !remote::sessions::input_receipt(host, &receipt)
                        {
                            remote::input_receipt(host, receipt);
                        }
                    }
                    for observation in sessions {
                        remote::sessions::session_observed(host, None, observation);
                    }
                }
                RemoteDeviceOutcome::Failed { request_id, code } => {
                    if remote::reconcile::failed(host, request_id)
                        || remote::sessions::failed(host, request_id, false)
                        || remote::failed(host, request_id)
                    {
                        host.devices.finish_remote(request_id);
                    } else if let Some(request) = take_request(&mut host.devices, request_id, None)
                    {
                        host.reject(&request.id, format!("기기 요청이 거부되었습니다: {code:?}"));
                    } else {
                        host.devices.poll_workspace = None;
                        host.status = format!("기기 상태를 읽지 못했습니다: {code:?}");
                    }
                }
                RemoteDeviceOutcome::ObservationUnknown { correlation } => {
                    remote::reconcile::failed(host, correlation.request_id);
                    remote::unknown(host, &correlation);
                    remote::sessions::failed(host, correlation.request_id, true);
                    let matches = host
                        .devices
                        .request
                        .as_ref()
                        .is_some_and(|r| native_correlation_matches(r, &correlation));
                    if let Some(request) = if matches {
                        host.devices.request.take()
                    } else {
                        None
                    } {
                        host.finish(&request.id,cc::ReceiptStatus::Uncertain,"기기 요청의 응답이 끊겼습니다. 자동으로 다시 연결하거나 실행하지 않습니다.");
                    }
                    if let Some(workspace) = correlation.workspace_id {
                        host.devices
                            .policies
                            .retain(|p| p.registration().workspace_id != workspace);
                        if let Some(views) = host.devices.views.get_mut(&workspace.0) {
                            for view in views {
                                view.can_execute = false;
                                view.connected = false;
                                view.unavailable =
                                    Some(wm::devices::DeviceUnavailable::Disconnected);
                            }
                        }
                    }
                    host.devices.poll_workspace = None;
                }
                RemoteDeviceOutcome::LaunchPending {
                    request_id,
                    lease,
                    verified,
                } => remote::launch_pending(host, request_id, lease, verified),
                RemoteDeviceOutcome::LaunchReceipt(receipt) => {
                    if !remote::reconcile::launch_receipt(host, &receipt) {
                        remote::launch_receipt(host, receipt);
                    }
                }
                RemoteDeviceOutcome::ExecutionObserved {
                    request_id,
                    lease,
                    execution,
                    verified,
                } => {
                    if !remote::sessions::observed(
                        host,
                        request_id,
                        &lease,
                        execution.clone(),
                        verified.clone(),
                    ) {
                        remote::observed(host, request_id, lease, execution, verified);
                    }
                }
                RemoteDeviceOutcome::InputPending {
                    request_id,
                    lease,
                    operation_id,
                } => {
                    if !remote::sessions::input_pending(host, request_id, &lease, &operation_id) {
                        remote::input_pending(host, request_id, &lease, &operation_id);
                    }
                }
                RemoteDeviceOutcome::InputReceipt(receipt) => {
                    if !remote::reconcile::input_receipt(host, &receipt)
                        && !remote::sessions::input_receipt(host, &receipt)
                    {
                        remote::input_receipt(host, receipt);
                    }
                }
                RemoteDeviceOutcome::SessionObserved {
                    request_id,
                    observation,
                } => remote::sessions::session_observed(host, Some(request_id), *observation),
                RemoteDeviceOutcome::CloseReceipt(receipt) => {
                    remote::sessions::close_receipt(host, receipt)
                }
            }
            if previous_views
                .as_ref()
                .is_none_or(|previous| previous != &host.devices.views)
            {
                host.publication_dirty = true;
            }
        }
        if host.devices.has_pending() {
            return;
        }
        let workspace = &self.active.id;
        if visible
            && !host.devices.loaded.contains(workspace)
            && host.projects.get(workspace).is_some_and(|p| p.loaded)
        {
            if host
                .devices
                .worker
                .try_submit(RemoteDeviceJob::Load {
                    workspace_id: WorkspaceId(workspace.clone()),
                })
                .is_ok()
            {
                host.devices.poll_workspace = Some(workspace.clone());
            }
            return;
        }
        let connected = host
            .devices
            .views
            .values()
            .any(|rows| rows.iter().any(|d| d.connected));
        if (visible || host.remotes.values().any(|p| p.pending())) && connected {
            if host.devices.last_poll.elapsed() >= std::time::Duration::from_millis(500)
                && let Some(workspace) = host.devices.next_connected_workspace()
                && host
                    .devices
                    .worker
                    .try_submit(RemoteDeviceJob::Poll {
                        workspace_id: WorkspaceId(workspace.clone()),
                    })
                    .is_ok()
            {
                host.devices.poll_workspace = Some(workspace);
                host.devices.last_poll = std::time::Instant::now();
            }
            self.egui_ctx
                .request_repaint_after(std::time::Duration::from_millis(500));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delayed_or_cross_project_device_response_does_not_consume_another_request() {
        let mut devices = Devices::new(
            std::env::temp_dir().join(format!("cc-inert-device-{}", uuid::Uuid::new_v4())),
            &egui::Context::default(),
        );
        devices.request = Some(Request {
            id: "operation-a".into(),
            workspace: "workspace-a".into(),
            token: 71,
            device: Some("worker-a".into()),
            candidate: Some("candidate-a".into()),
        });
        assert!(take_request(&mut devices, 70, Some("workspace-a")).is_none());
        assert!(take_request(&mut devices, 71, Some("workspace-b")).is_none());
        assert_eq!(devices.request.as_ref().unwrap().id, "operation-a");
        assert!(take_request(&mut devices, 71, Some("workspace-a")).is_some());
        assert!(!devices.has_pending());
    }
    #[test]
    fn disconnected_registered_device_remains_revocable_without_execution_permission() {
        let mut devices = Devices::new(
            std::env::temp_dir().join(format!("cc-inert-device-{}", uuid::Uuid::new_v4())),
            &egui::Context::default(),
        );
        devices.views.insert(
            "workspace-a".into(),
            vec![DeviceView {
                id: "registered-offline".into(),
                name: "Actual registered worker".into(),
                connected: false,
                can_execute: false,
                unavailable: Some(wm::devices::DeviceUnavailable::Disconnected),
                profiles: vec![],
            }],
        );
        let mut snapshot = cc::Snapshot::default();
        devices.project(&mut snapshot, "workspace-a");
        assert!(!snapshot.devices[0].can_execute);
        assert!(snapshot.devices[0].revoke.enabled);
        let mut other = cc::Snapshot::default();
        devices.project(&mut other, "workspace-b");
        assert!(other.devices.is_empty());
    }
    #[test]
    fn connected_workspace_polling_is_fair_and_excludes_offline_registry_rows() {
        let mut devices = Devices::new(
            std::env::temp_dir().join(format!("cc-inert-device-{}", uuid::Uuid::new_v4())),
            &egui::Context::default(),
        );
        for (workspace, connected) in [("a", true), ("b", false), ("c", true)] {
            devices.views.insert(
                workspace.into(),
                vec![DeviceView {
                    id: workspace.into(),
                    name: workspace.into(),
                    connected,
                    can_execute: false,
                    unavailable: None,
                    profiles: vec![],
                }],
            );
        }
        assert_eq!(
            (0..4)
                .map(|_| devices.next_connected_workspace().unwrap())
                .collect::<Vec<_>>(),
            vec!["a", "c", "a", "c"]
        );
    }
    #[test]
    fn infrastructure_failure_keeps_exact_native_device_and_candidate_correlation() {
        let request = Request {
            id: "op".into(),
            workspace: "workspace-a".into(),
            token: 19,
            device: Some("device-a".into()),
            candidate: Some("candidate-a".into()),
        };
        let correlation = wm::remote_controller::RemoteDeviceCorrelation {
            request_id: 19,
            workspace_id: Some(WorkspaceId("workspace-a".into())),
            device_id: Some("device-a".into()),
            candidate_id: Some("candidate-a".into()),
            attempt_id: None,
            operation_id: None,
        };
        assert!(native_correlation_matches(&request, &correlation));
        let mut wrong = correlation.clone();
        wrong.operation_id = Some(work::OperationId::new());
        assert!(!native_correlation_matches(&request, &wrong));
        let mut wrong = correlation.clone();
        wrong.device_id = Some("device-b".into());
        assert!(!native_correlation_matches(&request, &wrong));
        let mut wrong = correlation;
        wrong.candidate_id = Some("candidate-b".into());
        assert!(!native_correlation_matches(&request, &wrong));
    }
}
