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

/// One-shot asynchronous JSON result for history/catalog requests. Callers can
/// poll it without blocking the egui frame; the payload stays at the App Server
/// JSON boundary until a UI-specific view model consumes it.
#[allow(dead_code)] // PR-06 phase 2 wires the history UI consumer.
pub type CodexAppServerReply = Receiver<anyhow::Result<Value>>;

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

    /// List Deppy-created App Server threads, newest first. `cursor` is the
    /// opaque `nextCursor` from the preceding response.
    #[allow(dead_code)] // PR-06 phase 2 wires the history UI consumer.
    pub fn list_threads(
        &self,
        cursor: Option<String>,
        limit: Option<u32>,
        archived: bool,
    ) -> anyhow::Result<CodexAppServerReply> {
        let (reply, receiver) = mpsc::channel();
        self.send(ClientCommand::ListThreads {
            cursor,
            limit,
            archived,
            reply,
        })?;
        Ok(receiver)
    }

    #[allow(dead_code)] // PR-06 phase 2 wires the history UI consumer.
    pub fn read_thread(
        &self,
        thread_id: String,
        include_turns: bool,
    ) -> anyhow::Result<CodexAppServerReply> {
        let (reply, receiver) = mpsc::channel();
        self.send(ClientCommand::ReadThread {
            thread_id,
            include_turns,
            reply,
        })?;
        Ok(receiver)
    }

    /// Resume an existing Codex thread under an app-owned local session ID.
    #[allow(dead_code)] // PR-06 phase 2 wires the history UI consumer.
    pub fn resume_thread(
        &self,
        session_id: AgentSessionId,
        thread_id: String,
        cwd: Option<String>,
        model: Option<String>,
    ) -> anyhow::Result<CodexAppServerReply> {
        let (reply, receiver) = mpsc::channel();
        self.send(ClientCommand::ResumeThread {
            session_id,
            thread_id,
            cwd,
            model,
            reply,
        })?;
        Ok(receiver)
    }

    #[allow(dead_code)] // PR-06 phase 2 wires the history UI consumer.
    pub fn archive_thread(&self, thread_id: String) -> anyhow::Result<CodexAppServerReply> {
        let (reply, receiver) = mpsc::channel();
        self.send(ClientCommand::ArchiveThread { thread_id, reply })?;
        Ok(receiver)
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
    #[allow(dead_code)] // Constructed by the phase-2 public API consumer.
    ListThreads {
        cursor: Option<String>,
        limit: Option<u32>,
        archived: bool,
        reply: Sender<anyhow::Result<Value>>,
    },
    #[allow(dead_code)] // Constructed by the phase-2 public API consumer.
    ReadThread {
        thread_id: String,
        include_turns: bool,
        reply: Sender<anyhow::Result<Value>>,
    },
    #[allow(dead_code)] // Constructed by the phase-2 public API consumer.
    ResumeThread {
        session_id: AgentSessionId,
        thread_id: String,
        cwd: Option<String>,
        model: Option<String>,
        reply: Sender<anyhow::Result<Value>>,
    },
    #[allow(dead_code)] // Constructed by the phase-2 public API consumer.
    ArchiveThread {
        thread_id: String,
        reply: Sender<anyhow::Result<Value>>,
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
    ListThreads {
        reply: Sender<anyhow::Result<Value>>,
    },
    ReadThread {
        thread_id: String,
        reply: Sender<anyhow::Result<Value>>,
    },
    ResumeThread {
        session_id: AgentSessionId,
        thread_id: String,
        reply: Sender<anyhow::Result<Value>>,
    },
    ArchiveThread {
        thread_id: String,
        reply: Sender<anyhow::Result<Value>>,
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
    queued_rpc_commands: Vec<ClientCommand>,
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

    if let Err(error) = send_initialize(&mut stdin, &mut state, &options) {
        emit_transport_error(
            &events,
            &repaint,
            format!("Codex 초기화 전송 실패: {error:#}"),
        );
    } else {
        loop {
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
    if !state.initialized
        && matches!(
            &command,
            ClientCommand::ListThreads { .. }
                | ClientCommand::ReadThread { .. }
                | ClientCommand::ResumeThread { .. }
                | ClientCommand::ArchiveThread { .. }
        )
    {
        state.queued_rpc_commands.push(command);
        return;
    }
    let target = match &command {
        ClientCommand::StartSession { session_id, .. }
        | ClientCommand::SubmitTurn { session_id, .. }
        | ClientCommand::Interrupt { session_id }
        | ClientCommand::RespondApproval { session_id, .. }
        | ClientCommand::ResumeThread { session_id, .. } => Some(session_id.clone()),
        ClientCommand::ListThreads { .. }
        | ClientCommand::ReadThread { .. }
        | ClientCommand::ArchiveThread { .. }
        | ClientCommand::Shutdown => None,
    };
    let rpc_reply = match &command {
        ClientCommand::ListThreads { reply, .. }
        | ClientCommand::ReadThread { reply, .. }
        | ClientCommand::ResumeThread { reply, .. }
        | ClientCommand::ArchiveThread { reply, .. } => Some(reply.clone()),
        _ => None,
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
        ClientCommand::ListThreads {
            cursor,
            limit,
            archived,
            reply,
        } => request_thread_list(stdin, state, cursor, limit, archived, reply),
        ClientCommand::ReadThread {
            thread_id,
            include_turns,
            reply,
        } => request_thread_read(stdin, state, thread_id, include_turns, reply),
        ClientCommand::ResumeThread {
            session_id,
            thread_id,
            cwd,
            model,
            reply,
        } => request_thread_resume(stdin, state, session_id, thread_id, cwd, model, reply),
        ClientCommand::ArchiveThread { thread_id, reply } => {
            request_thread_archive(stdin, state, thread_id, reply)
        }
        ClientCommand::Shutdown => Ok(()),
    };

    if let Err(error) = result {
        let message = format!("Codex App Server 요청 실패: {error:#}");
        if let Some(reply) = rpc_reply {
            send_rpc_reply(reply, repaint, Err(anyhow::anyhow!(message.clone())));
        }
        if let Some(session_id) = target {
            emit_session(
                events,
                repaint,
                session_id,
                AgentSessionEvent::Failed { message },
            );
        } else {
            emit_transport_error(events, repaint, message);
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

fn request_thread_list(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    cursor: Option<String>,
    limit: Option<u32>,
    archived: bool,
    reply: Sender<anyhow::Result<Value>>,
) -> anyhow::Result<()> {
    let id = state.request_id();
    let key = rpc_key(&id);
    state
        .pending
        .insert(key.clone(), PendingRequest::ListThreads { reply });
    let frame = json!({
        "method": "thread/list",
        "id": id,
        "params": thread_list_params(cursor, limit, archived),
    });
    if let Err(error) = write_message(stdin, &frame) {
        state.pending.remove(&key);
        return Err(error);
    }
    Ok(())
}

fn request_thread_read(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    thread_id: String,
    include_turns: bool,
    reply: Sender<anyhow::Result<Value>>,
) -> anyhow::Result<()> {
    let id = state.request_id();
    let key = rpc_key(&id);
    state.pending.insert(
        key.clone(),
        PendingRequest::ReadThread {
            thread_id: thread_id.clone(),
            reply,
        },
    );
    let frame = json!({
        "method": "thread/read",
        "id": id,
        "params": {"threadId": thread_id, "includeTurns": include_turns},
    });
    if let Err(error) = write_message(stdin, &frame) {
        state.pending.remove(&key);
        return Err(error);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn request_thread_resume(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    session_id: AgentSessionId,
    thread_id: String,
    cwd: Option<String>,
    model: Option<String>,
    reply: Sender<anyhow::Result<Value>>,
) -> anyhow::Result<()> {
    let id = state.request_id();
    let key = rpc_key(&id);
    state.pending.insert(
        key.clone(),
        PendingRequest::ResumeThread {
            session_id,
            thread_id: thread_id.clone(),
            reply,
        },
    );
    let frame = json!({
        "method": "thread/resume",
        "id": id,
        "params": thread_resume_params(thread_id, cwd, model),
    });
    if let Err(error) = write_message(stdin, &frame) {
        state.pending.remove(&key);
        return Err(error);
    }
    Ok(())
}

fn request_thread_archive(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    thread_id: String,
    reply: Sender<anyhow::Result<Value>>,
) -> anyhow::Result<()> {
    let id = state.request_id();
    let key = rpc_key(&id);
    state.pending.insert(
        key.clone(),
        PendingRequest::ArchiveThread {
            thread_id: thread_id.clone(),
            reply,
        },
    );
    let frame = json!({
        "method": "thread/archive",
        "id": id,
        "params": {"threadId": thread_id},
    });
    if let Err(error) = write_message(stdin, &frame) {
        state.pending.remove(&key);
        return Err(error);
    }
    Ok(())
}

fn thread_list_params(cursor: Option<String>, limit: Option<u32>, archived: bool) -> Value {
    let mut params = serde_json::Map::from_iter([
        ("sourceKinds".to_owned(), json!(["appServer"])),
        ("archived".to_owned(), Value::Bool(archived)),
        ("sortKey".to_owned(), Value::String("updated_at".to_owned())),
        ("sortDirection".to_owned(), Value::String("desc".to_owned())),
    ]);
    if let Some(cursor) = cursor.filter(|cursor| !cursor.is_empty()) {
        params.insert("cursor".to_owned(), Value::String(cursor));
    }
    if let Some(limit) = limit {
        params.insert("limit".to_owned(), Value::from(limit));
    }
    Value::Object(params)
}

fn thread_resume_params(thread_id: String, cwd: Option<String>, model: Option<String>) -> Value {
    let mut params =
        serde_json::Map::from_iter([("threadId".to_owned(), Value::String(thread_id))]);
    if let Some(cwd) = cwd.filter(|cwd| !cwd.trim().is_empty()) {
        params.insert("cwd".to_owned(), Value::String(cwd));
    }
    if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
        params.insert("model".to_owned(), Value::String(model));
    }
    Value::Object(params)
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
            PendingRequest::ResumeThread {
                session_id, reply, ..
            } => {
                send_rpc_reply(reply, repaint, Err(anyhow::anyhow!(text.clone())));
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::Failed { message: text },
                );
            }
            PendingRequest::ListThreads { reply }
            | PendingRequest::ReadThread { reply, .. }
            | PendingRequest::ArchiveThread { reply, .. } => {
                send_rpc_reply(reply, repaint, Err(anyhow::anyhow!(text)));
            }
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
            let queued_commands = std::mem::take(&mut state.queued_rpc_commands);
            for command in queued_commands {
                handle_client_command(command, stdin, state, events, repaint);
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
        PendingRequest::ListThreads { reply } => {
            let response = validate_thread_list_result(&result).map(|()| result);
            send_rpc_reply(reply, repaint, response);
        }
        PendingRequest::ReadThread { thread_id, reply } => {
            let response = validate_thread_result(&result, &thread_id).map(|()| result);
            send_rpc_reply(reply, repaint, response);
        }
        PendingRequest::ResumeThread {
            session_id,
            thread_id,
            reply,
        } => {
            if let Err(error) = validate_thread_result(&result, &thread_id) {
                let message = format!("thread/resume 응답 오류: {error:#}");
                send_rpc_reply(reply, repaint, Err(anyhow::anyhow!(message.clone())));
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::Failed { message },
                );
                return;
            }
            state.known_sessions.insert(session_id.clone());
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
                    session_id,
                    AgentSessionEvent::ThreadStatusChanged { status },
                );
            }
            send_rpc_reply(reply, repaint, Ok(result));
        }
        PendingRequest::ArchiveThread { thread_id, reply } => {
            let response = if result.is_object() {
                Ok(result)
            } else {
                Err(anyhow::anyhow!("thread/archive 응답이 객체가 아닙니다"))
            };
            if response.is_ok()
                && let Some(session_id) = state.thread_to_session.remove(&thread_id)
            {
                state.session_to_thread.remove(&session_id);
                state.session_to_turn.remove(&session_id);
                state.known_sessions.remove(&session_id);
            }
            send_rpc_reply(reply, repaint, response);
        }
    }
}

fn send_rpc_reply(
    reply: Sender<anyhow::Result<Value>>,
    repaint: &egui::Context,
    result: anyhow::Result<Value>,
) {
    let _ = reply.send(result);
    repaint.request_repaint();
}

fn validate_thread_list_result(result: &Value) -> anyhow::Result<()> {
    anyhow::ensure!(result.is_object(), "thread/list 응답이 객체가 아닙니다");
    anyhow::ensure!(
        result.get("data").is_some_and(Value::is_array),
        "thread/list 응답에 data 배열이 없습니다"
    );
    if let Some(cursor) = result.get("nextCursor") {
        anyhow::ensure!(
            cursor.is_null() || cursor.is_string(),
            "thread/list nextCursor 형식이 잘못되었습니다"
        );
    }
    Ok(())
}

fn validate_thread_result(result: &Value, expected_thread_id: &str) -> anyhow::Result<()> {
    let thread_id = result
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .context("thread 응답에 thread.id가 없습니다")?;
    anyhow::ensure!(
        thread_id == expected_thread_id,
        "thread 응답 ID 불일치: expected {expected_thread_id}, got {thread_id}"
    );
    Ok(())
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

    #[test]
    fn thread_list_params_use_stable_app_server_filters_and_cursor() {
        let params = thread_list_params(Some("cursor-2".to_owned()), Some(40), false);
        assert_eq!(params["sourceKinds"], json!(["appServer"]));
        assert_eq!(params["archived"], false);
        assert_eq!(params["sortKey"], "updated_at");
        assert_eq!(params["sortDirection"], "desc");
        assert_eq!(params["cursor"], "cursor-2");
        assert_eq!(params["limit"], 40);

        let first_page = thread_list_params(Some(String::new()), None, true);
        assert_eq!(first_page["archived"], true);
        assert!(first_page.get("cursor").is_none());
        assert!(first_page.get("limit").is_none());
    }

    #[test]
    fn thread_resume_params_only_include_non_empty_overrides() {
        let params = thread_resume_params(
            "thread-1".to_owned(),
            Some("/repo".to_owned()),
            Some("gpt-test".to_owned()),
        );
        assert_eq!(
            params,
            json!({"threadId": "thread-1", "cwd": "/repo", "model": "gpt-test"})
        );

        let defaults = thread_resume_params(
            "thread-2".to_owned(),
            Some("  ".to_owned()),
            Some(String::new()),
        );
        assert_eq!(defaults, json!({"threadId": "thread-2"}));
    }

    #[test]
    fn history_response_parsers_accept_exact_shapes_and_reject_mismatches() {
        assert!(
            validate_thread_list_result(&json!({
                "data": [{"id": "thread-1"}],
                "nextCursor": "cursor-2"
            }))
            .is_ok()
        );
        assert!(validate_thread_list_result(&json!({"data": []})).is_ok());
        assert!(validate_thread_list_result(&json!({"data": {}})).is_err());
        assert!(validate_thread_list_result(&json!({"data": [], "nextCursor": 7})).is_err());

        let thread = json!({"thread": {"id": "thread-1", "turns": []}});
        assert!(validate_thread_result(&thread, "thread-1").is_ok());
        assert!(validate_thread_result(&thread, "thread-other").is_err());
        assert!(validate_thread_result(&json!({"thread": {}}), "thread-1").is_err());
    }
}
