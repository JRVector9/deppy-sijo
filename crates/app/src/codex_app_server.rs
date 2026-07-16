//! Stdio JSON-RPC client for `codex app-server`.
//!
//! The App Server is intentionally separate from the PTY terminal runtime. It
//! speaks newline-delimited JSON-RPC and streams typed thread/turn/item events,
//! which lets the UI render a real result table without altering terminal bytes.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::Context;
use serde_json::{Value, json};

use crate::agent_session::{
    AgentApproval, AgentApprovalDecision, AgentApprovalKind, AgentItem, AgentSessionEvent,
    AgentSessionId, AgentThreadStatus,
};

const MAX_BUFFERED_THREAD_STATUSES: usize = 64;

/// Configuration for one local App Server connection.
#[derive(Debug, Clone)]
pub struct CodexAppServerOptions {
    pub executable: OsString,
    pub client_name: String,
    pub client_title: String,
    pub client_version: String,
}

impl Default for CodexAppServerOptions {
    fn default() -> Self {
        Self {
            executable: OsString::from("codex"),
            client_name: "deppy_sijo".to_owned(),
            client_title: "Deppy Sijo".to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// Events are either tied to one local agent session or describe the transport
/// itself. The UI can preserve a finished session when the shared connection
/// later fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexAppServerEvent {
    Session {
        session_id: AgentSessionId,
        event: AgentSessionEvent,
    },
    TransportError {
        message: String,
    },
    ConnectionStopped,
}

/// A running local App Server process. It owns one writer worker and two reader
/// threads; all JSON-RPC writes occur on the worker to preserve message order.
pub struct CodexAppServerClient {
    commands: Sender<ClientCommand>,
    events: Receiver<CodexAppServerEvent>,
    worker: Option<thread::JoinHandle<()>>,
}

impl CodexAppServerClient {
    pub fn spawn(options: CodexAppServerOptions, repaint: egui::Context) -> anyhow::Result<Self> {
        let mut child = Command::new(&options.executable)
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "Codex App Server 실행 실패: {} app-server --listen stdio://",
                    options.executable.to_string_lossy()
                )
            })?;
        let stdin = child
            .stdin
            .take()
            .context("Codex App Server stdin을 열 수 없음")?;
        let stdout = child
            .stdout
            .take()
            .context("Codex App Server stdout을 열 수 없음")?;
        let stderr = child
            .stderr
            .take()
            .context("Codex App Server stderr을 열 수 없음")?;

        let (commands_tx, commands_rx) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("codex-app-server".to_owned())
            .spawn(move || {
                worker_loop(
                    child,
                    stdin,
                    stdout,
                    stderr,
                    options,
                    commands_rx,
                    events_tx,
                    repaint,
                )
            })
            .context("Codex App Server 워커 생성 실패")?;

