//! Native structured Codex Agent Sessions panel.
//!
//! It renders data from `codex app-server`, not a parsed terminal transcript.
//! The existing PTY workspace remains untouched and continues to display raw
//! terminal bytes exactly as before.

use crate::agent_session::{
    AgentApprovalDecision, AgentSession, AgentSessionEvent, AgentSessionId, AgentSessionStatus,
};
use crate::agent_surface::{
    AgentProvider, AgentSurfaceId, AgentSurfaceSnapshot, AgentTransport, AgentVisualState,
};
use crate::codex_app_server::{CodexAppServerClient, CodexAppServerEvent, CodexAppServerOptions};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionStatusNotice {
    pub workspace_id: String,
    pub session_id: AgentSessionId,
    pub title: String,
    pub status: AgentSessionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSessionsRequest {
    FocusPty(AgentSurfaceId),
    InterruptPty(AgentSurfaceId),
}

/// App-owned UI/controller for one shared local Codex App Server connection.
pub struct AgentSessionsUi {
    open: bool,
    client: Option<CodexAppServerClient>,
    sessions: Vec<AgentSession>,
    pty_surfaces: Vec<AgentSurfaceSnapshot>,
    selected_surface: Option<AgentSurfaceId>,
    selected_session: Option<AgentSessionId>,
    selected_item: Option<String>,
    new_prompt: String,
    new_model: String,
    follow_up: String,
    focus_new_prompt: bool,
    focus_follow_up: bool,
    status_notices: Vec<AgentSessionStatusNotice>,
    transport_error: Option<String>,
}

impl AgentSessionsUi {
    pub fn new() -> Self {
        Self {
            open: false,
            client: None,
            sessions: Vec::new(),
            pty_surfaces: Vec::new(),
            selected_surface: None,
            selected_session: None,
            selected_item: None,
            new_prompt: String::new(),
            new_model: String::new(),
            follow_up: String::new(),
            focus_new_prompt: false,
            focus_follow_up: false,
            status_notices: Vec::new(),
            transport_error: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    pub fn open(&mut self) {
        self.open = true;
    }

    pub fn session_ids(&self) -> Vec<String> {
        self.sessions
            .iter()
            .map(|session| session.id.clone())
            .collect()
    }

    pub fn drain_status_notices(&mut self) -> Vec<AgentSessionStatusNotice> {
        std::mem::take(&mut self.status_notices)
    }

    pub fn open_session(&mut self, session_id: &str) -> bool {
        if !self.sessions.iter().any(|session| session.id == session_id) {
            return false;
        }
        self.open = true;
        self.selected_session = Some(session_id.to_owned());
        self.selected_surface = Some(AgentSurfaceId::Structured {
            session_id: session_id.to_owned(),
        });
        self.selected_item = None;
        self.follow_up.clear();
        true
    }

    pub fn selected_surface_snapshot(&self) -> Option<AgentSurfaceSnapshot> {
        match self.selected_surface.as_ref()? {
            AgentSurfaceId::Pty { .. } => self
                .pty_surfaces
                .iter()
                .find(|surface| surface.id == *self.selected_surface.as_ref().expect("selected"))
                .cloned(),
            AgentSurfaceId::Structured { session_id } => {
                let session = self
                    .sessions
                    .iter()
                    .find(|session| &session.id == session_id)?;
                Some(AgentSurfaceSnapshot {
                    id: AgentSurfaceId::Structured {
                        session_id: session.id.clone(),
                    },
                    provider: AgentProvider::Codex,
                    transport: AgentTransport::AppServer,
                    title: one_line_title(&session.prompt),
                    model: None,
                    effort: None,
                    context_pct: None,
                    state: AgentVisualState::from_structured(session.status),
                })
            }
        }
    }

    pub fn selected_pending_approval_count(&self) -> usize {
        let Some(AgentSurfaceId::Structured { session_id }) = &self.selected_surface else {
            return 0;
        };
        self.sessions
            .iter()
            .find(|session| &session.id == session_id)
            .map_or(0, |session| session.approvals.len())
    }

    pub fn select_relative(&mut self, delta: isize) -> Option<AgentSurfaceId> {
        let ids = self.surface_ids();
        if ids.is_empty() {
            self.selected_surface = None;
            self.selected_session = None;
            return None;
        }
        let next = self
            .selected_surface
            .as_ref()
            .and_then(|selected| ids.iter().position(|id| id == selected))
            .map_or_else(
                || if delta < 0 { ids.len() - 1 } else { 0 },
                |current| (current as isize + delta).rem_euclid(ids.len() as isize) as usize,
            );
        let selected = ids[next].clone();
        self.select_surface(selected.clone());
        self.open = true;
        Some(selected)
    }

    pub fn open_new_prompt(&mut self) {
        self.open = true;
        self.focus_new_prompt = true;
    }

    pub fn focus_selected_input(&mut self) -> Option<AgentSessionsRequest> {
        self.open = true;
        match self.selected_surface.clone()? {
            id @ AgentSurfaceId::Pty { .. } => Some(AgentSessionsRequest::FocusPty(id)),
            AgentSurfaceId::Structured { session_id } => {
                let has_thread = self
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .is_some_and(|session| session.thread_id.is_some());
                if has_thread {
                    self.focus_follow_up = true;
                } else {
                    self.focus_new_prompt = true;
                }
                None
            }
        }
    }

    pub fn interrupt_selected(
        &mut self,
        ctx: &egui::Context,
    ) -> anyhow::Result<Option<AgentSessionsRequest>> {
        let Some(selected) = self.selected_surface.clone() else {
            anyhow::bail!("선택된 에이전트가 없습니다");
        };
        match selected {
            id @ AgentSurfaceId::Pty { .. } => Ok(Some(AgentSessionsRequest::InterruptPty(id))),
            AgentSurfaceId::Structured { session_id } => {
                self.client
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Codex App Server 연결이 없습니다"))?
                    .interrupt(session_id)?;
                ctx.request_repaint();
                Ok(None)
            }
        }
    }

    pub fn approve_selected_once(&mut self, ctx: &egui::Context) -> anyhow::Result<()> {
        self.respond_selected_approval(AgentApprovalDecision::Accept, ctx)
    }

    pub fn reject_selected(&mut self, ctx: &egui::Context) -> anyhow::Result<()> {
        self.respond_selected_approval(AgentApprovalDecision::Decline, ctx)
    }

    fn respond_selected_approval(
        &mut self,
        decision: AgentApprovalDecision,
        ctx: &egui::Context,
    ) -> anyhow::Result<()> {
        let Some(AgentSurfaceId::Structured { session_id }) = self.selected_surface.clone() else {
            anyhow::bail!("선택된 APP 에이전트가 없습니다");
        };
        let session = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .ok_or_else(|| anyhow::anyhow!("선택된 APP 세션을 찾을 수 없습니다"))?;
        if session.approvals.len() != 1 {
            anyhow::bail!(
                "승인 요청이 정확히 하나일 때만 실행할 수 있습니다 (현재 {})",
                session.approvals.len()
            );
        }
        let request_key = session.approvals[0].request_key.clone();
        self.client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Codex App Server 연결이 없습니다"))?
            .respond_approval(session_id, request_key, decision)?;
        ctx.request_repaint();
        Ok(())
    }

    pub fn shutdown(&mut self) {
        if let Some(client) = self.client.as_mut() {
            client.shutdown();
        }
        self.client = None;
    }

    /// Poll continuously even when the window is closed so an active structured
    /// session reaches a consistent terminal state in the background.
    pub fn poll(&mut self) {
        let mut connection_stopped = false;
        let events = self
            .client
            .as_ref()
            .map(CodexAppServerClient::drain_events)
            .unwrap_or_default();
        for event in events {
            match event {
                CodexAppServerEvent::Session { session_id, event } => {
                    self.apply_session_event(&session_id, event);
                }
                CodexAppServerEvent::TransportError { message } => {
                    self.transport_error = Some(message);
                }
                CodexAppServerEvent::ConnectionStopped => connection_stopped = true,
            }
        }
        if connection_stopped {
            let active = self
                .sessions
                .iter()
                .filter(|session| !session.status.is_terminal())
                .map(|session| session.id.clone())
                .collect::<Vec<_>>();
            for session_id in active {
                self.apply_session_event(&session_id, AgentSessionEvent::Stopped);
            }
            self.client = None;
        }
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        workspace_id: &str,
        workspace_cwd: Option<String>,
        pty_surfaces: Vec<AgentSurfaceSnapshot>,
    ) -> Vec<AgentSessionsRequest> {
        self.pty_surfaces = pty_surfaces;
        if matches!(self.selected_surface, Some(AgentSurfaceId::Pty { .. }))
            && !self
                .pty_surfaces
                .iter()
                .any(|surface| Some(&surface.id) == self.selected_surface.as_ref())
        {
            self.selected_surface = None;
        }
        self.poll();
        if !self.open {
            return Vec::new();
        }

        let mut window_open = self.open;
        let mut actions = Vec::new();
        let mut requests = Vec::new();
        egui::Window::new("Agents · [APP] Codex")
            .open(&mut window_open)
            .default_width(960.0)
            .default_height(650.0)
            .min_width(700.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("[APP] Codex");
                    ui.weak("structured thread · turn · item stream");
                });
                ui.label(
                    "PTY 터미널 출력은 변경하지 않습니다. 이 패널은 Codex App Server의 구조화 이벤트만 표시합니다.",
                );
                ui.horizontal(|ui| {
                    ui.weak("작업 폴더");
                    ui.monospace(
                        workspace_cwd
                            .as_deref()
                            .filter(|path| !path.is_empty())
                            .unwrap_or("현재 앱 작업 폴더"),
                    );
                });
                if let Some(error) = &self.transport_error {
                    ui.colored_label(egui::Color32::from_rgb(0xff, 0x7b, 0x72), error);
                }
                crate::ui::hairline(ui);

                ui.label("새 작업");
                let prompt_response = ui.add_sized(
                    [ui.available_width(), 72.0],
                    egui::TextEdit::multiline(&mut self.new_prompt)
                        .id_salt("agent-new-prompt")
                        .hint_text("Codex에게 맡길 작업을 입력하세요")
                        .desired_rows(3),
                );
                if self.focus_new_prompt {
                    prompt_response.request_focus();
                    self.focus_new_prompt = false;
                }
                ui.horizontal(|ui| {
                    ui.label("모델 (선택)");
                    ui.add_sized(
                        [180.0, 24.0],
                        egui::TextEdit::singleline(&mut self.new_model)
                            .hint_text("기본 Codex 모델"),
                    );
                    let can_start = !self.new_prompt.trim().is_empty();
                    if ui
                        .add_enabled(can_start, egui::Button::new("Codex 실행"))
                        .clicked()
                    {
                        actions.push(PanelAction::Start {
                            workspace_id: workspace_id.to_owned(),
                            prompt: std::mem::take(&mut self.new_prompt),
                            model: std::mem::take(&mut self.new_model),
                            cwd: workspace_cwd.clone(),
                        });
                    }
                });

                crate::ui::hairline(ui);
                self.render_surface_tabs(ui);
                crate::ui::hairline(ui);

                match self.selected_surface_snapshot() {
                    Some(surface) if surface.transport == AgentTransport::Pty => {
                        self.render_pty_surface(ui, surface, &mut actions);
                    }
                    Some(_) => {
                        if let Some(session) = self.selected_session_snapshot() {
                            self.render_session(ui, session, workspace_cwd.clone(), &mut actions);
                        }
                    }
                    None => {
                        ui.weak("탐색할 에이전트를 선택하거나 새 Codex APP 작업을 시작하세요.");
                    }
                }
            });
        self.open = window_open;

        for action in actions {
            if let Some(request) = self.apply_action(action, ctx) {
                requests.push(request);
            }
        }
        requests
    }

    fn render_surface_tabs(&mut self, ui: &mut egui::Ui) {
        ui.strong("에이전트");
        let pty_tabs = self
            .pty_surfaces
            .iter()
            .map(|surface| {
                let label = format!(
                    "[{}] {} · {}",
                    surface.transport.badge(),
                    surface.provider.label(),
                    surface.title
                );
                (surface.id.clone(), label, surface.state)
            })
            .collect::<Vec<_>>();
        let app_tabs = self
            .sessions
            .iter()
            .map(|session| {
                (
                    AgentSurfaceId::Structured {
                        session_id: session.id.clone(),
                    },
                    format!(
                        "[APP] Codex · {} · {}",
                        short_id(&session.id),
                        session.status.label()
                    ),
                    AgentVisualState::from_structured(session.status),
                )
            })
            .collect::<Vec<_>>();
        ui.horizontal_wrapped(|ui| {
            for (id, label, state) in pty_tabs.into_iter().chain(app_tabs) {
                ui.colored_label(crate::ui::agent_visuals::status_color(state), "●");
                let selected = self.selected_surface.as_ref() == Some(&id);
                if ui.selectable_label(selected, label).clicked() {
                    self.select_surface(id);
                }
            }
        });
    }

    fn render_pty_surface(
        &mut self,
        ui: &mut egui::Ui,
        surface: AgentSurfaceSnapshot,
        actions: &mut Vec<PanelAction>,
    ) {
        ui.horizontal(|ui| {
            ui.colored_label(
                crate::ui::agent_visuals::status_color(surface.state),
                format!(
                    "● [{}] {}",
                    surface.transport.badge(),
                    surface.provider.label()
                ),
            );
            ui.strong(&surface.title);
        });
        ui.horizontal_wrapped(|ui| {
            if let Some(model) = &surface.model {
                ui.weak("model");
                ui.monospace(model);
            }
            if let Some(effort) = &surface.effort {
                ui.weak("effort");
                ui.monospace(effort);
            }
            if let Some(context_pct) = surface.context_pct {
                ui.weak(format!("ctx {context_pct}%"));
            }
        });
        ui.label("이 에이전트는 기존 PTY 터미널에서 실행됩니다. 원본 ANSI 출력과 IME 입력 경로는 그대로 유지됩니다.");
        ui.horizontal(|ui| {
            if ui.button("터미널로 이동").clicked() {
                actions.push(PanelAction::FocusPty(surface.id.clone()));
            }
            if matches!(
                surface.state,
                AgentVisualState::Active | AgentVisualState::Waiting
            ) && ui.button("중단 (Ctrl-C)").clicked()
            {
                actions.push(PanelAction::InterruptPty(surface.id));
            }
        });
    }

    fn selected_session_snapshot(&self) -> Option<AgentSession> {
        self.selected_session.as_ref().and_then(|id| {
            self.sessions
                .iter()
                .find(|session| &session.id == id)
                .cloned()
        })
    }

    fn render_session(
        &mut self,
        ui: &mut egui::Ui,
        session: AgentSession,
        workspace_cwd: Option<String>,
        actions: &mut Vec<PanelAction>,
    ) {
        ui.horizontal(|ui| {
            let color = status_color(session.status);
            ui.colored_label(color, format!("● {}", session.status.label()));
            if let Some(thread_id) = &session.thread_id {
                ui.weak("thread");
                ui.monospace(short_id(thread_id));
            }
            if matches!(
                session.status,
                AgentSessionStatus::Running | AgentSessionStatus::AwaitingApproval
            ) && ui.button("중단").clicked()
            {
                actions.push(PanelAction::Interrupt(session.id.clone()));
            }
            if session.status == AgentSessionStatus::Completed && ui.button("완료 확인").clicked()
            {
                actions.push(PanelAction::Acknowledge(session.id.clone()));
            }
        });
        ui.horizontal(|ui| {
            ui.weak("요청");
            ui.add(egui::Label::new(&session.prompt).truncate())
                .on_hover_text(&session.prompt);
        });
        if let Some(cwd) = &session.cwd {
            ui.horizontal(|ui| {
                ui.weak("cwd");
                ui.monospace(cwd);
            });
        }
        if let Some(error) = &session.error {
            ui.colored_label(egui::Color32::from_rgb(0xff, 0x7b, 0x72), error);
        }

        for approval in &session.approvals {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xff, 0xbf, 0x69),
                        approval.kind.label(),
                    );
                    ui.strong(approval.command.as_deref().unwrap_or("사용자 승인 필요"));
                });
                if let Some(reason) = &approval.reason {
                    ui.label(reason);
                }
                if let Some(cwd) = &approval.cwd {
                    ui.monospace(cwd);
                }
                ui.horizontal(|ui| {
                    if ui.button("이번만 허용").clicked() {
                        actions.push(PanelAction::Approval {
                            session_id: session.id.clone(),
                            request_key: approval.request_key.clone(),
                            decision: AgentApprovalDecision::Accept,
                        });
                    }
                    if ui.button("세션 동안 허용").clicked() {
                        actions.push(PanelAction::Approval {
                            session_id: session.id.clone(),
                            request_key: approval.request_key.clone(),
                            decision: AgentApprovalDecision::AcceptForSession,
                        });
                    }
                    if ui.button("거절").clicked() {
                        actions.push(PanelAction::Approval {
                            session_id: session.id.clone(),
                            request_key: approval.request_key.clone(),
                            decision: AgentApprovalDecision::Decline,
                        });
                    }
                    if ui.button("작업 취소").clicked() {
                        actions.push(PanelAction::Approval {
                            session_id: session.id.clone(),
                            request_key: approval.request_key.clone(),
                            decision: AgentApprovalDecision::Cancel,
                        });
                    }
                });
            });
            ui.add_space(4.0);
        }

        ui.strong("구조화 결과");
        let rows = session.table_rows();
        egui::ScrollArea::vertical()
            .id_salt(("agent-session-table", &session.id))
            .max_height(230.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new(("agent-session-grid", &session.id))
                    .striped(true)
                    .min_col_width(64.0)
                    .show(ui, |ui| {
                        ui.strong("상태");
                        ui.strong("유형");
                        ui.strong("작업");
                        ui.strong("위치");
                        ui.strong("결과");
                        ui.end_row();
                        for row in rows {
                            let item_id = row.item_id;
                            let selected = self.selected_item.as_deref() == Some(item_id.as_str());
                            let state_response = ui.add_sized(
                                [90.0, 20.0],
                                egui::Label::new(row.state)
                                    .sense(egui::Sense::click())
                                    .truncate(),
                            );
                            let kind_response = ui.add_sized(
                                [86.0, 20.0],
                                egui::Label::new(row.kind)
                                    .sense(egui::Sense::click())
                                    .truncate(),
                            );
                            let subject_response = ui.add_sized(
                                [230.0, 20.0],
                                egui::Label::new(row.subject)
                                    .sense(egui::Sense::click())
                                    .truncate(),
                            );
                            let location_response = ui.add_sized(
                                [160.0, 20.0],
                                egui::Label::new(row.location)
                                    .sense(egui::Sense::click())
                                    .truncate(),
                            );
                            let outcome_response = ui.add_sized(
                                [220.0, 20.0],
                                egui::Label::new(row.outcome)
                                    .sense(egui::Sense::click())
                                    .truncate(),
                            );
                            if state_response.clicked()
                                || kind_response.clicked()
                                || subject_response.clicked()
                                || location_response.clicked()
                                || outcome_response.clicked()
                                || selected
                            {
                                self.selected_item = Some(item_id);
                            }
                            ui.end_row();
                        }
                    });
            });

        if let Some(item_id) = &self.selected_item
            && let Some(item) = session.items.iter().find(|item| &item.id == item_id)
        {
            ui.add_space(6.0);
            ui.strong(format!("상세 · {}", item.kind.label()));
            if let Some(detail) = &item.detail {
                ui.label(detail);
            }
            if !item.output.is_empty() {
                egui::ScrollArea::vertical()
                    .id_salt(("agent-session-detail", &session.id, &item.id))
                    .max_height(120.0)
                    .show(ui, |ui| {
                        ui.monospace(&item.output);
                    });
            }
        }

        if session.thread_id.is_some()
            && !matches!(
                session.status,
                AgentSessionStatus::Starting
                    | AgentSessionStatus::Running
                    | AgentSessionStatus::AwaitingApproval
            )
        {
            ui.add_space(6.0);
            crate::ui::hairline(ui);
            ui.label("후속 작업");
            let follow_up_response = ui.add_sized(
                [ui.available_width(), 48.0],
                egui::TextEdit::multiline(&mut self.follow_up)
                    .hint_text("같은 thread에 다음 작업을 보냅니다")
                    .desired_rows(2),
            );
            if self.focus_follow_up {
                follow_up_response.request_focus();
                self.focus_follow_up = false;
            }
            if ui
                .add_enabled(
                    !self.follow_up.trim().is_empty(),
                    egui::Button::new("후속 작업 전송"),
                )
                .clicked()
            {
                actions.push(PanelAction::Submit {
                    session_id: session.id,
                    prompt: std::mem::take(&mut self.follow_up),
                    model: String::new(),
                    cwd: workspace_cwd,
                });
            }
        }
    }

    fn apply_action(
        &mut self,
        action: PanelAction,
        ctx: &egui::Context,
    ) -> Option<AgentSessionsRequest> {
        let mut external_request = None;
        match action {
            PanelAction::Start {
                workspace_id,
                prompt,
                model,
                cwd,
            } => self.start(workspace_id, prompt, model, cwd, ctx),
            PanelAction::Submit {
                session_id,
                prompt,
                model,
                cwd,
            } => {
                let result = self
                    .client
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("Codex App Server 연결이 없습니다. 새 세션을 시작하세요.")
                    })
                    .and_then(|client| {
                        client.submit_turn(session_id.clone(), prompt, cwd, non_empty(model))
                    });
                if let Err(error) = result {
                    self.fail_session(&session_id, format!("후속 작업 전송 실패: {error:#}"));
                }
            }
            PanelAction::Interrupt(session_id) => {
                let result = self
                    .client
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Codex App Server 연결이 없습니다"))
                    .and_then(|client| client.interrupt(session_id.clone()));
                if let Err(error) = result {
                    self.fail_session(&session_id, format!("작업 중단 실패: {error:#}"));
                }
            }
            PanelAction::Acknowledge(session_id) => {
                if let Some(session) = self
                    .sessions
                    .iter_mut()
                    .find(|session| session.id == session_id)
                {
                    session.acknowledge_completion();
                }
            }
            PanelAction::Approval {
                session_id,
                request_key,
                decision,
            } => {
                let result = self
                    .client
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Codex App Server 연결이 없습니다"))
                    .and_then(|client| {
                        client.respond_approval(session_id.clone(), request_key, decision)
                    });
                if let Err(error) = result {
                    self.fail_session(&session_id, format!("승인 응답 전송 실패: {error:#}"));
                }
            }
            PanelAction::FocusPty(id) => {
                external_request = Some(AgentSessionsRequest::FocusPty(id));
            }
            PanelAction::InterruptPty(id) => {
                external_request = Some(AgentSessionsRequest::InterruptPty(id));
            }
        }
        ctx.request_repaint();
        external_request
    }

    fn start(
        &mut self,
        workspace_id: String,
        prompt: String,
        model: String,
        cwd: Option<String>,
        ctx: &egui::Context,
    ) {
        let session_id = uuid::Uuid::new_v4().to_string();
        let mut session = AgentSession::new(session_id.clone(), prompt.clone(), cwd.clone());
        session.workspace_id = Some(workspace_id);
        let result = if self.client.is_none() {
            match CodexAppServerClient::spawn(CodexAppServerOptions::default(), ctx.clone()) {
                Ok(client) => {
                    self.transport_error = None;
                    self.client = Some(client);
                    Ok(())
                }
                Err(error) => Err(error),
            }
        } else {
            Ok(())
        }
        .and_then(|()| {
            self.client
                .as_ref()
                .expect("성공한 App Server client가 존재")
                .start_session(session_id.clone(), prompt, cwd, non_empty(model))
        });
        if let Err(error) = result {
            session.apply(AgentSessionEvent::Failed {
                message: format!("Codex 실행 실패: {error:#}"),
            });
            self.transport_error = Some(format!("Codex App Server 연결 실패: {error:#}"));
        }
        let failed = session.status == AgentSessionStatus::Failed;
        self.sessions.push(session);
        self.selected_session = Some(session_id.clone());
        self.selected_surface = Some(AgentSurfaceId::Structured { session_id });
        self.selected_item = None;
        if failed {
            let selected = self.selected_session.clone().expect("방금 선택한 세션");
            self.queue_current_status_notice(&selected);
        }
    }

    fn fail_session(&mut self, session_id: &str, message: String) {
        self.apply_session_event(session_id, AgentSessionEvent::Failed { message });
    }

    fn apply_session_event(&mut self, session_id: &str, event: AgentSessionEvent) {
        let changed = self
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
            .is_some_and(|session| {
                let before = session.status;
                session.apply(event);
                session.status != before
            });
        if changed {
            self.queue_current_status_notice(session_id);
        }
    }

    fn queue_current_status_notice(&mut self, session_id: &str) {
        let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        let Some(workspace_id) = session.workspace_id.clone() else {
            return;
        };
        self.status_notices.push(AgentSessionStatusNotice {
            workspace_id,
            session_id: session.id.clone(),
            title: one_line_title(&session.prompt),
            status: session.status,
        });
    }

    fn surface_ids(&self) -> Vec<AgentSurfaceId> {
        self.pty_surfaces
            .iter()
            .map(|surface| surface.id.clone())
            .chain(
                self.sessions
                    .iter()
                    .map(|session| AgentSurfaceId::Structured {
                        session_id: session.id.clone(),
                    }),
            )
            .collect()
    }

    fn select_surface(&mut self, id: AgentSurfaceId) {
        self.selected_session = match &id {
            AgentSurfaceId::Structured { session_id } => Some(session_id.clone()),
            AgentSurfaceId::Pty { .. } => None,
        };
        self.selected_surface = Some(id);
        self.selected_item = None;
        self.follow_up.clear();
    }
}

