use super::*;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpStream},
    time::{Duration, Instant},
};

fn scope(operation: &str) -> CompletionScope {
    CompletionScope {
        workspace: "synthetic-workspace".into(),
        session_id: format!("synthetic-session-{operation}"),
        generation: "synthetic-generation".into(),
        operation_id: operation.into(),
        run_id: Some("synthetic-run".into()),
    }
}
fn identity(operation: &str) -> CompletionIdentity {
    CompletionIdentity {
        connection_id: "synthetic-actor".into(),
        transport_session_id: uuid::Uuid::new_v4().to_string(),
        stream_id: uuid::Uuid::new_v4().to_string(),
        request_id: json!(77),
        auth_epoch: 1,
        scope: scope(operation),
    }
}
fn receipt(identity: CompletionIdentity) -> CompletionReceipt {
    let at = now();
    CompletionReceipt {
        event_id: identity.event_id(),
        identity,
        admission: json!({"status":"queued","admission":"pty_queue","completion":"not_confirmed"}),
        outcome: json!({"status":"completed","local_turn_completed":true,"source":"verified_native_turn","observed_at":at}),
        created_at: at,
        completed_at: Some(at),
        expires_at: at + COMPLETION_RECEIPT_TTL,
    }
}
fn post(server: &Server, token: &str, session: &str, operation: &str) -> TcpStream {
    let mut socket = TcpStream::connect(server.addr).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let body = json!({"jsonrpc":"2.0","id":77,"method":"tools/call","params":{"name":"send_text","arguments":{"session_id":scope(operation).session_id,"generation":"synthetic-generation","operation_id":operation,"text":"synthetic prompt","submit":true}}}).to_string();
    write!(socket, "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\nMCP-Session-Id: {session}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", server.addr, body.len()).unwrap();
    socket
}
fn next_request(server: &Server) -> Request {
    server
        .requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
}
fn sse_head(socket: TcpStream) -> BufReader<TcpStream> {
    let mut reader = BufReader::new(socket);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        head.push_str(&line);
        assert!(head.len() <= 8192);
        if line == "\r\n" {
            break;
        }
    }
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(
        crate::tests::completion_header(&head, "content-type"),
        Some("text/event-stream")
    );
    reader
}
fn event(reader: &mut impl BufRead) -> String {
    let mut value = String::new();
    while !value.ends_with("\n\n") {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "SSE ended before event: {value}"
        );
        value.push_str(&line);
        assert!(value.len() <= MAX_COMPLETION_EVENT_BYTES);
    }
    value
}
fn resume(server: &Server, token: &str, session: &str, cursor: &str) -> TcpStream {
    let mut socket = TcpStream::connect(server.addr).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(socket, "GET /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\nMCP-Session-Id: {session}\r\nLast-Event-ID: {cursor}\r\nAccept: text/event-stream\r\n\r\n", server.addr).unwrap();
    socket
}

#[test]
fn completion_stream_fast_final_uses_original_post_and_owned_replay() {
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user();
    let session = crate::tests::completion_initialize(&server, &token);
    let socket = post(&server, &token, &session, "fast");
    let req = next_request(&server);
    let identity = req
        .deferred
        .as_ref()
        .unwrap()
        .reserve(scope("fast"), CompletionPermit::new())
        .unwrap();
    let expected = receipt(identity.clone());
    expected.validate().unwrap();
    // The final result may already be available when the HTTP worker sees handoff.
    req.reply.try_send(Ok(expected.tool_result())).unwrap();
    let mut reader = sse_head(socket);
    let prime = event(&mut reader);
    assert!(prime.contains(&format!("id: {}:0\n", identity.stream_id)));
    let final_event = event(&mut reader);
    assert!(final_event.contains(&format!("id: {}\n", expected.event_id)));
    let response: Value = serde_json::from_str(
        final_event
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(response["id"], 77);
    let payload: Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["local_completion"]["local_turn_completed"], true);
    let mut eof = String::new();
    reader.read_to_string(&mut eof).unwrap();
    assert!(eof.is_empty());
    let mut replay = sse_head(resume(
        &server,
        &token,
        &session,
        &format!("{}:0", identity.stream_id),
    ));
    event(&mut replay);
    assert_eq!(
        event(&mut replay),
        final_event,
        "same durable event ID and original request ID"
    );
    let foreign_session = crate::tests::completion_initialize(&server, &token);
    assert_eq!(
        crate::tests::completion_http(
            &server,
            &token,
            "GET",
            &format!(
                "MCP-Session-Id: {foreign_session}\r\nLast-Event-ID: {}:0\r\n",
                identity.stream_id
            ),
            None
        )
        .0,
        404
    );
    assert!(
        server.requests.try_recv().is_err(),
        "replay must not enqueue input"
    );
}

