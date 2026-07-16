//! Native structured Codex Agent Sessions panel.
//!
//! It renders data from `codex app-server`, not a parsed terminal transcript.
//! The existing PTY workspace remains untouched and continues to display raw
//! terminal bytes exactly as before.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::TryRecvError;

use crate::agent_session::{
    AgentApprovalDecision, AgentSession, AgentSessionEvent, AgentSessionId, AgentSessionStatus,
};
use crate::agent_surface::{
    AgentProvider, AgentSurfaceId, AgentSurfaceSnapshot, AgentTransport, AgentVisualState,
};
use crate::codex_app_server::{
    CodexAppServerClient, CodexAppServerEvent, CodexAppServerOptions, CodexAppServerReply,
};
use storage::StructuredThreadRow;

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

/// Storage mutations emitted by the structured-session controller. `App` owns
/// the `Db`, so it drains and executes these after the frame without exposing a
/// database connection to UI code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSessionPersistenceMutation {
    Upsert {
        local_session_id: AgentSessionId,
        workspace_id: String,
        thread_id: String,
        title: String,
        cwd: String,
        model: Option<String>,
        favorite: bool,
        archived: bool,
    },
    SetArchived {
        local_session_id: AgentSessionId,
        archived: bool,
    },
    Delete {
        local_session_id: AgentSessionId,
    },
}

enum PendingThreadRequest {
    Read {
        session_id: AgentSessionId,
        reply: CodexAppServerReply,
    },
    Resume {
        session_id: AgentSessionId,
        reply: CodexAppServerReply,
    },
    Archive {
        session_id: AgentSessionId,
        reply: CodexAppServerReply,
    },
}

impl PendingThreadRequest {
    fn session_id(&self) -> &str {
        match self {
            Self::Read { session_id, .. }
            | Self::Resume { session_id, .. }
            | Self::Archive { session_id, .. } => session_id,
        }
    }

    fn reply(&self) -> &CodexAppServerReply {
        match self {
            Self::Read { reply, .. } | Self::Resume { reply, .. } | Self::Archive { reply, .. } => {
                reply
            }
        }
    }
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
    persisted_threads: HashMap<AgentSessionId, StructuredThreadRow>,
    attached_threads: HashSet<AgentSessionId>,
    pending_thread_requests: Vec<PendingThreadRequest>,
    persistence_mutations: Vec<AgentSessionPersistenceMutation>,
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
            persisted_threads: HashMap::new(),
            attached_threads: HashSet::new(),
            pending_thread_requests: Vec::new(),
            persistence_mutations: Vec::new(),
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

    /// Project persisted metadata into lightweight, selectable APP rows. This
    /// does not spawn Codex: the one shared process is started only when the
    /// user explicitly reads or resumes a row.
    #[allow(dead_code)] // App wiring drains DB rows after this ownership phase.
    pub fn import_persisted_threads(&mut self, rows: Vec<StructuredThreadRow>) {
        for row in rows {
            let local_session_id = row.local_session_id.clone();
            let duplicate_thread = self.persisted_threads.iter().any(|(id, existing)| {
                id != &local_session_id && existing.thread_id == row.thread_id
            });
            if duplicate_thread {
                continue;
            }

            let was_persisted = self.persisted_threads.contains_key(&local_session_id);
            self.persisted_threads
                .insert(local_session_id.clone(), row.clone());
            if let Some(session) = self
                .sessions
                .iter_mut()
                .find(|session| session.id == local_session_id)
            {
                // A repeated DB projection may refresh a placeholder, but it
                // must never regress a live session that already owns events.
                if was_persisted && session.status == AgentSessionStatus::Stopped {
                    apply_persisted_metadata(session, &row);
                }
                continue;
            }

            let mut session = AgentSession::new(
                local_session_id,
                persisted_title(&row),
                non_empty(row.cwd.clone()),
            );
            apply_persisted_metadata(&mut session, &row);
            session.status = AgentSessionStatus::Stopped;
            self.sessions.push(session);
        }
    }

