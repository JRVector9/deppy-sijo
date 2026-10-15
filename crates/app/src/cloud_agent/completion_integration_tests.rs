//! Private HTTP/SQLite fixtures. No app, provider account, or real terminal is launched.
use super::*;
use crate::agent_detect::AgentKind;
use deppy_core::process_identity::HookExecutionStamp;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::PathBuf,
    time::Duration,
};

const PROMPT: &str = "own full prompt 한글";

pub(super) struct Fixture {
    pub(super) bridge: CloudAgent,
    pub(super) target: Target,
    pub(super) connection: String,
    pub(super) baseline: storage::NativeTurnProof,
    root: PathBuf,
    history_path: PathBuf,
    metadata_path: PathBuf,
    bearer: String,
    transport_session: String,
    sequence: u64,
    next_request: u64,
}

pub(super) struct Started {
    pub(super) internal: String,
    pub(super) identity: agent_mcp::CompletionIdentity,
    pub(super) sequence: u64,
    reader: BufReader<TcpStream>,
}

fn micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64
}

fn head(reader: &mut impl BufRead) -> String {
    let mut result = String::new();
    loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "HTTP ended before headers"
        );
        result.push_str(&line);
        assert!(result.len() <= 8192);
        if line == "\r\n" {
            return result;
        }
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

fn json_body(reader: &mut impl Read, head: &str) -> Value {
    let length = header(head, "content-length")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert!(length <= 65536);
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn event(reader: &mut impl BufRead) -> String {
    let mut result = String::new();
    while !result.ends_with("\n\n") {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "SSE ended before final event"
        );
        result.push_str(&line);
        assert!(result.len() <= agent_mcp::MAX_COMPLETION_EVENT_BYTES);
    }
    result
}

