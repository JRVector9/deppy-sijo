//! App-owned authorization and effects for the outward MCP bridge.
mod tunnel;
mod ui;
use crate::agent_detect::AgentExecutionIdentity;
use crate::agent_surface::AgentProvider;
use crate::cloud_history_worker::{Completion, HistoryWorker, Job, ResultKind};
use agent_mcp::{Claim, Record, Request, Server, encode_input, now};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, sync::mpsc, time::Instant};

const MAX_TARGETS: usize = 256;
pub const MAX_SCREEN: usize = 64 * 1024;
#[derive(Clone)]
pub struct Target {
    pub id: String,
    pub generation: String,
    pub workspace: String,
    pub workspace_name: String,
    pub title: String,
    pub agent_line: Option<String>,
    pub runtime: u64,
    pub session: runtime::SessionId,
    pub pane: runtime::MuxPaneId,
    pub live: bool,
    pub screen: Option<String>,
    pub provider: Option<AgentProvider>,
    pub execution: Option<AgentExecutionIdentity>,
    pub known_ai: bool,
    pub bracketed_paste: bool,
}
struct Grant {
    generation: String,
    incarnation: u64,
    input_epoch: u64,
    runtime: u64,
    session: runtime::SessionId,
    pane: runtime::MuxPaneId,
    workspace: String,
    input: bool,
    permit: runtime::InputPermit,
}
impl Drop for Grant {
    fn drop(&mut self) {
        self.permit.revoke();
    }
}
struct Screen {
    generation: String,
    cursor: u64,
    text: String,
}
pub enum Effect {
    Input {
        operation_id: String,
        bytes: Vec<u8>,
        admission: runtime::InputAdmission,
    },
    PasteInput {
        operation_id: String,
        parts: Vec<Vec<u8>>,
        admission: runtime::InputAdmission,
    },
    Watch,
}

pub struct AnswerNotice {
    pub target: Target,
    pub message: String,
    pub operation_id: String,
}
#[derive(Clone, Copy)]
pub enum Action {
    Start,
    Stop,
    Rotate,
}

struct PendingInput {
    runtime: u64,
    session: runtime::SessionId,
    bytes: usize,
    submit: bool,
    deadline: std::time::Instant,
    reply: Option<std::sync::mpsc::SyncSender<Result<Value, String>>>,
}

const MAX_OPERATIONS: usize = 16;
const MAX_PENDING_BYTES: usize = 512 * 1024;
struct PendingClaim {
    request: Request,
    target: Target,
    auth: std::sync::Arc<agent_mcp::Auth>,
    grant: u64,
    input_epoch: u64,
    permit: runtime::InputPermit,
    bytes: Option<Vec<u8>>,
    paste: Option<Vec<Vec<u8>>>,
    require_bracketed_paste: bool,
    submit: bool,
    answer: Option<String>,
}
struct PendingFinish {
    reply: Option<mpsc::SyncSender<Result<Value, String>>>,
    outcome: Value,
    notice: Option<AnswerNotice>,
}