        Ok(Self {
            commands: commands_tx,
            events: events_rx,
            worker: Some(worker),
        })
    }

    /// Create a new Codex thread and immediately submit its first turn.
    pub fn start_session(
        &self,
        session_id: AgentSessionId,
        prompt: String,
        cwd: Option<String>,
        model: Option<String>,
    ) -> anyhow::Result<()> {
        self.send(ClientCommand::StartSession {
            session_id,
            prompt,
            cwd,
            model,
        })
    }

    /// Submit a later turn to an existing application session.
    pub fn submit_turn(
        &self,
        session_id: AgentSessionId,
        prompt: String,
        cwd: Option<String>,
        model: Option<String>,
    ) -> anyhow::Result<()> {
        self.send(ClientCommand::SubmitTurn {
            session_id,
            prompt,
            cwd,
            model,
        })
    }

    /// Interrupt only the selected thread/turn. The shared App Server process
    /// remains available for other sessions.
    pub fn interrupt(&self, session_id: AgentSessionId) -> anyhow::Result<()> {
        self.send(ClientCommand::Interrupt { session_id })
    }

    pub fn respond_approval(
        &self,
        session_id: AgentSessionId,
        request_key: String,
        decision: AgentApprovalDecision,
    ) -> anyhow::Result<()> {
        self.send(ClientCommand::RespondApproval {
            session_id,
            request_key,
            decision,
        })
    }

    /// Drain without blocking; worker-side repaint requests make the next frame
    /// arrive when App Server output lands.
    pub fn drain_events(&self) -> Vec<CodexAppServerEvent> {
        self.events.try_iter().collect()
    }

    pub fn shutdown(&mut self) {
        let _ = self.commands.send(ClientCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    fn send(&self, command: ClientCommand) -> anyhow::Result<()> {
        self.commands
            .send(command)
            .map_err(|_| anyhow::anyhow!("Codex App Server 연결이 이미 종료되었습니다"))
    }
}

impl Drop for CodexAppServerClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug)]
enum ClientCommand {
    StartSession {
        session_id: AgentSessionId,
        prompt: String,
        cwd: Option<String>,
        model: Option<String>,
    },
    SubmitTurn {
        session_id: AgentSessionId,
        prompt: String,
        cwd: Option<String>,
        model: Option<String>,
    },
    Interrupt {
        session_id: AgentSessionId,
    },
    RespondApproval {
        session_id: AgentSessionId,
        request_key: String,
        decision: AgentApprovalDecision,
    },
    Shutdown,
}

#[derive(Debug)]
enum Incoming {
    Json(Value),
    StdoutError(String),
    Stderr(String),
}

#[derive(Debug)]
enum PendingRequest {
    Initialize,
    StartThread {
        session_id: AgentSessionId,
        prompt: String,
        cwd: Option<String>,
        model: Option<String>,
    },
    StartTurn {
        session_id: AgentSessionId,
    },
    Interrupt {
        session_id: AgentSessionId,
    },
}

#[derive(Debug)]
struct ServerRequest {
    session_id: AgentSessionId,
    id: Value,
}

#[derive(Debug, Default)]
struct WorkerState {
    next_request_id: u64,
    initialized: bool,
    pending: HashMap<String, PendingRequest>,
    queued_starts: Vec<QueuedStart>,
    thread_to_session: HashMap<String, AgentSessionId>,
    session_to_thread: HashMap<AgentSessionId, String>,
    /// A status notification can race ahead of `thread/start`'s response, which
    /// is where the local session mapping becomes known. Keep only the latest
    /// status for a small bounded number of such threads and replay it once the
    /// mapping is installed.
    buffered_thread_statuses: HashMap<String, AgentThreadStatus>,
    session_to_turn: HashMap<AgentSessionId, String>,
    server_requests: HashMap<String, ServerRequest>,
    known_sessions: HashSet<AgentSessionId>,
    stop_requested: bool,
}

#[derive(Debug)]
struct QueuedStart {
    session_id: AgentSessionId,
    prompt: String,
    cwd: Option<String>,
    model: Option<String>,
}

impl WorkerState {
    fn request_id(&mut self) -> Value {
        let id = self.next_request_id;
        self.next_request_id += 1;
        Value::from(id)
    }
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    mut child: Child,
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
    options: CodexAppServerOptions,
    commands: Receiver<ClientCommand>,
    events: Sender<CodexAppServerEvent>,
    repaint: egui::Context,
) {
    let (incoming_tx, incoming_rx) = mpsc::channel();
    let stdout_reader = spawn_stdout_reader(stdout, incoming_tx.clone());
    let stderr_reader = spawn_stderr_reader(stderr, incoming_tx);
    let mut state = WorkerState {
        next_request_id: 1,
        ..Default::default()
    };
    let mut last_stderr = String::new();
    let mut running = true;

    if let Err(error) = send_initialize(&mut stdin, &mut state, &options) {
        emit_transport_error(
            &events,
            &repaint,
            format!("Codex 초기화 전송 실패: {error:#}"),
        );
        running = false;
    }

    while running {
        drain_incoming(
            &incoming_rx,
            &mut stdin,
            &mut state,
            &events,
            &repaint,
            &mut last_stderr,
        );
        if state.stop_requested {
            break;
        }

        match commands.recv_timeout(Duration::from_millis(20)) {
            Ok(ClientCommand::Shutdown) => break,
            Ok(command) => {
                handle_client_command(command, &mut stdin, &mut state, &events, &repaint)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                let suffix = if last_stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", one_line(&last_stderr, 400))
                };
                emit_transport_error(
                    &events,
                    &repaint,
                    format!("Codex App Server가 종료되었습니다 ({status}){suffix}"),
                );
                break;
            }
            Ok(None) => {}
            Err(error) => {
                emit_transport_error(
                    &events,
                    &repaint,
                    format!("Codex App Server 상태 확인 실패: {error}"),
                );
                break;
            }
        }
    }

    // Closing stdin requests a graceful shutdown first. If it does not exit
    // quickly, kill/reap prevents a detached helper from surviving app exit.
    drop(stdin);
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();

    for session_id in &state.known_sessions {
        emit_session(
            &events,
            &repaint,
            session_id.clone(),
            AgentSessionEvent::Stopped,
        );
    }
    emit(&events, &repaint, CodexAppServerEvent::ConnectionStopped);
}

