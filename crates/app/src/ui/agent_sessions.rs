//! Native structured Codex Agent Sessions panel.
//!
//! It renders data from `codex app-server`, not a parsed terminal transcript.
//! The existing PTY workspace remains untouched and continues to display raw
//! terminal bytes exactly as before.

use crate::agent_session::{
    AgentApprovalDecision, AgentSession, AgentSessionEvent, AgentSessionId, AgentSessionStatus,
};
use crate::codex_app_server::{CodexAppServerClient, CodexAppServerEvent, CodexAppServerOptions};

/// App-owned UI/controller for one shared local Codex App Server connection.
pub struct AgentSessionsUi {
    open: bool,
    client: Option<CodexAppServerClient>,
    sessions: Vec<AgentSession>,
    selected_session: Option<AgentSessionId>,
    selected_item: Option<String>,
    new_prompt: String,
    new_model: String,
    follow_up: String,
    transport_error: Option<String>,
}

impl AgentSessionsUi {
    pub fn new() -> Self {
        Self {
            open: false,
            client: None,
            sessions: Vec::new(),
            selected_session: None,
            selected_item: None,
            new_prompt: String::new(),
            new_model: String::new(),
            follow_up: String::new(),
            transport_error: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
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
        if let Some(client) = &self.client {
            for event in client.drain_events() {
                match event {
                    CodexAppServerEvent::Session { session_id, event } => {
                        if let Some(session) = self
                            .sessions
                            .iter_mut()
                            .find(|session| session.id == session_id)
                        {
                            session.apply(event);
                        }
                    }
                    CodexAppServerEvent::TransportError { message } => {
                        self.transport_error = Some(message);
                    }
                    CodexAppServerEvent::ConnectionStopped => connection_stopped = true,
                }
            }
        }
        if connection_stopped {
            for session in &mut self.sessions {
                if !session.status.is_terminal() {
                    session.apply(AgentSessionEvent::Stopped);
                }
            }
            self.client = None;
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, workspace_cwd: Option<String>) {
        self.poll();
        if !self.open {
            return;
        }

        let mut window_open = self.open;
        let mut actions = Vec::new();
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
                ui.add_sized(
                    [ui.available_width(), 72.0],
                    egui::TextEdit::multiline(&mut self.new_prompt)
                        .hint_text("Codex에게 맡길 작업을 입력하세요")
                        .desired_rows(3),
                );
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
                            prompt: std::mem::take(&mut self.new_prompt),
                            model: std::mem::take(&mut self.new_model),
                            cwd: workspace_cwd.clone(),
                        });
                    }
                });

                crate::ui::hairline(ui);
                self.render_session_tabs(ui);
                crate::ui::hairline(ui);

                if let Some(session) = self.selected_session_snapshot() {
                    self.render_session(ui, session, workspace_cwd.clone(), &mut actions);
                } else {
                    ui.weak("아직 구조화된 Agent Session이 없습니다.");
                }
            });
        self.open = window_open;

        for action in actions {
            self.apply_action(action, ctx);
        }
    }

    fn render_session_tabs(&mut self, ui: &mut egui::Ui) {
        let tabs = self
            .sessions
            .iter()
            .map(|session| {
                (
                    session.id.clone(),
                    format!(
                        "[APP] Codex · {} · {}",
                        short_id(&session.id),
                        session.status.label()
                    ),
                )
            })
            .collect::<Vec<_>>();
        ui.horizontal_wrapped(|ui| {
            for (session_id, label) in tabs {
                let selected = self.selected_session.as_deref() == Some(session_id.as_str());
                if ui.selectable_label(selected, label).clicked() {
                    self.selected_session = Some(session_id);
                    self.selected_item = None;
                    self.follow_up.clear();
                }
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

        if session.status.is_terminal() && session.thread_id.is_some() {
            ui.add_space(6.0);
            crate::ui::hairline(ui);
            ui.label("후속 작업");
            ui.add_sized(
                [ui.available_width(), 48.0],
                egui::TextEdit::multiline(&mut self.follow_up)
                    .hint_text("같은 thread에 다음 작업을 보냅니다")
                    .desired_rows(2),
            );
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

    fn apply_action(&mut self, action: PanelAction, ctx: &egui::Context) {
        match action {
            PanelAction::Start { prompt, model, cwd } => self.start(prompt, model, cwd, ctx),
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
        }
        ctx.request_repaint();
    }

    fn start(&mut self, prompt: String, model: String, cwd: Option<String>, ctx: &egui::Context) {
        let session_id = uuid::Uuid::new_v4().to_string();
        let mut session = AgentSession::new(session_id.clone(), prompt.clone(), cwd.clone());
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
        self.sessions.push(session);
        self.selected_session = Some(session_id);
        self.selected_item = None;
    }

    fn fail_session(&mut self, session_id: &str, message: String) {
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
        {
            session.apply(AgentSessionEvent::Failed { message });
        }
    }
}

enum PanelAction {
    Start {
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
    Approval {
        session_id: AgentSessionId,
        request_key: String,
        decision: AgentApprovalDecision,
    },
}

fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

fn status_color(status: AgentSessionStatus) -> egui::Color32 {
    crate::ui::agent_visuals::status_color(crate::agent_surface::AgentVisualState::from_structured(
        status,
    ))
}
