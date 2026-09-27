//! App-owned authorization and effects for the outward MCP bridge.
mod tunnel;
mod ui;
use agent_mcp::{Claim, History, Record, Request, Server, encode_input, now};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path};

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
}
struct Grant {
    generation: String,
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
    grants: HashMap<String, Grant>,
    screens: HashMap<String, Screen>,
    history: Option<History>,
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
    pub fn new(path: &Path, redaction: secret::RedactionService) -> Self {
        let history = History::open(path);
        let error = history
            .as_ref()
            .err()
            .map(|_| "history_unavailable".to_string());
        let mut result = Self::with_history(history.ok(), redaction);
        result.error = error;
        result.reload_history();
        result
    }
    fn with_history(history: Option<History>, redaction: secret::RedactionService) -> Self {
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
            grants: HashMap::new(),
            screens: HashMap::new(),
            history,
            records: vec![],
            answers: std::sync::Arc::from([]),
            pending: HashMap::new(),
            redaction,
            token_lease: None,
        }
    }
    #[cfg(test)]
    fn memory() -> Self {
        Self::with_history(
            Some(History::open_memory().unwrap()),
            secret::RedactionService::new(),
        )
    }
    fn reload_history(&mut self) {
        if let Some(db) = &self.history {
            match db.recent() {
                Ok(rows) => {
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
                Err(_) => self.error = Some("history_read_failed".into()),
            }
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
    pub fn set_targets(&mut self, targets: Vec<Target>) {
        self.targets = targets.into_iter().take(MAX_TARGETS).collect();
        self.grants.retain(|id, g| {
            self.targets
                .iter()
                .any(|t| &t.id == id && t.generation == g.generation)
        });
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
            self.grants.insert(
                t.id.clone(),
                Grant {
                    generation: t.generation.clone(),
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
            if allow {
                g.permit = runtime::InputPermit::new();
            }
            g.input = allow;
        }
    }
    pub fn take_control(&mut self) {
        for g in self.grants.values_mut() {
            g.permit.revoke();
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
        if self.history.is_none() {
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
    pub fn next_request(&self) -> Option<Request> {
        self.server.as_ref()?.requests.try_recv().ok()
    }
    pub fn handle(
        &mut self,
        req: Request,
        mut send: impl FnMut(&Target, Effect) -> Result<(), String>,
    ) -> Option<AnswerNotice> {
        let valid = self.server.as_ref().is_some_and(|s| req.live(&s.auth));
        let mut notice = None;
        let result = if !valid {
            Err("expired_or_revoked_request_no_effect".into())
        } else {
            self.execute(&req, &mut send, &mut notice)
        };
        if result.as_ref().is_ok_and(|v| v["status"] == "awaiting_pty") {
            if let Some(p) = self
                .pending
                .get_mut(req.args["operation_id"].as_str().unwrap_or(""))
            {
                p.reply = Some(req.reply);
            }
        } else {
            let _ = req.reply.try_send(result);
        }
        notice
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
                Err(reason) => {
                    json!({"status":"rejected","error":format!("pty_{reason:?}"),"retry":false})
                }
            };
            let receipt = self
                .history
                .as_ref()
                .ok_or(())
                .and_then(|db| db.finish(operation_id, &outcome, "").map_err(|_| ()))
                .map(|()| outcome)
                .map_err(|()| "outcome_unknown_do_not_retry_input".to_string());
            if let Some(reply) = p.reply {
                let _ = reply.try_send(receipt);
            }
            self.reload_history();
        }
    }
    pub fn expire_pending(&mut self) {
        // Allow late results to update receipts for a bounded grace period.
        // Unknown operations retain their tombstone and are never retyped.
        self.pending.retain(|_, p| {
            let expired = std::time::Instant::now() >= p.deadline;
            if expired && let Some(reply) = p.reply.take() {
                let _ = reply.try_send(Err("outcome_unknown_do_not_retry_input".into()));
            }
            std::time::Instant::now() < p.deadline + std::time::Duration::from_secs(30)
        });
    }
    fn execute(
        &mut self,
        req: &Request,
        send: &mut impl FnMut(&Target, Effect) -> Result<(), String>,
        notice: &mut Option<AnswerNotice>,
    ) -> Result<Value, String> {
        let a = &req.args;
        let allowed: &[&str] = match req.tool.as_str() {
            "list_sessions" => &[],
            "read_output" => &["session_id", "generation", "cursor"],
            "send_text" => &["session_id", "generation", "operation_id", "text", "submit"],
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
            let sessions:Vec<_>=self.targets.iter().filter_map(|t|self.grants.get(&t.id).map(|g| json!({"session_id":t.id,"generation":t.generation,"workspace":t.workspace,"workspace_name":t.workspace_name,"title":t.title,"input_allowed":g.input&&t.live&&req.input_scope,"live":t.live}))).collect();
            return Ok(json!({"sessions":sessions}));
        }
        let id = field(a, "session_id", 128)?;
        let generation = field(a, "generation", 192)?;
        let is_input = matches!(req.tool.as_str(), "send_text" | "send_ctrl_c");
        if is_input && !req.input_scope {
            return Err("oauth_input_scope_required".into());
        }
        let t = self.authorize(id, generation, is_input)?;
        if req.tool == "read_output" {
            // Existing bounded visibility lease keeps hidden/warm terminal screens fresh.
            send(&t, Effect::Watch)?;
            let cursor = match a.get("cursor") {
                None => 0,
                Some(v) => v.as_u64().ok_or("invalid_cursor")?,
            };
            let Some(screen) = self.screens.get(id) else {
                return Ok(
                    json!({"cursor":0,"screen":null,"reset":true,"source":"visible_screen","lossless":false,"refresh_requested":true,"may_be_stale":true,"retry_after_ms":250}),
                );
            };
            let changed = cursor != screen.cursor;
            return Ok(
                json!({"cursor":screen.cursor,"screen":if changed {Some(&screen.text)} else {None},"reset":cursor==0 || cursor>screen.cursor,"source":"visible_screen","lossless":false,"refresh_requested":true,"may_be_stale":true,"retry_after_ms":250}),
            );
        }
        let op = field(a, "operation_id", 128)?;
        let submit = match a.get("submit") {
            None => false,
            Some(v) => v.as_bool().ok_or("invalid_submit")?,
        };
        let bytes = if req.tool == "send_text" {
            Some(
                encode_input(a["text"].as_str().ok_or("invalid_text")?, submit)
                    .map_err(|_| "text_contains_controls_or_invalid_size")?,
            )
        } else if req.tool == "send_ctrl_c" {
            Some(vec![3])
        } else {
            None
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
        if bytes.is_some() && self.pending.len() >= 16 {
            return Err("busy_no_effect".into());
        }
        let db = self
            .history
            .as_ref()
            .ok_or("history_unavailable_no_effect")?;
        match db
            .claim(op, &req.tool, a, &t.workspace, &t.id)
            .map_err(|_| "operation_conflict_or_history_unavailable_no_effect")?
        {
            Claim::Existing(result) => return Ok(result),
            Claim::New => {}
        }
        // Recheck token/deadline after SQLite's bounded wait, immediately before effects.
        if !self.server.as_ref().is_some_and(|s| req.live(&s.auth)) {
            let outcome = json!({"status":"rejected","error":"expired_or_revoked_request_no_effect","retry":false});
            db.finish(op, &outcome, "")
                .map_err(|_| "outcome_unknown_do_not_retry_input")?;
            self.reload_history();
            return Ok(outcome);
        }
        let outcome = if let Some(bytes) = bytes {
            let size = bytes.len();
            let auth = self.server.as_ref().unwrap().auth.clone();
            let epoch = req.epoch;
            let access_key = req.access_key.clone();
            let admission = runtime::InputAdmission::new(
                self.grants[&t.id].permit.clone(),
                req.deadline,
                move |write| auth.admit_current_access(epoch, access_key.as_deref(), write),
            );
            match send(
                &t,
                Effect::Input {
                    operation_id: op.to_owned(),
                    bytes,
                    admission,
                },
            ) {
                Ok(()) => {
                    self.pending.insert(
                        op.to_owned(),
                        PendingInput {
                            runtime: t.runtime,
                            session: t.session,
                            bytes: size,
                            submit,
                            deadline: req.deadline,
                            reply: None,
                        },
                    );
                    // Claim remains durable unknown until the worker responds.
                    return Ok(json!({"status":"awaiting_pty"}));
                }
                Err(code) => json!({"status":"rejected","error":code,"retry":false}),
            }
        } else {
            json!({"status":"stored","operation_id":op})
        };
        // Failed receipt writes leave a durable unknown claim. Never retry the side effect.
        db.finish(op, &outcome, answer.as_deref().unwrap_or(""))
            .map_err(|_| "outcome_unknown_do_not_retry_input")?;
        if let Some(message) = answer {
            *notice = Some(AnswerNotice {
                target: t,
                message,
                operation_id: op.to_owned(),
            });
        }
        self.reload_history();
        Ok(outcome)
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn input_and_groks_own_answer_are_independent_deduplicated_effects() {
        let (mut b, t) = setup();
        let mut effects = 0;
        let args = json!({"session_id":t.id,"generation":t.generation,"operation_id":"input-1","text":"pwd","submit":true});
        let (req, rx) = request(&b, "send_text", args.clone());
        assert!(
            b.handle(req, |_, _| {
                effects += 1;
                Ok(())
            })
            .is_none()
        );
        assert!(rx.recv().unwrap().is_err());
        assert_eq!(effects, 0);
        b.allow_input(&t.id, true);
        let (req, rx) = request(&b, "send_text", args.clone());
        b.handle(req, |target, effect| {
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
        b.observe_input(
            t.runtime,
            &[runtime::RuntimeEvent::InputAdmitted {
                session: t.session,
                operation_id: "input-1".into(),
                result: Ok(()),
            }],
        );
        assert_eq!(rx.recv().unwrap().unwrap()["status"], "queued");
        let (req, rx) = request(&b, "send_text", args);
        b.handle(req, |_, _| {
            effects += 1;
            Ok(())
        });
        assert_eq!(rx.recv().unwrap().unwrap()["status"], "queued");
        assert_eq!(effects, 1);
        b.take_control();
        let answer = json!({"session_id":t.id,"generation":t.generation,"operation_id":"answer-1","message":"그록봇 자체 답변\n분석 완료"});
        let (req, rx) = request(&b, "notify", answer.clone());
        let notice = b
            .handle(req, |_, _| panic!("answer must not enter PTY"))
            .unwrap();
        assert_eq!(notice.message, "그록봇 자체 답변\n분석 완료");
        assert_eq!(rx.recv().unwrap().unwrap()["status"], "stored");
        let (req, rx) = request(&b, "notify", answer);
        assert!(b.handle(req, |_, _| panic!()).is_none());
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
        b.handle(req, |_, _| panic!("read-only token typed"));
        assert!(rx.recv().unwrap().is_err());
        let (req, rx) = request(&b, "send_text", args.clone());
        let mut writes = 0;
        b.handle(req, |_, _| {
            writes += 1;
            Ok(())
        });
        assert!(rx.try_recv().is_err());
        let ack = runtime::RuntimeEvent::InputAdmitted {
            session: t.session,
            operation_id: "rejected-input".into(),
            result: Err(runtime::PtyInputRejectReason::QueueFull),
        };
        b.observe_input(t.runtime + 1, std::slice::from_ref(&ack));
        assert!(rx.try_recv().is_err());
        b.observe_input(t.runtime, &[ack]);
        assert_eq!(rx.recv().unwrap().unwrap()["status"], "rejected");
        let (req, rx) = request(&b, "send_text", args);
        b.handle(req, |_, _| {
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
        b.handle(req, |_, _| Ok(()));
        b.pending.get_mut("unknown-input").unwrap().deadline =
            Instant::now() - Duration::from_secs(1);
        b.expire_pending();
        assert!(rx.recv().unwrap().is_err());
        b.observe_input(
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
        b.handle(req, |_, _| panic!("revoked request dispatched"));
        assert!(rx.recv().unwrap().is_err());
        let (mut req, rx) = request(&b, "send_text", args);
        req.deadline = Instant::now() - Duration::from_secs(1);
        b.handle(req, |_, _| panic!("expired request dispatched"));
        assert!(rx.recv().unwrap().is_err());
        assert!(b.records.is_empty());
    }
    #[test]
    fn claim_expiry_records_no_effect_for_exact_retries() {
        let dir = std::env::temp_dir().join(format!("deppy-claim-expiry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.db");
        let mut b = CloudAgent::new(&path, secret::RedactionService::new());
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
        b.handle(req, |_, _| panic!("expired claim dispatched"));
        let first = rx.recv().unwrap();
        lock.join().unwrap();
        let (req, rx) = request(&b, "send_text", args);
        b.handle(req, |_, _| panic!("no-effect retry dispatched"));
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
        b.handle(req, |_, _| panic!());
        let result = rx.recv().unwrap().unwrap();
        assert_eq!(result["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(result["sessions"][0]["session_id"], t.id);
        let (req, rx) = request(
            &b,
            "read_output",
            json!({"session_id":t.id,"generation":t.generation}),
        );
        b.handle(req, |_, e| {
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
        b.handle(req, |_, _| Ok(()));
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
            b.handle(req, |t, e| match e {
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
                b.observe_input(t.runtime, &events);
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
            bridge.observe_input(target.runtime, &batch);
            for e in &batch {
                if let Some((_, snapshot, _, _)) = e.viewport() {
                    target.screen = Some(screen_text(snapshot));
                }
            }
            bridge.set_targets(vec![target.clone()]);
            while let Some(req) = bridge.next_request() {
                notice |= bridge
                    .handle(req, |t, e| {
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
            if let Some(notice) = bridge.handle(req, |t, e| {
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
            bridge.observe_input(target.runtime, &acknowledgements);
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
