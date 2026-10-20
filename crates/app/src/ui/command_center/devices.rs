//! Native-only pairing state. The token never enters a serializable intent or snapshot.
use super::super::credentials::SensitiveInput;
use super::super::popup;
use super::*;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default)]
pub struct ExecutionSetup {
    pub assignments: Vec<AssignmentChoice>,
    pub enabled: bool,
    pub reason: String,
    pub connecting: bool,
    pub candidates: Vec<ExecutionCandidate>,
}
#[derive(Clone, Debug, Default)]
pub struct AssignmentChoice {
    pub workspace_id: String,
    pub device_id: Option<String>,
    pub profile_id: String,
    pub label: String,
    pub provider: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub capability: Capability,
}
#[derive(Clone, Debug, Default)]
pub struct ExecutionCandidate {
    pub id: String,
    pub workspace_id: String,
    pub device_id: String,
    pub name: String,
    pub endpoint: String,
    pub pinned_fingerprint: String,
    pub folder: String,
    pub profiles: Vec<ExecutionProfile>,
    pub max_workers: u8,
    pub active_workers: u8,
}
#[derive(Clone, Debug, Default)]
pub struct ExecutionProfile {
    pub id: String,
    pub provider: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}
pub struct NativeDeviceSetupRequest {
    pub workspace_id: String,
    pub name: String,
    pub endpoint: String,
    pub pinned_fingerprint: String,
    pub expected_folder: String,
    pub auth_token: SensitiveInput,
}
#[derive(Default)]
struct SetupForm {
    workspace_id: String,
    name: String,
    endpoint: String,
    pin: String,
    folder: String,
    token: String,
}
impl SetupForm {
    fn for_project(project: &Project) -> Self {
        Self {
            workspace_id: project.id.clone(),
            folder: project.directory.clone(),
            name: String::new(),
            endpoint: String::new(),
            pin: String::new(),
            token: String::new(),
        }
    }
}
impl Drop for SetupForm {
    fn drop(&mut self) {
        drop(SensitiveInput::try_new(std::mem::take(&mut self.token)));
    }
}
#[derive(Default)]
pub(super) struct DeviceSetupState {
    form: SetupForm,
    opened: bool,
    error: Option<String>,
    sequence: u64,
    pending: Option<(u64, NativeDeviceSetupRequest)>,
    profiles: BTreeMap<String, BTreeSet<String>>,
    limits: BTreeMap<String, u8>,
}
impl DeviceSetupState {
    fn queue(&mut self) -> Result<u64, &'static str> {
        if self.pending.is_some() {
            return Err("기존 연결 요청의 접수를 기다려 주세요.");
        }
        let f = &self.form;
        let label = |s: &str, limit| {
            !s.trim().is_empty() && s.len() <= limit && !s.chars().any(char::is_control)
        };
        let pin = f.pin.as_bytes();
        if !label(&f.workspace_id, 128)
            || !label(&f.name, 256)
            || f.endpoint.parse::<std::net::SocketAddr>().is_err()
            || pin.len() != 95
            || !pin.iter().enumerate().all(|(i, b)| {
                if i % 3 == 2 {
                    *b == b':'
                } else {
                    b.is_ascii_hexdigit()
                }
            })
            || !label(&f.folder, 4096)
            || !std::path::Path::new(&f.folder).is_absolute()
            || f.token.is_empty()
            || f.token.len() > 8192
            || f.token.chars().any(char::is_control)
        {
            return Err(
                "기기 이름, IP:포트, 인증서 지문, 프로젝트 폴더와 연결 토큰을 확인해 주세요.",
            );
        }
        let id = self
            .sequence
            .checked_add(1)
            .ok_or("연결 요청 한도입니다. 화면을 새로 열어 주세요.")?;
        self.sequence = id;
        self.pending = Some((
            id,
            NativeDeviceSetupRequest {
                workspace_id: f.workspace_id.clone(),
                name: f.name.clone(),
                endpoint: f.endpoint.clone(),
                pinned_fingerprint: f.pin.clone(),
                expected_folder: f.folder.clone(),
                auth_token: SensitiveInput::try_new(std::mem::take(&mut self.form.token))
                    .map_err(|_| "연결 토큰이 너무 깁니다.")?,
            },
        ));
        Ok(id)
    }
}