#[test]
fn completion_stream_disconnect_retains_original_receiver_and_revoke_blocks_replay() {
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user();
    let session = crate::tests::completion_initialize(&server, &token);
    let socket = post(&server, &token, &session, "disconnect");
    let req = next_request(&server);
    let permit = CompletionPermit::new();
    let identity = req
        .deferred
        .as_ref()
        .unwrap()
        .reserve(scope("disconnect"), permit.clone())
        .unwrap();
    let mut reader = sse_head(socket);
    event(&mut reader);
    reader.get_ref().shutdown(Shutdown::Both).unwrap();
    drop(reader);
    req.reply
        .try_send(Ok(receipt(identity.clone()).tool_result()))
        .unwrap();
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(event) = server
            .delivery_events
            .recv_timeout(Duration::from_millis(50))
            && event.identity == identity
            && event.state == CompletionDeliveryState::Disconnected
        {
            break;
        }
        assert!(
            Instant::now() < end,
            "original disconnect must release the reader without dropping its result"
        );
    }
    let mut replay = sse_head(resume(
        &server,
        &token,
        &session,
        &format!("{}:0", identity.stream_id),
    ));
    event(&mut replay);
    assert!(event(&mut replay).contains("local_turn_completed"));
    permit.revoke();
    assert_eq!(
        crate::tests::completion_http(
            &server,
            &token,
            "GET",
            &format!(
                "MCP-Session-Id: {session}\r\nLast-Event-ID: {}:0\r\n",
                identity.stream_id
            ),
            None
        )
        .0,
        404
    );
    assert!(server.requests.try_recv().is_err());
}

#[test]
fn completion_stream_waits_do_not_consume_http_or_actor_call_budgets() {
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user();
    let session = crate::tests::completion_initialize(&server, &token);
    let mut held = Vec::new();
    for index in 0..5 {
        let op = format!("wait-{index}");
        let socket = post(&server, &token, &session, &op);
        let req = next_request(&server);
        req.deferred
            .as_ref()
            .unwrap()
            .reserve(scope(&op), CompletionPermit::new())
            .unwrap();
        let mut reader = sse_head(socket);
        event(&mut reader);
        held.push((reader, req.reply));
    }
    let (status, _, body) = crate::tests::completion_http(
        &server,
        &token,
        "POST",
        &format!("MCP-Session-Id: {session}\r\n"),
        Some(json!({"jsonrpc":"2.0","id":99,"method":"ping"})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["id"], 99);
    assert_eq!(held.len(), 5);
}

#[test]
fn completion_stream_late_redaction_preserves_only_real_routing_identity() {
    let redaction = secret::RedactionService::new();
    let server = Server::start_with_redaction(0, "", redaction.clone(), || {}).unwrap();
    let token = server.auth.token_for_user();
    let session = crate::tests::completion_initialize(&server, &token);
    let socket = post(&server, &token, &session, "late-redaction");
    let req = next_request(&server);
    let mut original_scope = scope("late-redaction");
    let secret = "synthetic-late-completion-secret";
    original_scope.session_id = secret.into();
    let identity = req
        .deferred
        .as_ref()
        .unwrap()
        .reserve(original_scope, CompletionPermit::new())
        .unwrap();
    let mut reader = sse_head(socket);
    event(&mut reader);
    let mut expected = receipt(identity);
    expected.outcome["display"] = json!({"operation_id":secret});
    expected.validate().unwrap();
    // Register after the original request/stream exists, immediately before send.
    redaction.register(&secret::SecretString::new(secret.into()));
    req.reply.try_send(Ok(expected.tool_result())).unwrap();
    let wire = event(&mut reader);
    let response: Value = serde_json::from_str(
        wire.lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap(),
    )
    .unwrap();
    let payload: Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        payload["session_id"], secret,
        "the real routing identity remains exact"
    );
    assert_ne!(
        payload["local_completion"]["display"]["operation_id"], secret,
        "a nested display object cannot bypass redaction by naming a field operation_id"
    );
}