fn payload(response: &Value) -> Value {
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

impl Fixture {
    pub(super) fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("deppy-completion-app-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let history_path = root.join("history.sqlite3");
        let metadata_path = root.join("metadata.sqlite3");
        let mut bridge = CloudAgent::new(
            &history_path,
            secret::RedactionService::new(),
            egui::Context::default(),
        );
        bridge.drain_history(|_, _| panic!("history initialization cannot type"));
        bridge.server = Some(Server::start(0, "", || {}).unwrap());
        let mut target =
            Target::fixture(&uuid::Uuid::new_v4().to_string(), "completion-generation");
        target.workspace = uuid::Uuid::new_v4().to_string();
        target.runtime = 7;
        target.runtime_token = "completion-fixture-runtime".into();
        target.known_ai = true;
        target.provider = Some(AgentProvider::Codex);
        target.execution =
            AgentExecutionIdentity::fixture_current(AgentKind::Codex, std::process::id());
        assert!(target.execution.unwrap().is_current());
        target.bracketed_paste = true;
        bridge.set_targets(vec![target.clone()]);
        let server = bridge.server.as_ref().unwrap();
        let (principal, bearer) = server
            .auth
            .mint_manual_connection("Private completion fixture", true, now())
            .unwrap();
        let bearer = bearer.to_string();
        let connection = principal.id;
        let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"private-app-fixture","version":"1"}}});
        let socket = Self::post_to(server, &bearer, "", &initialize);
        let mut reader = BufReader::new(socket);
        let initial_head = head(&mut reader);
        assert!(initial_head.starts_with("HTTP/1.1 200"));
        let transport_session = header(&initial_head, "mcp-session-id").unwrap().to_owned();
        assert_eq!(json_body(&mut reader, &initial_head)["id"], 1);
        bridge
            .set_connection_access(&connection, &target, true, true)
            .unwrap();
        bridge
            .set_connection_local_completion(&connection, &target, true)
            .unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        let transcript = root.join(format!("rollout-fixture-{native}.jsonl"));
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                json!({"type":"session_meta","payload":{"id":native}})
            ),
        )
        .unwrap();
        let at = micros() - 2_000_000;
        let stamp = HookExecutionStamp::capture(&target.runtime_token).unwrap();
        let db = storage::Db::open(&metadata_path).unwrap();
        let key = format!("{}:{}", target.workspace, target.session.0);
        for (kind, event_at) in [
            (storage::AttentionEventKind::SessionStart, at),
            (storage::AttentionEventKind::TurnStart, at + 1),
        ] {
            let native_event = storage::AgentAttentionEvent {
                runtime_instance: Some(target.runtime_token.clone()),
                native_session_id: native.clone(),
                turn_id: Some("baseline-native-turn".into()),
                request_id: String::new(),
                kind,
                at_micros: event_at,
                tool_group: None,
                tool_input_hash: None,
                child_id: None,
            };
            db.record_agent_attention_with_proof(
                &key,
                &native_event,
                Some(&stamp),
                Some(&storage::native_prompt_digest("previous local prompt")),
                transcript.to_str(),
            )
            .unwrap();
        }
        let mut job = storage::AgentStateJob::projection(&target.workspace);
        job.native_turn_keys = vec![key];
        let baseline = db
            .apply_agent_state_job(&job)
            .unwrap()
            .native_turns
            .remove(0);
        assert!(baseline.is_valid());
        bridge.install_native_turns(vec![baseline.clone()]);
        bridge.observe_input(
            target.runtime,
            &[runtime::RuntimeEvent::SessionInputAccepted {
                session: target.session,
                sequence: 10,
                operation_id: None,
                at_micros: at + 2,
            }],
        );
        let mut result = Self {
            bridge,
            target,
            connection,
            baseline,
            root,
            history_path,
            metadata_path,
            bearer,
            transport_session,
            sequence: 10,
            next_request: 77,
        };
        let control = result.rpc(
            "acquire_control",
            json!({"session_id":result.target.id,"generation":result.target.generation}),
        );
        assert_eq!(payload(&control)["control"], "acquired");
        result
    }

    fn post_to(server: &Server, bearer: &str, transport_session: &str, body: &Value) -> TcpStream {
        let mut socket = TcpStream::connect(server.addr).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let body = body.to_string();
        let session = if transport_session.is_empty() {
            String::new()
        } else {
            format!("MCP-Session-Id: {transport_session}\r\n")
        };
        write!(socket, "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {bearer}\r\n{session}Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", server.addr, body.len()).unwrap();
        socket
    }

    fn request(&mut self, tool: &str, args: Value) -> (Request, BufReader<TcpStream>, u64) {
        let request_id = self.next_request;
        self.next_request += 1;
        let body = json!({"jsonrpc":"2.0","id":request_id,"method":"tools/call","params":{"name":tool,"arguments":args}});
        let server = self.bridge.server.as_ref().unwrap();
        let socket = Self::post_to(server, &self.bearer, &self.transport_session, &body);
        let request = server
            .requests
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        (request, BufReader::new(socket), request_id)
    }

    pub(super) fn rpc(&mut self, tool: &str, args: Value) -> Value {
        let (request, mut reader, _) = self.request(tool, args);
        self.bridge.handle_and_drain(request, |_, _| {
            panic!("fixture management call cannot type")
        });
        let response_head = head(&mut reader);
        json_body(&mut reader, &response_head)
    }

    pub(super) fn try_start(
        &mut self,
        operation: &str,
        run: Option<&str>,
    ) -> Result<Started, Value> {
        let mut args = json!({"session_id":self.target.id,"generation":self.target.generation,"operation_id":operation,"text":PROMPT,"submit":true});
        if let Some(run) = run {
            args["run_id"] = json!(run);
        }
        let (request, mut reader, request_id) = self.request("send_text", args);
        let mut effects = 0;
        let expected_target = self.target.id.clone();
        self.bridge.handle_and_drain(request, |target, effect| {
            assert_eq!(target.id, expected_target);
            match effect {
                Effect::Input { .. } | Effect::PasteInput { .. } => effects += 1,
                _ => panic!("unexpected tracked input effect"),
            }
            Ok(())
        });
        let response_head = head(&mut reader);
        if header(&response_head, "content-type") != Some("text/event-stream") {
            assert_eq!(effects, 0, "a no-effect rejection must not dispatch input");
            return Err(json_body(&mut reader, &response_head));
        }
        assert_eq!(effects, 1);
        let prime = event(&mut reader);
        let cursor = prime
            .lines()
            .find_map(|line| line.strip_prefix("id: "))
            .unwrap();
        let stream_id = cursor.strip_suffix(":0").unwrap().to_owned();
        assert!(
            !prime.contains("queued"),
            "original request must not finish at queue admission"
        );
        let identity = agent_mcp::CompletionIdentity {
            connection_id: self.connection.clone(),
            transport_session_id: self.transport_session.clone(),
            stream_id,
            request_id: json!(request_id),
            auth_epoch: self
                .bridge
                .server
                .as_ref()
                .unwrap()
                .auth
                .authenticate(&format!("Bearer {}", self.bearer), now())
                .unwrap(),
            scope: agent_mcp::CompletionScope {
                workspace: self.target.workspace.clone(),
                session_id: self.target.id.clone(),
                generation: self.target.generation.clone(),
                operation_id: operation.into(),
                run_id: run.map(str::to_owned),
            },
        };
        identity.validate().unwrap();
        let internal = agent_mcp::operation_key(&self.connection, operation).unwrap();
        assert!(self.bridge.pending.contains_key(&internal));
        Ok(Started {
            internal,
            identity,
            sequence: self.sequence + 1,
            reader,
        })
    }

    pub(super) fn start(&mut self, operation: &str, run: Option<&str>) -> Started {
        self.try_start(operation, run)
            .unwrap_or_else(|response| panic!("tracked input rejected: {response}"))
    }

    pub(super) fn admit(
        &mut self,
        started: &Started,
        result: Result<(), runtime::PtyInputRejectReason>,
    ) -> i64 {
        let at = micros();
        let mut events = Vec::new();
        if result.is_ok() {
            self.sequence = started.sequence;
            events.push(runtime::RuntimeEvent::SessionInputAccepted {
                session: self.target.session,
                sequence: started.sequence,
                operation_id: Some(started.internal.clone()),
                at_micros: at,
            });
        }
        events.push(runtime::RuntimeEvent::InputAdmitted {
            session: self.target.session,
            operation_id: started.internal.clone(),
            result,
        });
        self.bridge.observe_and_drain(self.target.runtime, &events);
        at
    }

    pub(super) fn completed_proof(
        &self,
        started: &Started,
        accepted_at: i64,
    ) -> storage::NativeTurnProof {
        let db = storage::Db::open(&self.metadata_path).unwrap();
        let stamp = HookExecutionStamp::capture(&self.target.runtime_token).unwrap();
        let turn = format!("native-{}", started.identity.scope.operation_id);
        let key = self.baseline.session_key.clone();
        for (kind, at) in [
            (storage::AttentionEventKind::TurnStart, accepted_at + 1),
            (storage::AttentionEventKind::Completed, accepted_at + 2),
        ] {
            let native_event = storage::AgentAttentionEvent {
                runtime_instance: Some(self.target.runtime_token.clone()),
                native_session_id: self.baseline.native_session_id.clone(),
                turn_id: Some(turn.clone()),
                request_id: String::new(),
                kind,
                at_micros: at,
                tool_group: None,
                tool_input_hash: None,
                child_id: None,
            };
            db.record_agent_attention_with_proof(
                &key,
                &native_event,
                Some(&stamp),
                Some(&storage::native_prompt_digest(PROMPT)),
                Some(&self.baseline.transcript_path),
            )
            .unwrap();
        }
        let mut job = storage::AgentStateJob::projection(&self.target.workspace);
        job.native_turn_keys = vec![key];
        let proof = db
            .apply_agent_state_job(&job)
            .unwrap()
            .native_turns
            .remove(0);
        let transcript = format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":proof.native_session_id}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":proof.turn_id}})
        );
        std::fs::write(&proof.transcript_path, transcript).unwrap();
        assert!(
            completion::confirm_codex_rollout(&proof),
            "private fixture must have a real matching final marker"
        );
        proof
    }

    pub(super) fn finish(&mut self, started: &mut Started) -> Value {
        self.bridge
            .drain_history(|_, _| panic!("finishing a watch cannot type"));
        let final_event = event(&mut started.reader);
        assert!(final_event.contains(&format!("id: {}\n", started.identity.event_id())));
        let response: Value = serde_json::from_str(
            final_event
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(response["id"], started.identity.request_id);
        let mut eof = String::new();
        started.reader.read_to_string(&mut eof).unwrap();
        assert!(
            eof.is_empty(),
            "the original request must have exactly one final result"
        );
        payload(&response)
    }

    pub(super) fn receipt(&self, started: &Started) -> agent_mcp::CompletionReceipt {
        agent_mcp::History::open(&self.history_path)
            .unwrap()
            .get_completion(&started.identity)
            .unwrap()
            .unwrap()
    }

    pub(super) fn public_admission(&self, started: &Started) -> Value {
        agent_mcp::History::open(&self.history_path)
            .unwrap()
            .get_operation(&started.identity.scope.operation_id, &self.connection)
            .unwrap()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.bridge.shutdown();
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn completion_original_http_request_waits_for_matching_native_final_and_durable_receipt() {
    let mut fixture = Fixture::new();
    let mut started = fixture.start("original", None);
    let accepted = fixture.admit(&started, Ok(()));
    assert_eq!(fixture.public_admission(&started)["status"], "queued");
    started
        .reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(30)))
        .unwrap();
    let mut byte = [0];
    assert!(
        matches!(started.reader.read(&mut byte), Err(error) if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock)),
        "queue admission must leave original SSE waiting"
    );
    started
        .reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let proof = fixture.completed_proof(&started, accepted);
    let mut raw_stop = proof.clone();
    std::fs::write(
        &proof.transcript_path,
        format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":proof.native_session_id}})
        ),
    )
    .unwrap();
    assert!(!completion::confirm_codex_rollout(&raw_stop));
    raw_stop.completed = None;
    fixture.bridge.install_native_turns(vec![raw_stop]);
    assert!(fixture.bridge.has_completion_watches());
    let proof = fixture.completed_proof(&started, accepted);
    fixture.bridge.install_native_turns(vec![proof]);
    let response = fixture.finish(&mut started);
    assert_eq!(response["local_completion"]["local_turn_completed"], true);
    let stored = fixture.receipt(&started);
    assert_eq!(stored.tool_result(), response);
    assert_eq!(stored.admission, fixture.public_admission(&started));
    assert_eq!(stored.identity, started.identity);
    assert_eq!(
        fixture
            .bridge
            .completion_status_for_operation(
                &fixture.connection,
                &fixture.target.id,
                &fixture.target.generation,
                "original"
            )
            .unwrap()
            .local,
        completion::LocalCompletionState::Completed
    );
    assert!(
        fixture
            .bridge
            .completion_status_for_run(
                &fixture.connection,
                &fixture.target.id,
                &fixture.target.generation,
                "foreign-run"
            )
            .is_none()
    );
}

