//! App-owned authorization and effects for the outward MCP bridge.
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
    pub runtime: u64,
    pub session: runtime::SessionId,
    pub pane: runtime::MuxPaneId,
    pub live: bool,
    pub screen: Option<String>,
}
#[derive(Clone)]
struct Grant {
    generation: String,
    input: bool,
}
struct Screen {
    generation: String,
    cursor: u64,
    text: String,
}
pub enum Effect {
    Input(Vec<u8>),
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

pub struct CloudAgent {
    pub boot: String,
    pub port: u16,
    pub hostname: String,
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
    redaction: secret::RedactionService,
    token_lease: Option<secret::RedactionLease>,
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
                Ok(rows) => self.records = rows,
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
                },
            );
        } else {
            self.grants.remove(&t.id);
            self.screens.remove(&t.id);
        }
    }
    pub fn allow_input(&mut self, id: &str, allow: bool) {
        if let Some(g) = self.grants.get_mut(id) {
            g.input = allow;
        }
    }
    pub fn take_control(&mut self) {
        for g in self.grants.values_mut() {
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
    pub fn apply_action(&mut self, ctx: &egui::Context) {
        match self.action.take() {
            Some(Action::Start) => {
                if self.history.is_none() {
                    self.error = Some("history_unavailable".into());
                    return;
                }
                let wake = ctx.clone();
                match Server::start(self.port, &self.hostname, move || wake.request_repaint()) {
                    Ok(server) => {
                        let token =
                            secret::SecretString::new(server.auth.token_for_user().to_string());
                        let Ok(lease) = self.redaction.acquire_rotating(&token) else {
                            self.error = Some("token_redaction_capacity_no_connection".into());
                            return;
                        };
                        self.token_lease = Some(lease);
                        ctx.request_repaint_after(std::time::Duration::from_secs(
                            agent_mcp::TOKEN_TTL,
                        ));
                        self.server = Some(server);
                        self.error = None;
                    }
                    Err(_) => self.error = Some("server_start_failed_check_port_hostname".into()),
                }
            }
            Some(Action::Stop) => {
                self.server = None;
                self.token_lease = None;
                self.take_control();
                self.reveal = false;
            }
            Some(Action::Rotate) => {
                if let Some(s) = &self.server {
                    s.auth.rotate_at(now());
                    let token = secret::SecretString::new(s.auth.token_for_user().to_string());
                    match self.redaction.acquire_rotating(&token) {
                        Ok(lease) => self.token_lease = Some(lease),
                        Err(_) => {
                            self.server = None;
                            self.token_lease = None;
                            self.error = Some("token_redaction_capacity_connection_revoked".into());
                        }
                    }
                    ctx.request_repaint_after(std::time::Duration::from_secs(agent_mcp::TOKEN_TTL));
                }
                self.take_control();
                self.reveal = false;
            }
            None => {}
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
        let _ = req.reply.try_send(result);
        notice
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
            let sessions:Vec<_>=self.targets.iter().filter_map(|t|self.grants.get(&t.id).map(|g| json!({"session_id":t.id,"generation":t.generation,"workspace":t.workspace,"workspace_name":t.workspace_name,"title":t.title,"input_allowed":g.input&&t.live,"live":t.live}))).collect();
            return Ok(json!({"sessions":sessions}));
        }
        let id = field(a, "session_id", 128)?;
        let generation = field(a, "generation", 192)?;
        let is_input = matches!(req.tool.as_str(), "send_text" | "send_ctrl_c");
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
            return Err("expired_or_revoked_request_no_effect".into());
        }
        let outcome = if let Some(bytes) = bytes {
            let size = bytes.len();
            match send(&t, Effect::Input(bytes)) {
                Ok(()) => {
                    json!({"status":"queued","bytes":size,"submit":submit,"retry":false,"completion":"not_confirmed"})
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
    if s.cols == 0 {
        return out;
    }
    for (row_index, row) in s
        .visible_cells
        .chunks(s.cols as usize)
        .take(s.rows as usize)
        .enumerate()
    {
        let mut line = String::new();
        for (col, c) in row.iter().enumerate() {
            if s.is_trailing_wide_spacer(row_index * s.cols as usize + col) || c.wide_spacer {
                continue;
            }
            if !c.c.is_control() {
                line.push(c.c);
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
impl Target {
    fn fixture(id: &str, generation: &str) -> Self {
        Self {
            id: id.into(),
            generation: generation.into(),
            workspace: "w".into(),
            workspace_name: "workspace".into(),
            title: "session".into(),
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
            let Effect::Input(bytes) = effect else {
                panic!()
            };
            assert_eq!(bytes, b"pwd\r");
            effects += 1;
            Ok(())
        });
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
                if let Effect::Input(bytes) = e {
                    input_bytes.push(bytes);
                }
                Ok(())
            }) {
                assert_eq!(notice.message, "Grok 자체 답변입니다");
                notices += 1;
            }
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