fn spawn_stdout_reader(stdout: ChildStdout, tx: Sender<Incoming>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("codex-app-server-stdout".to_owned())
        .spawn(move || read_json_lines(stdout, tx))
        .expect("Codex App Server stdout reader 생성 실패")
}

fn spawn_stderr_reader(stderr: ChildStderr, tx: Sender<Incoming>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("codex-app-server-stderr".to_owned())
        .spawn(move || read_stderr_lines(stderr, tx))
        .expect("Codex App Server stderr reader 생성 실패")
}

fn read_json_lines(reader: impl Read, tx: Sender<Incoming>) {
    for line in BufReader::new(reader).lines() {
        match line {
            Ok(line) if line.trim().is_empty() => {}
            Ok(line) => match serde_json::from_str::<Value>(&line) {
                Ok(value) => {
                    if tx.send(Incoming::Json(value)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = tx.send(Incoming::StdoutError(format!(
                        "Codex App Server JSONL 파싱 실패: {error}: {}",
                        one_line(&line, 240)
                    )));
                }
            },
            Err(error) => {
                let _ = tx.send(Incoming::StdoutError(format!(
                    "Codex App Server stdout 읽기 실패: {error}"
                )));
                return;
            }
        }
    }
}

fn read_stderr_lines(reader: impl Read, tx: Sender<Incoming>) {
    for line in BufReader::new(reader).lines().map_while(Result::ok) {
        if tx.send(Incoming::Stderr(line)).is_err() {
            return;
        }
    }
}

fn drain_incoming(
    incoming: &Receiver<Incoming>,
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    events: &Sender<CodexAppServerEvent>,
    repaint: &egui::Context,
    last_stderr: &mut String,
) {
    loop {
        match incoming.try_recv() {
            Ok(Incoming::Json(message)) => {
                handle_server_message(message, stdin, state, events, repaint)
            }
            Ok(Incoming::StdoutError(message)) => emit_transport_error(events, repaint, message),
            Ok(Incoming::Stderr(line)) => append_limited(last_stderr, &line),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
        }
    }
}

fn handle_client_command(
    command: ClientCommand,
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    events: &Sender<CodexAppServerEvent>,
    repaint: &egui::Context,
) {
    let target = match &command {
        ClientCommand::StartSession { session_id, .. }
        | ClientCommand::SubmitTurn { session_id, .. }
        | ClientCommand::Interrupt { session_id }
        | ClientCommand::RespondApproval { session_id, .. } => Some(session_id.clone()),
        ClientCommand::Shutdown => None,
    };
    let result = match command {
        ClientCommand::StartSession {
            session_id,
            prompt,
            cwd,
            model,
        } => {
            state.known_sessions.insert(session_id.clone());
            if state.initialized {
                start_thread(stdin, state, session_id, prompt, cwd, model)
            } else {
                state.queued_starts.push(QueuedStart {
                    session_id,
                    prompt,
                    cwd,
                    model,
                });
                Ok(())
            }
        }
        ClientCommand::SubmitTurn {
            session_id,
            prompt,
            cwd,
            model,
        } => start_turn_for_session(stdin, state, session_id, prompt, cwd, model),
        ClientCommand::Interrupt { session_id } => interrupt_turn(stdin, state, session_id),
        ClientCommand::RespondApproval {
            session_id,
            request_key,
            decision,
        } => respond_approval(stdin, state, &session_id, &request_key, decision),
        ClientCommand::Shutdown => Ok(()),
    };

    if let Err(error) = result {
        if let Some(session_id) = target {
            emit_session(
                events,
                repaint,
                session_id,
                AgentSessionEvent::Failed {
                    message: format!("Codex App Server 요청 실패: {error:#}"),
                },
            );
        } else {
            emit_transport_error(events, repaint, format!("Codex 요청 실패: {error:#}"));
        }
    }
}

fn send_initialize(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    options: &CodexAppServerOptions,
) -> anyhow::Result<()> {
    let id = state.request_id();
    state
        .pending
        .insert(rpc_key(&id), PendingRequest::Initialize);
    write_message(
        stdin,
        &json!({
            "method": "initialize",
            "id": id,
            "params": {
                "clientInfo": {
                    "name": options.client_name,
                    "title": options.client_title,
                    "version": options.client_version,
                }
            }
        }),
    )
}

fn start_thread(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    session_id: AgentSessionId,
    prompt: String,
    cwd: Option<String>,
    model: Option<String>,
) -> anyhow::Result<()> {
    let id = state.request_id();
    state.pending.insert(
        rpc_key(&id),
        PendingRequest::StartThread {
            session_id,
            prompt,
            cwd: cwd.clone(),
            model: model.clone(),
        },
    );
    let mut params = serde_json::Map::new();
    if let Some(cwd) = cwd {
        params.insert("cwd".to_owned(), Value::String(cwd));
    }
    if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
        params.insert("model".to_owned(), Value::String(model));
    }
    write_message(
        stdin,
        &json!({"method": "thread/start", "id": id, "params": params}),
    )
}

