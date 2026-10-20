//! Render-only Command Center. All I/O and admission stay in the host.
//!
//! Opaque session keys must identify one exact native pane or structured session.
//! The host validates every returned target again before executing an intent.

mod devices;
mod layout;
mod main;
mod server;
mod settings;
mod workers;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use devices::{
    AssignmentChoice, ExecutionCandidate, ExecutionProfile, ExecutionSetup,
    NativeDeviceSetupRequest,
};
pub use layout::LayoutRects;
pub use server::ServerExecutionSetup;

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RouteTarget {
    pub workspace_id: String,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum View {
    #[default]
    Main,
    Tasks,
    Task,
    Terminal,
    Devices,
    History,
    Settings,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailTab {
    #[default]
    Work,
    Result,
    Git,
    History,
    Terminal,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorFilter {
    #[default]
    All,
    Attention,
    Running,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    PlanReview,
    #[default]
    Queued,
    Running,
    WaitingApproval,
    WaitingInput,
    ResultSubmitted,
    VerificationFailed,
    AwaitingUserConfirmation,
    Completed,
    Failed,
    Cancelled,
    ObservationUnknown,
}

impl TaskState {
    pub fn needs_attention(self) -> bool {
        matches!(
            self,
            Self::PlanReview
                | Self::WaitingApproval
                | Self::WaitingInput
                | Self::ResultSubmitted
                | Self::VerificationFailed
                | Self::AwaitingUserConfirmation
                | Self::Failed
                | Self::ObservationUnknown
        )
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Capability {
    pub enabled: bool,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub terminal_session_id: Option<String>,
    pub target: RouteTarget,
    pub title: String,
    pub provider: String,
    pub device: String,
    pub state: TaskState,
    pub input: Capability,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    pub command: String,
    pub directory: String,
    pub scope: String,
    pub action: Capability,
}

/// Exact structured-session request; it does not imply a workflow task or PTY authority.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PendingApproval {
    pub target: RouteTarget,
    pub session_title: String,
    pub approval: Approval,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RemoteTerminal {
    pub text: String,
    pub cols: u16,
    pub rows: u16,
    pub connected: bool,
    pub closed: bool,
    pub status: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Task {
    #[serde(skip)]
    pub reconcile_remote: Capability,
    #[serde(default)]
    pub remote_terminal: Option<RemoteTerminal>,
    #[serde(default)]
    pub close_worker: Capability,
    #[serde(skip)]
    pub selected_assignment: Option<String>,
    #[serde(default)]
    pub create_worker: Capability,
    #[serde(default)]
    pub attempt_id: Option<String>,
    #[serde(default)]
    pub result_report: Capability,
    #[serde(default)]
    pub verify_result: Capability,
    #[serde(default)]
    pub verification: String,
    #[serde(default)]
    pub eligible_sessions: Vec<Session>,
    #[serde(default)]
    pub terminal_session_id: Option<String>,
    pub target: RouteTarget,
    pub title: String,
    pub role: String,
    pub scope: String,
    pub acceptance_criteria: String,
    pub provider: String,
    pub device: String,
    pub directory: String,
    pub state: TaskState,
    pub activity: String,
    /// Observation time supplied by the host. Never synthesized from the frame clock.
    pub observed_at: String,
    pub result: Option<String>,
    pub result_revision: Option<String>,
    pub approval: Option<Approval>,
    pub input: Capability,
    pub terminal: Capability,
    pub git: Capability,
    pub confirm_result: Capability,
    pub dispatch: Capability,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub target: Option<RouteTarget>,
    pub time: String,
    pub text: String,
    pub detail: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub allowed_scope_roots: Vec<String>,
    pub name: String,
    pub directory: String,
    /// Explicit binding; None means unbound, never the first available session.
    pub main_session_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub effort: String,
    pub worker_providers: Vec<String>,
    pub parallel_limit: u8,
    pub automation: String,
    pub use_worktrees: bool,
    pub revision: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Project {
    #[serde(default)]
    pub pending_approvals: Vec<PendingApproval>,
    #[serde(default)]
    pub create_main: Capability,
    #[serde(default)]
    pub loaded: bool,
    #[serde(default)]
    pub storage_retry: Capability,
    #[serde(default)]
    pub manual_plan: Capability,
    #[serde(default)]
    pub planner_prompt: Option<String>,
    pub id: String,
    pub name: String,
    pub directory: String,
    pub goal: String,
    pub main_summary: String,
    pub pending_goal_id: Option<String>,
    pub plan_confirmation: Capability,
    pub main_target: RouteTarget,
    pub main_provider: String,
    pub main_model: String,
    pub main_effort: String,
    pub main_input: Capability,
    pub sessions: Vec<Session>,
    pub tasks: Vec<Task>,
    pub history: Vec<HistoryEntry>,
    pub config: ProjectConfig,
    pub save_settings: Capability,
    /// Real provider choices/capabilities supplied by the host.
    pub available_providers: Vec<String>,
    pub available_models: Vec<String>,
    pub available_efforts: Vec<String>,
    pub available_automation: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Device {
    #[serde(default)]
    pub revoke: Capability,
    pub id: String,
    pub name: String,
    pub description: String,
    pub capabilities: String,
    pub connection: String,
    pub connected: bool,
    pub can_execute: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(skip)]
    pub server_execution: ServerExecutionSetup,
    /// Pairing and execution grants are trusted native-computer actions. Never publish this form.
    #[serde(skip)]
    pub execution_setup: ExecutionSetup,
    #[serde(default)]
    pub receipts: Vec<RemoteReceipt>,
    pub projects: Vec<Project>,
    pub selected_workspace_id: Option<String>,
    pub devices: Vec<Device>,
    pub dark_mode: bool,
    pub font_scale: f32,
    /// `command_center.*` translations; missing keys use the approved Korean labels.
    pub labels: BTreeMap<String, String>,
    pub add_project: Capability,
    pub manage_devices: Capability,
    pub status: String,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            server_execution: ServerExecutionSetup::default(),
            execution_setup: ExecutionSetup::default(),
            receipts: Vec::new(),
            projects: Vec::new(),
            selected_workspace_id: None,
            devices: Vec::new(),
            dark_mode: true,
            font_scale: 1.0,
            labels: BTreeMap::new(),
            add_project: Capability::default(),
            manage_devices: Capability::default(),
            status: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Intent {
    #[serde(skip)]
    ReconcileRemote {
        target: RouteTarget,
    },
    ObserveRemoteSession {
        target: RouteTarget,
    },
    #[serde(skip)]
    CloseWorker {
        target: RouteTarget,
        attempt_id: String,
    },
    #[serde(skip)]
    AllowThisComputer {
        workspace_id: String,
        config_revision: String,
        exported_workspace_id: String,
    },
    #[serde(skip)]
    RevokeThisComputer,
    #[serde(skip)]
    CopyThisComputerConnection,
    #[serde(skip)]
    OpenThisComputerSettings,
    #[serde(skip)]
    SetTaskAssignment {
        target: RouteTarget,
        config_revision: String,
        device_id: Option<String>,
        profile_id: String,
    },
    #[serde(skip)]
    ConnectExecutionDevice {
        workspace_id: String,
        request_id: u64,
    },
    #[serde(skip)]
    GrantExecutionDevice {
        workspace_id: String,
        candidate_id: String,
        profile_ids: Vec<String>,
        max_workers: u8,
    },
    #[serde(skip)]
    RevokeExecutionDevice {
        workspace_id: String,
        device_id: String,
    },
    CreateMain {
        workspace_id: String,
    },
    CreateWorker {
        target: RouteTarget,
    },
    LoadProject {
        workspace_id: String,
    },
    ReportResult {
        target: RouteTarget,
        attempt_id: String,
        text: String,
    },
    VerifyResult {
        target: RouteTarget,
        attempt_id: String,
        revision: String,
        passed: bool,
        evidence: String,
    },
    RetryStorage {
        workspace_id: String,
    },
    SelectProject {
        workspace_id: String,
    },
    OpenSession(RouteTarget),
    OpenGit(RouteTarget),
    OpenHistory(RouteTarget),
    ManageDevices {
        workspace_id: Option<String>,
    },
    AddProject,
    SaveProject {
        workspace_id: String,
        config: ProjectConfig,
    },
    Submit {
        target: RouteTarget,
        text: String,
    },
    DraftChanged {
        target: RouteTarget,
        text: String,
    },
    Approval {
        target: RouteTarget,
        approval_id: String,
        allow: bool,
    },
    ConfirmResult {
        target: RouteTarget,
        revision: String,
    },
    InstallPlanJson {
        workspace_id: String,
        json: String,
    },
    ConfirmPlan {
        workspace_id: String,
        goal_id: String,
    },
    DispatchTask {
        target: RouteTarget,
    },
    SetTheme {
        dark_mode: bool,
    },
    UseOriginalLayout,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteReceipt {
    pub operation_id: String,
    pub status: ReceiptStatus,
    pub detail: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    Acknowledged,
    Rejected,
    Uncertain,
}
#[derive(Default)]
pub struct UiState {
    server_workspace: Option<(String, String)>,
    device_setup: devices::DeviceSetupState,
    pub view: View,
    pub detail_tab: DetailTab,
    pub selected_task_id: Option<String>,
    pub monitor_filter: MonitorFilter,
    pub left_drawer: bool,
    pub right_drawer: bool,
    pub monitor_collapsed: bool,
    pub last_layout: Option<LayoutRects>,
    pub composer_focused: bool,
    /// Native terminal host slot. Set only for a terminal view with an exact session target.
    pub terminal_rect: Option<egui::Rect>,
    pub terminal_target: Option<RouteTarget>,
    pub aux_rect: Option<egui::Rect>,
    pub aux_target: Option<RouteTarget>,
    pub aux_tab: Option<DetailTab>,
    result_reports: BTreeMap<RouteTarget, String>,
    verification_reports: BTreeMap<RouteTarget, String>,
    task_workers: BTreeMap<String, Option<String>>,
    plan_json: BTreeMap<String, String>,
    active_workspace: Option<String>,
    requested_terminal_target: Option<RouteTarget>,
    requested_task_target: Option<RouteTarget>,
    drafts: BTreeMap<RouteTarget, String>,
    settings_draft: Option<(String, ProjectConfig)>,
    settings_error: Option<String>,
    ime_composing: bool,
    ime_target: Option<RouteTarget>,
}

impl UiState {
    pub fn clear_draft(&mut self, target: &RouteTarget) {
        self.drafts.remove(target);
    }
    pub fn set_draft(&mut self, target: RouteTarget, text: String) {
        self.drafts.insert(target, text);
    }
    pub fn draft(&self, target: &RouteTarget) -> &str {
        self.drafts
            .get(target)
            .map(String::as_str)
            .unwrap_or_default()
    }
    pub fn select_task(&mut self, task_id: String) {
        self.selected_task_id = Some(task_id);
        self.view = View::Task;
        self.detail_tab = DetailTab::Work;
        self.left_drawer = false;
        self.right_drawer = false;
    }
    pub fn open_terminal(&mut self, target: RouteTarget) {
        self.view = View::Terminal;
        self.requested_terminal_target = Some(target);
        self.left_drawer = false;
        self.right_drawer = false;
    }
    pub fn open_task(&mut self, target: RouteTarget) {
        if let Some(id) = &target.task_id {
            self.select_task(id.clone());
            self.requested_task_target = Some(target);
        }
    }
}

/// UI labels may be populated with the host's catalog without importing global app state.
pub const LABELS: &[(&str, &str)] = &[
    ("main", "메인 에이전트"),
    ("tasks", "작업"),
    ("terminal", "터미널"),
    ("devices", "에이전트·기기"),
    ("history", "이력"),
    ("settings", "프로젝트 설정"),
    ("original", "기존 화면"),
    ("light", "라이트"),
    ("dark", "다크"),
    ("workspaces", "워크스페이스"),
    ("add_project", "프로젝트 추가"),
    ("no_projects", "프로젝트를 선택하거나 추가해 주세요."),
    ("no_sessions", "연결된 독립 세션이 없습니다."),
    ("no_tasks", "배정된 작업이 없습니다."),
    ("project_scope", "프로젝트 범위"),
    ("select_project", "작업할 프로젝트"),
    ("workers", "작업자"),
    ("all", "전체"),
    ("attention", "확인 필요"),
    ("running", "실행 중"),
    ("task_detail", "작업 상세"),
    ("result_check", "결과 확인"),
    ("approval_check", "승인 확인"),
    ("rules", "이 프로젝트의 실행 규칙"),
    ("manage_devices", "기기 관리"),
    ("close", "닫기"),
    ("projects_open", "프로젝트 목록 열기"),
    ("workers_open", "작업자 패널 열기"),
    ("collapse_workers", "작업자 패널 접기"),
    ("all_history", "전체 기록"),
    ("back_main", "메인으로"),
    ("goal", "프로젝트 목표"),
    ("first_goal", "이 프로젝트에서 어떤 작업을 진행할까요?"),
    ("first_title", "첫 목표를 메인 에이전트에게 맡겨 주세요."),
    (
        "first_body",
        "아래 입력창에서 목표를 전달하세요. 연결된 세션과 실제 작업의 진행을 여기서 확인할 수 있습니다.",
    ),
    ("main_collects", "메인이 작업자 진행과 결과를 모읍니다."),
    (
        "no_main_summary",
        "아직 메인 에이전트가 제출한 진행 내용이 없습니다.",
    ),
    ("plan_assign", "계획·배정"),
    ("independent", "독립 실행"),
    ("collect", "결과 취합"),
    ("verify", "검증"),
    ("needs_check", "지금 확인할 작업"),
    (
        "check_help",
        "명령 승인과 제출된 결과를 해당 작업 안에서 확인하세요.",
    ),
    ("role_scope", "역할 · 수정 범위"),
    ("owner_device", "담당 · 실행 기기"),
    ("state", "상태"),
    ("confirm_plan", "배정안 확인 후 실행"),
    ("dispatch_task", "작업 실행 · 재시도"),
    ("no_filtered", "이 필터에 해당하는 작업이 없습니다."),
    ("work", "작업"),
    ("result", "결과"),
    ("git", "Git 변경"),
    ("scope", "수정 범위"),
    ("criteria", "완료 조건"),
    ("directory", "작업 폴더"),
    ("device", "실행 기기"),
    ("unavailable_task", "선택한 작업을 찾을 수 없습니다."),
    ("no_result", "아직 제출된 결과가 없습니다."),
    ("confirm_result", "이 결과 확인 완료"),
    ("rework", "보완 요청 작성"),
    ("open_git", "작업의 Git 변경 열기"),
    ("open_terminal", "원문 터미널 열기"),
    ("open_history", "작업 기록 열기"),
    ("no_history", "아직 이 프로젝트의 기록이 없습니다."),
    ("no_devices", "연결된 기기가 없습니다."),
    ("viewer_only", "열람·입력 기기 · 작업 실행 없음"),
    ("connection", "연결 상태"),
    ("approval_title", "명령 실행 승인 요청"),
    ("allow_once", "이번 명령 허용"),
    ("deny", "허용 안 함"),
    (
        "approval_note",
        "허용은 실행 시작 또는 성공을 의미하지 않습니다.",
    ),
    ("send", "전송"),
    ("composer_main", "메인에게 목표나 변경할 내용을 알려 주세요"),
    ("composer_task", "이 작업자에게 추가 내용을 알려 주세요"),
    (
        "input_unavailable",
        "이 대상의 직접 입력은 아직 연결되지 않았습니다.",
    ),
    ("send_hint", "Enter 전송 · Shift+Enter 줄바꿈"),
    (
        "input_limit",
        "입력은 16 KiB 이하여야 합니다. 초안은 보존됩니다.",
    ),
    ("name", "프로젝트 이름"),
    ("main_binding", "메인 세션"),
    ("unbound", "연결 안 됨"),
    ("provider", "메인 에이전트"),
    ("model", "모델"),
    ("effort", "추론 강도"),
    ("worker_providers", "허용할 작업자"),
    ("parallel_limit", "동시 실행 한도"),
    ("automation", "자동화"),
    ("worktrees", "작업별 독립 worktree"),
    (
        "settings_note",
        "설정 변경은 다음 배정부터 적용합니다. 기존 작업의 담당과 폴더는 유지됩니다.",
    ),
    ("save", "설정 저장"),
    ("cancel", "취소"),
    (
        "invalid_settings",
        "이름·폴더와 동시 실행 한도 1~8을 확인해 주세요.",
    ),
    ("unsupported", "아직 연결되지 않은 기능입니다."),
    (
        "project_independent",
        "프로젝트·작업·입력은 각각 독립적입니다.",
    ),
    ("state_plan_review", "계획 확인 대기"),
    ("state_queued", "실행 대기"),
    ("state_running", "실행 중"),
    ("state_approval", "승인 대기"),
    ("state_input", "사용자 입력 대기"),
    ("state_result", "결과 제출"),
    ("state_verification", "검증 실패"),
    ("state_confirmation", "검토 대기"),
    ("state_completed", "확인 완료"),
    ("state_failed", "실패"),
    ("state_cancelled", "취소됨"),
    ("state_unknown", "관측 확인 필요"),
];

pub(super) fn text<'a>(snapshot: &'a Snapshot, key: &str) -> &'a str {
    snapshot
        .labels
        .get(&format!("command_center.{key}"))
        .map(String::as_str)
        .unwrap_or_else(|| {
            LABELS
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| *value)
                .unwrap_or("")
        })
}

#[derive(Clone, Copy)]
pub(super) struct Colors {
    pub bg: egui::Color32,
    pub canvas: egui::Color32,
    pub raised: egui::Color32,
    pub line: egui::Color32,
    pub text: egui::Color32,
    pub muted: egui::Color32,
    pub accent: egui::Color32,
    pub green: egui::Color32,
    pub amber: egui::Color32,
    pub red: egui::Color32,
}

pub(super) fn colors(snapshot: &Snapshot) -> Colors {
    use egui::Color32 as C;
    if snapshot.dark_mode {
        Colors {
            bg: C::from_rgb(25, 28, 33),
            canvas: C::from_rgb(16, 18, 22),
            raised: C::from_rgb(37, 42, 50),
            line: C::from_rgb(48, 52, 61),
            text: C::from_rgb(225, 229, 235),
            muted: C::from_rgb(155, 165, 179),
            accent: C::from_rgb(98, 197, 232),
            green: C::from_rgb(120, 213, 171),
            amber: C::from_rgb(237, 193, 125),
            red: C::from_rgb(239, 156, 165),
        }
    } else {
        Colors {
            bg: C::from_rgb(243, 245, 247),
            canvas: C::WHITE,
            raised: C::from_rgb(230, 235, 240),
            line: C::from_rgb(201, 208, 217),
            text: C::from_rgb(34, 42, 52),
            muted: C::from_rgb(86, 100, 116),
            accent: C::from_rgb(8, 118, 146),
            green: C::from_rgb(22, 114, 71),
            amber: C::from_rgb(130, 86, 8),
            red: C::from_rgb(174, 51, 68),
        }
    }
}

pub(super) fn setup_style(ui: &mut egui::Ui, snapshot: &Snapshot) {
    let c = colors(snapshot);
    let scale = snapshot.font_scale.clamp(0.75, 2.0);
    let style = ui.style_mut();
    style.visuals.dark_mode = snapshot.dark_mode;
    style.visuals.override_text_color = Some(c.text);
    style.visuals.weak_text_color = Some(c.muted);
    style.visuals.panel_fill = c.bg;
    style.visuals.extreme_bg_color = c.canvas;
    style.visuals.window_fill = c.bg;
    style.visuals.selection.bg_fill = c.raised;
    style.visuals.selection.stroke = egui::Stroke::new(1.0, c.accent);
    for widget in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
    ] {
        widget.bg_fill = c.raised;
        widget.weak_bg_fill = c.raised;
        widget.bg_stroke = egui::Stroke::new(1.0, c.line);
        widget.fg_stroke = egui::Stroke::new(1.0, c.text);
        widget.corner_radius = egui::CornerRadius::same(4);
    }
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.interact_size.y = 34.0_f32.max(22.0 * scale + 10.0);
    for (kind, size) in [
        (egui::TextStyle::Body, 14.0),
        (egui::TextStyle::Button, 13.0),
        (egui::TextStyle::Small, 11.0),
        (egui::TextStyle::Heading, 21.0),
        (egui::TextStyle::Monospace, 12.0),
    ] {
        style.text_styles.insert(
            kind,
            egui::FontId::new(
                size * scale,
                if size == 12.0 {
                    egui::FontFamily::Monospace
                } else {
                    egui::FontFamily::Proportional
                },
            ),
        );
    }
}

pub(super) fn region<R>(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    fill: egui::Color32,
    id: &str,
    margin: f32,
    draw: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.painter().rect_filled(rect, 0.0, fill);
    let inner = rect.shrink(margin);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(id)
            .max_rect(inner)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    draw(&mut child)
}

pub(super) fn weak(ui: &mut egui::Ui, snapshot: &Snapshot, value: impl Into<String>) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(value.into())
                .small()
                .color(colors(snapshot).muted),
        )
        .wrap(),
    );
}

pub(super) fn rule(ui: &mut egui::Ui, snapshot: &Snapshot) {
    ui.add_space(8.0);
    let (r, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(
        r.x_range(),
        r.top(),
        egui::Stroke::new(1.0, colors(snapshot).line),
    );
    ui.add_space(8.0);
}

pub(super) fn action(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    label: &str,
    cap: &Capability,
) -> bool {
    let response = ui.add_enabled(cap.enabled, egui::Button::new(label));
    if !cap.enabled {
        response
            .clone()
            .on_disabled_hover_text(if cap.reason.is_empty() {
                text(snapshot, "unsupported")
            } else {
                &cap.reason
            });
    }
    response.clicked()
}

pub(super) fn project<'a>(state: &UiState, snapshot: &'a Snapshot) -> Option<&'a Project> {
    let selected = state
        .active_workspace
        .as_ref()
        .or(snapshot.selected_workspace_id.as_ref());
    snapshot.projects.iter().find(|p| Some(&p.id) == selected)
}

pub(super) fn selected_task<'a>(state: &UiState, p: &'a Project) -> Option<&'a Task> {
    p.tasks
        .iter()
        .find(|t| t.target.workspace_id == p.id && t.target.task_id == state.selected_task_id)
}