    /// Mutations are ordered exactly as emitted by the controller. In
    /// particular, archive is not emitted until `thread/archive` succeeds.
    #[allow(dead_code)] // App wiring executes these against its owned Db.
    pub fn drain_persistence_mutations(&mut self) -> Vec<AgentSessionPersistenceMutation> {
        std::mem::take(&mut self.persistence_mutations)
    }

    /// Surface App-owned database failures in the existing Agents error area.
    /// Mutation execution and any retry policy remain with `App`.
    #[allow(dead_code)] // Called by the App-level Db mutation executor.
    pub fn report_persistence_error(&mut self, message: String) {
        self.transport_error = Some(message);
    }

    pub fn read_selected_persisted(&mut self, ctx: &egui::Context) -> anyhow::Result<()> {
        let (session_id, row) = self.selected_persisted_thread()?;
        self.ensure_client(ctx)?;
        let reply = self
            .client
            .as_ref()
            .expect("ensure_client 성공 후 client 존재")
            .read_thread(row.thread_id, true)?;
        self.mark_history_request_started(&session_id);
        self.pending_thread_requests
            .push(PendingThreadRequest::Read { session_id, reply });
        Ok(())
    }

    pub fn resume_selected_persisted(&mut self, ctx: &egui::Context) -> anyhow::Result<()> {
        let (session_id, row) = self.selected_persisted_thread()?;
        self.ensure_client(ctx)?;
        let reply = self
            .client
            .as_ref()
            .expect("ensure_client 성공 후 client 존재")
            .resume_thread(
                session_id.clone(),
                row.thread_id,
                non_empty(row.cwd),
                row.model,
            )?;
        self.mark_history_request_started(&session_id);
        self.pending_thread_requests
            .push(PendingThreadRequest::Resume { session_id, reply });
        Ok(())
    }

    pub fn archive_selected_persisted(&mut self, ctx: &egui::Context) -> anyhow::Result<()> {
        let (session_id, row) = self.selected_persisted_thread()?;
        self.ensure_client(ctx)?;
        let reply = self
            .client
            .as_ref()
            .expect("ensure_client 성공 후 client 존재")
            .archive_thread(row.thread_id)?;
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
        {
            session.error = None;
        }
        self.pending_thread_requests
            .push(PendingThreadRequest::Archive { session_id, reply });
        Ok(())
    }

    /// Delete only Deppy's local recovery projection. Remote Codex history is
    /// retained unless the user separately archives it first.
    pub fn delete_selected_persisted(&mut self) -> anyhow::Result<()> {
        let (session_id, _) = self.selected_persisted_thread()?;
        anyhow::ensure!(
            !self.attached_threads.contains(&session_id),
            "재개된 APP thread는 보관하거나 연결이 종료된 뒤 로컬 기록을 삭제하세요"
        );
        self.persisted_threads.remove(&session_id);
        self.sessions.retain(|session| session.id != session_id);
        self.pending_thread_requests
            .retain(|pending| pending.session_id() != session_id);
        self.persistence_mutations
            .push(AgentSessionPersistenceMutation::Delete {
                local_session_id: session_id.clone(),
            });
        if self.selected_session.as_deref() == Some(session_id.as_str()) {
            self.selected_session = None;
            self.selected_surface = None;
            self.selected_item = None;
            self.follow_up.clear();
        }
        Ok(())
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
                    model: session.model.clone(),
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
                    .is_some_and(|session| session.thread_id.is_some())
                    && (!self.persisted_threads.contains_key(&session_id)
                        || self.attached_threads.contains(&session_id));
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
        self.attached_threads.clear();
    }

    fn ensure_client(&mut self, ctx: &egui::Context) -> anyhow::Result<()> {
        if self.client.is_some() {
            return Ok(());
        }
        let client = CodexAppServerClient::spawn(CodexAppServerOptions::default(), ctx.clone())?;
        self.transport_error = None;
        self.client = Some(client);
        Ok(())
    }