fn start_turn_for_session(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    session_id: AgentSessionId,
    prompt: String,
    cwd: Option<String>,
    model: Option<String>,
) -> anyhow::Result<()> {
    let Some(thread_id) = state.session_to_thread.get(&session_id).cloned() else {
        anyhow::bail!("아직 Codex thread가 준비되지 않았습니다");
    };
    let id = state.request_id();
    state
        .pending
        .insert(rpc_key(&id), PendingRequest::StartTurn { session_id });
    let mut params = serde_json::Map::new();
    params.insert("threadId".to_owned(), Value::String(thread_id));
    params.insert(
        "input".to_owned(),
        json!([{ "type": "text", "text": prompt }]),
    );
    if let Some(cwd) = cwd.filter(|cwd| !cwd.trim().is_empty()) {
        params.insert("cwd".to_owned(), Value::String(cwd));
    }
    if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
        params.insert("model".to_owned(), Value::String(model));
    }
    write_message(
        stdin,
        &json!({"method": "turn/start", "id": id, "params": params}),
    )
}

fn interrupt_turn(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    session_id: AgentSessionId,
) -> anyhow::Result<()> {
    let Some(thread_id) = state.session_to_thread.get(&session_id).cloned() else {
        anyhow::bail!("중단할 Codex thread가 없습니다");
    };
    let Some(turn_id) = state.session_to_turn.get(&session_id).cloned() else {
        anyhow::bail!("중단할 실행 중 turn이 없습니다");
    };
    let id = state.request_id();
    state
        .pending
        .insert(rpc_key(&id), PendingRequest::Interrupt { session_id });
    write_message(
        stdin,
        &json!({
            "method": "turn/interrupt",
            "id": id,
            "params": { "threadId": thread_id, "turnId": turn_id }
        }),
    )
}

