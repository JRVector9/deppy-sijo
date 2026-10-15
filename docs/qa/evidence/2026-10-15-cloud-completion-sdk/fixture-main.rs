//! Private synthetic transport fixture. No GUI, PTY, real account or user DB.
use agent_mcp::{
    CompletionIdentity, CompletionPermit, CompletionReceipt, CompletionScope, History, Server,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{BufRead, Write},
    sync::mpsc,
    time::Duration,
};

struct Pending {
    identity: CompletionIdentity,
    permit: CompletionPermit,
    reply: mpsc::SyncSender<Result<Value, String>>,
    created: u64,
}
fn emit(value: Value) {
    println!("{value}");
    std::io::stdout().flush().unwrap();
}
fn main() -> anyhowless::Result {
    let server = Server::start(0, "", || {}).map_err(|e| e.to_string())?;
    let history = History::open_memory().map_err(|e| e.to_string())?;
    let (control, controls) = mpsc::sync_channel(64);
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            let Ok(command) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if control.send(command).is_err() {
                break;
            }
        }
    });
    emit(
        json!({"event":"ready","url":format!("http://{}/mcp",server.addr),"token":server.auth.token_for_user().as_str()}),
    );
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut submitted = 0usize;
    let mut completed = 0usize;
    let mut tool_calls = Vec::new();
    loop {
        while let Ok(command) = controls.try_recv() {
            if command["shutdown"] == true {
                emit(
                    json!({"event":"summary","submitted":submitted,"completed":completed,"tool_calls":tool_calls}),
                );
                return Ok(());
            }
            if let Some(operation) = command["complete"].as_str() {
                let Some(work) = pending.remove(operation) else {
                    return Err("unknown synthetic operation".into());
                };
                let at = agent_mcp::now();
                let receipt = CompletionReceipt {
                    event_id: work.identity.event_id(),
                    identity: work.identity.clone(),
                    admission: json!({"status":"queued","admission":"pty_queue","completion":"not_confirmed"}),
                    outcome: json!({"status":"completed","local_turn_completed":true,"source":"synthetic_verified_turn","observed_at":at}),
                    created_at: work.created,
                    completed_at: Some(at),
                    expires_at: at + agent_mcp::COMPLETION_RECEIPT_TTL,
                };
                history
                    .finish_completion(&work.identity, &receipt)
                    .map_err(|e| e.to_string())?;
                work.reply
                    .try_send(Ok(receipt.tool_result()))
                    .map_err(|e| e.to_string())?;
                completed += 1;
                emit(json!({"event":"completed","operation_id":operation}));
            }
            if let Some(operation) = command["revoke"].as_str()
                && let Some(work) = pending.get(operation)
            {
                work.permit.revoke();
            }
        }
        match server.requests.recv_timeout(Duration::from_millis(10)) {
            Ok(request) => {
                tool_calls.push(request.tool.clone());
                if request.tool != "send_text" || request.args["submit"] != true {
                    let _ = request.reply.try_send(Err("invalid_arguments".into()));
                    continue;
                }
                let field = |name: &str| {
                    request.args[name]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("missing synthetic {name}"))
                };
                let scope = CompletionScope {
                    workspace: "synthetic-workspace".into(),
                    session_id: field("session_id")?,
                    generation: field("generation")?,
                    operation_id: field("operation_id")?,
                    run_id: None,
                };
                let permit = CompletionPermit::new();
                let identity = request
                    .deferred
                    .as_ref()
                    .ok_or("missing deferred transport")?
                    .reserve(scope, permit.clone())
                    .map_err(|e| format!("synthetic reserve: {e}"))?;
                history
                    .claim_completion_scoped("send_text", &request.args, &identity)
                    .map_err(|e| e.to_string())?;
                submitted += 1;
                emit(
                    json!({"event":"submitted","operation_id":identity.scope.operation_id,"stream_id":identity.stream_id,"request_id":identity.request_id}),
                );
                pending.insert(
                    identity.scope.operation_id.clone(),
                    Pending {
                        identity,
                        permit,
                        reply: request.reply,
                        created: agent_mcp::now(),
                    },
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("server closed".into()),
        }
    }
}
mod anyhowless {
    pub type Result = std::result::Result<(), String>;
}