pub struct CloudAgent {
    pub boot: String,
    pub port: u16,
    pub hostname: String,
    pub automatic: bool,
    connection: Connection,
    generated_hostname: Option<String>,
    tunnel: Option<tunnel::Tunnel>,
    pub error: Option<String>,
    pub reveal: bool,
    pub selected_record: Option<String>,
    pub action: Option<Action>,
    pub server: Option<Server>,
    targets: Vec<Target>,
    ended_sessions: Vec<storage::CloudEndedSession>,
    ended_rx: Option<mpsc::Receiver<Result<Vec<storage::CloudEndedSession>, ()>>>,
    ended_last_refresh: Option<Instant>,
    ended_load_failed: bool,
    grants: HashMap<String, Grant>,
    screens: HashMap<String, Screen>,
    history: HistoryWorker,
    history_ready: bool,
    history_initialized: bool,
    history_start_requested: bool,
    history_open_pending: bool,
    history_dirty: bool,
    history_list: Option<u64>,
    history_revision: u64,
    next_operation: u64,
    grant_incarnation: u64,
    claims: HashMap<u64, PendingClaim>,
    finishes: HashMap<u64, PendingFinish>,
    finish_queue: std::collections::VecDeque<(u64, Job)>,
    records: Vec<Record>,
    pub answers: std::sync::Arc<[crate::ui::cloud_answer::Answer]>,
    pending: HashMap<String, PendingInput>,
    redaction: secret::RedactionService,
    token_lease: Option<secret::RedactionLease>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Connection {
    Idle,
    Preparing,
    Verifying,
    Ready,
    Stopping,
}
impl CloudAgent {
    pub fn new(path: &Path, redaction: secret::RedactionService, ctx: egui::Context) -> Self {
        Self::with_worker(
            HistoryWorker::new(Some(path.to_owned()), move || ctx.request_repaint()),
            redaction,
        )
    }
    fn with_worker(history: HistoryWorker, redaction: secret::RedactionService) -> Self {
        Self {
            boot: uuid::Uuid::new_v4().to_string(),
            port: 8739,
            hostname: String::new(),
            automatic: true,
            connection: Connection::Idle,
            generated_hostname: None,
            tunnel: None,
            error: None,
            reveal: false,
            selected_record: None,
            action: None,
            server: None,
            targets: vec![],
            ended_sessions: vec![],
            ended_rx: None,
            ended_last_refresh: None,
            ended_load_failed: false,
            grants: HashMap::new(),
            screens: HashMap::new(),
            history,
            history_ready: false,
            history_initialized: false,
            history_start_requested: false,
            history_open_pending: false,
            history_dirty: false,
            history_list: None,
            history_revision: 0,
            next_operation: 0,
            grant_incarnation: 0,
            claims: HashMap::new(),
            finishes: HashMap::new(),
            finish_queue: std::collections::VecDeque::new(),
            records: vec![],
            answers: std::sync::Arc::from([]),
            pending: HashMap::new(),
            redaction,
            token_lease: None,
        }
    }
    #[cfg(test)]
    fn memory() -> Self {
        let mut result = Self::with_worker(
            HistoryWorker::new(None, || {}),
            secret::RedactionService::new(),
        );
        result.drain_history(|_, _| panic!("unexpected startup input"));
        result
    }
    fn install_history(&mut self, revision: u64, rows: Vec<Record>) {
        if revision < self.history_revision {
            return;
        }
        self.history_revision = revision;
        self.answers = rows
            .iter()
            .filter(|r| r.tool == "notify" && !r.message.is_empty())
            .map(|r| crate::ui::cloud_answer::Answer {
                id: r.id.clone(),
                session: r.session.clone(),
                created: r.created,
                message: r.message.clone(),
            })
            .collect::<Vec<_>>()
            .into();
        self.records = rows;
    }
    fn operation_token(&mut self) -> u64 {
        self.next_operation = self
            .next_operation
            .checked_add(1)
            .expect("cloud operation token exhausted");
        self.next_operation
    }
    fn initialize_history(&mut self) {
        if !self.history_initialized
            && !self.history_open_pending
            && self.history.request(0, Job::Open).is_ok()
        {
            self.history_open_pending = true;
            self.history_initialized = true;
        }
    }
    pub fn redact(&self, text: &str) -> String {
        let mut redactor = self.redaction.stream_redactor();
        let mut bytes = redactor.redact_chunk(text.as_bytes());
        bytes.extend(redactor.flush());
        String::from_utf8_lossy(&bytes).into_owned()
    }
    pub fn wants_screen(&self, id: &str) -> bool {
        self.server.is_some() && self.grants.contains_key(id)
    }
    /// Historical rows are display-only; they never enter `targets` or MCP authorization.
    pub fn refresh_ended_sessions(
        &mut self,
        ctx: &egui::Context,
        db_path: &Path,
        load: impl FnOnce(&Path) -> Result<Vec<storage::CloudEndedSession>, ()> + Send + 'static,
    ) {
        if let Some(rx) = &self.ended_rx {
            match rx.try_recv() {
                Ok(Ok(rows)) => {
                    self.ended_sessions = rows;
                    self.ended_load_failed = false;
                    self.ended_rx = None;
                }
                Ok(Err(())) | Err(mpsc::TryRecvError::Disconnected) => {
                    self.ended_load_failed = true;
                    self.ended_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => return,
            }
        }
        if self.ended_rx.is_some()
            || self
                .ended_last_refresh
                .is_some_and(|at| at.elapsed().as_secs() < 30)
        {
            return;
        }
        self.ended_last_refresh = Some(Instant::now());
        let (tx, rx) = mpsc::sync_channel(1);
        let path = db_path.to_owned();
        let wake = ctx.clone();
        match std::thread::Builder::new()
            .name("cloud-ended-sessions".into())
            .spawn(move || {
                let rows = load(&path);
                let _ = tx.send(rows);
                wake.request_repaint();
            }) {
            Ok(_) => self.ended_rx = Some(rx),
            Err(_) => self.ended_load_failed = true,
        }
    }
    pub fn release_ended_sessions(&mut self) {
        self.ended_sessions = Vec::new();
        self.ended_load_failed = false;
        if let Some(rx) = &self.ended_rx
            && matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty))
        {
            // Keep the single in-flight reader owned across settings navigation. Dropping
            // its receiver here would allow quick tab switches to spawn unbounded readers.
            return;
        }
        self.ended_rx = None;
        self.ended_last_refresh = None;
    }
    fn historical_rows(&self) -> Vec<&storage::CloudEndedSession> {
        let current: std::collections::HashSet<&str> = self
            .targets
            .iter()
            .map(|target| target.id.as_str())
            .collect();
        self.ended_sessions
            .iter()
            .filter(|row| !current.contains(row.id.as_str()))
            .collect()
    }
    pub fn set_targets(&mut self, targets: Vec<Target>) {
        if self.targets.len() != targets.len()
            || self
                .targets
                .iter()
                .zip(&targets)
                .any(|(old, new)| old.id != new.id)
        {
            // A just-closed pane falls out of the live projection before the periodic
            // history poll. Refresh the archived side on the next frame.
            self.ended_last_refresh = None;
        }
        self.targets = targets.into_iter().take(MAX_TARGETS).collect();
        self.grants.retain(|id, g| {
            self.targets.iter().any(|t| {
                &t.id == id
                    && t.generation == g.generation
                    && t.runtime == g.runtime
                    && t.session == g.session
                    && t.pane == g.pane
                    && t.workspace == g.workspace
            })
        });
        for target in &self.targets {
            if !target.live
                && let Some(grant) = self.grants.get_mut(&target.id)
                && grant.input
            {
                grant.permit.revoke();
                grant.input = false;
                grant.input_epoch = grant.input_epoch.wrapping_add(1);
            }
        }
        self.screens.retain(|id, _| self.grants.contains_key(id));
        for t in &self.targets {
            if self.grants.contains_key(&t.id)
                && let Some(text) = &t.screen
            {
                let mut text = self.redact(text);
                truncate_utf8(&mut text, MAX_SCREEN);
                let screen = self.screens.entry(t.id.clone()).or_insert_with(|| Screen {
                    generation: t.generation.clone(),
                    cursor: 0,
                    text: String::new(),
                });
                if screen.generation != t.generation {
                    screen.generation = t.generation.clone();
                    screen.cursor = 0;
                    screen.text.clear();
                }
                if screen.cursor == 0 || screen.text != text {
                    screen.cursor = screen.cursor.saturating_add(1);
                    screen.text = text;
                }
            }
        }
        for target in &mut self.targets {
            target.screen = None;
        }
    }
    pub fn share(&mut self, t: &Target, shared: bool) {
        if shared {
            self.grant_incarnation = self.grant_incarnation.wrapping_add(1).max(1);
            self.grants.insert(
                t.id.clone(),
                Grant {
                    generation: t.generation.clone(),
                    incarnation: self.grant_incarnation,
                    input_epoch: 0,
                    runtime: t.runtime,
                    session: t.session,
                    pane: t.pane.clone(),
                    workspace: t.workspace.clone(),
                    input: false,
                    permit: runtime::InputPermit::new(),
                },
            );
        } else {
            self.grants.remove(&t.id);
            self.screens.remove(&t.id);
        }
    }
    pub fn allow_input(&mut self, id: &str, allow: bool) {
        if let Some(g) = self.grants.get_mut(id) {
            g.permit.revoke();
            g.input_epoch = g.input_epoch.wrapping_add(1);
            if allow {
                g.permit = runtime::InputPermit::new();
            }
            g.input = allow;
        }
    }
    pub fn take_control(&mut self) {
        for g in self.grants.values_mut() {
            g.permit.revoke();
            g.input_epoch = g.input_epoch.wrapping_add(1);
            g.input = false;
        }
    }
    fn authorize(&self, id: &str, generation: &str, input: bool) -> Result<Target, String> {
        let target = self
            .targets
            .iter()
            .find(|t| t.id == id && t.generation == generation)
            .ok_or("session_changed_or_closed")?;
        let grant = self
            .grants
            .get(id)
            .filter(|g| g.generation == generation)
            .ok_or("session_not_shared")?;
        if input && (!grant.input || !target.live) {
            return Err("input_not_allowed_or_session_exited".into());
        }
        Ok(target.clone())
    }
    fn busy(&self) -> bool {
        self.server.is_some() || self.tunnel.is_some()
    }
    fn ready(&self) -> bool {
        self.server.is_some() && (self.tunnel.is_none() || self.connection == Connection::Ready)
    }
    fn endpoint(&self) -> Option<String> {
        if !self.ready() {
            return None;
        }
        let server = self.server.as_ref()?;
        let host = self.generated_hostname.as_deref().unwrap_or(&self.hostname);
        Some(if host.is_empty() {
            format!("http://{}/mcp", server.addr)
        } else {
            format!("https://{host}/mcp")
        })
    }
    fn start_server(&mut self, ctx: &egui::Context, host: &str) -> bool {
        if !self.history_ready {
            self.error = Some("history_unavailable".into());
            return false;
        }
        let wake = ctx.clone();
        match Server::start_with_redaction(self.port, host, self.redaction.clone(), move || {
            wake.request_repaint()
        }) {
            Ok(server) => {
                let token = secret::SecretString::new(server.auth.token_for_user().to_string());
                let Ok(lease) = self.redaction.acquire_rotating(&token) else {
                    self.error = Some("token_redaction_capacity_no_connection".into());
                    return false;
                };
                self.token_lease = Some(lease);
                self.server = Some(server);
                self.error = None;
                ctx.request_repaint_after(std::time::Duration::from_secs(agent_mcp::TOKEN_TTL));
                true
            }
            Err(_) => {
                self.error = Some("server_start_failed_check_port_hostname".into());
                false
            }
        }
    }
    fn start_auto(&mut self, ctx: &egui::Context, executable: std::path::PathBuf) {
        if self.busy() || !self.start_server(ctx, "") {
            return;
        }
        let port = self.server.as_ref().unwrap().addr.port();
        let wake = ctx.clone();
        match tunnel::Tunnel::start(
            executable,
            port,
            std::time::Duration::from_secs(90),
            move || wake.request_repaint(),
        ) {
            Ok(worker) => {
                self.tunnel = Some(worker);
                self.connection = Connection::Preparing;
            }
            Err(_) => {
                self.stop_connection();
                self.error = Some("tunnel_worker_failed".into());
            }
        }
    }
    fn stop_connection(&mut self) {
        self.history_start_requested = false;
        self.take_control();
        self.server = None;
        self.token_lease = None;
        self.generated_hostname = None;
        self.reveal = false;
        self.connection = if let Some(worker) = &self.tunnel {
            worker.cancel();
            Connection::Stopping
        } else {
            Connection::Idle
        };
    }
    pub fn shutdown(&mut self) {
        self.stop_connection();
        if let Some(mut worker) = self.tunnel.take() {
            worker.shutdown();
        }
        self.connection = Connection::Idle;
        // No actor job can type. Revoke all queued admission leases before draining DB work.
        self.claims.clear();
        self.pending.clear();
        while let Some((token, job)) = self.finish_queue.pop_front() {
            if let Err(job) = self.history.request(token, job) {
                self.finish_queue.push_front((token, *job));
                let Some(completion) = self.history.drain_one() else {
                    break;
                };
                self.complete_history(
                    completion,
                    &mut |_, _| Err("shutdown_no_effect".into()),
                    &mut Vec::new(),
                );
            }
        }
        while let Some(completion) = self.history.drain_one() {
            self.complete_history(
                completion,
                &mut |_, _| Err("shutdown_no_effect".into()),
                &mut Vec::new(),
            );
        }
        self.history.shutdown();
        self.finishes.clear();
    }
    fn poll_connection(&mut self, ctx: &egui::Context) {
        let event = self.tunnel.as_mut().and_then(tunnel::Tunnel::poll);
        if self.connection != Connection::Stopping {
            match event {
                Some(tunnel::Event::Address(host)) => {
                    if self
                        .server
                        .as_ref()
                        .is_some_and(|server| server.set_public_host(&host).is_ok())
                    {
                        if let Some(tunnel) = &self.tunnel {
                            tunnel.acknowledge_address(&host);
                        }
                        self.generated_hostname = Some(host);
                        self.connection = Connection::Verifying;
                    } else {
                        self.stop_connection();
                        self.error = Some("tunnel_hostname_failed".into());
                    }
                }
                Some(tunnel::Event::Ready(host)) => {
                    // The final state can replace Address between frames; publish before checking it.
                    if self
                        .server
                        .as_ref()
                        .is_some_and(|server| server.set_public_host(&host).is_ok())
                    {
                        self.generated_hostname = Some(host);
                        self.connection = Connection::Ready;
                    } else {
                        self.stop_connection();
                        self.error = Some("tunnel_hostname_failed".into());
                    }
                }
                Some(tunnel::Event::Failed(code)) => {
                    self.stop_connection();
                    self.error = Some(code.into());
                }
                Some(tunnel::Event::Stopped) => {
                    self.stop_connection();
                }
                None => {}
            }
        }
        if self.tunnel.as_ref().is_some_and(tunnel::Tunnel::finished) {
            if self.connection != Connection::Stopping {
                self.stop_connection();
                self.error = Some("tunnel_worker_failed".into());
            }
            self.tunnel = None;
            self.connection = Connection::Idle;
        }
        if self.tunnel.is_some() && self.connection != Connection::Ready {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
    pub fn apply_action(&mut self, ctx: &egui::Context) {
        self.poll_connection(ctx);
        match self.action.take() {
            Some(Action::Start) if !self.busy() => {
                if !self.history_ready {
                    if !self.history_open_pending {
                        self.history_initialized = false;
                        self.error = None;
                        self.initialize_history();
                    }
                    self.history_start_requested = true;
                    return;
                }
                if self.automatic {
                    if let Some(executable) = tunnel::companion() {
                        self.start_auto(ctx, executable);
                    } else {
                        self.error = Some("tunnel_companion_missing".into());
                    }
                } else {
                    let host = self.hostname.clone();
                    if self.start_server(ctx, &host) {
                        self.connection = Connection::Ready;
                    }
                }
            }
            Some(Action::Stop) => {
                self.stop_connection();
                self.error = None;
            }
            Some(Action::Rotate) => {
                if let Some(s) = &self.server {
                    s.auth.rotate_at(now());
                    let token = secret::SecretString::new(s.auth.token_for_user().to_string());
                    match self.redaction.acquire_rotating(&token) {
                        Ok(lease) => self.token_lease = Some(lease),
                        Err(_) => {
                            self.stop_connection();
                            self.error = Some("token_redaction_capacity_connection_revoked".into());
                        }
                    }
                    ctx.request_repaint_after(std::time::Duration::from_secs(agent_mcp::TOKEN_TTL));
                }
                self.take_control();
                self.reveal = false;
            }
            _ => {}
        }
    }
    pub fn needs_target_projection(&self) -> bool {
        self.server.is_some() || !self.claims.is_empty()
    }
    pub fn next_request(&self) -> Option<Request> {
        self.server.as_ref()?.requests.try_recv().ok()
    }
    /// Validate/prepare on App, then enqueue durable work. SQLite never runs here.
    pub fn handle(
        &mut self,
        mut req: Request,
        mut send: impl FnMut(&Target, Effect) -> Result<(), String>,
    ) -> Option<AnswerNotice> {
        let result = if !self.server.as_ref().is_some_and(|s| req.live(&s.auth)) {
            Some(Err("expired_or_revoked_request_no_effect".into()))
        } else {
            self.prepare(&mut req, &mut send)
        };
        if let Some(result) = result {
            let _ = req.reply.try_send(result);
        }
        None
    }
    /// Called after current runtime/pane projection, including when the listener is stopped.
    /// Bounded nonblocking result consumption and completion wake replace DB waits/repaint loops.
    pub fn poll_history(
        &mut self,
        mut send: impl FnMut(&Target, Effect) -> Result<(), String>,
    ) -> Vec<AnswerNotice> {
        self.initialize_history();
        let mut notices = Vec::new();
        for _ in 0..crate::cloud_history_worker::MAX_JOBS {
            let Some(completion) = self.history.poll() else {
                break;
            };
            self.complete_history(completion, &mut send, &mut notices);
        }
        self.dispatch_history();
        notices
    }
    fn dispatch_history(&mut self) {
        while let Some((token, job)) = self.finish_queue.pop_front() {
            if let Err(job) = self.history.request(token, job) {
                self.finish_queue.push_front((token, *job));
                break;
            }
        }
        if self.history_ready
            && self.history_dirty
            && self.history_list.is_none()
            && self.finish_queue.is_empty()
        {
            let token = self.operation_token();
            if self.history.request(token, Job::Recent).is_ok() {
                self.history_list = Some(token);
                self.history_dirty = false;
            }
        }
    }
    fn complete_history(
        &mut self,
        completion: Completion,
        send: &mut impl FnMut(&Target, Effect) -> Result<(), String>,
        notices: &mut Vec<AnswerNotice>,
    ) {
        let token = completion.token;
        match completion.result {
            ResultKind::Open(result) => {
                self.history_open_pending = false;
                match result {
                    Ok(rows) => {
                        self.history_ready = true;
                        if self.error.as_deref() == Some("history_unavailable") {
                            self.error = None;
                        }
                        self.install_history(token, rows);
                        if self.history_start_requested {
                            self.history_start_requested = false;
                            self.action = Some(Action::Start);
                            self.history.request_wake();
                        }
                    }
                    Err(()) => {
                        self.history_start_requested = false;
                        self.error = Some("history_unavailable".into());
                    }
                }
            }
            ResultKind::Recent(result) => {
                if self.history_list != Some(token) {
                    return;
                }
                self.history_list = None;
                // A list issued before a newer finish cannot replace the newer cache.
                if self.history_dirty {
                    return;
                }
                match result {
                    Ok(rows) => self.install_history(token, rows),
                    Err(()) => self.error = Some("history_read_failed".into()),
                }
            }
            ResultKind::Claim(result) => {
                let Some(claim) = self.claims.remove(&token) else {
                    return;
                };
                let op = claim.request.args["operation_id"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                match result {
                    Err(()) => {
                        let _ = claim.request.reply.try_send(Err(
                            "operation_conflict_or_history_unavailable_no_effect".into(),
                        ));
                    }
                    Ok(Claim::Existing(outcome)) => {
                        let _ = claim.request.reply.try_send(Ok(outcome));
                    }
                    Ok(Claim::New) => self.dispatch_claim(op, claim, send),
                }
            }
            ResultKind::Finish(result) => {
                let Some(finish) = self.finishes.remove(&token) else {
                    return;
                };
                self.history_dirty = true;
                let receipt = match result {
                    Ok(()) => {
                        if let Some(notice) = finish.notice {
                            notices.push(notice);
                        }
                        Ok(finish.outcome)
                    }
                    Err(()) => Err("outcome_unknown_do_not_retry_input".into()),
                };
                if let Some(reply) = finish.reply {
                    let _ = reply.try_send(receipt);
                }
            }
        }
    }
    fn finish_operation(
        &mut self,
        op: String,
        outcome: Value,
        message: String,
        reply: Option<mpsc::SyncSender<Result<Value, String>>>,
        notice: Option<AnswerNotice>,
    ) {
        let token = self.operation_token();
        self.finishes.insert(
            token,
            PendingFinish {
                reply,
                outcome: outcome.clone(),
                notice,
            },
        );
        self.finish_queue.push_back((
            token,
            Job::Finish {
                id: op,
                outcome,
                message,
            },
        ));
        self.dispatch_history();
    }
    fn dispatch_claim(
        &mut self,
        op: String,
        claim: PendingClaim,
        send: &mut impl FnMut(&Target, Effect) -> Result<(), String>,
    ) {
        let input = claim.bytes.is_some() || claim.paste.is_some();
        let paste = claim.paste.is_some();
        let valid_auth = self.server.as_ref().is_some_and(|s| {
            std::sync::Arc::ptr_eq(&s.auth, &claim.auth) && claim.request.live(&s.auth)
        });
        let current_target = self.authorize(&claim.target.id, &claim.target.generation, input);
        let valid_target = current_target.as_ref().is_ok_and(|target| {
            target.runtime == claim.target.runtime
                && target.session == claim.target.session
                && target.pane == claim.target.pane
                && target.workspace == claim.target.workspace
                && (!paste
                    || (target.execution == claim.target.execution
                        && target.known_ai == claim.target.known_ai
                        && target.provider == claim.target.provider
                        && (!claim.require_bracketed_paste || target.bracketed_paste)
                        && claim
                            .target
                            .execution
                            .is_none_or(AgentExecutionIdentity::is_current)))
        });
        let valid_grant = self.grants.get(&claim.target.id).is_some_and(|g| {
            g.incarnation == claim.grant && (!input || g.input_epoch == claim.input_epoch)
        });
        if !valid_auth || !valid_target || !valid_grant {
            let code = if !valid_auth {
                "expired_or_revoked_request_no_effect"
            } else {
                "session_or_permission_changed_no_effect"
            };
            self.finish_operation(
                op,
                json!({"status":"rejected","error":code,"retry":false}),
                String::new(),
                Some(claim.request.reply),
                None,
            );
            return;
        }
        if input {
            let size = claim.bytes.as_ref().map_or(0, Vec::len)
                + claim
                    .paste
                    .as_ref()
                    .map_or(0, |parts| parts.iter().map(Vec::len).sum());
            let auth = claim.auth;
            let epoch = claim.request.epoch;
            let access_key = claim.request.access_key;
            let execution = if paste { claim.target.execution } else { None };
            let mut admission =
                runtime::InputAdmission::new(claim.permit, claim.request.deadline, move |write| {
                    if execution.is_none_or(AgentExecutionIdentity::is_current) {
                        auth.admit_current_access(epoch, access_key.as_deref(), &mut || {
                            if execution.is_none_or(AgentExecutionIdentity::is_current) {
                                write();
                            }
                        })
                    }
                });
            if let Some(execution) = execution {
                admission =
                    admission.with_agent_guard(execution.input_guard_for(if claim.submit {
                        runtime::AgentInputIntent::ExplicitPrompt
                    } else {
                        runtime::AgentInputIntent::ExplicitAppend
                    }));
            }
            if claim.require_bracketed_paste {
                admission = admission.with_bracketed_paste_required();
            }
            let effect = if let Some(parts) = claim.paste {
                Effect::PasteInput {
                    operation_id: op.clone(),
                    parts,
                    admission,
                }
            } else {
                Effect::Input {
                    operation_id: op.clone(),
                    bytes: claim.bytes.unwrap(),
                    admission,
                }
            };
            match send(&claim.target, effect) {
                Ok(()) => {
                    self.pending.insert(
                        op,
                        PendingInput {
                            runtime: claim.target.runtime,
                            session: claim.target.session,
                            bytes: size,
                            submit: claim.submit,
                            deadline: claim.request.deadline,
                            reply: Some(claim.request.reply),
                        },
                    );
                }
                Err(code) => self.finish_operation(
                    op,
                    json!({"status":"rejected","error":code,"retry":false}),
                    String::new(),
                    Some(claim.request.reply),
                    None,
                ),
            }
        } else {
            let message = claim.answer.unwrap_or_default();
            let notice = AnswerNotice {
                target: claim.target,
                message: message.clone(),
                operation_id: op.clone(),
            };
            self.finish_operation(
                op.clone(),
                json!({"status":"stored","operation_id":op}),
                message,
                Some(claim.request.reply),
                Some(notice),
            );
        }
    }
    pub fn observe_input(&mut self, runtime: u64, events: &[runtime::RuntimeEvent]) {
        for event in events {
            let runtime::RuntimeEvent::InputAdmitted {
                session,
                operation_id,
                result,
            } = event
            else {
                continue;
            };
            if !self
                .pending
                .get(operation_id)
                .is_some_and(|p| p.runtime == runtime && p.session == *session)
            {
                continue;
            }
            let p = self.pending.remove(operation_id).unwrap();
            let outcome = match result {
                Ok(()) => {
                    json!({"status":"queued","admission":"pty_queue","bytes":p.bytes,"submit":p.submit,"retry":false,"completion":"not_confirmed"})
                }
                Err(runtime::PtyInputRejectReason::AdmissionUnknown) => {
                    json!({"status":"unknown","error":"pty_admission_unknown","retry":false,"completion":"not_confirmed"})
                }
                Err(reason) => {
                    json!({"status":"rejected","error":format!("pty_{reason:?}"),"retry":false})
                }
            };
            self.finish_operation(operation_id.clone(), outcome, String::new(), p.reply, None);
        }
    }
    pub fn expire_pending(&mut self) {
        self.pending.retain(|_, p| {
            let expired = Instant::now() >= p.deadline;
            if expired && let Some(reply) = p.reply.take() {
                let _ = reply.try_send(Err("outcome_unknown_do_not_retry_input".into()));
            }
            Instant::now() < p.deadline + std::time::Duration::from_secs(30)
        });
    }
    fn prepare(
        &mut self,
        req: &mut Request,
        send: &mut impl FnMut(&Target, Effect) -> Result<(), String>,
    ) -> Option<Result<Value, String>> {
        match self.prepare_inner(req, send) {
            Ok(None) => None,
            Ok(Some(value)) => Some(Ok(value)),
            Err(error) => Some(Err(error)),
        }
    }
    fn prepare_inner(
        &mut self,
        req: &mut Request,
        send: &mut impl FnMut(&Target, Effect) -> Result<(), String>,
    ) -> Result<Option<Value>, String> {
        let a = &req.args;
        let allowed: &[&str] = match req.tool.as_str() {
            "list_sessions" => &[],
            "read_output" => &["session_id", "generation", "cursor"],
            "send_text" | "paste_text" => {
                &["session_id", "generation", "operation_id", "text", "submit"]
            }
            "send_ctrl_c" => &["session_id", "generation", "operation_id"],
            "notify" => &["session_id", "generation", "operation_id", "message"],
            _ => return Err("unknown_tool".into()),
        };
        if !a.is_object()
            || a.as_object()
                .unwrap()
                .keys()
                .any(|k| !allowed.contains(&k.as_str()))
        {
            return Err("invalid_arguments".into());
        }
        if req.tool == "list_sessions" {
            let sessions: Vec<_> = self.targets.iter().filter_map(|t| self.grants.get(&t.id).map(|g| json!({"session_id":t.id,"generation":t.generation,"workspace":t.workspace,"workspace_name":t.workspace_name,"title":t.title,"input_allowed":g.input&&t.live&&req.input_scope,"live":t.live,"paste_bracketed":t.bracketed_paste,"paste_text_max_bytes":agent_mcp::MAX_PASTE,"paste_ai_confirmed":!t.known_ai||t.execution.is_some()}))).collect();
            return Ok(Some(json!({"sessions":sessions})));
        }
        let id = field(a, "session_id", 128)?;
        let generation = field(a, "generation", 192)?;
        let input = matches!(
            req.tool.as_str(),
            "send_text" | "paste_text" | "send_ctrl_c"
        );
        if input && !req.input_scope {
            return Err("oauth_input_scope_required".into());
        }
        let target = self.authorize(id, generation, input)?;
        if req.tool == "read_output" {
            send(&target, Effect::Watch)?;
            let cursor = match a.get("cursor") {
                None => 0,
                Some(v) => v.as_u64().ok_or("invalid_cursor")?,
            };
            let Some(screen) = self.screens.get(id) else {
                return Ok(Some(
                    json!({"cursor":0,"screen":null,"reset":true,"source":"visible_screen","lossless":false,"refresh_requested":true,"may_be_stale":true,"retry_after_ms":250}),
                ));
            };
            return Ok(Some(
                json!({"cursor":screen.cursor,"screen":if cursor != screen.cursor {Some(&screen.text)} else {None},"reset":cursor==0 || cursor>screen.cursor,"source":"visible_screen","lossless":false,"refresh_requested":true,"may_be_stale":true,"retry_after_ms":250}),
            ));
        }
        let op = field(a, "operation_id", 128)?.to_owned();
        let submit = match a.get("submit") {
            None => false,
            Some(v) => v.as_bool().ok_or("invalid_submit")?,
        };
        let bytes = match req.tool.as_str() {
            "send_text" => Some(
                encode_input(a["text"].as_str().ok_or("invalid_text")?, submit)
                    .map_err(|_| "text_contains_controls_or_invalid_size")?,
            ),
            "send_ctrl_c" => Some(vec![3]),
            _ => None,
        };
        let (paste, require_bracketed_paste) = if req.tool == "paste_text" {
            let text = a["text"].as_str().ok_or("invalid_text")?;
            agent_mcp::validate_paste_text(text, submit)
                .map_err(|_| "paste_contains_controls_or_invalid_size")?;
            if serde_json::to_vec(a)
                .map_err(|_| "invalid_arguments")?
                .len()
                > crate::cloud_history_worker::MAX_ARGS
            {
                return Err("paste_arguments_too_large_no_effect".into());
            }
            if target.known_ai && target.execution.is_none() {
                return Err("ai_execution_unconfirmed_no_effect".into());
            }
            if !target.bracketed_paste && text.contains(['\n', '\r', '\t']) {
                return Err("bracketed_paste_required_no_effect".into());
            }
            // Never force Codex framing while the actual observed mode is off.
            let provider = target.bracketed_paste.then_some(target.provider).flatten();
            let plan = crate::ui::composer::plan_composer_input(
                text,
                submit,
                target.bracketed_paste,
                provider,
            )
            .ok_or("invalid_paste_size")?;
            (Some(plan.into_parts()), target.bracketed_paste)
        } else {
            (None, false)
        };
        let answer = if req.tool == "notify" {
            let message = field(a, "message", agent_mcp::MAX_ANSWER)?;
            if message.contains('\0') {
                return Err("invalid_message".into());
            }
            let clean: String = message
                .chars()
                .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
                .collect();
            let mut clean = self.redact(&clean);
            truncate_utf8(&mut clean, agent_mcp::MAX_ANSWER);
            Some(clean)
        } else {
            None
        };
        if !self.history_ready {
            return Err("history_unavailable_no_effect".into());
        }
        let retained: usize = self
            .claims
            .values()
            .map(|c| {
                c.bytes.as_ref().map_or(0, Vec::len)
                    + c.paste
                        .as_ref()
                        .map_or(0, |parts| parts.iter().map(Vec::len).sum::<usize>())
                    + c.answer.as_ref().map_or(0, String::len)
            })
            .sum();
        let size = bytes.as_ref().map_or(0, Vec::len)
            + paste
                .as_ref()
                .map_or(0, |parts| parts.iter().map(Vec::len).sum::<usize>())
            + answer.as_ref().map_or(0, String::len);
        if self.claims.len() + self.pending.len() + self.finishes.len() >= MAX_OPERATIONS
            || retained + size > MAX_PENDING_BYTES
        {
            return Err("busy_no_effect".into());
        }
        let token = self.operation_token();
        let job = Job::Claim {
            id: op.clone(),
            tool: req.tool.clone(),
            args: std::mem::take(&mut req.args),
            workspace: target.workspace.clone(),
            session: target.id.clone(),
        };
        self.history
            .request(token, job)
            .map_err(|_| "busy_no_effect")?;
        let grant = &self.grants[&target.id];
        // Retain only operation_id in App, not another full copy of request arguments.
        let stored_request = Request {
            epoch: req.epoch,
            access_key: req.access_key.clone(),
            input_scope: req.input_scope,
            deadline: req.deadline,
            tool: req.tool.clone(),
            args: json!({"operation_id":op}),
            reply: req.reply.clone(),
        };
        self.claims.insert(
            token,
            PendingClaim {
                request: stored_request,
                target,
                auth: self.server.as_ref().unwrap().auth.clone(),
                grant: grant.incarnation,
                input_epoch: grant.input_epoch,
                permit: grant.permit.clone(),
                bytes,
                paste,
                require_bracketed_paste,
                submit,
                answer,
            },
        );
        Ok(None)
    }
    #[cfg(test)]
    fn drain_history(
        &mut self,
        mut send: impl FnMut(&Target, Effect) -> Result<(), String>,
    ) -> Vec<AnswerNotice> {
        let mut notices = self.poll_history(&mut send);
        for _ in 0..128 {
            let Some(completion) = self.history.wait() else {
                return notices;
            };
            self.complete_history(completion, &mut send, &mut notices);
            self.dispatch_history();
        }
        panic!("bounded history drain exceeded");
    }
    #[cfg(test)]
    fn handle_and_drain(
        &mut self,
        req: Request,
        mut send: impl FnMut(&Target, Effect) -> Result<(), String>,
    ) -> Option<AnswerNotice> {
        self.handle(req, &mut send);
        self.drain_history(&mut send).into_iter().next()
    }
    #[cfg(test)]
    fn observe_and_drain(&mut self, runtime: u64, events: &[runtime::RuntimeEvent]) {
        self.observe_input(runtime, events);
        self.drain_history(|_, _| panic!("unexpected effect while finishing input"));
    }
}
fn truncate_utf8(text: &mut String, max: usize) {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

fn field<'a>(a: &'a Value, key: &str, max: usize) -> Result<&'a str, String> {
    a[key]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= max)
        .ok_or_else(|| format!("invalid_{key}"))
}

pub fn target_matches(
    target: &Target,
    runtime: u64,
    workspace: &str,
    mux: &runtime::MuxSnapshot,
    exited: bool,
    input: bool,
) -> bool {
    target.runtime == runtime
        && target.workspace == workspace
        && (!input || !exited)
        && mux.tabs.iter().flat_map(|tab| &tab.panes).any(|p| {
            p.id == target.pane
                && p.session_id == Some(target.session)
                && p.persistent_session_id.as_deref() == Some(target.id.as_str())
        })
}

pub fn screen_text(s: &terminal::TerminalViewportSnapshot) -> String {
    let mut out = String::new();
    if s.cols == 0 || s.rows == 0 || s.visible_cells.is_empty() {
        return out;
    }
    let mut line = String::with_capacity(usize::from(s.cols).min(MAX_SCREEN));
    for (row_index, row) in s
        .visible_cells
        .chunks(s.cols as usize)
        .take(s.rows as usize)
        .enumerate()
    {
        line.clear();
        for (col, c) in row.iter().enumerate() {
            if s.is_trailing_wide_spacer(row_index * s.cols as usize + col) || c.wide_spacer() {
                continue;
            }
            if !c.c.is_control() {
                s.push_cell_text(row_index * s.cols as usize + col, &mut line);
            }
            if out.len() + line.len() > MAX_SCREEN {
                return out;
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
        if out.len() >= MAX_SCREEN {
            break;
        }
    }
    out
}
#[cfg(test)]
mod grapheme_snapshot_tests {
    #[test]
    fn cloud_screen_rows_reuse_scratch_without_retaining_prior_text() {
        use terminal::TerminalBackend;
        for (input, expected) in [
            ("long-name\r\nx", "long-name\nx\n\n"),
            ("\x1b[31m한글\x1b[0m  \r\n  x", "한글\n  x\n\n"),
        ] {
            let mut backend = terminal::AlacrittyBackend::new(10, 3, 10);
            backend.feed(input.as_bytes()).unwrap();
            let snapshot = backend.viewport_snapshot().unwrap();
            assert_eq!(super::screen_text(&snapshot), expected);
            let mut empty = snapshot.clone();
            empty.cols = 0;
            assert!(super::screen_text(&empty).is_empty());
            empty.cols = 10;
            empty.rows = 0;
            assert!(super::screen_text(&empty).is_empty());
        }
    }
    #[test]
    fn cloud_screen_text_preserves_non_composable_graphemes() {
        use terminal::TerminalBackend;
        for text in ["가ᇹ", "a\u{301}\u{308}"] {
            let mut backend = terminal::AlacrittyBackend::new(20, 3, 10);
            backend.feed(text.as_bytes()).unwrap();
            assert_eq!(
                super::screen_text(&backend.viewport_snapshot().unwrap()).trim_end(),
                text
            );
        }
    }
}

#[cfg(test)]
impl Target {
    fn fixture(id: &str, generation: &str) -> Self {
        Self {
            id: id.into(),
            generation: generation.into(),
            workspace: "w".into(),
            workspace_name: "workspace".into(),
            title: "session".into(),
            agent_line: None,
            runtime: 1,
            session: runtime::SessionId(1),
            pane: runtime::MuxPaneId::new(),
            live: true,
            screen: Some("hello".into()),
            provider: None,
            execution: None,
            known_ai: false,
            bracketed_paste: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ended_history_shows_closed_sessions_once_without_mcp_authority() {
        let mut bridge = CloudAgent::memory();
        let current = Target::fixture("current", "generation");
        bridge.set_targets(vec![current]);
        bridge.ended_sessions = ["current", "closed"]
            .into_iter()
            .map(|id| storage::CloudEndedSession {
                id: id.into(),
                workspace_id: "workspace".into(),
                workspace_name: "Workspace".into(),
                title: id.into(),
            })
            .collect();
        assert_eq!(
            bridge
                .historical_rows()
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["closed"]
        );
        assert!(bridge.authorize("closed", "generation", false).is_err());
    }
    #[test]
    fn ended_history_loader_finishes_off_thread_and_releases_its_cache() {
        let mut bridge = CloudAgent::memory();
        let ctx = egui::Context::default();
        let row = storage::CloudEndedSession {
            id: "old".into(),
            workspace_id: "workspace".into(),
            workspace_name: "Workspace".into(),
            title: "Old task".into(),
        };
        bridge.refresh_ended_sessions(&ctx, Path::new("unused"), move |_| Ok(vec![row]));
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while bridge.ended_rx.is_some() {
            bridge
                .refresh_ended_sessions(&ctx, Path::new("unused"), |_| panic!("unexpected reload"));
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(bridge.historical_rows()[0].id, "old");
        bridge.release_ended_sessions();
        assert!(bridge.historical_rows().is_empty());
    }
    #[test]
    fn changed_current_targets_schedule_history_refresh() {
        let mut bridge = CloudAgent::memory();
        bridge.set_targets(vec![Target::fixture("old", "generation")]);
        bridge.ended_last_refresh = Some(Instant::now());
        bridge.set_targets(vec![Target::fixture("old", "generation")]);
        assert!(bridge.ended_last_refresh.is_some());
        bridge.set_targets(vec![]);
        assert!(bridge.ended_last_refresh.is_none());
    }
    #[test]
    fn closing_settings_does_not_spawn_a_second_history_reader() {
        let mut bridge = CloudAgent::memory();
        let ctx = egui::Context::default();
        let (continue_tx, continue_rx) = mpsc::channel();
        bridge.refresh_ended_sessions(&ctx, Path::new("unused"), move |_| {
            continue_rx.recv().unwrap();
            Ok(vec![storage::CloudEndedSession {
                id: "old".into(),
                workspace_id: "workspace".into(),
                workspace_name: "Workspace".into(),
                title: "Old task".into(),
            }])
        });
        bridge.release_ended_sessions();
        assert!(bridge.ended_rx.is_some());
        bridge.refresh_ended_sessions(&ctx, Path::new("unused"), |_| panic!("second reader"));
        continue_tx.send(()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while bridge.ended_rx.is_some() {
            bridge.refresh_ended_sessions(&ctx, Path::new("unused"), |_| panic!("second reader"));
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(bridge.historical_rows()[0].id, "old");
    }
    #[test]
    fn input_permission_requires_exact_incarnation_and_live_session() {
        let mut bridge = CloudAgent::memory();
        let mut t = Target::fixture("a", "generation-1");
        bridge.set_targets(vec![t.clone()]);
        bridge.share(&t, true);
        assert!(bridge.authorize("a", "generation-1", true).is_err());
        bridge.allow_input("a", true);
        assert!(bridge.authorize("a", "generation-1", true).is_ok());
        bridge.take_control();
        assert!(bridge.authorize("a", "generation-1", true).is_err());
        assert!(bridge.authorize("a", "generation-1", false).is_ok());
        t.generation = "generation-2".into();
        bridge.set_targets(vec![t.clone()]);
        assert!(bridge.authorize("a", "generation-1", false).is_err());
        bridge.share(&t, true);
        bridge.allow_input("a", true);
        t.live = false;
        bridge.set_targets(vec![t]);
        assert!(bridge.authorize("a", "generation-2", true).is_err());
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    fn request(
        bridge: &CloudAgent,
        tool: &str,
        args: Value,
    ) -> (Request, mpsc::Receiver<Result<Value, String>>) {
        let auth = &bridge.server.as_ref().unwrap().auth;
        let epoch = auth
            .authenticate(&format!("Bearer {}", auth.token_for_user().as_str()), now())
            .unwrap();
        let (reply, rx) = mpsc::sync_channel(1);
        (
            Request {
                epoch,
                access_key: None,
                input_scope: true,
                deadline: Instant::now() + Duration::from_secs(3),
                tool: tool.into(),
                args,
                reply,
            },
            rx,
        )
    }
    fn setup() -> (CloudAgent, Target) {
        let mut b = CloudAgent::memory();
        b.server = Some(Server::start(0, "", || {}).unwrap());
        let t = Target::fixture("original-session", "generation-1");
        b.set_targets(vec![t.clone()]);
        b.share(&t, true);
        b.set_targets(vec![t.clone()]);
        (b, t)
    }
    #[test]
    fn pr9_explicit_short_paste_claims_before_dispatch_and_is_idempotent() {
        let (mut b, t) = setup();
        b.allow_input(&t.id, true);
        let args = json!({"session_id":t.id,"generation":t.generation,"operation_id":"explicit-paste","text":"한글 😀"});
        let (req, rx) = request(&b, "paste_text", args.clone());
        b.handle(req, |_, _| panic!("input before durable claim"));
        assert!(
            rx.try_recv().is_err(),
            "valid paste must remain pending until claim completion"
        );
        let mut effects = 0;
        b.drain_history(|target, _| {
            assert_eq!(target.id, t.id);
            effects += 1;
            Ok(())
        });
        assert_eq!(effects, 1);
        let (retry, rx) = request(&b, "paste_text", args);
        b.handle_and_drain(retry, |_, _| panic!("unknown paste must not be replayed"));
        let receipt = rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
        assert_eq!(receipt["status"], "unknown");
        assert_eq!(receipt["retry"], false);
    }
    #[test]
    fn pr9_multiline_unicode_crlf_tab_is_one_shared_atomic_plan() {
        for submit in [false, true] {
            let (mut b, mut t) = setup();
            t.bracketed_paste = true;
            b.set_targets(vec![t.clone()]);
            b.allow_input(&t.id, true);
            let args = json!({"session_id":t.id,"generation":t.generation,"operation_id":"multiline","text":"한글 😀\r\nsecond\tline\nthird","submit":submit});
            let (req, rx) = request(&b, "paste_text", args);
            let mut effects = 0;
            b.handle_and_drain(req, |_, effect| {
                let Effect::PasteInput { parts, .. } = effect else {
                    panic!("paste must use batch admission")
                };
                assert_eq!(parts.len(), if submit { 2 } else { 1 });
                assert_eq!(
                    parts[0],
                    "\x1b[200~한글 😀\nsecond\tline\nthird\x1b[201~".as_bytes()
                );
                if submit {
                    assert_eq!(parts[1], b"\r");
                }
                effects += 1;
                Ok(())
            });
            assert_eq!(effects, 1);
            assert!(
                rx.try_recv().is_err(),
                "execution is not confirmed by dispatch"
            );
            b.observe_and_drain(
                t.runtime,
                &[runtime::RuntimeEvent::InputAdmitted {
                    session: t.session,
                    operation_id: "multiline".into(),
                    result: Ok(()),
                }],
            );
            let receipt = rx.recv().unwrap().unwrap();
            assert_eq!(receipt["submit"], submit);
            assert_eq!(receipt["completion"], "not_confirmed");
        }
    }
    #[test]
    fn pr9_paste_rejects_unsafe_controls_size_mode_and_unconfirmed_ai_before_claim() {
        let (mut b, mut t) = setup();
        b.allow_input(&t.id, true);
        for text in [
            "a\nb",
            "a\r\nb",
            "a\tb",
            "a\rb",
            "\x1b[200~evil",
            "\x03",
            "\x7f",
            "\u{85}",
        ] {
            let (req, rx) = request(
                &b,
                "paste_text",
                json!({"session_id":t.id,"generation":t.generation,"operation_id":"invalid","text":text}),
            );
            b.handle(req, |_, _| panic!("unsafe paste dispatched"));
            assert!(rx.recv().unwrap().is_err(), "{text:?}");
            assert!(b.claims.is_empty());
        }
        t.bracketed_paste = true;
        b.set_targets(vec![t.clone()]);
        for text in [
            "a".repeat(agent_mcp::MAX_PASTE + 1),
            "\\".repeat(agent_mcp::MAX_PASTE),
        ] {
            let (req, rx) = request(
                &b,
                "paste_text",
                json!({"session_id":t.id,"generation":t.generation,"operation_id":"oversize","text":text}),
            );
            b.handle(req, |_, _| panic!("oversize paste dispatched"));
            assert!(rx.recv().unwrap().is_err());
            assert!(b.claims.is_empty());
        }
        t.known_ai = true;
        t.provider = Some(AgentProvider::Codex);
        b.set_targets(vec![t.clone()]);
        let (req, rx) = request(
            &b,
            "paste_text",
            json!({"session_id":t.id,"generation":t.generation,"operation_id":"unknown-ai","text":"prompt"}),
        );
        b.handle(req, |_, _| panic!("unconfirmed AI dispatched"));
        assert_eq!(
            rx.recv().unwrap().unwrap_err(),
            "ai_execution_unconfirmed_no_effect"
        );
        // Legacy authorized raw typing into that same target retains its contract.
        let (req, _) = request(
            &b,
            "send_text",
            json!({"session_id":t.id,"generation":t.generation,"operation_id":"legacy","text":"raw"}),
        );
        b.handle_and_drain(req, |_, effect| {
            assert!(matches!(effect, Effect::Input { .. }));
            Ok(())
        });
    }
    #[test]
    fn pr9_pending_paste_revalidates_original_mode_execution_and_permission() {
        for action in [
            "mode",
            "execution",
            "provider",
            "fallback",
            "revoke",
            "reenable",
            "expired",
            "rotated",
            "pane",
        ] {
            let (mut b, mut t) = setup();
            t.bracketed_paste = true;
            b.set_targets(vec![t.clone()]);
            b.allow_input(&t.id, true);
            let args = json!({"session_id":t.id,"generation":t.generation,"operation_id":"frozen-paste","text":"one\ntwo","submit":true});
            let (req, rx) = request(&b, "paste_text", args.clone());
            b.handle(req, |_, _| panic!("claim dispatched inline"));
            let mut changed = t.clone();
            match action {
                "mode" => changed.bracketed_paste = false,
                "execution" => {
                    changed.execution = Some(AgentExecutionIdentity::fixture(
                        crate::agent_detect::AgentKind::Codex,
                        2,
                    ))
                }
                "provider" => changed.provider = Some(AgentProvider::Codex),
                "fallback" => changed.known_ai = true,
                "revoke" => b.allow_input(&t.id, false),
                "reenable" => {
                    b.allow_input(&t.id, false);
                    b.allow_input(&t.id, true);
                }
                "expired" => {
                    b.claims.values_mut().next().unwrap().request.deadline =
                        Instant::now() - Duration::from_secs(1)
                }
                "rotated" => {
                    b.server.as_ref().unwrap().auth.rotate_at(now());
                }
                "pane" => changed.pane = runtime::MuxPaneId::new(),
                _ => unreachable!(),
            }
            b.set_targets(vec![changed]);
            b.drain_history(|_, _| panic!("{action} admitted stale paste"));
            let rejected = rx.recv().unwrap().unwrap();
            assert_eq!(rejected["status"], "rejected", "{action}");
            b.set_targets(vec![t.clone()]);
            b.share(&t, true);
            b.allow_input(&t.id, true);
            let (req, rx) = request(&b, "paste_text", args);
            b.handle_and_drain(req, |_, _| {
                panic!("stale paste retry must remain tombstoned")
            });
            assert_eq!(rx.recv().unwrap().unwrap(), rejected);
        }
    }
    #[test]
    fn pr9_notify_own_answer_stays_original_after_switch_and_connection_stop() {
        let (mut b, t) = setup();
        let (req, rx) = request(
            &b,
            "notify",
            json!({"session_id":t.id,"generation":t.generation,"operation_id":"own-answer","message":"Cloud/Grok own final answer"}),
        );
        b.handle(req, |_, _| panic!("notify must never enter stdin"));
        let completion = b.history.wait().unwrap();
        assert!(matches!(completion.result, ResultKind::Claim(_)));
        b.complete_history(
            completion,
            &mut |_, _| panic!("notify must never type"),
            &mut Vec::new(),
        );
        // Claim has committed; finish is asynchronous and has not been projected to App.
        b.set_targets(vec![Target::fixture("new-selection", "g2"), t.clone()]);
        b.stop_connection();
        let notices = b.drain_history(|_, _| panic!("stopped notify typed"));
        assert_eq!(rx.recv().unwrap().unwrap()["status"], "stored");
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].target.id, t.id);
        assert_eq!(notices[0].message, "Cloud/Grok own final answer");
        assert_eq!(b.answers[0].session, t.id);
    }
    #[test]
    fn pr9_original_execution_exit_during_claim_cannot_rebind_to_fallback() {
        let (mut b, mut t) = setup();
        let mut process = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let execution = AgentExecutionIdentity::fixture_current(
            crate::agent_detect::AgentKind::Claude,
            process.id(),
        )
        .unwrap();
        t.execution = Some(execution);
        t.known_ai = true;
        t.provider = Some(AgentProvider::Claude);
        t.bracketed_paste = true;
        b.set_targets(vec![t.clone()]);
        b.allow_input(&t.id, true);
        let (req, rx) = request(
            &b,
            "paste_text",
            json!({"session_id":t.id,"generation":t.generation,"operation_id":"dead-execution","text":"one\ntwo","submit":true}),
        );
        b.handle(req, |_, _| panic!("claim dispatched inline"));
        process.kill().unwrap();
        process.wait().unwrap();
        assert!(!execution.is_current());
        // The session/generation/provider projection is deliberately unchanged and stale.
        b.drain_history(|_, _| panic!("dead execution reached fallback stdin"));
        assert_eq!(rx.recv().unwrap().unwrap()["status"], "rejected");
    }
    #[test]
    fn pr7_locked_sqlite_claim_does_not_block_interactive_handle() {
        let dir = std::env::temp_dir().join(format!("deppy-pr7-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("history.db");
        let mut b = CloudAgent::new(
            &path,
            secret::RedactionService::new(),
            egui::Context::default(),
        );
        b.drain_history(|_, _| panic!("startup input"));
        b.server = Some(Server::start(0, "", || {}).unwrap());
        let t = Target::fixture("original-session", "generation-1");
        b.set_targets(vec![t.clone()]);
        b.share(&t, true);
        b.allow_input(&t.id, true);
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let (req, rx) = request(
            &b,
            "send_text",
            json!({"session_id":t.id,"generation":t.generation,"operation_id":"locked-claim","text":"pwd"}),
        );
        let started = Instant::now();
        let mut effects = 0;
        b.handle(req, |_, _| {
            effects += 1;
            Ok(())
        });
        for _ in 0..100 {
            b.poll_history(|_, _| {
                effects += 1;
                Ok(())
            });
        }
        let elapsed = started.elapsed();
        eprintln!("PR7 locked claim handle + 100 interactive polls: {elapsed:?}");
        db.execute_batch("COMMIT").unwrap();
        drop(b);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(
            elapsed < Duration::from_millis(20),
            "interactive claim blocked for {elapsed:?}"
        );
        assert_eq!(
            effects, 0,
            "claim completion must return to App before input"
        );
        assert!(
            rx.try_recv().is_err(),
            "no response before durable completion"
        );
    }
    #[test]
    fn pr7_claim_completion_is_required_before_input_dispatch() {
        let (mut b, t) = setup();
        b.allow_input(&t.id, true);
        let (req, rx) = request(
            &b,
            "send_text",
            json!({"session_id":t.id,"generation":t.generation,"operation_id":"deferred-claim","text":"pwd"}),
        );
        let mut effects = 0;
        b.handle(req, |_, _| {
            effects += 1;
            Ok(())
        });
        assert_eq!(
            effects, 0,
            "durable claim must complete asynchronously before input dispatch"
        );
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn pr7_pending_claim_rechecks_revoke_expiry_and_exact_target() {
        for action in [
            "expire",
            "take_control",
            "reenable",
            "unshare",
            "reshare",
            "rotate",
            "stop",
            "generation",
            "runtime",
            "session",
            "pane",
            "workspace",
            "exited",
        ] {
            let (mut b, t) = setup();
            b.allow_input(&t.id, true);
            let args = json!({"session_id":t.id,"generation":t.generation,"operation_id":"pending-claim","text":"pwd"});
            let (req, rx) = request(&b, "send_text", args.clone());
            b.handle(req, |_, _| panic!("claim is asynchronous"));
            let mut changed = t.clone();
            match action {
                "expire" => {
                    b.claims.values_mut().next().unwrap().request.deadline =
                        Instant::now() - Duration::from_secs(1)
                }
                "take_control" => b.take_control(),
                "reenable" => {
                    b.take_control();
                    b.allow_input(&t.id, true);
                }
                "unshare" => b.share(&t, false),
                "reshare" => {
                    b.share(&t, false);
                    b.share(&t, true);
                    b.allow_input(&t.id, true);
                }
                "rotate" => {
                    b.server.as_ref().unwrap().auth.rotate_at(now());
                }
                "stop" => b.stop_connection(),
                "generation" => changed.generation = "changed".into(),
                "runtime" => changed.runtime += 1,
                "session" => changed.session = runtime::SessionId(2),
                "pane" => changed.pane = runtime::MuxPaneId::new(),
                "workspace" => changed.workspace = "another".into(),
                "exited" => changed.live = false,
                _ => unreachable!(),
            }
            if matches!(
                action,
                "generation" | "runtime" | "session" | "pane" | "workspace" | "exited"
            ) {
                b.set_targets(vec![changed]);
            }
            b.drain_history(|_, _| panic!("{action} dispatched input after pending claim"));
            let rejected = rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
            assert_eq!(rejected["status"], "rejected", "{action}");
            assert_eq!(rejected["retry"], false);
            if b.server.is_none() {
                b.server = Some(Server::start(0, "", || {}).unwrap());
            }
            b.set_targets(vec![t.clone()]);
            b.share(&t, true);
            b.allow_input(&t.id, true);
            let (retry, rx) = request(&b, "send_text", args);
            b.handle_and_drain(retry, |_, _| panic!("{action} retry dispatched input"));
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap(),
                rejected
            );
        }
    }
    #[test]
    fn pr7_locked_finish_does_not_block_or_publish_uncommitted_answer() {
        let dir = std::env::temp_dir().join(format!("deppy-pr7-finish-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("history.db");
        let mut b = CloudAgent::new(
            &path,
            secret::RedactionService::new(),
            egui::Context::default(),
        );
        b.drain_history(|_, _| panic!());
        b.server = Some(Server::start(0, "", || {}).unwrap());
        let t = Target::fixture("original-session", "generation-1");
        b.set_targets(vec![t.clone()]);
        b.share(&t, true);
        let (req, rx) = request(
            &b,
            "notify",
            json!({"session_id":t.id,"generation":t.generation,"operation_id":"locked-answer","message":"Grok 자체 답변"}),
        );
        b.handle(req, |_, _| panic!("notify must never type"));
        let claim = b.history.wait().unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let mut notices = Vec::new();
        let started = Instant::now();
        b.complete_history(claim, &mut |_, _| panic!(), &mut notices);
        for _ in 0..100 {
            notices.extend(b.poll_history(|_, _| panic!()));
        }
        let elapsed = started.elapsed();
        eprintln!("PR7 locked finish + 100 interactive polls: {elapsed:?}");
        assert!(
            elapsed < Duration::from_millis(20),
            "interactive finish/polls blocked {elapsed:?}"
        );
        assert!(rx.try_recv().is_err());
        assert!(notices.is_empty());
        assert!(b.answers.is_empty());
        db.execute_batch("COMMIT").unwrap();
        notices.extend(b.drain_history(|_, _| panic!()));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap()["status"],
            "stored"
        );
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].target.id, t.id);
        assert_eq!(b.answers[0].session, t.id);
        assert_eq!(b.answers[0].message, "Grok 자체 답변");
        drop(b);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn pr7_durable_claim_exists_at_effect_and_admission_unknown_is_not_retried() {
        let dir = std::env::temp_dir().join(format!("deppy-pr7-durable-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("history.db");
        let mut b = CloudAgent::new(
            &path,
            secret::RedactionService::new(),
            egui::Context::default(),
        );
        b.drain_history(|_, _| panic!());
        b.server = Some(Server::start(0, "", || {}).unwrap());
        let t = Target::fixture("original-session", "generation-1");
        b.set_targets(vec![t.clone()]);
        b.share(&t, true);
        b.allow_input(&t.id, true);
        let args = json!({"session_id":t.id,"generation":t.generation,"operation_id":"durable-before-input","text":"pwd"});
        let (req, rx) = request(&b, "send_text", args.clone());
        b.handle(req, |_, _| panic!());
        let mut effects = 0;
        b.drain_history(|_, _| {
            let db = agent_mcp::History::open(&path).unwrap();
            let Claim::Existing(outcome) = db
                .claim(
                    "durable-before-input",
                    "send_text",
                    &args,
                    &t.workspace,
                    &t.id,
                )
                .unwrap()
            else {
                panic!("input ran before durable claim");
            };
            assert_eq!(outcome, json!({"status":"unknown","retry":false}));
            effects += 1;
            Ok(())
        });
        b.observe_input(
            t.runtime,
            &[runtime::RuntimeEvent::InputAdmitted {
                session: t.session,
                operation_id: "durable-before-input".into(),
                result: Err(runtime::PtyInputRejectReason::AdmissionUnknown),
            }],
        );
        assert!(
            rx.try_recv().is_err(),
            "response must wait for async finish"
        );
        b.drain_history(|_, _| panic!());
        let outcome = rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
        assert_eq!(outcome["status"], "unknown");
        assert_eq!(outcome["retry"], false);
        assert_eq!(outcome["error"], "pty_admission_unknown");
        let (req, rx) = request(&b, "send_text", args);
        b.handle_and_drain(req, |_, _| panic!("unknown retry typed"));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap(),
            outcome
        );
        assert_eq!(effects, 1);
        drop(b);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn pr7_operation_and_actor_byte_budgets_reject_known_unsent_without_eviction() {
        let (mut b, t) = setup();
        b.allow_input(&t.id, true);
        let mut receivers = Vec::new();
        for n in 0..MAX_OPERATIONS {
            let (req, rx) = request(
                &b,
                "send_text",
                json!({"session_id":t.id,"generation":t.generation,"operation_id":format!("cap-{n}"),"text":"x".repeat(agent_mcp::MAX_TEXT)}),
            );
            b.handle(req, |_, _| panic!());
            receivers.push(rx);
        }
        assert_eq!(b.claims.len(), MAX_OPERATIONS);
        assert!(b.history.retained_bytes() <= crate::cloud_history_worker::MAX_BYTES);
        let (req, rx) = request(
            &b,
            "send_text",
            json!({"session_id":t.id,"generation":t.generation,"operation_id":"cap-over","text":"pwd"}),
        );
        b.handle(req, |_, _| panic!());
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err("busy_no_effect".into())
        );
        b.take_control();
        b.drain_history(|_, _| panic!());
        for rx in receivers {
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap()["status"],
                "rejected"
            );
        }
        assert_eq!(b.history.retained_bytes(), 0);
        assert!(b.claims.is_empty());
        assert!(b.finishes.is_empty());
        assert!(b.records.iter().all(|r| r.id != "cap-over"));
    }
    #[test]
    fn pr7_oversized_at_rest_claim_fields_are_rejected_without_effect_or_row_changes() {
        for column in ["fingerprint", "outcome"] {
            let dir =
                std::env::temp_dir().join(format!("deppy-pr7-at-rest-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            let path = dir.join("history.db");
            let t = Target::fixture("original-session", "generation-1");
            let args = json!({"session_id":t.id,"generation":t.generation,"operation_id":"oversized-row","text":"pwd"});
            let db = agent_mcp::History::open(&path).unwrap();
            assert_eq!(
                db.claim("oversized-row", "send_text", &args, &t.workspace, &t.id)
                    .unwrap(),
                Claim::New
            );
            drop(db);
            let mut b = CloudAgent::new(
                &path,
                secret::RedactionService::new(),
                egui::Context::default(),
            );
            b.drain_history(|_, _| panic!());
            b.server = Some(Server::start(0, "", || {}).unwrap());
            b.set_targets(vec![t.clone()]);
            b.share(&t, true);
            b.allow_input(&t.id, true);
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute(
                &format!(
                    "UPDATE operations SET {column}=zeroblob(8388608) WHERE id='oversized-row'"
                ),
                [],
            )
            .unwrap();
            let (req, rx) = request(&b, "send_text", args);
            b.handle(req, |_, _| panic!());
            assert!(b.history.retained_bytes() <= crate::cloud_history_worker::MAX_BYTES);
            b.drain_history(|_, _| panic!("oversized receipt must never type"));
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(1)).unwrap(),
                Err("operation_conflict_or_history_unavailable_no_effect".into())
            );
            assert_eq!(
                db.query_row(
                    &format!("SELECT length({column}) FROM operations WHERE id='oversized-row'"),
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                8_388_608
            );
            assert_eq!(b.history.retained_bytes(), 0);
            assert!(b.pending.is_empty());
            drop(b);
            drop(db);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
    #[test]
    fn pr7_older_list_cannot_erase_new_answer_and_new_runtime_does_not_retarget_notice() {
        let (mut b, t) = setup();
        b.history_dirty = true;
        b.dispatch_history();
        let older_list = b.history.wait().unwrap();
        let (req, rx) = request(
            &b,
            "notify",
            json!({"session_id":t.id,"generation":t.generation,"operation_id":"new-answer","message":"last answer"}),
        );
        b.handle(req, |_, _| panic!());
        let claim = b.history.wait().unwrap();
        let mut notices = Vec::new();
        b.complete_history(claim, &mut |_, _| panic!(), &mut notices);
        let finish = b.history.wait().unwrap();
        let mut changed = t.clone();
        changed.runtime += 1;
        changed.generation = "new-runtime".into();
        b.set_targets(vec![changed]);
        b.complete_history(finish, &mut |_, _| panic!(), &mut notices);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap()["status"],
            "stored"
        );
        assert_eq!(notices[0].target.runtime, t.runtime);
        assert_eq!(notices[0].target.generation, t.generation);
        b.complete_history(older_list, &mut |_, _| panic!(), &mut notices);
        b.drain_history(|_, _| panic!());
        assert_eq!(b.answers[0].message, "last answer");
        assert_eq!(b.answers[0].session, t.id);
    }
    #[test]
    fn pr7_shutdown_cancels_pending_input_claim_and_drains_accepted_answer_finish() {
        for phase in ["claim", "finish"] {
            let dir =
                std::env::temp_dir().join(format!("deppy-pr7-shutdown-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            let path = dir.join("history.db");
            let mut b = CloudAgent::new(
                &path,
                secret::RedactionService::new(),
                egui::Context::default(),
            );
            b.drain_history(|_, _| panic!());
            b.server = Some(Server::start(0, "", || {}).unwrap());
            let t = Target::fixture("original-session", "generation-1");
            b.set_targets(vec![t.clone()]);
            b.share(&t, true);
            b.allow_input(&t.id, true);
            let (tool, args) = if phase == "claim" {
                (
                    "send_text",
                    json!({"session_id":t.id,"generation":t.generation,"operation_id":"shutdown-operation","text":"pwd"}),
                )
            } else {
                (
                    "notify",
                    json!({"session_id":t.id,"generation":t.generation,"operation_id":"shutdown-operation","message":"final answer"}),
                )
            };
            let (req, rx) = request(&b, tool, args.clone());
            b.handle(req, |_, _| panic!());
            if phase == "finish" {
                let completion = b.history.wait().unwrap();
                b.complete_history(completion, &mut |_, _| panic!(), &mut Vec::new());
                assert_eq!(b.finishes.len(), 1);
            }
            b.shutdown();
            assert!(b.claims.is_empty());
            assert!(b.pending.is_empty());
            assert!(b.finishes.is_empty());
            assert_eq!(b.history.retained_bytes(), 0);
            let db = agent_mcp::History::open(&path).unwrap();
            let Claim::Existing(outcome) = db
                .claim("shutdown-operation", tool, &args, &t.workspace, &t.id)
                .unwrap()
            else {
                panic!("shutdown lost durable tombstone");
            };
            if phase == "claim" {
                assert_eq!(outcome, json!({"status":"unknown","retry":false}));
                assert!(rx.recv_timeout(Duration::from_secs(1)).is_err());
            } else {
                assert_eq!(outcome["status"], "stored");
                assert_eq!(
                    rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap(),
                    outcome
                );
                assert_eq!(db.recent().unwrap()[0].message, "final answer");
            }
            drop(db);
            drop(b);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
    #[test]
    fn input_and_groks_own_answer_are_independent_deduplicated_effects() {
        let (mut b, t) = setup();
        let mut effects = 0;
        let args = json!({"session_id":t.id,"generation":t.generation,"operation_id":"input-1","text":"pwd","submit":true});
        let (req, rx) = request(&b, "send_text", args.clone());
        assert!(
            b.handle_and_drain(req, |_, _| {
                effects += 1;
                Ok(())
            })
            .is_none()
        );
        assert!(rx.recv().unwrap().is_err());
        assert_eq!(effects, 0);
        b.allow_input(&t.id, true);
        let (req, rx) = request(&b, "send_text", args.clone());
        b.handle_and_drain(req, |target, effect| {
            assert_eq!(target.id, t.id);
            assert_eq!(target.workspace, t.workspace);
            assert_eq!(target.runtime, t.runtime);
            let Effect::Input { bytes, .. } = effect else {
                panic!()
            };
            assert_eq!(bytes, b"pwd\r");
            effects += 1;
            Ok(())
        });
        b.observe_and_drain(
            t.runtime,
            &[runtime::RuntimeEvent::InputAdmitted {
                session: t.session,
                operation_id: "input-1".into(),
                result: Ok(()),
            }],
        );
        assert_eq!(rx.recv().unwrap().unwrap()["status"], "queued");
        let (req, rx) = request(&b, "send_text", args);
        b.handle_and_drain(req, |_, _| {
            effects += 1;
            Ok(())
        });
        assert_eq!(rx.recv().unwrap().unwrap()["status"], "queued");
        assert_eq!(effects, 1);
        b.take_control();
        let answer = json!({"session_id":t.id,"generation":t.generation,"operation_id":"answer-1","message":"그록봇 자체 답변\n분석 완료"});
        let (req, rx) = request(&b, "notify", answer.clone());
        let notice = b
            .handle_and_drain(req, |_, _| panic!("answer must not enter PTY"))
            .unwrap();
        assert_eq!(notice.message, "그록봇 자체 답변\n분석 완료");
        assert_eq!(rx.recv().unwrap().unwrap()["status"], "stored");
        let (req, rx) = request(&b, "notify", answer);
        assert!(b.handle_and_drain(req, |_, _| panic!()).is_none());
        assert_eq!(rx.recv().unwrap().unwrap()["status"], "stored");
        assert_eq!(b.records.iter().filter(|r| r.tool == "notify").count(), 1);
        assert_eq!(b.records[0].message, "그록봇 자체 답변\n분석 완료");
    }
    #[test]
    fn input_waits_for_correlated_pty_result_and_read_only_oauth_cannot_type() {
        let (mut b, t) = setup();
        b.allow_input(&t.id, true);
        let args = json!({"session_id":t.id,"generation":t.generation,"operation_id":"rejected-input","text":"pwd"});
        let (mut req, rx) = request(&b, "send_text", args.clone());
        req.input_scope = false;
        b.handle_and_drain(req, |_, _| panic!("read-only token typed"));
        assert!(rx.recv().unwrap().is_err());
        let (req, rx) = request(&b, "send_text", args.clone());
        let mut writes = 0;
        b.handle_and_drain(req, |_, _| {
            writes += 1;
            Ok(())
        });
        assert!(rx.try_recv().is_err());
        let ack = runtime::RuntimeEvent::InputAdmitted {
            session: t.session,
            operation_id: "rejected-input".into(),
            result: Err(runtime::PtyInputRejectReason::QueueFull),
        };
        b.observe_and_drain(t.runtime + 1, std::slice::from_ref(&ack));
        assert!(rx.try_recv().is_err());
        b.observe_and_drain(t.runtime, &[ack]);
        assert_eq!(rx.recv().unwrap().unwrap()["status"], "rejected");
        let (req, rx) = request(&b, "send_text", args);
        b.handle_and_drain(req, |_, _| {
            writes += 1;
            Ok(())
        });
        assert_eq!(rx.recv().unwrap().unwrap()["status"], "rejected");
        assert_eq!(writes, 1);
        let (req, rx) = request(
            &b,
            "send_text",
            json!({"session_id":t.id,"generation":t.generation,"operation_id":"unknown-input","text":"pwd"}),
        );
        b.handle_and_drain(req, |_, _| Ok(()));
        b.pending.get_mut("unknown-input").unwrap().deadline =
            Instant::now() - Duration::from_secs(1);
        b.expire_pending();
        assert!(rx.recv().unwrap().is_err());
        b.observe_and_drain(
            t.runtime,
            &[runtime::RuntimeEvent::InputAdmitted {
                session: t.session,
                operation_id: "unknown-input".into(),
                result: Ok(()),
            }],
        );
        assert!(
            b.records
                .iter()
                .find(|r| r.id == "unknown-input")
                .unwrap()
                .outcome
                .contains("pty_queue")
        );
    }
    #[test]
    fn revoked_or_expired_requests_cannot_type_or_store_answers() {
        let (mut b, t) = setup();
        b.allow_input(&t.id, true);
        let args =
            json!({"session_id":t.id,"generation":t.generation,"operation_id":"old","text":"pwd"});
        let (req, rx) = request(&b, "send_text", args.clone());
        b.server.as_ref().unwrap().auth.rotate_at(now());
        b.handle_and_drain(req, |_, _| panic!("revoked request dispatched"));
        assert!(rx.recv().unwrap().is_err());
        let (mut req, rx) = request(&b, "send_text", args);
        req.deadline = Instant::now() - Duration::from_secs(1);
        b.handle_and_drain(req, |_, _| panic!("expired request dispatched"));
        assert!(rx.recv().unwrap().is_err());
        assert!(b.records.is_empty());
    }
    #[test]
    fn claim_expiry_records_no_effect_for_exact_retries() {
        let dir = std::env::temp_dir().join(format!("deppy-claim-expiry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.db");
        let mut b = CloudAgent::new(
            &path,
            secret::RedactionService::new(),
            egui::Context::default(),
        );
        b.drain_history(|_, _| panic!("startup input"));
        b.server = Some(Server::start(0, "", || {}).unwrap());
        let t = Target::fixture("original-session", "generation-1");
        b.set_targets(vec![t.clone()]);
        b.share(&t, true);
        b.allow_input(&t.id, true);
        let (ready, ready_rx) = mpsc::sync_channel(1);
        let lock = std::thread::spawn(move || {
            let db = rusqlite::Connection::open(path).unwrap();
            db.execute_batch("BEGIN EXCLUSIVE").unwrap();
            ready.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(80));
            db.execute_batch("COMMIT").unwrap();
        });
        ready_rx.recv().unwrap();
        let args = json!({"session_id":t.id,"generation":t.generation,"operation_id":"expired-claim","text":"pwd"});
        let (mut req, rx) = request(&b, "send_text", args.clone());
        req.deadline = Instant::now() + Duration::from_millis(25);
        b.handle_and_drain(req, |_, _| panic!("expired claim dispatched"));
        let first = rx.recv().unwrap();
        lock.join().unwrap();
        let (req, rx) = request(&b, "send_text", args);
        b.handle_and_drain(req, |_, _| panic!("no-effect retry dispatched"));
        let retry = rx.recv().unwrap().unwrap();
        assert_eq!(
            retry["status"], "rejected",
            "a known no-effect claim must not remain unknown"
        );
        assert_eq!(retry["error"], "expired_or_revoked_request_no_effect");
        assert_eq!(first.unwrap(), retry);
        assert!(b.records.iter().any(|r| r.id == "expired-claim"
            && r.outcome.contains("expired_or_revoked_request_no_effect")));
    }
    #[test]
    fn list_and_cursor_reads_are_scoped_and_refresh_hidden_screens() {
        let (mut b, t) = setup();
        let other = Target::fixture("not-shared", "generation-2");
        b.set_targets(vec![t.clone(), other]);
        let (req, rx) = request(&b, "list_sessions", json!({}));
        b.handle_and_drain(req, |_, _| panic!());
        let result = rx.recv().unwrap().unwrap();
        assert_eq!(result["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(result["sessions"][0]["session_id"], t.id);
        let (req, rx) = request(
            &b,
            "read_output",
            json!({"session_id":t.id,"generation":t.generation}),
        );
        b.handle_and_drain(req, |_, e| {
            assert!(matches!(e, Effect::Watch));
            Ok(())
        });
        let result = rx.recv().unwrap().unwrap();
        assert_eq!(result["screen"], "hello");
        assert_eq!(result["reset"], true);
        let (req, rx) = request(
            &b,
            "read_output",
            json!({"session_id":t.id,"generation":t.generation,"cursor":result["cursor"]}),
        );
        b.handle_and_drain(req, |_, _| Ok(()));
        assert!(rx.recv().unwrap().unwrap()["screen"].is_null());
    }
}

#[cfg(test)]
mod guarded_input_tests {
    use super::*;
    use runtime::{RuntimeCommandSink as _, RuntimeEventStream as _};
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };
    struct NoSecrets;
    impl runtime::RuntimeSecretResolver for NoSecrets {
        fn resolve(&self, _: &str) -> anyhow::Result<runtime::RuntimeSecret> {
            anyhow::bail!("unused")
        }
    }
    #[test]
    fn queued_cloud_input_is_cancelled_before_pty_admission() {
        for action in [
            "take_control",
            "unshare",
            "disable",
            "rotate",
            "stop",
            "generation",
            "expire",
            "reenable",
        ] {
            let root =
                std::env::temp_dir().join(format!("deppy-guarded-input-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let factory = runtime::InProcessRuntimeHostFactory::new(
                Arc::new(NoSecrets),
                secret::RedactionService::new(),
            );
            let mut host = factory
                .create_client(runtime::RuntimeHostConfig {
                    scrollback_policy: None,
                    output_batch_ms: 5,
                    logs_root: root.join("logs"),
                    persist: None,
                    cwd: Some(root),
                    extra_env: vec![],
                })
                .unwrap();
            let events = host.subscribe();
            host.send_command(runtime::RuntimeCommand::SpawnShell {
                cols: 100,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
            let end = Instant::now() + Duration::from_secs(8);
            let session = loop {
                if let Some(session) = events.drain().iter().find_map(|e| match e {
                    runtime::RuntimeEvent::ShellSpawned { session } => Some(*session),
                    _ => None,
                }) {
                    break session;
                }
                assert!(Instant::now() < end, "shell spawn timed out");
                std::thread::sleep(Duration::from_millis(5));
            };
            let armed = Arc::new(AtomicBool::new(false));
            let a = armed.clone();
            let (paused, paused_rx) = mpsc::sync_channel(1);
            let (release, release_rx) = mpsc::sync_channel(1);
            let release_rx = Mutex::new(release_rx);
            let gate_events = host.subscribe_with_wake_background(Arc::new(move || {
                if a.swap(false, Ordering::SeqCst) {
                    paused.send(()).unwrap();
                    release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                }
            }));
            armed.store(true, Ordering::SeqCst);
            host.send_command(runtime::RuntimeCommand::SetRemoteViewing {
                session,
                viewing: true,
                ttl_ms: 15000,
            })
            .unwrap();
            paused_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            let mut b = CloudAgent::memory();
            b.server = Some(Server::start(0, "", || {}).unwrap());
            let mut t = Target::fixture("guarded-session", "generation-1");
            t.session = session;
            b.set_targets(vec![t.clone()]);
            b.share(&t, true);
            b.allow_input(&t.id, true);
            let auth = &b.server.as_ref().unwrap().auth;
            let epoch = auth
                .authenticate(&format!("Bearer {}", auth.token_for_user().as_str()), now())
                .unwrap();
            let (reply, rx) = mpsc::sync_channel(1);
            let req = Request {
                epoch,
                access_key: None,
                input_scope: true,
                deadline: Instant::now()
                    + if action == "expire" {
                        Duration::from_millis(25)
                    } else {
                        Duration::from_secs(3)
                    },
                tool: "send_text".into(),
                args: json!({"session_id":t.id,"generation":t.generation,"operation_id":"queued-before-revoke","text":"printf 'DEPPY_REVOKED_INPUT\\n'","submit":true}),
                reply,
            };
            b.handle_and_drain(req, |t, e| match e {
                Effect::Input {
                    operation_id,
                    bytes,
                    admission,
                } => host
                    .send_guarded_input(t.session, operation_id, bytes, admission)
                    .map_err(|_| "queue rejected".into()),
                _ => panic!("unexpected watch"),
            });
            assert!(rx.try_recv().is_err());
            match action {
                "take_control" => b.take_control(),
                "unshare" => b.share(&t, false),
                "disable" => b.allow_input(&t.id, false),
                "rotate" => {
                    b.action = Some(Action::Rotate);
                    b.apply_action(&egui::Context::default());
                }
                "stop" => {
                    b.action = Some(Action::Stop);
                    b.apply_action(&egui::Context::default());
                }
                "generation" => {
                    t.generation = "generation-2".into();
                    b.set_targets(vec![t.clone()]);
                }
                "expire" => std::thread::sleep(Duration::from_millis(40)),
                "reenable" => {
                    b.take_control();
                    b.allow_input(&t.id, true);
                }
                _ => unreachable!(),
            }
            release.send(()).unwrap();
            let end = Instant::now() + Duration::from_secs(3);
            let mut receipt = None;
            let mut output = false;
            while Instant::now() < end && receipt.is_none() {
                let events = gate_events.drain();
                b.observe_and_drain(t.runtime, &events);
                output |= events
                    .iter()
                    .filter_map(|e| e.viewport())
                    .any(|(_, snap, _, _)| screen_text(snap).contains("DEPPY_REVOKED_INPUT"));
                receipt = rx.try_recv().ok();
                std::thread::sleep(Duration::from_millis(5));
            }
            host.shutdown();
            assert_eq!(
                receipt.unwrap().unwrap()["status"],
                "rejected",
                "revocation {action} must cancel the queued input"
            );
            assert!(!output, "revocation {action} leaked text to the terminal");
        }
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    #[test]
    fn reused_worker_session_numbers_do_not_alias_another_workspace_or_incarnation() {
        let target = Target::fixture("session-a", "generation-a");
        let pane = runtime::PaneSnapshot {
            id: target.pane.clone(),
            session_id: Some(target.session),
            title: "a".into(),
            persistent_session_id: Some(target.id.clone()),
        };
        let mux = runtime::MuxSnapshot {
            tabs: vec![runtime::TabSnapshot {
                id: runtime::MuxTabId::new(),
                title: "a".into(),
                layout: runtime::LayoutNode::Pane(target.pane.clone()),
                panes: vec![pane],
            }],
            active_tab: None,
            focused_pane: None,
        };
        assert!(target_matches(&target, 1, "w", &mux, false, true));
        assert!(!target_matches(&target, 2, "w", &mux, false, true));
        assert!(!target_matches(
            &target,
            1,
            "other-workspace",
            &mux,
            false,
            true
        ));
        assert!(!target_matches(&target, 1, "w", &mux, true, true));
        assert!(target_matches(&target, 1, "w", &mux, true, false));
        let mut changed = mux.clone();
        changed.tabs[0].panes[0].persistent_session_id = Some("session-b".into());
        assert!(!target_matches(&target, 1, "w", &changed, false, true));
    }
}

#[cfg(test)]
mod http_end_to_end_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpStream,
        time::Duration,
    };
    #[cfg(unix)]
    #[test]
    fn automatic_start_cancel_keeps_manual_host_and_never_copies_unverified_url() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("deppy-auto-bridge-{}", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            "#!/bin/sh\nprintf 'https://fixture.trycloudflare.com\\n' >&2\nexec /bin/sleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut bridge = CloudAgent::memory();
        bridge.port = 0;
        bridge.hostname = "saved.example.com".into();
        let ctx = egui::Context::default();
        bridge.start_auto(&ctx, path.clone());
        let addr = bridge.server.as_ref().unwrap().addr;
        bridge.start_auto(&ctx, path.clone());
        assert_eq!(bridge.server.as_ref().unwrap().addr, addr);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while bridge.connection != Connection::Verifying {
            bridge.apply_action(&ctx);
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(bridge.endpoint().is_none());
        assert_eq!(bridge.hostname, "saved.example.com");
        let auth = bridge.server.as_ref().unwrap().auth.clone();
        let token = auth.token_for_user();
        bridge.action = Some(Action::Stop);
        bridge.apply_action(&ctx);
        assert!(bridge.server.is_none());
        assert!(
            auth.authenticate(&format!("Bearer {}", token.as_str()), now())
                .is_none()
        );
        bridge.shutdown();
        assert!(!bridge.busy());
        assert_eq!(bridge.hostname, "saved.example.com");
        std::fs::remove_file(path).unwrap();
    }
    fn rpc(addr: std::net::SocketAddr, token: &str, id: u64, tool: &str, args: Value) -> Value {
        let body=json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":args}}).to_string();
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(s,"POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len()).unwrap();
        let mut response = String::new();
        s.read_to_string(&mut response).unwrap();
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
    }
    #[test]
    #[cfg(unix)]
    fn pr9_http_private_pty_unicode_paste_and_own_answer_roundtrip() {
        use runtime::{RuntimeCommandSink as _, RuntimeEventStream as _};
        struct NoSecrets;
        impl runtime::RuntimeSecretResolver for NoSecrets {
            fn resolve(&self, _: &str) -> anyhow::Result<runtime::RuntimeSecret> {
                anyhow::bail!("unused")
            }
        }
        let root = std::env::temp_dir().join(format!("deppy-pr9-private-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let mut host = runtime::InProcessRuntimeClient::try_new_with_resolver(
            5,
            std::sync::Arc::new(NoSecrets),
            root.clone(),
            secret::RedactionService::new(),
            None,
            Some(root.clone()),
            vec![],
        )
        .unwrap();
        let events = host.subscribe();
        // Noninteractive shell reads no startup files; cat is a private byte echo fixture, no AI.
        host.send_command(runtime::RuntimeCommand::SpawnAgent {
            agent_config_id: None,
            cols: 100,
            rows: 24,
            scrollback_lines: 100,
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                r#"stty -icanon -echo; printf '\033[?2004hOWNER:%s\r\n❯ ' "$$"; exec /bin/cat"#
                    .into(),
            ],
            env_plain: vec![],
            env_secrets: vec![],
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut session = None;
        let mut owner = None;
        while session.is_none() || owner.is_none() {
            for event in events.drain() {
                if let runtime::RuntimeEvent::AgentSpawned { session: id } = event {
                    session = Some(id);
                }
                if let Some((_, snapshot, true, _)) = event.viewport() {
                    let text = screen_text(snapshot);
                    owner = text
                        .split_once("OWNER:")
                        .and_then(|(_, tail)| tail.split(|c: char| !c.is_ascii_digit()).next())
                        .and_then(|pid| pid.parse::<u32>().ok());
                }
            }
            assert!(
                Instant::now() < deadline,
                "private paste fixture startup timed out"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let session = session.unwrap();
        let execution = AgentExecutionIdentity::fixture_current(
            crate::agent_detect::AgentKind::Claude,
            owner.unwrap(),
        )
        .unwrap();
        assert!(execution.is_current());
        let mut bridge = CloudAgent::memory();
        bridge.server = Some(Server::start(0, "", || {}).unwrap());
        let mut target = Target::fixture("paste-original", "g1");
        target.session = session;
        target.known_ai = true;
        target.provider = Some(AgentProvider::Claude);
        target.execution = Some(execution);
        target.bracketed_paste = true;
        target.screen = None;
        bridge.set_targets(vec![target.clone()]);
        bridge.share(&target, true);
        bridge.allow_input(&target.id, true);
        let server = bridge.server.as_ref().unwrap();
        let addr = server.addr;
        let token = server.auth.token_for_user().to_string();
        let client = std::thread::spawn(move || {
            let args = json!({"session_id":"paste-original","generation":"g1","operation_id":"wire-paste","text":"한글 😀\r\nPR9_SECOND\t끝"});
            let first = rpc(addr, &token, 1, "paste_text", args.clone());
            assert_eq!(first["result"]["isError"], false, "{first}");
            assert_eq!(
                rpc(addr, &token, 2, "paste_text", args)["result"],
                first["result"]
            );
            let mut found = false;
            for id in 3..43 {
                let reply = rpc(
                    addr,
                    &token,
                    id,
                    "read_output",
                    json!({"session_id":"paste-original","generation":"g1"}),
                );
                let reply: Value =
                    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap())
                        .unwrap();
                assert_eq!(reply["lossless"], false);
                if reply["screen"].as_str().is_some_and(|screen| {
                    screen.contains("한글 😀") && screen.contains("PR9_SECOND")
                }) {
                    found = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                found,
                "visible private PTY output must include the pasted Unicode lines"
            );
            let rejected = rpc(
                addr,
                &token,
                50,
                "paste_text",
                json!({"session_id":"paste-original","generation":"g1","operation_id":"wire-submit","text":"MUST_NOT_SUBMIT","submit":true}),
            );
            assert_eq!(
                rejected["result"]["isError"], true,
                "existing draft must deny a whole new prompt"
            );
            let append = rpc(
                addr,
                &token,
                51,
                "paste_text",
                json!({"session_id":"paste-original","generation":"g1","operation_id":"wire-append","text":"APPEND_OK"}),
            );
            assert_eq!(
                append["result"]["isError"], false,
                "deliberate no-submit append must retain an existing draft"
            );
            let reply = rpc(
                addr,
                &token,
                100,
                "notify",
                json!({"session_id":"paste-original","generation":"g1","operation_id":"wire-own-answer","message":"Grok/Cloud own final answer"}),
            );
            assert_eq!(reply["result"]["isError"], false);
        });
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut writes = 0;
        let mut notices = Vec::new();
        while !client.is_finished() {
            assert!(
                Instant::now() < deadline,
                "private MCP paste roundtrip timed out"
            );
            let batch = events.drain();
            bridge.observe_input(target.runtime, &batch);
            for event in &batch {
                if let Some((_, snapshot, _, _)) = event.viewport() {
                    target.screen = Some(screen_text(snapshot));
                }
            }
            // The original session keeps its answer while another session is selected first.
            bridge.set_targets(vec![
                Target::fixture("different-selection", "g2"),
                target.clone(),
            ]);
            let mut send = |t: &Target, effect| {
                assert_eq!(t.id, target.id);
                match effect {
                    Effect::PasteInput {
                        operation_id,
                        parts,
                        admission,
                    } => {
                        writes += 1;
                        assert_eq!(
                            parts.len(),
                            if operation_id == "wire-submit" { 2 } else { 1 },
                            "only explicit submit adds one separate Enter"
                        );
                        host.send_guarded_input_batch(t.session, operation_id, parts, admission)
                            .map_err(|_| "private_runtime_rejected".into())
                    }
                    Effect::Watch => host
                        .send_command(runtime::RuntimeCommand::SetRemoteViewing {
                            session: t.session,
                            viewing: true,
                            ttl_ms: 15000,
                        })
                        .map_err(|_| "watch_rejected".into()),
                    Effect::Input { .. } => panic!("paste and notify must not use legacy typing"),
                }
            };
            notices.extend(bridge.poll_history(&mut send));
            while let Some(req) = bridge.next_request() {
                bridge.handle(req, &mut send);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        client.join().unwrap();
        assert_eq!(writes, 3, "exact retry must not dispatch another effect");
        let auth = &bridge.server.as_ref().unwrap().auth;
        let epoch = auth
            .authenticate(&format!("Bearer {}", auth.token_for_user().as_str()), now())
            .unwrap();
        let (reply, receipt) = mpsc::sync_channel(1);
        let req = Request {
            epoch,
            access_key: None,
            input_scope: true,
            deadline: Instant::now() + Duration::from_secs(3),
            tool: "paste_text".into(),
            args: json!({"session_id":target.id,"generation":target.generation,"operation_id":"actual-mode-queue","text":"MUST_NOT_REACH\nSECOND"}),
            reply,
        };
        bridge.handle(req, |_, _| panic!("claim dispatched inline"));
        // Toggle the private echo fixture's live mode while the durable claim is pending.
        host.send_command(runtime::RuntimeCommand::WriteInput {
            session,
            bytes: b"\x1b[?2004l".to_vec(),
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if events
                .drain()
                .iter()
                .any(|event| event.viewport().is_some_and(|(_, _, mode, _)| !mode))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "private mode toggle did not reach runtime"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            target.bracketed_paste,
            "App projection is intentionally stale"
        );
        let mut queued = 0;
        bridge.drain_history(|t, effect| {
            let Effect::PasteInput {
                operation_id,
                parts,
                admission,
            } = effect
            else {
                panic!("unexpected effect")
            };
            queued += 1;
            host.send_guarded_input_batch(t.session, operation_id, parts, admission)
                .map_err(|_| "runtime_rejected".into())
        });
        assert_eq!(
            queued, 1,
            "stale App projection reaches only guarded runtime admission"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let rejected = loop {
            let batch = events.drain();
            assert!(
                !batch
                    .iter()
                    .filter_map(|event| event.viewport())
                    .any(|(_, snapshot, _, _)| screen_text(snapshot).contains("MUST_NOT_REACH"))
            );
            bridge.observe_and_drain(target.runtime, &batch);
            if let Ok(result) = receipt.try_recv() {
                break result.unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "runtime mode rejection receipt timed out"
            );
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(rejected["status"], "rejected");
        assert_eq!(rejected["error"], "pty_AdmissionDenied");
        bridge.stop_connection();
        notices.extend(bridge.drain_history(|_, _| panic!("stopped connection input")));
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].target.id, target.id);
        assert_eq!(bridge.answers[0].session, target.id);
        assert_eq!(bridge.answers[0].message, "Grok/Cloud own final answer");
        host.shutdown();
        bridge.shutdown();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oauth_http_to_real_pty_output_and_own_answer_roundtrip() {
        real_roundtrip(false);
    }
    #[test]
    #[ignore = "requires public Cloudflare access and the bundled companion"]
    fn automatic_public_tunnel_to_real_pty_and_own_answer_roundtrip() {
        real_roundtrip(true);
    }
    fn real_roundtrip(public: bool) {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        use runtime::{RuntimeCommandSink as _, RuntimeEventStream as _};
        use sha2::{Digest, Sha256};
        struct NoSecrets;
        impl runtime::RuntimeSecretResolver for NoSecrets {
            fn resolve(&self, _: &str) -> anyhow::Result<runtime::RuntimeSecret> {
                anyhow::bail!("unused")
            }
        }
        let logs = std::env::temp_dir().join(format!("deppy-cloud-pty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&logs).unwrap();
        let factory = runtime::InProcessRuntimeHostFactory::new(
            std::sync::Arc::new(NoSecrets),
            secret::RedactionService::new(),
        );
        let host = factory
            .create_client(runtime::RuntimeHostConfig {
                scrollback_policy: None,
                output_batch_ms: 5,
                logs_root: logs.clone(),
                persist: None,
                cwd: Some(logs.clone()),
                extra_env: vec![],
            })
            .unwrap();
        let events = host.subscribe();
        host.send_command(runtime::RuntimeCommand::SpawnShell {
            cols: 100,
            rows: 24,
            scrollback_lines: 100,
        })
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let session = loop {
            if let Some(session) = events.drain().iter().find_map(|e| match e {
                runtime::RuntimeEvent::ShellSpawned { session } => Some(*session),
                _ => None,
            }) {
                break session;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        let mut bridge = CloudAgent::memory();
        let ctx = egui::Context::default();
        let endpoint = if public {
            bridge.port = 0;
            bridge.start_auto(
                &ctx,
                tunnel::companion().expect("bundled companion required"),
            );
            let deadline = std::time::Instant::now() + Duration::from_secs(95);
            while !bridge.ready() {
                bridge.apply_action(&ctx);
                assert!(
                    bridge.error.is_none(),
                    "automatic connection failed: {:?}",
                    bridge.error
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "automatic connection timed out"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
            bridge.endpoint()
        } else {
            bridge.server = Some(Server::start(0, "", || {}).unwrap());
            None
        };
        let mut target = Target::fixture("real-session", "real-generation");
        target.session = session;
        target.screen = None;
        bridge.set_targets(vec![target.clone()]);
        bridge.share(&target, true);
        bridge.allow_input(&target.id, true);
        let server = bridge.server.as_ref().unwrap();
        let addr = server.addr;
        fn http(
            addr: std::net::SocketAddr,
            method: &str,
            path: &str,
            kind: &str,
            body: &str,
        ) -> String {
            let mut s = TcpStream::connect(addr).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            write!(s,"{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\r\n{body}",body.len()).unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        }
        let register = http(
            addr,
            "POST",
            "/oauth/register",
            "application/json",
            r#"{"client_name":"Fixture Bot","redirect_uris":["http://127.0.0.1:23456/callback"]}"#,
        );
        let client: Value =
            serde_json::from_str(register.split_once("\r\n\r\n").unwrap().1).unwrap();
        let verifier = "v".repeat(43);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier));
        let resource = endpoint
            .clone()
            .unwrap_or_else(|| format!("http://{addr}/mcp"));
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("client_id", client["client_id"].as_str().unwrap()),
                ("redirect_uri", "http://127.0.0.1:23456/callback"),
                ("resource", &resource),
                ("response_type", "code"),
                ("code_challenge_method", "S256"),
                ("code_challenge", &challenge),
                ("scope", "deppy.read deppy.input"),
            ])
            .finish();
        assert!(
            http(addr, "GET", &format!("/oauth/authorize?{query}"), "", "")
                .starts_with("HTTP/1.1 200")
        );
        let approval = server.auth.approvals().pop().unwrap();
        assert!(server.auth.approve(&approval.id, true));
        let redirect = http(
            addr,
            "GET",
            &format!("/oauth/authorize?request={}", approval.id),
            "",
            "",
        );
        let location = redirect
            .lines()
            .find_map(|l| l.strip_prefix("Location: "))
            .unwrap();
        let code = url::Url::parse(location)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "code")
            .unwrap()
            .1
            .into_owned();
        let form = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("grant_type", "authorization_code"),
                ("client_id", client["client_id"].as_str().unwrap()),
                ("redirect_uri", "http://127.0.0.1:23456/callback"),
                ("resource", &resource),
                ("code", &code),
                ("code_verifier", &verifier),
            ])
            .finish();
        let token_response = http(
            addr,
            "POST",
            "/oauth/token",
            "application/x-www-form-urlencoded",
            &form,
        );
        let tokens: Value =
            serde_json::from_str(token_response.split_once("\r\n\r\n").unwrap().1).unwrap();
        let token = tokens["access_token"].as_str().unwrap().to_owned();
        let client = std::thread::spawn(move || {
            let call = |id, tool, args| {
                if let Some(url) = &endpoint {
                    // This client runs on the same Mac whose negative DNS cache
                    // caused the startup bug; use verified lookup for public tests too.
                    let agent = super::tunnel::public_test_agent();
                    let request = json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":args}}).to_string();
                    let mut response = agent
                        .post(url)
                        .header("Authorization", &format!("Bearer {token}"))
                        .header("Accept", "application/json, text/event-stream")
                        .header("Content-Type", "application/json")
                        .config()
                        .timeout_global(Some(Duration::from_secs(12)))
                        .build()
                        .send(request.as_bytes())
                        .unwrap();
                    serde_json::from_str::<Value>(&response.body_mut().read_to_string().unwrap())
                        .unwrap()
                } else {
                    rpc(addr, &token, id, tool, args)
                }
            };
            let args = json!({"session_id":"real-session","generation":"real-generation","operation_id":"real-input","text":"printf 'DEPPY_%s\\n' REAL_OUTPUT","submit":true});
            let response = call(1, "send_text", args.clone());
            assert_eq!(response["result"]["isError"], false);
            let result: Value =
                serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            assert_eq!(result["admission"], "pty_queue");
            assert_eq!(call(2, "send_text", args)["result"]["isError"], false);
            let mut found = false;
            for n in 0..40 {
                let r = call(
                    10 + n,
                    "read_output",
                    json!({"session_id":"real-session","generation":"real-generation"}),
                );
                let r: Value =
                    serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap())
                        .unwrap();
                if r["screen"]
                    .as_str()
                    .is_some_and(|s| s.contains("DEPPY_REAL_OUTPUT"))
                {
                    found = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            assert!(found, "executed output must return from the real PTY");
            assert_eq!(
                call(
                    100,
                    "notify",
                    json!({"session_id":"real-session","generation":"real-generation","operation_id":"real-answer","message":"Grok own final answer"})
                )["result"]["isError"],
                false
            );
        });
        let deadline =
            std::time::Instant::now() + Duration::from_secs(if public { 90 } else { 15 });
        let mut writes = 0;
        let mut notice = false;
        while !client.is_finished() {
            bridge.apply_action(&ctx);
            assert!(
                std::time::Instant::now() < deadline,
                "real roundtrip timed out"
            );
            let batch = events.drain();
            bridge.observe_and_drain(target.runtime, &batch);
            for e in &batch {
                if let Some((_, snapshot, _, _)) = e.viewport() {
                    target.screen = Some(screen_text(snapshot));
                }
            }
            bridge.set_targets(vec![target.clone()]);
            while let Some(req) = bridge.next_request() {
                notice |= bridge
                    .handle_and_drain(req, |t, e| {
                        let command = match e {
                            Effect::Input {
                                operation_id,
                                bytes,
                                admission,
                            } => {
                                writes += 1;
                                return host
                                    .send_guarded_input(t.session, operation_id, bytes, admission)
                                    .map_err(|_| "runtime_error".into());
                            }
                            Effect::PasteInput {
                                operation_id,
                                parts,
                                admission,
                            } => {
                                writes += 1;
                                return host
                                    .send_guarded_input_batch(
                                        t.session,
                                        operation_id,
                                        parts,
                                        admission,
                                    )
                                    .map_err(|_| "runtime_error".into());
                            }
                            Effect::Watch => runtime::RuntimeCommand::SetRemoteViewing {
                                session: t.session,
                                viewing: true,
                                ttl_ms: 15000,
                            },
                        };
                        host.send_command(command)
                            .map_err(|_| "runtime_error".into())
                    })
                    .is_some();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        client.join().unwrap();
        assert_eq!(writes, 1);
        assert!(notice);
        assert_eq!(bridge.answers[0].message, "Grok own final answer");
        drop(host);
        bridge.shutdown();
        assert!(!bridge.busy());
        drop(bridge);
        std::fs::remove_dir_all(logs).unwrap();
    }
    #[test]
    fn http_client_reads_types_interrupts_and_delivers_its_own_answer_to_original_session() {
        let mut bridge = CloudAgent::memory();
        bridge.server = Some(Server::start(0, "", || {}).unwrap());
        let target = Target::fixture("original-session", "original-generation");
        bridge.set_targets(vec![target.clone()]);
        bridge.share(&target, true);
        bridge.set_targets(vec![target.clone()]);
        let server = bridge.server.as_ref().unwrap();
        let addr = server.addr;
        let token = server.auth.token_for_user().to_string();
        let client = std::thread::spawn(move || {
            let list = rpc(addr, &token, 1, "list_sessions", json!({}));
            let listed: Value =
                serde_json::from_str(list["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            let id = listed["sessions"][0]["session_id"].as_str().unwrap();
            let generation = listed["sessions"][0]["generation"].as_str().unwrap();
            assert_eq!(id, "original-session");
            assert_eq!(
                rpc(
                    addr,
                    &token,
                    2,
                    "read_output",
                    json!({"session_id":id,"generation":generation})
                )["result"]["isError"],
                false
            );
            let input = json!({"session_id":id,"generation":generation,"operation_id":"http-input","text":"pwd","submit":true});
            assert_eq!(
                rpc(addr, &token, 3, "send_text", input.clone())["result"]["isError"],
                true
            );
            assert_eq!(
                rpc(addr, &token, 4, "send_text", input.clone())["result"]["isError"],
                false
            );
            assert_eq!(
                rpc(addr, &token, 5, "send_text", input)["result"]["isError"],
                false
            );
            assert_eq!(
                rpc(
                    addr,
                    &token,
                    6,
                    "send_ctrl_c",
                    json!({"session_id":id,"generation":generation,"operation_id":"http-interrupt"})
                )["result"]["isError"],
                false
            );
            let answer = json!({"session_id":id,"generation":generation,"operation_id":"http-answer","message":"Grok 자체 답변입니다"});
            assert_eq!(
                rpc(addr, &token, 7, "notify", answer.clone())["result"]["isError"],
                false
            );
            assert_eq!(
                rpc(addr, &token, 8, "notify", answer)["result"]["isError"],
                false
            );
        });
        let mut input_bytes = Vec::new();
        let mut notices = 0;
        for n in 0..8 {
            let req = bridge
                .server
                .as_ref()
                .unwrap()
                .requests
                .recv_timeout(Duration::from_secs(4))
                .unwrap();
            if n == 3 {
                bridge.allow_input("original-session", true);
            }
            if n == 6 {
                bridge.take_control();
            }
            if let Some(notice) = bridge.handle_and_drain(req, |t, e| {
                assert_eq!(t.id, "original-session");
                if let Effect::Input { bytes, .. } = e {
                    input_bytes.push(bytes);
                }
                Ok(())
            }) {
                assert_eq!(notice.message, "Grok 자체 답변입니다");
                notices += 1;
            }
            let acknowledgements: Vec<_> = bridge
                .pending
                .iter()
                .map(|(op, p)| runtime::RuntimeEvent::InputAdmitted {
                    session: p.session,
                    operation_id: op.clone(),
                    result: Ok(()),
                })
                .collect();
            bridge.observe_and_drain(target.runtime, &acknowledgements);
        }
        client.join().unwrap();
        assert_eq!(input_bytes, vec![b"pwd\r".to_vec(), vec![3]]);
        assert_eq!(notices, 1);
        assert_eq!(
            bridge
                .records
                .iter()
                .find(|r| r.id == "http-answer")
                .unwrap()
                .message,
            "Grok 자체 답변입니다"
        );
    }
}