/// Draw into the host's remaining native content area. Never creates an app/window/process.
pub fn show(ui: &mut egui::Ui, state: &mut UiState, snapshot: &Snapshot) -> Vec<Intent> {
    let mut intents = Vec::new();
    if state.active_workspace != snapshot.selected_workspace_id {
        state
            .active_workspace
            .clone_from(&snapshot.selected_workspace_id);
        if state
            .requested_terminal_target
            .as_ref()
            .is_some_and(|t| Some(&t.workspace_id) == state.active_workspace.as_ref())
        {
            state.view = View::Terminal;
        } else if let Some(target) = state
            .requested_task_target
            .as_ref()
            .filter(|t| Some(&t.workspace_id) == state.active_workspace.as_ref())
        {
            state.view = View::Task;
            state.selected_task_id = target.task_id.clone();
        } else {
            state.view = View::Main;
            state.selected_task_id = None;
            state.requested_terminal_target = None;
            state.requested_task_target = None;
        }
        state.settings_draft = None;
        state.left_drawer = false;
        state.right_drawer = false;
    }
    let p = project(state, snapshot);
    let has_composer =
        p.is_some() && matches!(state.view, View::Main | View::Task | View::Terminal);
    let rect = ui.available_rect_before_wrap();
    let layout = LayoutRects::new(
        rect,
        state.monitor_collapsed,
        state.composer_focused,
        has_composer,
        snapshot.font_scale,
    );
    state.last_layout = Some(layout);
    state.terminal_target = p
        .and_then(|p| {
            if state.view == View::Task && state.detail_tab == DetailTab::Terminal {
                selected_task(state, p).map(|t| t.target.clone())
            } else if state.view == View::Terminal {
                state
                    .requested_terminal_target
                    .as_ref()
                    .filter(|t| t.workspace_id == p.id)
                    .cloned()
                    .or_else(|| selected_task(state, p).map(|t| t.target.clone()))
                    .or_else(|| Some(p.main_target.clone()))
            } else {
                None
            }
        })
        .filter(|target| target.session_id.is_some());
    let native_rect = if state.view == View::Task {
        egui::Rect::from_min_max(
            egui::pos2(
                layout.content.left() + 16.0,
                (layout.content.top() + 146.0).min(layout.content.bottom()),
            ),
            layout.content.max - egui::vec2(16.0, 8.0),
        )
    } else {
        layout.content
    };
    state.terminal_rect = state.terminal_target.as_ref().map(|_| native_rect);
    state.aux_target = p
        .and_then(|p| {
            (state.view == View::Task
                && matches!(state.detail_tab, DetailTab::Git | DetailTab::History))
            .then(|| selected_task(state, p).map(|t| t.target.clone()))
            .flatten()
        })
        .filter(|t| t.session_id.is_some());
    state.aux_rect = state.aux_target.as_ref().map(|_| native_rect);
    state.aux_tab = state.aux_target.as_ref().map(|_| state.detail_tab);
    if !layout.mobile {
        state.left_drawer = false;
    }
    if !layout.monitor_is_drawer {
        state.right_drawer = false;
    }
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        state.left_drawer = false;
        state.right_drawer = false;
        ui.memory_mut(|m| {
            if let Some(id) = m.focused() {
                m.surrender_focus(id)
            }
        });
    }
    let drawer_open = state.left_drawer || state.right_drawer;
    ui.scope(|ui| {
        setup_style(ui, snapshot);
        if drawer_open {
            ui.disable();
        }
        let c = colors(snapshot);
        ui.painter().rect_filled(rect, 0.0, c.canvas);
        region(ui, layout.header, c.bg, "cc-header", 0.0, |ui| {
            header(ui, state, snapshot, p, &layout, &mut intents)
        });
        if let Some(sidebar) = layout.sidebar {
            region(ui, sidebar, c.bg, "cc-projects", 12.0, |ui| {
                workers::projects(ui, state, snapshot, &mut intents)
            });
        }
        region(
            ui,
            layout.projectbar,
            c.canvas,
            "cc-projectbar",
            0.0,
            |ui| projectbar(ui, state, snapshot, p, &layout, &mut intents),
        );
        region(ui, layout.worktop, c.canvas, "cc-worktop", 0.0, |ui| {
            worktop(ui, state, snapshot, p, &layout, &mut intents)
        });
        if state.terminal_rect.is_none() {
            region(
                ui,
                layout.content,
                c.canvas,
                "cc-content",
                if layout.mobile { 16.0 } else { 28.0 },
                |ui| main::body(ui, state, snapshot, p, &mut intents),
            );
        }
        if has_composer {
            region(
                ui,
                layout.composer,
                c.canvas,
                "cc-composer",
                if layout.mobile { 12.0 } else { 18.0 },
                |ui| main::composer(ui, state, snapshot, p.unwrap(), &mut intents),
            );
        }
        if let Some(monitor) = layout.monitor {
            region(ui, monitor, c.bg, "cc-monitor", 16.0, |ui| {
                workers::monitor(ui, state, snapshot, p, &mut intents)
            });
        }
        region(ui, layout.footer, c.bg, "cc-footer", 3.0, |ui| {
            weak(
                ui,
                snapshot,
                if snapshot.status.is_empty() {
                    text(snapshot, "project_independent")
                } else {
                    &snapshot.status
                },
            )
        });
        for r in [layout.header, layout.projectbar, layout.worktop] {
            ui.painter()
                .hline(r.x_range(), r.bottom(), egui::Stroke::new(1.0, c.line));
        }
        if let Some(r) = layout.sidebar {
            ui.painter()
                .vline(r.right(), r.y_range(), egui::Stroke::new(1.0, c.line));
        }
        if let Some(r) = layout.monitor {
            ui.painter()
                .vline(r.left(), r.y_range(), egui::Stroke::new(1.0, c.line));
        }
    });
    if state.left_drawer || state.right_drawer {
        let left = state.left_drawer;
        let drawer = if left {
            layout.left_drawer()
        } else {
            layout.right_drawer()
        };
        egui::Area::new(egui::Id::new("cc-drawer"))
            .order(egui::Order::Foreground)
            .fixed_pos(rect.min)
            .constrain(false)
            .show(ui.ctx(), |ui| {
                setup_style(ui, snapshot);
                ui.set_min_size(rect.size());
                ui.painter()
                    .rect_filled(rect, 0.0, egui::Color32::from_black_alpha(130));
                let backdrop = ui.interact(rect, ui.id().with("backdrop"), egui::Sense::click());
                if backdrop.clicked()
                    && backdrop
                        .interact_pointer_pos()
                        .is_some_and(|pos| !drawer.contains(pos))
                {
                    state.left_drawer = false;
                    state.right_drawer = false;
                }
                region(
                    ui,
                    drawer,
                    colors(snapshot).bg,
                    "cc-drawer-content",
                    16.0,
                    |ui| {
                        if ui.button(text(snapshot, "close")).clicked() {
                            state.left_drawer = false;
                            state.right_drawer = false;
                        }
                        if left {
                            workers::projects(ui, state, snapshot, &mut intents);
                        } else {
                            workers::monitor(ui, state, snapshot, p, &mut intents);
                        }
                    },
                );
            });
    }
    ui.advance_cursor_after_rect(rect);
    intents
}