fn respond_approval(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    session_id: &AgentSessionId,
    request_key: &str,
    decision: AgentApprovalDecision,
) -> anyhow::Result<()> {
    let Some(request) = state.server_requests.get(request_key) else {
        anyhow::bail!("해당 승인 요청이 이미 해소되었습니다");
    };
    anyhow::ensure!(
        &request.session_id == session_id,
        "다른 세션의 승인 요청에는 응답할 수 없습니다"
    );
    write_message(
        stdin,
        &json!({"id": request.id, "result": {"decision": decision.wire_value()}}),
    )
}

fn handle_server_message(
    message: Value,
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    events: &Sender<CodexAppServerEvent>,
    repaint: &egui::Context,
) {
    if message.get("method").is_some() && message.get("id").is_some() {
        handle_server_request(message, stdin, state, events, repaint);
    } else if message.get("method").is_some() {
        handle_notification(message, state, events, repaint);
    } else if message.get("id").is_some() {
        handle_response(message, stdin, state, events, repaint);
    } else {
        emit_transport_error(
            events,
            repaint,
            format!(
                "알 수 없는 Codex App Server 메시지: {}",
                one_line(&message.to_string(), 240)
            ),
        );
    }
}

fn handle_response(
    message: Value,
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    events: &Sender<CodexAppServerEvent>,
    repaint: &egui::Context,
) {
    let Some(id) = message.get("id") else { return };
    let Some(pending) = state.pending.remove(&rpc_key(id)) else {
        return;
    };
    if let Some(error) = message.get("error") {
        let text = rpc_error_text(error);
        match pending {
            PendingRequest::Initialize => {
                emit_transport_error(events, repaint, text.clone());
                for session_id in &state.known_sessions {
                    emit_session(
                        events,
                        repaint,
                        session_id.clone(),
                        AgentSessionEvent::Failed {
                            message: format!("Codex App Server 초기화 실패: {text}"),
                        },
                    );
                }
                state.stop_requested = true;
            }
            PendingRequest::StartThread { session_id, .. }
            | PendingRequest::StartTurn { session_id }
            | PendingRequest::Interrupt { session_id } => emit_session(
                events,
                repaint,
                session_id,
                AgentSessionEvent::Failed { message: text },
            ),
        }
        return;
    }
    let result = message.get("result").cloned().unwrap_or(Value::Null);
    match pending {
        PendingRequest::Initialize => {
            state.initialized = true;
            if let Err(error) =
                write_message(stdin, &json!({"method": "initialized", "params": {}}))
            {
                emit_transport_error(events, repaint, format!("initialized 전송 실패: {error:#}"));
                return;
            }
            let starts = std::mem::take(&mut state.queued_starts);
            for start in starts {
                if let Err(error) = start_thread(
                    stdin,
                    state,
                    start.session_id.clone(),
                    start.prompt,
                    start.cwd,
                    start.model,
                ) {
                    emit_session(
                        events,
                        repaint,
                        start.session_id,
                        AgentSessionEvent::Failed {
                            message: format!("thread 시작 실패: {error:#}"),
                        },
                    );
                }
            }
            for session_id in &state.known_sessions {
                emit_session(
                    events,
                    repaint,
                    session_id.clone(),
                    AgentSessionEvent::ConnectionReady,
                );
            }
        }
        PendingRequest::StartThread {
            session_id,
            prompt,
            cwd,
            model,
        } => {
            let Some(thread_id) = result.pointer("/thread/id").and_then(Value::as_str) else {
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::Failed {
                        message: "thread/start 응답에 thread.id가 없습니다".to_owned(),
                    },
                );
                return;
            };
            let thread_id = thread_id.to_owned();
            state
                .thread_to_session
                .insert(thread_id.clone(), session_id.clone());
            state
                .session_to_thread
                .insert(session_id.clone(), thread_id.clone());
            emit_session(
                events,
                repaint,
                session_id.clone(),
                AgentSessionEvent::ThreadStarted {
                    thread_id: thread_id.clone(),
                },
            );
            if let Some(status) = state.buffered_thread_statuses.remove(&thread_id) {
                emit_session(
                    events,
                    repaint,
                    session_id.clone(),
                    AgentSessionEvent::ThreadStatusChanged { status },
                );
            }
            if let Err(error) =
                start_turn_for_session(stdin, state, session_id.clone(), prompt, cwd, model)
            {
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::Failed {
                        message: format!("turn 시작 실패: {error:#}"),
                    },
                );
            }
        }
        PendingRequest::StartTurn { session_id } => {
            if let Some(turn_id) = result.pointer("/turn/id").and_then(Value::as_str) {
                let turn_id = turn_id.to_owned();
                state
                    .session_to_turn
                    .insert(session_id.clone(), turn_id.clone());
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::TurnStarted { turn_id },
                );
            } else {
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::Failed {
                        message: "turn/start 응답에 turn.id가 없습니다".to_owned(),
                    },
                );
            }
        }
        // Completion is confirmed by turn/completed, not this acknowledgement.
        PendingRequest::Interrupt { .. } => {}
    }
}