pub(super) fn show(
    ui: &mut egui::Ui,
    state: &mut UiState,
    snapshot: &Snapshot,
    p: &Project,
    intents: &mut Vec<Intent>,
) {
    ui.heading(text(snapshot, "devices"));
    if snapshot.devices.is_empty() {
        weak(ui, snapshot, text(snapshot, "no_devices"));
    }
    for device in &snapshot.devices {
        rule(ui, snapshot);
        ui.label(egui::RichText::new(&device.name).strong());
        weak(ui, snapshot, &device.description);
        weak(ui, snapshot, &device.capabilities);
        ui.label(
            egui::RichText::new(&device.connection).color(if device.connected {
                colors(snapshot).green
            } else {
                colors(snapshot).red
            }),
        );
        if device.revoke.enabled
            && snapshot.execution_setup.enabled
            && ui.small_button("실행 허용 회수").clicked()
        {
            intents.push(Intent::RevokeExecutionDevice {
                workspace_id: p.id.clone(),
                device_id: device.id.clone(),
            });
        }
    }
    egui::CollapsingHeader::new("이 컴퓨터에서 프로젝트 실행 · TLS 연결 설정")
        .id_salt("cc-server-execution-settings")
        .show(ui, |ui| {
            super::server::show(ui, state, snapshot, p, intents)
        });
    let setup = &mut state.device_setup;
    if setup.form.workspace_id != p.id {
        setup.form = SetupForm::for_project(p);
        setup.opened = false;
        setup.error = None;
    }
    rule(ui, snapshot);
    if snapshot.execution_setup.connecting {
        popup::notice(
            ui,
            "기기의 인증서와 실행 환경을 확인하고 있습니다.",
            popup::NoticeTone::Info,
        );
    }
    if snapshot.execution_setup.enabled {
        if ui.button("실행 기기 연결").clicked() {
            setup.opened = !setup.opened;
        }
    } else if !snapshot.execution_setup.reason.is_empty() {
        weak(ui, snapshot, &snapshot.execution_setup.reason);
    }
    if setup.opened {
        ui.add_space(12.0);
        popup::notice(
            ui,
            "연결할 컴퓨터에서 받은 주소·인증서 지문·토큰을 입력합니다. 연결 확인 후 프로젝트 폴더와 사용할 에이전트를 직접 허용합니다.",
            popup::NoticeTone::Info,
        );
        for (label, value, limit) in [
            ("기기 이름", &mut setup.form.name, 256),
            ("연결 주소 · IP:포트", &mut setup.form.endpoint, 256),
            ("인증서 SHA-256 지문", &mut setup.form.pin, 95),
            ("이 기기의 프로젝트 폴더", &mut setup.form.folder, 4096),
        ] {
            popup::field(ui, label, None, |ui| {
                ui.add(
                    egui::TextEdit::singleline(value)
                        .char_limit(limit)
                        .desired_width(f32::INFINITY),
                );
            });
            ui.add_space(10.0);
        }
        popup::field(ui, "연결 토큰", None, |ui| {
            let mut masked = popup::MaskedSecretBuffer::new(&mut setup.form.token, 8192);
            popup::secret_input(
                ui,
                &mut masked,
                ui.id().with("execution-token"),
                "기기에서 발급한 토큰",
                false,
            );
            if masked.finish() {
                setup.error = Some("연결 토큰은 8KiB 이하여야 합니다.".into());
            }
        });
        if let Some(error) = &setup.error {
            popup::notice(ui, error, popup::NoticeTone::Error);
        }
        popup::footer(ui, None, |ui| {
            if ui
                .add_enabled(
                    !snapshot.execution_setup.connecting && setup.pending.is_none(),
                    egui::Button::new("연결 확인"),
                )
                .clicked()
            {
                match setup.queue() {
                    Ok(request_id) => {
                        setup.error = None;
                        intents.push(Intent::ConnectExecutionDevice {
                            workspace_id: p.id.clone(),
                            request_id,
                        });
                    }
                    Err(error) => setup.error = Some(error.into()),
                }
            }
            if ui.button("취소").clicked() {
                setup.form = SetupForm::for_project(p);
                setup.opened = false;
                setup.error = None;
            }
        });
    }
    setup.profiles.retain(|id, _| {
        snapshot
            .execution_setup
            .candidates
            .iter()
            .any(|c| &c.id == id)
    });
    setup.limits.retain(|id, _| {
        snapshot
            .execution_setup
            .candidates
            .iter()
            .any(|c| &c.id == id)
    });
    for candidate in snapshot
        .execution_setup
        .candidates
        .iter()
        .filter(|c| c.workspace_id == p.id)
        .take(32)
    {
        rule(ui, snapshot);
        ui.label(egui::RichText::new(format!("{} · 연결 확인됨", candidate.name)).strong());
        for (label, value) in [
            ("기기 ID", &candidate.device_id),
            ("연결 주소", &candidate.endpoint),
            ("인증서 지문", &candidate.pinned_fingerprint),
            ("작업 폴더", &candidate.folder),
        ] {
            ui.label(egui::RichText::new(label).weak());
            ui.add(egui::Label::new(value).wrap().selectable(true));
        }
        ui.add_space(8.0);
        ui.label("이 프로젝트에 허용할 에이전트");
        let profiles = setup.profiles.entry(candidate.id.clone()).or_default();
        profiles.retain(|id| candidate.profiles.iter().any(|p| &p.id == id));
        for profile in candidate.profiles.iter().take(32) {
            let mut selected = profiles.contains(&profile.id);
            let label = format!(
                "{} · {} · {}",
                profile.provider,
                profile.model.as_deref().unwrap_or("기기 설정"),
                profile.effort.as_deref().unwrap_or("기기 설정")
            );
            if ui.checkbox(&mut selected, label).changed() {
                if selected {
                    profiles.insert(profile.id.clone());
                } else {
                    profiles.remove(&profile.id);
                }
            }
        }
        let limit = setup
            .limits
            .entry(candidate.id.clone())
            .or_insert(candidate.max_workers.clamp(1, 8));
        ui.horizontal_wrapped(|ui| {
            ui.label("동시 작업 한도");
            ui.add(egui::DragValue::new(limit).range(1..=candidate.max_workers.clamp(1, 8)));
            ui.weak(format!("현재 실행 {}", candidate.active_workers));
        });
        if ui
            .add_enabled(
                snapshot.execution_setup.enabled
                    && !snapshot.execution_setup.connecting
                    && !profiles.is_empty(),
                egui::Button::new("선택한 에이전트의 실행 허용"),
            )
            .clicked()
        {
            intents.push(Intent::GrantExecutionDevice {
                workspace_id: p.id.clone(),
                candidate_id: candidate.id.clone(),
                profile_ids: profiles.iter().cloned().collect(),
                max_workers: *limit,
            });
        }
    }
    rule(ui, snapshot);
    if action(
        ui,
        snapshot,
        "휴대폰·클라우드 연결 관리",
        &snapshot.manage_devices,
    ) {
        intents.push(Intent::ManageDevices {
            workspace_id: Some(p.id.clone()),
        });
    }
}
impl UiState {
    pub fn take_device_setup_request(
        &mut self,
        request_id: u64,
    ) -> Option<NativeDeviceSetupRequest> {
        if self
            .device_setup
            .pending
            .as_ref()
            .is_some_and(|(id, _)| *id == request_id)
        {
            self.device_setup.pending.take().map(|(_, request)| request)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> DeviceSetupState {
        DeviceSetupState {
            form: SetupForm {
                workspace_id: "workspace-a".into(),
                name: "Work Mac".into(),
                endpoint: "100.64.0.2:4270".into(),
                pin: vec!["ab"; 32].join(":"),
                folder: "/projects/work".into(),
                token: "fixture-only-token".into(),
            },
            ..Default::default()
        }
    }
    #[test]
    fn native_device_token_is_one_shot_and_exact_request_bound() {
        let mut ui = UiState {
            device_setup: state(),
            ..Default::default()
        };
        let id = ui.device_setup.queue().expect("valid setup");
        assert!(ui.device_setup.form.token.is_empty());
        assert!(ui.take_device_setup_request(id + 1).is_none());
        assert_eq!(
            ui.take_device_setup_request(id)
                .unwrap()
                .auth_token
                .into_inner(),
            "fixture-only-token"
        );
        assert!(ui.take_device_setup_request(id).is_none());
    }
    #[test]
    fn native_pairing_controls_are_absent_from_serialized_snapshot_and_intents() {
        let snapshot = Snapshot {
            execution_setup: ExecutionSetup {
                enabled: true,
                connecting: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(
            serde_json::to_value(snapshot)
                .unwrap()
                .get("execution_setup")
                .is_none()
        );
        for intent in [
            Intent::ConnectExecutionDevice {
                workspace_id: "a".into(),
                request_id: 1,
            },
            Intent::GrantExecutionDevice {
                workspace_id: "a".into(),
                candidate_id: "c".into(),
                profile_ids: vec!["p".into()],
                max_workers: 1,
            },
            Intent::RevokeExecutionDevice {
                workspace_id: "a".into(),
                device_id: "d".into(),
            },
        ] {
            assert!(serde_json::to_value(intent).is_err());
        }
    }
    #[test]
    fn invalid_or_concurrent_native_setup_preserves_secret_and_request_fence() {
        let mut s = state();
        s.form.pin = "invalid".into();
        assert!(s.queue().is_err());
        assert_eq!(s.form.token, "fixture-only-token");
        s.form.pin = vec!["ab"; 32].join(":");
        let first = s.queue().unwrap();
        s.form.token = "next-token".into();
        assert!(s.queue().is_err());
        assert_eq!(s.form.token, "next-token");
        assert_eq!(s.pending.as_ref().unwrap().0, first);
        let mut ui = UiState {
            device_setup: s,
            ..Default::default()
        };
        assert!(ui.take_device_setup_request(first).is_some());
        let second = ui.device_setup.queue().unwrap();
        assert!(second > first);
    }
}