fn header(
    ui: &mut egui::Ui,
    state: &mut UiState,
    snapshot: &Snapshot,
    p: Option<&Project>,
    l: &LayoutRects,
    intents: &mut Vec<Intent>,
) {
    let drag = ui.interact(l.header, ui.id().with("titlebar-drag"), egui::Sense::drag());
    if drag.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
        if !l.mobile {
            ui.add_space(76.0);
            ui.label(
                egui::RichText::new("D")
                    .strong()
                    .color(colors(snapshot).accent),
            );
            ui.label(egui::RichText::new("Deppy Sijo").strong());
            ui.add_space(20.0);
        } else {
            ui.add_space(10.0);
            ui.label(egui::RichText::new("D").color(colors(snapshot).accent));
        }
        let actions_width =
            if l.mobile { 110.0 } else { 172.0 } * snapshot.font_scale.clamp(1.0, 2.0);
        egui::ScrollArea::horizontal()
            .id_salt("cc-navigation")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .max_width((ui.available_width() - actions_width).max(100.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (view, key) in [
                        (View::Main, "main"),
                        (View::Tasks, "tasks"),
                        (View::Terminal, "terminal"),
                        (View::Devices, "devices"),
                        (View::History, "history"),
                    ] {
                        let active =
                            state.view == view || (view == View::Tasks && state.view == View::Task);
                        let label = if view == View::Tasks {
                            format!("{} {}", text(snapshot, key), p.map_or(0, |p| p.tasks.len()))
                        } else {
                            text(snapshot, key).to_owned()
                        };
                        let nav = ui.add(
                            egui::Button::new(egui::RichText::new(label).color(if active {
                                colors(snapshot).text
                            } else {
                                colors(snapshot).muted
                            }))
                            .frame(false),
                        );
                        if active {
                            ui.painter().hline(
                                nav.rect.x_range(),
                                l.header.bottom() - 1.0,
                                egui::Stroke::new(2.0, colors(snapshot).accent),
                            );
                        }
                        if nav.clicked() {
                            state.view = view;
                            state.left_drawer = false;
                            state.right_drawer = false;
                            if let Some(p) = p {
                                match view {
                                    View::Terminal => {
                                        let target = selected_task(state, p)
                                            .map(|t| &t.target)
                                            .unwrap_or(&p.main_target)
                                            .clone();
                                        if target.session_id.is_some() {
                                            state.open_terminal(target.clone());
                                            intents.push(Intent::OpenSession(target));
                                        }
                                    }
                                    View::Devices => {
                                        if snapshot.manage_devices.enabled {
                                            intents.push(Intent::ManageDevices {
                                                workspace_id: Some(p.id.clone()),
                                            });
                                        }
                                    }
                                    View::History => {
                                        intents.push(Intent::OpenHistory(p.main_target.clone()))
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                });
            });
        if !l.mobile
            && ui
                .small_button(text(
                    snapshot,
                    if snapshot.dark_mode { "light" } else { "dark" },
                ))
                .clicked()
        {
            intents.push(Intent::SetTheme {
                dark_mode: !snapshot.dark_mode,
            });
        }
        if ui.small_button(text(snapshot, "original")).clicked() {
            intents.push(Intent::UseOriginalLayout);
        }
    });
}

fn projectbar(
    ui: &mut egui::Ui,
    state: &mut UiState,
    snapshot: &Snapshot,
    p: Option<&Project>,
    l: &LayoutRects,
    intents: &mut Vec<Intent>,
) {
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(if l.mobile { 16 } else { 24 }, 12))
        .show(ui, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !l.mobile
                    && !l.monitor_is_drawer
                    && ui
                        .button("◫")
                        .on_hover_text(text(snapshot, "collapse_workers"))
                        .clicked()
                {
                    state.monitor_collapsed = !state.monitor_collapsed;
                }
                if ui.button(text(snapshot, "settings")).clicked() {
                    state.view = View::Settings;
                    state.settings_draft = None;
                }
                if !l.mobile {
                    egui::ComboBox::from_id_salt("cc-project-select")
                        .selected_text(
                            p.map(|p| p.name.as_str())
                                .unwrap_or(text(snapshot, "select_project")),
                        )
                        .width(130.0)
                        .show_ui(ui, |ui| {
                            for p in &snapshot.projects {
                                if ui
                                    .selectable_label(
                                        state.active_workspace.as_ref() == Some(&p.id),
                                        &p.name,
                                    )
                                    .clicked()
                                {
                                    intents.push(Intent::SelectProject {
                                        workspace_id: p.id.clone(),
                                    });
                                }
                            }
                        });
                }
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), l.projectbar.height() - 24.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_min_width(ui.max_rect().width());
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(p.map(|p| p.name.as_str()).unwrap_or("Deppy"))
                                    .size(19.0 * snapshot.font_scale)
                                    .strong(),
                            )
                            .truncate(),
                        );
                        if let Some(p) = p {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!(
                                        "● {} · {}",
                                        p.directory,
                                        text(snapshot, "project_scope")
                                    ))
                                    .small()
                                    .color(colors(snapshot).muted),
                                )
                                .truncate(),
                            )
                            .on_hover_text(&p.directory);
                        }
                    },
                );
            });
        });
}