    fn selected_persisted_thread(&self) -> anyhow::Result<(AgentSessionId, StructuredThreadRow)> {
        let Some(AgentSurfaceId::Structured { session_id }) = self.selected_surface.as_ref() else {
            anyhow::bail!("선택된 저장 APP thread가 없습니다");
        };
        let row = self
            .persisted_threads
            .get(session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("선택된 APP thread는 저장된 복구 항목이 아닙니다"))?;
        anyhow::ensure!(!row.archived, "이미 보관된 APP thread입니다");
        anyhow::ensure!(
            !self
                .pending_thread_requests
                .iter()
                .any(|pending| pending.session_id() == session_id),
            "선택된 APP thread 요청이 이미 진행 중입니다"
        );
        Ok((session_id.clone(), row))
    }

    fn mark_history_request_started(&mut self, session_id: &str) {
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
        {
            session.error = None;
            session.status = AgentSessionStatus::Starting;
        }
    }

    fn poll_thread_replies(&mut self) {
        let pending = std::mem::take(&mut self.pending_thread_requests);
        let mut still_pending = Vec::with_capacity(pending.len());
        for request in pending {
            match request.reply().try_recv() {
                Ok(result) => self.finish_thread_request(request, result),
                Err(TryRecvError::Empty) => still_pending.push(request),
                Err(TryRecvError::Disconnected) => self.finish_thread_request(
                    request,
                    Err(anyhow::anyhow!(
                        "Codex App Server 응답 채널이 종료되었습니다"
                    )),
                ),
            }
        }
        self.pending_thread_requests = still_pending;
    }

    fn finish_thread_request(
        &mut self,
        request: PendingThreadRequest,
        result: anyhow::Result<serde_json::Value>,
    ) {
        match request {
            PendingThreadRequest::Read { session_id, .. } => match result {
                Ok(result) => self.load_history_result(&session_id, &result, false),
                Err(error) => {
                    self.fail_session(&session_id, format!("thread 기록 읽기 실패: {error:#}"));
                }
            },
            PendingThreadRequest::Resume { session_id, .. } => match result {
                Ok(result) => {
                    self.attached_threads.insert(session_id.clone());
                    self.load_history_result(&session_id, &result, true);
                }
                Err(error) => {
                    self.fail_session(&session_id, format!("thread 복구 실패: {error:#}"));
                }
            },
            PendingThreadRequest::Archive { session_id, .. } => match result {
                Ok(_) => {
                    self.attached_threads.remove(&session_id);
                    self.persisted_threads.remove(&session_id);
                    self.sessions.retain(|session| session.id != session_id);
                    if self.selected_session.as_deref() == Some(session_id.as_str()) {
                        self.selected_session = None;
                        self.selected_surface = None;
                        self.selected_item = None;
                        self.follow_up.clear();
                    }
                    self.persistence_mutations
                        .push(AgentSessionPersistenceMutation::SetArchived {
                            local_session_id: session_id,
                            archived: true,
                        });
                }
                Err(error) => {
                    self.fail_session(&session_id, format!("thread 보관 실패: {error:#}"));
                }
            },
        }
    }

    fn load_history_result(
        &mut self,
        session_id: &str,
        result: &serde_json::Value,
        persist_resume: bool,
    ) {
        let loaded = self
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
            .ok_or_else(|| anyhow::anyhow!("복구할 로컬 APP 세션이 없습니다"))
            .and_then(|session| session.load_thread_snapshot(result));
        match loaded {
            Ok(()) => {
                if persist_resume {
                    self.queue_thread_upsert(session_id, true);
                }
                self.queue_current_status_notice(session_id);
            }
            Err(error) => {
                self.fail_session(session_id, format!("thread 응답 적용 실패: {error:#}"));
            }
        }
    }

