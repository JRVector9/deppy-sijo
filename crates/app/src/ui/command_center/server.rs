//! Native-only controls for explicitly exporting a local project to the TLS worker.
use super::*;

#[derive(Clone, Debug, Default)]
pub struct ServerExecutionSetup {
    pub running: bool,
    pub busy: bool,
    pub endpoint: String,
    pub fingerprint: String,
    pub workspace_id: Option<String>,
    pub exported_workspace_id: Option<String>,
    pub folder: String,
    pub agents: Vec<String>,
    pub requested_agents: Vec<String>,
    pub status: String,
    pub allow: Capability,
    pub revoke: Capability,
    pub copy: Capability,
}

pub(super) fn show(
    ui: &mut egui::Ui,
    state: &mut UiState,
    snapshot: &Snapshot,
    p: &Project,
    intents: &mut Vec<Intent>,
) {
    let server = &snapshot.server_execution;
    rule(ui, snapshot);
    ui.label(egui::RichText::new("이 컴퓨터에서 프로젝트 실행").strong());
    if !server.running {
        weak(
            ui,
            snapshot,
            "다른 컴퓨터가 이 프로젝트에 작업을 보낼 수 있게 하려면 TLS 연결 설정을 먼저 켜 주세요.",
        );
        if ui.button("TLS 연결 설정").clicked() {
            intents.push(Intent::OpenThisComputerSettings);
        }
        return;
    }
    weak(ui, snapshot, &server.status);
    for (label, value) in [
        ("연결 주소", &server.endpoint),
        ("인증서 지문", &server.fingerprint),
    ] {
        ui.label(egui::RichText::new(label).weak());
        ui.add(egui::Label::new(value).wrap().selectable(true));
    }
    if let Some(workspace) = &server.workspace_id {
        ui.label(format!("허용한 로컬 프로젝트: {workspace}"));
        ui.label(format!(
            "연결할 프로젝트 ID: {}",
            server.exported_workspace_id.as_deref().unwrap_or_default()
        ));
        ui.add(egui::Label::new(&server.folder).wrap().selectable(true));
        weak(ui, snapshot, server.agents.join(" / "));
        ui.horizontal_wrapped(|ui| {
            if action(ui, snapshot, "주소·지문·토큰 복사", &server.copy) {
                intents.push(Intent::CopyThisComputerConnection);
            }
            if action(ui, snapshot, "이 컴퓨터 실행 허용 회수", &server.revoke) {
                intents.push(Intent::RevokeThisComputer);
            }
        });
    } else if server.revoke.enabled && action(ui, snapshot, "미확인 실행 허용 회수", &server.revoke)
    {
        intents.push(Intent::RevokeThisComputer);
    }
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        weak(ui, snapshot, format!("현재 프로젝트 ID: {}", p.id));
        if ui.small_button("프로젝트 ID 복사").clicked() {
            ui.ctx().copy_text(p.id.clone());
        }
    });
    if state
        .server_workspace
        .as_ref()
        .is_none_or(|(project, _)| project != &p.id)
    {
        state.server_workspace = Some((p.id.clone(), p.id.clone()));
    }
    let (_, exported) = state.server_workspace.as_mut().unwrap();
    super::super::popup::field(
        ui,
        "연결할 프로젝트 ID",
        Some("작업을 보내는 컴퓨터의 프로젝트 ID를 붙여 넣으세요."),
        |ui| {
            ui.add_enabled(
                !server.busy,
                egui::TextEdit::singleline(exported)
                    .char_limit(128)
                    .desired_width(f32::INFINITY),
            );
        },
    );
    weak(ui, snapshot, format!("허용할 폴더: {}", p.directory));
    weak(
        ui,
        snapshot,
        format!("허용할 에이전트: {}", server.requested_agents.join(" / ")),
    );
    weak(
        ui,
        snapshot,
        format!("동시 작업 한도: {}", p.config.parallel_limit),
    );
    if action(ui, snapshot, "선택한 프로젝트의 실행 허용", &server.allow) {
        intents.push(Intent::AllowThisComputer {
            workspace_id: p.id.clone(),
            config_revision: p.config.revision.clone(),
            exported_workspace_id: exported.clone(),
        });
    }
    weak(
        ui,
        snapshot,
        "다른 컴퓨터는 보안 터널로 연결한 주소를 사용하세요. 연결 토큰은 복사 버튼을 누를 때만 클립보드로 보냅니다.",
    );
}