fn worktop(
    ui: &mut egui::Ui,
    state: &mut UiState,
    snapshot: &Snapshot,
    p: Option<&Project>,
    l: &LayoutRects,
    intents: &mut Vec<Intent>,
) {
    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.add_space(if l.mobile { 10.0 } else { 24.0 });
        if l.mobile
            && ui
                .button("☰")
                .on_hover_text(text(snapshot, "projects_open"))
                .clicked()
        {
            state.left_drawer = true;
            state.right_drawer = false;
        }
        let title = match state.view {
            View::Main => text(snapshot, "main"),
            View::Tasks | View::Task => text(snapshot, "tasks"),
            View::Terminal => text(snapshot, "terminal"),
            View::Devices => text(snapshot, "devices"),
            View::History => text(snapshot, "history"),
            View::Settings => text(snapshot, "settings"),
        };
        ui.label(egui::RichText::new(title).strong());
        if !l.mobile
            && state.view == View::Main
            && let Some(p) = p
        {
            weak(
                ui,
                snapshot,
                format!("{} · {} {}", p.main_provider, p.main_model, p.main_effort),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(12.0);
            if (l.monitor_is_drawer || state.monitor_collapsed)
                && ui
                    .button(format!(
                        "{} {}",
                        text(snapshot, "workers"),
                        p.map_or(0, |p| p.tasks.len())
                    ))
                    .on_hover_text(text(snapshot, "workers_open"))
                    .clicked()
            {
                if l.monitor_is_drawer {
                    state.right_drawer = true;
                    state.left_drawer = false;
                } else {
                    state.monitor_collapsed = false;
                }
            }
            if state.view != View::Main {
                if ui.small_button(text(snapshot, "back_main")).clicked() {
                    state.view = View::Main;
                }
            } else if !l.mobile && ui.small_button(text(snapshot, "all_history")).clicked() {
                state.view = View::History;
                if let Some(p) = p {
                    intents.push(Intent::OpenHistory(p.main_target.clone()));
                }
            }
        });
    });
}

#[cfg(test)]
mod tests;