fn handle_notification(
    message: Value,
    state: &mut WorkerState,
    events: &Sender<CodexAppServerEvent>,
    repaint: &egui::Context,
) {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return;
    };
    let params = message.get("params").unwrap_or(&Value::Null);
    match method {
        "thread/status/changed" => {
            let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
                return;
            };
            let Some(status) = params.get("status").and_then(AgentThreadStatus::from_codex) else {
                return;
            };
            if let Some(session_id) = state.thread_to_session.get(thread_id).cloned() {
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::ThreadStatusChanged { status },
                );
            } else if state.buffered_thread_statuses.contains_key(thread_id)
                || state.buffered_thread_statuses.len() < MAX_BUFFERED_THREAD_STATUSES
            {
                state
                    .buffered_thread_statuses
                    .insert(thread_id.to_owned(), status);
            }
        }
        "thread/started" => {
            let Some(thread_id) = params.pointer("/thread/id").and_then(Value::as_str) else {
                return;
            };
            if let Some(session_id) = state.thread_to_session.get(thread_id).cloned() {
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::ThreadStarted {
                        thread_id: thread_id.to_owned(),
                    },
                );
            }
        }
        "turn/started" => {
            let Some((session_id, turn_id)) = session_and_turn(params, state) else {
                return;
            };
            state
                .session_to_turn
                .insert(session_id.clone(), turn_id.clone());
            emit_session(
                events,
                repaint,
                session_id,
                AgentSessionEvent::TurnStarted { turn_id },
            );
        }
        "item/started" => {
            let Some(session_id) = session_for_params(params, state) else {
                return;
            };
            let Some(item) = params.get("item").and_then(AgentItem::from_codex) else {
                return;
            };
            emit_session(
                events,
                repaint,
                session_id,
                AgentSessionEvent::ItemStarted { item },
            );
        }
        "item/completed" => {
            let Some(session_id) = session_for_params(params, state) else {
                return;
            };
            let Some(item) = params.get("item").and_then(AgentItem::from_codex) else {
                return;
            };
            emit_session(
                events,
                repaint,
                session_id,
                AgentSessionEvent::ItemCompleted { item },
            );
        }
        "item/agentMessage/delta"
        | "item/plan/delta"
        | "item/reasoning/summaryTextDelta"
        | "item/reasoning/textDelta"
        | "item/commandExecution/outputDelta" => {
            let Some(session_id) = session_for_params(params, state) else {
                return;
            };
            let Some(item_id) = params.get("itemId").and_then(Value::as_str) else {
                return;
            };
            let Some(delta) = params.get("delta").and_then(Value::as_str) else {
                return;
            };
            emit_session(
                events,
                repaint,
                session_id,
                AgentSessionEvent::ItemDelta {
                    item_id: item_id.to_owned(),
                    delta: delta.to_owned(),
                },
            );
        }
        "turn/completed" => {
            let Some(session_id) = session_for_params(params, state) else {
                return;
            };
            let status = params
                .pointer("/turn/status")
                .or_else(|| params.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("completed")
                .to_owned();
            state.session_to_turn.remove(&session_id);
            emit_session(
                events,
                repaint,
                session_id,
                AgentSessionEvent::TurnCompleted { status },
            );
        }
        "serverRequest/resolved" => {
            let Some(session_id) = session_for_params(params, state) else {
                return;
            };
            let Some(request_id) = params.get("requestId") else {
                return;
            };
            let request_key = rpc_key(request_id);
            state.server_requests.remove(&request_key);
            emit_session(
                events,
                repaint,
                session_id,
                AgentSessionEvent::ApprovalResolved { request_key },
            );
        }
        _ => {}
    }
}