#[test]
fn completion_stream_closed_after_handoff_does_not_assert_no_effect() {
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user();
    let session = crate::tests::completion_initialize(&server, &token);
    let mut socket = post(&server, &token, &session, "handoff-close");
    let req = next_request(&server);
    let deferred = req.deferred.as_ref().unwrap();
    let (entered, release) = deferred.pause_next_begin();
    let permit = CompletionPermit::new();
    let identity = deferred
        .reserve(scope("handoff-close"), permit.clone())
        .unwrap();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    // App may have admitted input after reserve. Close the lane before the HTTP
    // worker can take its original receiver, without assuming the effect state.
    permit.revoke();
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(event) = server
            .delivery_events
            .recv_timeout(Duration::from_millis(50))
            && event.identity == identity
            && event.state == CompletionDeliveryState::Closed
        {
            break;
        }
        assert!(Instant::now() < end, "revoked stream was not closed");
    }
    release.try_send(()).unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    assert_eq!(
        crate::tests::completion_header(head, "content-type"),
        Some("application/json")
    );
    let rpc: Value = serde_json::from_str(body).unwrap();
    assert_eq!(rpc["id"], 77);
    let payload: Value =
        serde_json::from_str(rpc["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["error"], "outcome_unknown_do_not_retry_input");
    assert_eq!(payload["effect_state"], "unknown");
    assert_eq!(payload["retryable"], false);
}

#[test]
fn completion_stream_bounds_retained_app_result_before_replay() {
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user();
    let session = crate::tests::completion_initialize(&server, &token);
    let socket = post(&server, &token, &session, "bounded-cache");
    let req = next_request(&server);
    let deferred = req.deferred.as_ref().unwrap();
    let identity = deferred
        .reserve(scope("bounded-cache"), CompletionPermit::new())
        .unwrap();
    let mut reader = sse_head(socket);
    event(&mut reader);
    req.reply
        .try_send(Ok(json!({"display":"x".repeat(64 * 1024)})))
        .unwrap();
    let final_event = event(&mut reader);
    assert!(final_event.contains("completion_result_too_large_unknown"));
    let retained = deferred
        .retained_result_bytes(&identity.stream_id)
        .expect("returned original result remains available for owned replay");
    assert!(
        retained <= MAX_COMPLETION_EVENT_BYTES,
        "oversized App result must not remain in the bounded replay cache: {retained} bytes"
    );
}

#[test]
fn completion_stream_reserve_budgets_maximum_original_result_before_effect() {
    let mut boundary = None;
    for length in 1..=128 {
        let mut candidate = identity("boundary");
        candidate.scope.session_id = "s".repeat(length);
        candidate.scope.generation = "g".repeat(length);
        candidate.scope.operation_id = "o".repeat(length);
        candidate.scope.run_id = Some("r".repeat(length));
        let mut maximum = receipt(candidate.clone());
        maximum.admission = json!({"status":"queued","admission":"pty_queue","bytes":u64::MAX,"submit":false,"retry":false,"completion":"not_confirmed"});
        maximum.outcome = json!({"status":"unknown","local_turn_completed":false,"source":"verified_native_turn","reason":"x".repeat(64),"observed_at":u64::MAX});
        if maximum.validate().is_err() {
            boundary = Some(candidate.scope);
            break;
        }
    }
    let boundary = boundary.expect("maximum legal routing sizes exceed the bounded wire event");
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user();
    let session = crate::tests::completion_initialize(&server, &token);
    let _socket = post(&server, &token, &session, "boundary");
    let req = next_request(&server);
    let reserved = req
        .deferred
        .as_ref()
        .unwrap()
        .reserve(boundary, CompletionPermit::new());
    assert!(
        matches!(reserved, Err(ref code) if code == "completion_result_too_large_no_effect"),
        "reserve must reject before input when the declared maximum receipt cannot fit: {reserved:?}"
    );
    req.reply
        .try_send(Err("completion_result_too_large_no_effect".into()))
        .unwrap();
}

#[test]
fn completion_ledger_atomic_claim_capacity_preserves_no_effect() {
    let db = History::open_memory().unwrap();
    for index in 0..MAX_COMPLETION_WATCHES {
        let id = identity(&format!("op-{index}"));
        assert_eq!(
            db.claim_completion_scoped("send_text", &json!({"text":"synthetic"}), &id)
                .unwrap(),
            Claim::New
        );
    }
    let rejected = identity("over-capacity");
    assert!(
        db.claim_completion_scoped("send_text", &json!({"text":"synthetic"}), &rejected)
            .is_err()
    );
    assert!(
        db.get_operation("over-capacity", &rejected.connection_id)
            .unwrap()
            .is_none(),
        "capacity rejection must roll back the operation claim too"
    );
}

#[test]
fn completion_ledger_final_is_immutable_private_and_preserves_public_admission() {
    let db = History::open_memory().unwrap();
    let id = identity("immutable");
    let expected = receipt(id.clone());
    db.claim_completion_scoped("send_text", &json!({"text":"synthetic"}), &id)
        .unwrap();
    db.finish_completion(&id, &expected).unwrap();
    db.finish_completion(&id, &expected).unwrap();
    assert_eq!(db.get_completion(&id).unwrap(), Some(expected.clone()));
    let ordinary = db
        .get_operation("immutable", &id.connection_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        ordinary, expected.admission,
        "get_operation is admission-only even after final completion"
    );
    assert!(ordinary.get("event_id").is_none());
    let mut wrong = id.clone();
    wrong.transport_session_id = uuid::Uuid::new_v4().to_string();
    assert!(db.get_completion(&wrong).unwrap().is_none());
    assert!(
        db.finish_completion(&wrong, &receipt(wrong.clone()))
            .is_err()
    );
    let mut changed = expected;
    changed.outcome = json!({"status":"completed","source":"different-proof"});
    assert!(db.finish_completion(&id, &changed).is_err());
}

#[test]
fn completion_ledger_restart_marks_pending_unknown_without_changing_admission() {
    let directory =
        std::env::temp_dir().join(format!("deppy-completion-ledger-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("synthetic.sqlite");
    let id = identity("restart");
    let admission = receipt(id.clone()).admission;
    {
        let db = History::open(&path).unwrap();
        db.claim_completion_scoped("send_text", &json!({"text":"synthetic"}), &id)
            .unwrap();
        db.finish_scoped("restart", &id.connection_id, &admission, "")
            .unwrap();
    }
    {
        let restored = History::open(&path).unwrap();
        let result = restored
            .get_completion(&id)
            .unwrap()
            .expect("pending original identity needs an explicit restart outcome");
        assert_eq!(result.outcome["status"], "unknown");
        assert_eq!(result.event_id, id.event_id());
        assert_eq!(
            restored
                .get_operation("restart", &id.connection_id)
                .unwrap(),
            Some(admission)
        );
    }
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn completion_ledger_aged_restart_retains_receipt_from_terminalization_time() {
    let directory =
        std::env::temp_dir().join(format!("deppy-completion-aged-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("synthetic.sqlite");
    let id = identity("aged-restart");
    let admission = receipt(id.clone()).admission;
    {
        let db = History::open(&path).unwrap();
        db.claim_completion_scoped("send_text", &json!({"text":"synthetic"}), &id)
            .unwrap();
        db.finish_scoped("aged-restart", &id.connection_id, &admission, "")
            .unwrap();
    }
    let original_created = now() - 2 * COMPLETION_RECEIPT_TTL;
    {
        // Age only this private synthetic DB, simulating two days without a restart.
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute(
            "UPDATE local_completion_receipts SET created=?1,expires=?2",
            rusqlite::params![
                i64::try_from(original_created).unwrap(),
                i64::try_from(original_created + 3600).unwrap()
            ],
        )
        .unwrap();
    }
    let restored_at = now();
    {
        let db = History::open(&path).unwrap();
        let result = db.get_completion(&id).unwrap().unwrap();
        assert_eq!(result.created_at, original_created);
        assert_eq!(result.outcome["status"], "unknown");
        assert!(result.expires_at >= restored_at + COMPLETION_RECEIPT_TTL);
        assert_eq!(
            db.get_operation("aged-restart", &id.connection_id).unwrap(),
            Some(admission)
        );
    }
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn completion_ledger_terminal_capacity_prunes_only_expired_private_receipts() {
    let directory =
        std::env::temp_dir().join(format!("deppy-completion-cap-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("synthetic.sqlite");
    let db = History::open(&path).unwrap();
    let mut first = None;
    for index in 0..MAX_COMPLETION_RECEIPTS {
        let id = identity(&format!("retained-{index}"));
        db.claim_completion_scoped("send_text", &json!({"text":"synthetic"}), &id)
            .unwrap();
        db.finish_completion(&id, &receipt(id.clone())).unwrap();
        first.get_or_insert(id);
    }
    let rejected = identity("terminal-capacity");
    assert!(
        db.claim_completion_scoped("send_text", &json!({"text":"synthetic"}), &rejected)
            .is_err()
    );
    assert!(
        db.get_operation("terminal-capacity", &rejected.connection_id)
            .unwrap()
            .is_none()
    );
    let first = first.unwrap();
    {
        let fixture = rusqlite::Connection::open(&path).unwrap();
        fixture
            .execute(
                "UPDATE local_completion_receipts SET expires=?1 WHERE operation_key=?2",
                rusqlite::params![
                    i64::try_from(now() - 1).unwrap(),
                    operation_key(&first.connection_id, &first.scope.operation_id).unwrap()
                ],
            )
            .unwrap();
    }
    assert_eq!(
        db.claim_completion_scoped("send_text", &json!({"text":"synthetic"}), &rejected)
            .unwrap(),
        Claim::New
    );
    assert!(db.get_completion(&first).unwrap().is_none());
    assert_eq!(
        db.get_operation(&first.scope.operation_id, &first.connection_id)
            .unwrap()
            .unwrap()["status"],
        "queued",
        "retention cannot erase at-most-once input tombstones"
    );
    drop(db);
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}