#[test]
fn completion_timeout_then_late_accepted_keeps_one_immutable_unknown_admission() {
    assert_timeout_stays_unknown(Some(Ok(())), false);
}

#[test]
fn completion_timeout_then_late_rejected_keeps_one_immutable_unknown_admission() {
    assert_timeout_stays_unknown(
        Some(Err(runtime::PtyInputRejectReason::AdmissionDenied)),
        false,
    );
}

#[test]
fn completion_timeout_then_abandon_keeps_public_and_private_admission_equal() {
    assert_timeout_stays_unknown(None, true);
}

fn assert_timeout_stays_unknown(
    late: Option<Result<(), runtime::PtyInputRejectReason>>,
    abandon: bool,
) {
    let mut fixture = Fixture::new();
    let mut started = fixture.start("timeout", None);
    fixture
        .bridge
        .pending
        .get_mut(&started.internal)
        .unwrap()
        .deadline = Instant::now() - Duration::from_secs(1);
    fixture.bridge.expire_pending();
    fixture
        .bridge
        .drain_history(|_, _| panic!("admission timeout cannot send more input"));
    let response = fixture.finish(&mut started);
    assert_eq!(response["local_completion"]["status"], "unknown");
    let stored = fixture.receipt(&started);
    assert_eq!(stored.admission["status"], "unknown");
    assert_eq!(fixture.public_admission(&started), stored.admission);
    if let Some(late) = late {
        fixture.admit(&started, late);
    }
    if abandon {
        fixture
            .bridge
            .pending
            .get_mut(&started.internal)
            .unwrap()
            .deadline = Instant::now() - Duration::from_secs(31);
        fixture.bridge.expire_pending();
        fixture
            .bridge
            .drain_history(|_, _| panic!("abandoning input cannot send more input"));
    }
    assert_eq!(
        fixture.receipt(&started),
        stored,
        "terminal private receipt is immutable"
    );
    assert_eq!(
        fixture.public_admission(&started),
        stored.admission,
        "late runtime/abandon must not overwrite the public terminal admission"
    );
}