fn handle_server_request(
    message: Value,
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    events: &Sender<CodexAppServerEvent>,
    repaint: &egui::Context,
) {
    let Some(id) = message.get("id").cloned() else {
        return;
    };
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = message.get("params").unwrap_or(&Value::Null);
    let kind = match method {
        "item/commandExecution/requestApproval" => Some(AgentApprovalKind::CommandExecution),
        "item/fileChange/requestApproval" => Some(AgentApprovalKind::FileChange),
        _ => None,
    };
    let Some(kind) = kind else {
        let _ = write_message(
            stdin,
            &json!({
                "id": id,
                "error": {"code": -32601, "message": format!("Unsupported App Server request: {method}")}
            }),
        );
        emit_transport_error(
            events,
            repaint,
            format!("지원하지 않는 Codex App Server 요청: {method}"),
        );
        return;
    };
    let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
        return;
    };
    let Some(session_id) = state.thread_to_session.get(thread_id).cloned() else {
        return;
    };
    let Some(turn_id) = params.get("turnId").and_then(Value::as_str) else {
        return;
    };
    let Some(item_id) = params.get("itemId").and_then(Value::as_str) else {
        return;
    };
    let request_key = rpc_key(&id);
    state.server_requests.insert(
        request_key.clone(),
        ServerRequest {
            session_id: session_id.clone(),
            id,
        },
    );
    emit_session(
        events,
        repaint,
        session_id,
        AgentSessionEvent::ApprovalRequested {
            approval: AgentApproval {
                request_key,
                kind,
                thread_id: thread_id.to_owned(),
                turn_id: turn_id.to_owned(),
                item_id: item_id.to_owned(),
                reason: params
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                command: params
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                cwd: params.get("cwd").and_then(Value::as_str).map(str::to_owned),
            },
        },
    );
}

fn session_for_params(params: &Value, state: &WorkerState) -> Option<AgentSessionId> {
    let thread_id = params.get("threadId").and_then(Value::as_str)?;
    state.thread_to_session.get(thread_id).cloned()
}

fn session_and_turn(params: &Value, state: &WorkerState) -> Option<(AgentSessionId, String)> {
    let session_id = session_for_params(params, state)?;
    let turn_id = params
        .pointer("/turn/id")
        .and_then(Value::as_str)?
        .to_owned();
    Some((session_id, turn_id))
}

fn write_message(stdin: &mut ChildStdin, message: &Value) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *stdin, message)?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    Ok(())
}

fn rpc_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| "null".to_owned())
}

fn rpc_error_text(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Codex App Server 오류");
    match error.get("code").and_then(Value::as_i64) {
        Some(code) => format!("{message} (code {code})"),
        None => message.to_owned(),
    }
}

fn emit(events: &Sender<CodexAppServerEvent>, repaint: &egui::Context, event: CodexAppServerEvent) {
    let _ = events.send(event);
    repaint.request_repaint();
}

fn emit_session(
    events: &Sender<CodexAppServerEvent>,
    repaint: &egui::Context,
    session_id: AgentSessionId,
    event: AgentSessionEvent,
) {
    emit(
        events,
        repaint,
        CodexAppServerEvent::Session { session_id, event },
    );
}

fn emit_transport_error(
    events: &Sender<CodexAppServerEvent>,
    repaint: &egui::Context,
    message: String,
) {
    emit(
        events,
        repaint,
        CodexAppServerEvent::TransportError { message },
    );
}