enum PanelAction {
    Start {
        workspace_id: String,
        prompt: String,
        model: String,
        cwd: Option<String>,
    },
    Submit {
        session_id: AgentSessionId,
        prompt: String,
        model: String,
        cwd: Option<String>,
    },
    Interrupt(AgentSessionId),
    Acknowledge(AgentSessionId),
    Approval {
        session_id: AgentSessionId,
        request_key: String,
        decision: AgentApprovalDecision,
    },
    FocusPty(AgentSurfaceId),
    InterruptPty(AgentSurfaceId),
}

fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

fn one_line_title(value: &str) -> String {
    let title = value.lines().next().unwrap_or_default().trim();
    if title.is_empty() {
        "Codex 작업".to_owned()
    } else {
        title.chars().take(80).collect()
    }
}

fn status_color(status: AgentSessionStatus) -> egui::Color32 {
    crate::ui::agent_visuals::status_color(crate::agent_surface::AgentVisualState::from_structured(
        status,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_session::{AgentApproval, AgentApprovalKind};

    fn pty_surface(id: u64) -> AgentSurfaceSnapshot {
        AgentSurfaceSnapshot {
            id: AgentSurfaceId::Pty {
                workspace_id: "ws-1".to_owned(),
                pane_id: format!("pane-{id}"),
                session_id: runtime::SessionId(id),
            },
            provider: AgentProvider::Codex,
            transport: AgentTransport::Pty,
            title: format!("PTY {id}"),
            model: None,
            effort: None,
            context_pct: None,
            state: AgentVisualState::Idle,
        }
    }

    fn approval(key: &str) -> AgentApproval {
        AgentApproval {
            request_key: key.to_owned(),
            kind: AgentApprovalKind::CommandExecution,
            thread_id: "thread-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            item_id: "item-1".to_owned(),
            reason: None,
            command: Some("cargo test".to_owned()),
            cwd: None,
        }
    }

    #[test]
    fn relative_selection_wraps_across_pty_and_app_surfaces() {
        let mut ui = AgentSessionsUi::new();
        ui.pty_surfaces = vec![pty_surface(1), pty_surface(2)];
        ui.sessions.push(AgentSession::new(
            "app-1".to_owned(),
            "review".to_owned(),
            None,
        ));

        assert!(matches!(
            ui.select_relative(1),
            Some(AgentSurfaceId::Pty {
                session_id: runtime::SessionId(1),
                ..
            })
        ));
        assert!(
            matches!(ui.select_relative(-1), Some(AgentSurfaceId::Structured { session_id }) if session_id == "app-1")
        );
        assert!(matches!(
            ui.select_relative(1),
            Some(AgentSurfaceId::Pty {
                session_id: runtime::SessionId(1),
                ..
            })
        ));
    }

    #[test]
    fn structured_status_notice_keeps_exact_workspace_and_click_target() {
        let mut ui = AgentSessionsUi::new();
        let mut session =
            AgentSession::new("app-1".to_owned(), "First line\nmore".to_owned(), None);
        session.workspace_id = Some("ws-1".to_owned());
        ui.sessions.push(session);
        ui.apply_session_event(
            "app-1",
            AgentSessionEvent::TurnCompleted {
                status: "completed".to_owned(),
            },
        );

        assert_eq!(
            ui.drain_status_notices(),
            vec![AgentSessionStatusNotice {
                workspace_id: "ws-1".to_owned(),
                session_id: "app-1".to_owned(),
                title: "First line".to_owned(),
                status: AgentSessionStatus::Completed,
            }]
        );
        assert!(ui.open_session("app-1"));
        assert!(matches!(
            ui.selected_surface,
            Some(AgentSurfaceId::Structured { ref session_id }) if session_id == "app-1"
        ));
    }

    #[test]
    fn shortcut_approval_rechecks_zero_one_and_many_pending_requests() {
        let mut ui = AgentSessionsUi::new();
        ui.sessions.push(AgentSession::new(
            "app-1".to_owned(),
            "review".to_owned(),
            None,
        ));
        ui.open_session("app-1");
        let ctx = egui::Context::default();

        let zero = ui.approve_selected_once(&ctx).unwrap_err().to_string();
        assert!(zero.contains("현재 0"));

        ui.sessions[0].approvals.push(approval("one"));
        let one = ui.approve_selected_once(&ctx).unwrap_err().to_string();
        assert!(one.contains("App Server 연결"));

        ui.sessions[0].approvals.push(approval("two"));
        let many = ui.reject_selected(&ctx).unwrap_err().to_string();
        assert!(many.contains("현재 2"));
    }
}