#[test]
fn completion_manual_input_between_owned_admission_and_native_end_is_unknown() {
    let mut fixture = Fixture::new();
    let mut started = fixture.start("manual-intervention", None);
    let accepted = fixture.admit(&started, Ok(()));
    fixture.bridge.observe_and_drain(
        fixture.target.runtime,
        &[runtime::RuntimeEvent::SessionInputAccepted {
            session: fixture.target.session,
            sequence: started.sequence + 1,
            operation_id: None,
            at_micros: accepted + 1,
        }],
    );
    let proof = fixture.completed_proof(&started, accepted);
    fixture.bridge.install_native_turns(vec![proof]);
    assert_eq!(
        fixture.finish(&mut started)["local_completion"]["status"],
        "unknown"
    );
    assert_eq!(fixture.receipt(&started).outcome["status"], "unknown");
}

#[test]
fn completion_input_only_revoke_preserves_accepted_watch_but_read_or_optout_revokes() {
    let mut fixture = Fixture::new();
    let mut started = fixture.start("input-revoked", None);
    let accepted = fixture.admit(&started, Ok(()));
    fixture
        .bridge
        .set_connection_access(&fixture.connection, &fixture.target, true, false)
        .unwrap();
    assert!(
        fixture
            .bridge
            .completion_input_permit(&started.internal)
            .unwrap()
            .is_live()
    );
    let proof = fixture.completed_proof(&started, accepted);
    fixture.bridge.install_native_turns(vec![proof]);
    assert_eq!(
        fixture.finish(&mut started)["local_completion"]["status"],
        "completed"
    );
    for read in [true, false] {
        let mut fixture = Fixture::new();
        let started = fixture.start("return-revoked", None);
        fixture.admit(&started, Ok(()));
        let permit = fixture
            .bridge
            .completion_input_permit(&started.internal)
            .unwrap();
        if read {
            fixture
                .bridge
                .set_connection_access(&fixture.connection, &fixture.target, false, false)
                .unwrap();
        } else {
            fixture
                .bridge
                .set_connection_local_completion(&fixture.connection, &fixture.target, false)
                .unwrap();
        }
        assert!(!permit.is_live());
        fixture
            .bridge
            .drain_history(|_, _| panic!("revoking return cannot type"));
        assert_eq!(fixture.receipt(&started).outcome["status"], "unknown");
    }
}