fn append_limited(target: &mut String, line: &str) {
    if target.len() >= 4096 {
        return;
    }
    if !target.is_empty() {
        target.push('\n');
    }
    target.push_str(line);
    if target.len() > 4096 {
        target.truncate(4096);
    }
}

fn one_line(text: &str, max_chars: usize) -> String {
    let condensed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut indices = condensed.char_indices();
    let Some((cut, _)) = indices.nth(max_chars) else {
        return condensed;
    };
    format!("{}…", &condensed[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_frame_uses_json_rpc_without_the_jsonrpc_header() {
        let options = CodexAppServerOptions::default();
        let mut state = WorkerState {
            next_request_id: 1,
            ..Default::default()
        };
        let id = state.request_id();
        let frame = json!({
            "method": "initialize",
            "id": id,
            "params": {"clientInfo": {
                "name": options.client_name,
                "title": options.client_title,
                "version": options.client_version,
            }}
        });

        assert_eq!(frame.get("jsonrpc"), None);
        assert_eq!(frame["method"], "initialize");
        assert_eq!(frame["id"], 1);
    }

    #[test]
    fn structured_delta_notification_requires_thread_mapping() {
        let mut state = WorkerState::default();
        state
            .thread_to_session
            .insert("thread-1".to_owned(), "session-1".to_owned());
        let params = json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "item-1",
            "delta": "hello"
        });

        assert_eq!(
            session_for_params(&params, &state).as_deref(),
            Some("session-1")
        );
        assert_eq!(params["itemId"], "item-1");
    }

    #[test]
    fn approval_response_preserves_the_server_request_id_type() {
        let id = Value::String("request-42".to_owned());
        let frame = json!({"id": id, "result": {"decision": "accept"}});
        assert_eq!(frame["id"], "request-42");
        assert_eq!(frame["result"]["decision"], "accept");
    }

    #[test]
    fn thread_status_notification_emits_authoritative_session_event() {
        let mut state = WorkerState::default();
        state
            .thread_to_session
            .insert("thread-1".to_owned(), "session-1".to_owned());
        let (events, received) = mpsc::channel();

        handle_notification(
            json!({
                "method": "thread/status/changed",
                "params": {
                    "threadId": "thread-1",
                    "status": {
                        "type": "active",
                        "activeFlags": ["waitingOnUserInput"]
                    }
                }
            }),
            &mut state,
            &events,
            &egui::Context::default(),
        );

        assert_eq!(
            received.try_recv().unwrap(),
            CodexAppServerEvent::Session {
                session_id: "session-1".to_owned(),
                event: AgentSessionEvent::ThreadStatusChanged {
                    status: AgentThreadStatus::Active {
                        waiting_on_approval: false,
                        waiting_on_user_input: true,
                    },
                },
            }
        );
    }

    #[test]
    fn early_thread_status_is_buffered_until_session_mapping_exists() {
        let mut state = WorkerState::default();
        let (events, received) = mpsc::channel();
        handle_notification(
            json!({
                "method": "thread/status/changed",
                "params": {
                    "threadId": "thread-early",
                    "status": {"type": "idle"}
                }
            }),
            &mut state,
            &events,
            &egui::Context::default(),
        );

        assert!(matches!(received.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(
            state.buffered_thread_statuses.get("thread-early"),
            Some(&AgentThreadStatus::Idle)
        );
    }

    #[test]
    fn unknown_thread_status_variant_is_ignored_without_transport_failure() {
        let mut state = WorkerState::default();
        state
            .thread_to_session
            .insert("thread-1".to_owned(), "session-1".to_owned());
        let (events, received) = mpsc::channel();
        handle_notification(
            json!({
                "method": "thread/status/changed",
                "params": {
                    "threadId": "thread-1",
                    "status": {"type": "futureStatus", "futureField": true}
                }
            }),
            &mut state,
            &events,
            &egui::Context::default(),
        );

        assert!(matches!(received.try_recv(), Err(TryRecvError::Empty)));
    }
}