    fn queue_thread_upsert(&mut self, session_id: &str, force: bool) {
        let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .cloned()
        else {
            return;
        };
        let (Some(workspace_id), Some(thread_id)) =
            (session.workspace_id.clone(), session.thread_id.clone())
        else {
            return;
        };
        let existing = self.persisted_threads.get(session_id);
        let favorite = existing.is_some_and(|row| row.favorite);
        let archived = existing.is_some_and(|row| row.archived);
        let title = one_line_title(&session.prompt);
        let cwd = session.cwd.clone().unwrap_or_default();
        let unchanged = existing.is_some_and(|row| {
            row.workspace_id == workspace_id
                && row.thread_id == thread_id
                && row.title == title
                && row.cwd == cwd
                && row.model == session.model
                && row.favorite == favorite
                && row.archived == archived
        });
        if unchanged && !force {
            return;
        }

        let (created_at, updated_at) = existing
            .map(|row| (row.created_at, row.updated_at))
            .unwrap_or((0, 0));
        self.persisted_threads.insert(
            session_id.to_owned(),
            StructuredThreadRow {
                local_session_id: session_id.to_owned(),
                workspace_id: workspace_id.clone(),
                thread_id: thread_id.clone(),
                title: title.clone(),
                cwd: cwd.clone(),
                model: session.model.clone(),
                favorite,
                archived,
                created_at,
                updated_at,
            },
        );
        self.persistence_mutations
            .push(AgentSessionPersistenceMutation::Upsert {
                local_session_id: session_id.to_owned(),
                workspace_id,
                thread_id,
                title,
                cwd,
                model: session.model,
                favorite,
                archived,
            });
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
        self.poll_thread_replies();
        if connection_stopped {
            self.attached_threads.clear();
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
        egui::Window::new("Agents")
            .open(&mut window_open)
            .default_width(960.0)
            .default_height(650.0)
            .min_width(700.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.heading("Agents");
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
                self.render_surface_tabs(ui, &mut actions);
                crate::ui::hairline(ui);

                match self.selected_surface_snapshot() {
                    Some(surface) if surface.transport == AgentTransport::Pty => {
                        render_selected_surface_header(ui, &surface);
                        self.render_pty_surface(ui, surface, &mut actions);
                    }
                    Some(surface) => {
                        render_selected_surface_header(ui, &surface);
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

    fn render_surface_tabs(&mut self, ui: &mut egui::Ui, actions: &mut Vec<PanelAction>) {
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
                        one_line_title(&session.prompt),
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
                    self.activate_surface(id, actions);
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
        let is_persisted = self.persisted_threads.contains_key(&session.id);
        let is_attached = self.attached_threads.contains(&session.id);
        let request_pending = self
            .pending_thread_requests
            .iter()
            .any(|pending| pending.session_id() == session.id);
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
            ) && (!is_persisted || is_attached)
                && ui.button("중단").clicked()
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
        if let Some(model) = &session.model {
            ui.horizontal(|ui| {
                ui.weak("model");
                ui.monospace(model);
            });
        }
        if is_persisted {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !request_pending && !is_attached,
                        egui::Button::new("thread 재개"),
                    )
                    .clicked()
                {
                    actions.push(PanelAction::ResumePersisted(session.id.clone()));
                }
                if ui
                    .add_enabled(!request_pending, egui::Button::new("기록 읽기"))
                    .clicked()
                {
                    actions.push(PanelAction::ReadPersisted(session.id.clone()));
                }
                if ui
                    .add_enabled(!request_pending, egui::Button::new("보관"))
                    .clicked()
                {
                    actions.push(PanelAction::ArchivePersisted(session.id.clone()));
                }
                if ui
                    .add_enabled(
                        !request_pending && !is_attached,
                        egui::Button::new("로컬 기록 삭제"),
                    )
                    .clicked()
                {
                    actions.push(PanelAction::DeletePersisted(session.id.clone()));
                }
                if request_pending {
                    ui.weak("App Server 응답 대기 중…");
                }
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
            && (!is_persisted || is_attached)
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
            PanelAction::ReadPersisted(session_id) => {
                if let Err(error) = self.read_selected_persisted(ctx) {
                    self.fail_session(&session_id, format!("thread 기록 읽기 실패: {error:#}"));
                }
            }
            PanelAction::ResumePersisted(session_id) => {
                if let Err(error) = self.resume_selected_persisted(ctx) {
                    self.fail_session(&session_id, format!("thread 복구 시작 실패: {error:#}"));
                }
            }
            PanelAction::ArchivePersisted(session_id) => {
                if let Err(error) = self.archive_selected_persisted(ctx) {
                    self.fail_session(&session_id, format!("thread 보관 시작 실패: {error:#}"));
                }
            }
            PanelAction::DeletePersisted(session_id) => {
                if let Err(error) = self.delete_selected_persisted() {
                    self.fail_session(&session_id, format!("로컬 thread 삭제 실패: {error:#}"));
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
        session.model = non_empty(model.clone());
        let result = self.ensure_client(ctx).and_then(|()| {
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
        if !failed {
            self.attached_threads.insert(session_id.clone());
        }
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
        let gained_thread = matches!(&event, AgentSessionEvent::ThreadStarted { .. });
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
        let pending_resume = self.pending_thread_requests.iter().any(|pending| {
            matches!(pending, PendingThreadRequest::Resume { session_id: pending_id, .. } if pending_id == session_id)
        });
        if gained_thread && !pending_resume {
            self.queue_thread_upsert(session_id, false);
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

    fn activate_surface(&mut self, id: AgentSurfaceId, actions: &mut Vec<PanelAction>) {
        self.select_surface(id.clone());
        if matches!(id, AgentSurfaceId::Pty { .. }) {
            actions.push(PanelAction::FocusPty(id));
        }
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
    ReadPersisted(AgentSessionId),
    ResumePersisted(AgentSessionId),
    ArchivePersisted(AgentSessionId),
    DeletePersisted(AgentSessionId),
    FocusPty(AgentSurfaceId),
    InterruptPty(AgentSurfaceId),
}

#[allow(dead_code)] // Reachable from the pending App-level history import.
fn persisted_title(row: &StructuredThreadRow) -> String {
    let title = row.title.trim();
    if title.is_empty() {
        format!("Codex thread {}", short_id(&row.thread_id))
    } else {
        title.to_owned()
    }
}

#[allow(dead_code)] // Reachable from the pending App-level history import.
fn apply_persisted_metadata(session: &mut AgentSession, row: &StructuredThreadRow) {
    session.workspace_id = Some(row.workspace_id.clone());
    session.prompt = persisted_title(row);
    session.cwd = non_empty(row.cwd.clone());
    session.model = row.model.clone();
    session.thread_id = Some(row.thread_id.clone());
}

fn render_selected_surface_header(ui: &mut egui::Ui, surface: &AgentSurfaceSnapshot) {
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
    ui.weak(match surface.transport {
        AgentTransport::AppServer => "structured thread · turn · item stream",
        AgentTransport::Pty => "interactive terminal · raw ANSI/IME input",
    });
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
    use serde_json::json;
    use std::sync::mpsc;

    fn persisted_row(local_session_id: &str, thread_id: &str) -> StructuredThreadRow {
        StructuredThreadRow {
            local_session_id: local_session_id.to_owned(),
            workspace_id: "ws-1".to_owned(),
            thread_id: thread_id.to_owned(),
            title: "복구 작업".to_owned(),
            cwd: "/repo".to_owned(),
            model: Some("gpt-test".to_owned()),
            favorite: true,
            archived: false,
            created_at: 10,
            updated_at: 20,
        }
    }

    fn thread_result(thread_id: &str) -> serde_json::Value {
        json!({
            "thread": {
                "id": thread_id,
                "cwd": "/repo",
                "model": "gpt-test",
                "status": {"type": "idle"},
                "turns": [{
                    "id": "turn-1",
                    "items": [{
                        "id": "answer-1",
                        "type": "agentMessage",
                        "text": "restored",
                        "status": "completed"
                    }]
                }]
            }
        })
    }

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

    #[test]
    fn persisted_import_deduplicates_local_and_thread_ids_without_spawning() {
        let mut ui = AgentSessionsUi::new();
        let first = persisted_row("local-1", "thread-1");
        let mut refreshed = first.clone();
        refreshed.title = "갱신된 제목".to_owned();
        refreshed.cwd = "/repo/new".to_owned();
        let duplicate_thread = persisted_row("local-2", "thread-1");

        ui.import_persisted_threads(vec![first, refreshed, duplicate_thread]);

        assert!(ui.client.is_none());
        assert_eq!(ui.sessions.len(), 1);
        assert_eq!(ui.sessions[0].id, "local-1");
        assert_eq!(ui.sessions[0].prompt, "갱신된 제목");
        assert_eq!(ui.sessions[0].cwd.as_deref(), Some("/repo/new"));
        assert_eq!(ui.sessions[0].model.as_deref(), Some("gpt-test"));
        assert_eq!(ui.sessions[0].status, AgentSessionStatus::Stopped);
        assert!(ui.open_session("local-1"));
        assert!(matches!(
            ui.selected_surface,
            Some(AgentSurfaceId::Structured { ref session_id }) if session_id == "local-1"
        ));
    }

    #[test]
    fn read_and_resume_replies_poll_nonblocking_and_keep_recovery_selected() {
        let mut read_ui = AgentSessionsUi::new();
        read_ui.import_persisted_threads(vec![persisted_row("local-read", "thread-read")]);
        assert!(read_ui.open_session("local-read"));
        let (read_tx, read_rx) = mpsc::channel();
        read_ui.mark_history_request_started("local-read");
        read_ui
            .pending_thread_requests
            .push(PendingThreadRequest::Read {
                session_id: "local-read".to_owned(),
                reply: read_rx,
            });
        read_ui.poll_thread_replies();
        assert_eq!(read_ui.pending_thread_requests.len(), 1);
        read_tx.send(Ok(thread_result("thread-read"))).unwrap();
        read_ui.poll_thread_replies();
        assert_eq!(read_ui.sessions[0].items[0].summary, "restored");
        assert_eq!(read_ui.sessions[0].status, AgentSessionStatus::Ready);
        assert!(!read_ui.attached_threads.contains("local-read"));
        assert!(read_ui.drain_persistence_mutations().is_empty());

        let mut resume_ui = AgentSessionsUi::new();
        resume_ui.import_persisted_threads(vec![persisted_row("local-1", "thread-1")]);
        assert!(resume_ui.open_session("local-1"));
        let (resume_tx, resume_rx) = mpsc::channel();
        resume_ui.mark_history_request_started("local-1");
        resume_ui
            .pending_thread_requests
            .push(PendingThreadRequest::Resume {
                session_id: "local-1".to_owned(),
                reply: resume_rx,
            });
        resume_tx.send(Ok(thread_result("thread-1"))).unwrap();
        resume_ui.poll_thread_replies();

        assert!(resume_ui.attached_threads.contains("local-1"));
        assert_eq!(resume_ui.selected_session.as_deref(), Some("local-1"));
        assert!(matches!(
            resume_ui.selected_surface,
            Some(AgentSurfaceId::Structured { ref session_id }) if session_id == "local-1"
        ));
        assert_eq!(
            resume_ui.drain_persistence_mutations(),
            vec![AgentSessionPersistenceMutation::Upsert {
                local_session_id: "local-1".to_owned(),
                workspace_id: "ws-1".to_owned(),
                thread_id: "thread-1".to_owned(),
                title: "복구 작업".to_owned(),
                cwd: "/repo".to_owned(),
                model: Some("gpt-test".to_owned()),
                favorite: true,
                archived: false,
            }]
        );
    }

    #[test]
    fn archive_emits_db_mutation_only_after_app_server_success() {
        let mut ui = AgentSessionsUi::new();
        ui.import_persisted_threads(vec![persisted_row("local-1", "thread-1")]);
        ui.open_session("local-1");
        let (reply_tx, reply_rx) = mpsc::channel();
        ui.pending_thread_requests
            .push(PendingThreadRequest::Archive {
                session_id: "local-1".to_owned(),
                reply: reply_rx,
            });

        ui.poll_thread_replies();
        assert!(ui.drain_persistence_mutations().is_empty());
        assert!(!ui.persisted_threads["local-1"].archived);
        assert_eq!(ui.selected_session.as_deref(), Some("local-1"));

        reply_tx.send(Ok(json!({}))).unwrap();
        ui.poll_thread_replies();
        assert!(!ui.persisted_threads.contains_key("local-1"));
        assert!(!ui.sessions.iter().any(|session| session.id == "local-1"));
        assert!(ui.selected_session.is_none());
        assert!(ui.selected_surface.is_none());
        assert_eq!(
            ui.drain_persistence_mutations(),
            vec![AgentSessionPersistenceMutation::SetArchived {
                local_session_id: "local-1".to_owned(),
                archived: true,
            }]
        );
    }

    #[test]
    fn deleting_persisted_thread_removes_only_its_projection() {
        let mut ui = AgentSessionsUi::new();
        ui.import_persisted_threads(vec![
            persisted_row("local-1", "thread-1"),
            persisted_row("local-2", "thread-2"),
        ]);
        ui.open_session("local-1");

        ui.delete_selected_persisted().unwrap();

        assert_eq!(ui.session_ids(), vec!["local-2".to_owned()]);
        assert!(!ui.persisted_threads.contains_key("local-1"));
        assert!(ui.persisted_threads.contains_key("local-2"));
        assert!(ui.selected_surface.is_none());
        assert_eq!(
            ui.drain_persistence_mutations(),
            vec![AgentSessionPersistenceMutation::Delete {
                local_session_id: "local-1".to_owned(),
            }]
        );
    }

    #[test]
    fn thread_started_emits_one_deduplicated_upsert() {
        let mut ui = AgentSessionsUi::new();
        let mut session = AgentSession::new(
            "local-new".to_owned(),
            "새 작업\n상세".to_owned(),
            Some("/repo".to_owned()),
        );
        session.workspace_id = Some("ws-1".to_owned());
        session.model = Some("gpt-test".to_owned());
        ui.sessions.push(session);

        for _ in 0..2 {
            ui.apply_session_event(
                "local-new",
                AgentSessionEvent::ThreadStarted {
                    thread_id: "thread-new".to_owned(),
                },
            );
        }

        let mutations = ui.drain_persistence_mutations();
        assert_eq!(mutations.len(), 1);
        assert!(matches!(
            &mutations[0],
            AgentSessionPersistenceMutation::Upsert {
                local_session_id,
                thread_id,
                title,
                ..
            } if local_session_id == "local-new"
                && thread_id == "thread-new"
                && title == "새 작업"
        ));
    }

    #[test]
    fn pty_row_activation_focuses_exact_pane_immediately() {
        let mut ui = AgentSessionsUi::new();
        let expected = pty_surface(9).id;
        let mut actions = Vec::new();

        ui.activate_surface(expected.clone(), &mut actions);

        assert!(matches!(
            actions.as_slice(),
            [PanelAction::FocusPty(id)] if id == &expected
        ));
        assert_eq!(ui.selected_surface.as_ref(), Some(&expected));
    }

    #[test]
    fn completion_acknowledge_returns_controller_projection_to_idle() {
        let mut ui = AgentSessionsUi::new();
        let mut session = AgentSession::new("app-1".to_owned(), "done".to_owned(), None);
        session.thread_status = Some(crate::agent_session::AgentThreadStatus::Idle);
        session.status = AgentSessionStatus::Completed;
        ui.sessions.push(session);

        ui.apply_action(
            PanelAction::Acknowledge("app-1".to_owned()),
            &egui::Context::default(),
        );

        assert_eq!(ui.sessions[0].status, AgentSessionStatus::Ready);
        assert_eq!(
            AgentVisualState::from_structured(ui.sessions[0].status),
            AgentVisualState::Idle
        );
    }
}
