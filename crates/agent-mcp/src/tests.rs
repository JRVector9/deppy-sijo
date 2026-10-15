use super::*;

fn completion_http(
    server: &Server,
    token: &str,
    method: &str,
    extra: &str,
    body: Option<Value>,
) -> (u16, String, String) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let body = body.map(|body| body.to_string()).unwrap_or_default();
    write!(stream, "{method} /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n{body}", server.addr, body.len()).unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        assert!(head.len() < 8192, "response head exceeded fixture bound");
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let length = completion_header(&head, "content-length")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert!(length <= 4 * 1024 * 1024);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    (status, head, String::from_utf8(body).unwrap())
}

fn completion_header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

fn completion_initialize(server: &Server, token: &str) -> String {
    let (status, head, body) = completion_http(
        server,
        token,
        "POST",
        "",
        Some(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"synthetic-completion-client","version":"1"}}})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["id"], 1);
    completion_header(&head, "mcp-session-id")
        .expect("initialize must issue an authenticated host session for original-stream ownership")
        .to_owned()
}

#[test]
fn completion_transport_initialize_separates_hosts_sharing_one_credential() {
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user();
    let first = completion_initialize(&server, &token);
    let second = completion_initialize(&server, &token);
    assert_ne!(first, second, "a credential is not a transport session");
    for session in [first, second] {
        assert!(session.len() >= 32 && session.len() <= 128);
        assert!(session.bytes().all(|byte| (0x21..=0x7e).contains(&byte)));
    }
}

#[test]
fn completion_transport_foreign_actor_cannot_reuse_initialized_host_session() {
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user();
    let session = completion_initialize(&server, &token);
    let (_, other_token) = server
        .auth
        .mint_manual_connection("Another synthetic actor", true, now())
        .unwrap();
    let body = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
    let header = format!("MCP-Session-Id: {session}\r\n");
    assert_eq!(
        completion_http(&server, &other_token, "POST", &header, Some(body.clone())).0,
        404,
        "host session ownership must be checked before dispatch"
    );
    let (status, _, response) = completion_http(&server, &token, "POST", &header, Some(body));
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap()["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        14,
        "ordinary JSON discovery retains all existing tools"
    );
    assert!(server.requests.try_recv().is_err());
}

#[test]
fn completion_transport_unknown_original_stream_cursor_is_not_a_subscription() {
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user();
    let session = completion_initialize(&server, &token);
    let headers = format!(
        "MCP-Session-Id: {session}\r\nLast-Event-ID: 09c446dc-7080-4895-baf1-6d2d81fc4604:0\r\n"
    );
    assert_eq!(
        completion_http(&server, &token, "GET", &headers, None).0,
        404,
        "unknown or foreign stream cursors cannot subscribe to completion results"
    );
    assert_eq!(
        completion_http(
            &server,
            &token,
            "GET",
            &format!("MCP-Session-Id: {session}\r\n"),
            None
        )
        .0,
        405,
        "there is no unsolicited completion broadcast"
    );
}

#[test]
fn pr9_explicit_paste_is_discovered_with_no_implicit_submit() {
    let discovery = tools();
    let paste = discovery["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "paste_text")
        .expect("explicit bounded multiline paste must be discoverable");
    assert_eq!(
        paste["inputSchema"]["properties"]["submit"]["default"],
        false
    );
    assert_eq!(paste["inputSchema"]["additionalProperties"], false);
}

#[test]
fn pr9_paste_utf8_controls_and_legacy_limits_remain_explicit() {
    assert!(validate_paste_text("한글 😀\r\nnext\tline\nlast", false).is_ok());
    assert!(validate_paste_text(&"a".repeat(MAX_PASTE), false).is_ok());
    assert!(validate_paste_text(&"😀".repeat(MAX_PASTE / 4 + 1), false).is_err());
    for text in [
        "",
        "\x1b[201~\rmalicious",
        "\x00",
        "\x03",
        "\x7f",
        "\u{85}",
        "one\rtwo",
    ] {
        assert!(validate_paste_text(text, false).is_err(), "{text:?}");
    }
    assert!(validate_paste_text("", true).is_err());
    assert!(encode_input(&"a".repeat(MAX_TEXT), false).is_ok());
    assert!(encode_input(&"a".repeat(MAX_TEXT + 1), false).is_err());
    for text in ["a\nb", "a\r\nb", "a\tb", "\x1b", "\x03"] {
        assert!(encode_input(text, false).is_err());
    }
    assert_eq!(encode_input("", true).unwrap(), b"\r");
}

#[test]
fn pr9_actual_http_dispatch_preserves_paste_args_and_envelope_budget() {
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user().to_string();
    let addr = server.addr;
    let args = json!({"session_id":"original", "generation":"g", "operation_id":"paste-wire", "text":"한글 😀\r\nsecond\tline", "submit":true});
    let expected = args.clone();
    let client = std::thread::spawn(move || {
        let body = json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"paste_text", "arguments":args}});
        let (status, response) = http_call_addr(addr, &token, body);
        (status, response)
    });
    let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let req = loop {
        if let Ok(req) = server.requests.try_recv() {
            break req;
        }
        assert!(
            std::time::Instant::now() < end,
            "paste_text was not dispatched by MCP parser"
        );
        std::thread::yield_now();
    };
    assert_eq!(req.tool, "paste_text");
    assert_eq!(req.args, expected);
    req.reply
        .send(Ok(json!({"status":"queued", "completion":"not_confirmed"})))
        .unwrap();
    assert_eq!(client.join().unwrap().0, 200);
    let large = json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":"paste_text", "arguments":{"text":"\\".repeat(MAX_PASTE)}}});
    assert!(large.to_string().len() > 64 * 1024);
    let (status, _) = http_call_addr(server.addr, &server.auth.token_for_user(), large);
    assert_eq!(status, 413);
    assert!(server.requests.try_recv().is_err());
}

fn http_call_addr(addr: std::net::SocketAddr, token: &str, body: Value) -> (u16, String) {
    use std::io::Write;
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let body = body.to_string();
    write!(stream, "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
    // This size check needs only the header; unread-body delivery is tested separately.
    if body.len() <= 64 * 1024 {
        stream.write_all(body.as_bytes()).unwrap();
    }
    framed_http_response(&mut stream).unwrap()
}

#[test]
fn plain_text_cannot_execute_without_submit() {
    for text in ["ls\npwd", "ls\rpwd", "\u{1b}[200~evil", "\u{3}", "\u{7}"] {
        assert!(encode_input(text, false).is_err());
    }
    assert_eq!(
        encode_input("한글 test", false).unwrap(),
        "한글 test".as_bytes()
    );
    assert_eq!(encode_input("pwd", true).unwrap(), b"pwd\r");
}

#[test]
fn credentials_expire_and_rotate_without_reusing_authority() {
    let auth = Auth::new_at(100);
    let token = auth.token_for_user();
    let token = token.as_str();
    let old = auth.authenticate(&format!("Bearer {token}"), 101).unwrap();
    assert!(
        auth.authenticate(&format!("Bearer {token}"), 100 + TOKEN_TTL)
            .is_none()
    );
    auth.rotate_at(200);
    assert!(!auth.current(old, 201));
    assert!(auth.authenticate(&format!("Bearer {token}"), 201).is_none());
}

#[test]
fn credential_revocation_waits_for_queue_admission_and_blocks_later_writes() {
    use std::{
        sync::{Arc, mpsc},
        time::Duration,
    };
    let auth = Arc::new(Auth::new_at(now()));
    let epoch = auth
        .authenticate(&format!("Bearer {}", auth.token_for_user().as_str()), now())
        .unwrap();
    let (entered, entered_rx) = mpsc::sync_channel(1);
    let (finish, finish_rx) = mpsc::sync_channel(1);
    let writer_auth = auth.clone();
    let writer = std::thread::spawn(move || {
        writer_auth.admit_current_access(epoch, None, &mut || {
            entered.send(()).unwrap();
            finish_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        })
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let (started, started_rx) = mpsc::sync_channel(1);
    let (revoked, revoked_rx) = mpsc::sync_channel(1);
    let revoking_auth = auth.clone();
    let revoker = std::thread::spawn(move || {
        started.send(()).unwrap();
        revoking_auth.revoke();
        revoked.send(()).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        revoked_rx.recv_timeout(Duration::from_millis(30)).is_err(),
        "revoke must not return during an admitted write"
    );
    finish.send(()).unwrap();
    writer.join().unwrap();
    revoker.join().unwrap();
    revoked_rx.recv().unwrap();
    auth.admit_current_access(epoch, None, &mut || {
        panic!("revoked credential admitted a later write")
    });
}

#[test]
fn claimed_operations_survive_restart_and_cannot_be_reexecuted() {
    let db = History::open_memory().unwrap();
    let args = serde_json::json!({"session_id":"a", "text":"pwd"});
    assert_eq!(
        db.claim("op1", "send_text", &args, "w", "a").unwrap(),
        Claim::New
    );
    assert_eq!(
        db.claim("op1", "send_text", &args, "w", "a").unwrap(),
        Claim::Existing(serde_json::json!({"status":"unknown", "retry":false}))
    );
    assert!(
        db.claim(
            "op1",
            "send_text",
            &serde_json::json!({"text":"other"}),
            "w",
            "a"
        )
        .is_err()
    );
}

fn http_call(server: &Server, token: &str, extra: &str, body: serde_json::Value) -> (u16, String) {
    use std::io::Write;
    let mut stream = std::net::TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let body = body.to_string();
    write!(stream,"POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}\r\n{}",server.addr,token,body.len(),extra,body).unwrap();
    framed_http_response(&mut stream).unwrap()
}

fn framed_http_response(reader: &mut impl std::io::Read) -> std::io::Result<(u16, String)> {
    use std::io::{Error, ErrorKind};
    let invalid = || Error::new(ErrorKind::InvalidData, "invalid_http_response");
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() == 8192 {
            return Err(invalid());
        }
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        head.push(byte[0]);
    }
    let head = std::str::from_utf8(&head).map_err(|_| invalid())?;
    let status = head
        .split_whitespace()
        .nth(1)
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    let len: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    if len > 4 * 1024 * 1024 {
        return Err(invalid());
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    Ok((status, String::from_utf8(body).map_err(|_| invalid())?))
}

fn raw_http_post(
    server: &Server,
    token: &str,
    extra: &str,
    declared: usize,
    body: &[u8],
) -> std::net::TcpStream {
    use std::io::Write;
    let mut stream = std::net::TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut wire = format!("POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {declared}\r\n{extra}\r\n", server.addr).into_bytes();
    wire.extend_from_slice(body);
    stream.write_all(&wire).unwrap();
    stream
}

#[test]
fn early_http_rejections_deliver_framed_errors_with_unread_large_body() {
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user();
    let body = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"},"padding":"x".repeat(60_000)}).to_string();
    assert!(body.len() < 64 * 1024);
    for (credential, extra, status) in [
        ("bad", "", 401),
        (token.as_str(), "Origin: https://evil.example\r\n", 403),
        (token.as_str(), "Authorization: Bearer other\r\n", 401),
        (token.as_str(), "MCP-Protocol-Version: invalid\r\n", 400),
        (token.as_str(), "", 200),
    ] {
        let mut stream = raw_http_post(&server, credential, extra, body.len(), body.as_bytes());
        let response = framed_http_response(&mut stream);
        assert!(response.is_ok(), "status={status}, response={response:?}");
        assert_eq!(response.unwrap().0, status);
    }
    assert!(server.requests.try_recv().is_err());
}

#[test]
fn busy_http_rejection_delivers_complete_error_with_unread_large_body() {
    use std::io::{Read, Write};
    let server = Server::start(0, "", || {}).unwrap();
    let mut stalled = Vec::new();
    for _ in 0..4 {
        let mut stream = std::net::TcpStream::connect(server.addr).unwrap();
        stream.write_all(b"POST /mcp HTTP/1.1\r\n").unwrap();
        stalled.push(stream);
    }
    let body = vec![b'x'; 60_000];
    let mut stream = raw_http_post(&server, "bad", "", body.len(), &body);
    let mut response = String::new();
    let result = stream.read_to_string(&mut response);
    assert!(result.is_ok(), "read={result:?}, response={response}");
    let (status, body) = framed_http_response(&mut response.as_bytes()).unwrap();
    assert_eq!(status, 503);
    assert!(!body.is_empty());
    drop(stalled);
}

#[test]
fn rejected_http_body_cleanup_never_waits_for_declared_length_or_client_eof() {
    let server = Server::start(0, "", || {}).unwrap();
    let mut rejected = Vec::new();
    for _ in 0..4 {
        let mut stream = raw_http_post(&server, "bad", "", usize::MAX, b"");
        assert_eq!(framed_http_response(&mut stream).unwrap().0, 401);
        rejected.push(stream);
    }
    let token = server.auth.token_for_user();
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}).to_string();
    let started = std::time::Instant::now();
    loop {
        let mut stream = raw_http_post(&server, &token, "", init.len(), init.as_bytes());
        let status = framed_http_response(&mut stream).unwrap().0;
        if status == 200 {
            break;
        }
        assert_eq!(status, 503);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        std::thread::yield_now();
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    drop(rejected);
}

#[test]
fn framed_http_fixture_rejects_truncated_or_oversized_responses() {
    for response in [
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nabc",
        "HTTP/1.1 200 OK\r\nContent-Length: 4194305\r\n\r\n",
        "HTTP/1.1 200 OK\r\n\r\n",
    ] {
        assert!(framed_http_response(&mut response.as_bytes()).is_err());
    }
    assert_eq!(
        framed_http_response(
            &mut b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabcignored".as_slice()
        )
        .unwrap(),
        (200, "abc".to_owned())
    );
}

#[test]
fn actual_http_auth_origin_initialize_and_answer_tool_discovery() {
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user();
    let init = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}});
    assert_eq!(http_call(&server, "bad", "", init.clone()).0, 401);
    assert_eq!(
        http_call(
            &server,
            &token,
            "Origin: https://evil.example\r\n",
            init.clone()
        )
        .0,
        403
    );
    assert_eq!(
        http_call(
            &server,
            &token,
            "Authorization: Bearer other\r\n",
            init.clone()
        )
        .0,
        401
    );
    assert_eq!(
        http_call(
            &server,
            &token,
            "MCP-Protocol-Version: invalid\r\n",
            init.clone()
        )
        .0,
        400
    );
    let (status, body) = http_call(&server, &token, "", init);
    assert_eq!(status, 200);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["result"]["protocolVersion"], "2025-11-25");
    let (_, body) = http_call(
        &server,
        &token,
        "",
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["result"]["tools"].as_array().unwrap().len(), 14);
    assert!(
        result["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "notify")
    );
    server.auth.revoke();
    assert_eq!(
        http_call(
            &server,
            &token,
            "",
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"ping"})
        )
        .0,
        401
    );
}

#[test]
fn cloud_management_tools_are_additive_and_run_linkage_is_optional() {
    let discovery = tools();
    let tools = discovery["tools"].as_array().unwrap();
    for name in [
        "start_run",
        "update_run",
        "get_run",
        "list_runs",
        "read_history",
        "get_operation",
        "acquire_control",
        "release_control",
    ] {
        assert!(
            tools.iter().any(|tool| tool["name"] == name),
            "missing {name}"
        );
    }
    for name in ["send_text", "paste_text", "send_ctrl_c", "notify"] {
        let schema = &tools.iter().find(|tool| tool["name"] == name).unwrap()["inputSchema"];
        assert_eq!(schema["properties"]["run_id"]["type"], "string");
        assert!(
            !schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item == "run_id")
        );
    }
}

#[test]
fn cloud_management_wire_auth_kind_and_history_schema_are_canonical() {
    assert_eq!(serde_json::to_value(AuthKind::OAuth).unwrap(), "oauth");
    assert_eq!(
        serde_json::from_value::<AuthKind>(json!("oauth")).unwrap(),
        AuthKind::OAuth
    );
    assert!(serde_json::from_value::<AuthKind>(json!("o_auth")).is_err());
    assert_eq!(
        serde_json::to_value(AuthKind::ManualBearer).unwrap(),
        "manual_bearer"
    );
    let discovery = tools();
    let tools = discovery["tools"].as_array().unwrap();
    let history = tools
        .iter()
        .find(|tool| tool["name"] == "read_history")
        .unwrap();
    assert_eq!(
        history["inputSchema"]["properties"]["max_bytes"]["minimum"],
        1024
    );
    assert_eq!(
        history["inputSchema"]["properties"]["max_bytes"]["maximum"],
        65536
    );
    let acquire = tools
        .iter()
        .find(|tool| tool["name"] == "acquire_control")
        .unwrap();
    assert_eq!(
        acquire["inputSchema"]["properties"]["ttl_seconds"]["maximum"],
        120
    );
    for name in ["get_run", "get_operation", "read_history", "list_runs"] {
        assert_eq!(
            tools.iter().find(|tool| tool["name"] == name).unwrap()["annotations"]["readOnlyHint"],
            true
        );
    }
}

#[test]
fn cloud_management_fixed_management_errors_reach_live_http_without_authority_changes() {
    let server = Server::start(0, "", || {}).unwrap();
    for (tool, code, retryable, effect) in [
        ("start_run", "invalid_run", false, "none"),
        ("update_run", "invalid_run_progress", false, "none"),
        ("get_run", "run_not_owned_or_session_changed", false, "none"),
        ("get_operation", "history_reply_mismatch", false, "unknown"),
        (
            "read_history",
            "history_query_timeout_no_effect",
            true,
            "none",
        ),
        ("update_run", "run_update_busy_no_effect", true, "none"),
        (
            "update_run",
            "run_save_unknown_query_before_retry",
            false,
            "unknown",
        ),
        ("update_run", "run_save_failed", false, "unknown"),
        ("update_run", "run_recovery_failed", false, "unknown"),
        (
            "start_run",
            "token_redaction_capacity_no_connection",
            false,
            "none",
        ),
        ("get_run", "cloud_snapshot_unavailable", false, "none"),
        ("start_run", "invalid_run_private_secret", false, "unknown"),
    ] {
        let addr = server.addr;
        let token = server.auth.token_for_user();
        let args = match tool {
            "start_run" => {
                json!({"session_id":"original","generation":"g","run_id":if code == "invalid_run" { "\0" } else { "run-a" },"title":"Analyze"})
            }
            "update_run" => {
                json!({"session_id":"original","generation":"g","run_id":"run-a","state":"running","progress":if code == "invalid_run_progress" { 101 } else { 50 }})
            }
            "get_run" => json!({"session_id":"original","generation":"g","run_id":"run-a"}),
            "get_operation" => {
                json!({"session_id":"original","generation":"g","operation_id":"op-a"})
            }
            "read_history" => {
                json!({"session_id":"original","generation":"g","cursor":0,"max_bytes":65536})
            }
            _ => unreachable!(),
        };
        let expected_args = args.clone();
        let caller = std::thread::spawn(move || {
            http_call_addr(
                addr,
                &token,
                json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":args}}),
            )
        });
        let request = server
            .requests
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert_eq!(request.principal.id, LEGACY_CONNECTION_ID);
        assert_eq!(request.tool, tool);
        assert_eq!(request.args, expected_args);
        request.reply.send(Err(code.into())).unwrap();
        let (status, response) = caller.join().unwrap();
        assert_eq!(status, 200);
        let envelope: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(envelope["result"]["isError"], true);
        let payload: Value =
            serde_json::from_str(envelope["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(
            payload["code"],
            if code == "invalid_run_private_secret" {
                "internal_error_outcome_unknown"
            } else {
                code
            }
        );
        assert_eq!(payload["retryable"], retryable, "{code}");
        assert_eq!(payload["effect_state"], effect, "{code}");
        if retryable {
            assert_eq!(payload["retry_after_ms"], 1000);
        }
    }
}

#[test]
fn cloud_management_optional_public_port_preserves_exact_host_and_oauth_resource() {
    use std::io::{Read, Write};
    for host in ["mac.example.ts.net", "mac.example.ts.net:8443"] {
        assert!(valid_public_host(host));
        let server = Server::start(0, host, || {}).unwrap();
        let get = |supplied_host: &str| {
            let mut stream = std::net::TcpStream::connect(server.addr).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            write!(stream, "GET /.well-known/oauth-protected-resource/mcp HTTP/1.1\r\nHost: {supplied_host}\r\n\r\n").unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        };
        let response = get(host);
        assert!(response.starts_with("HTTP/1.1 200"));
        let body: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["resource"], format!("https://{host}/mcp"));
        if host.contains(':') {
            assert!(get("mac.example.ts.net").starts_with("HTTP/1.1 403"));
        }
    }
    assert!(valid_public_host(""));
    for host in [
        "https://mac.example.ts.net",
        "mac.example.ts.net:0",
        "mac.example.ts.net:65536",
        "mac.example.ts.net:8443/mcp",
        "mac.example.ts.net:8443:8444",
        "mac.example.ts.net\n",
    ] {
        assert!(!valid_public_host(host));
        assert!(Server::start(0, host, || {}).is_err());
    }
}

#[test]
fn cloud_management_live_http_old_and_new_tool_dispatch_remains_additive() {
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user();
    let (status, response) = http_call(
        &server,
        &token,
        "MCP-Protocol-Version: 2025-03-26\r\n",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}),
    );
    assert_eq!(status, 200);
    let init: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    let (status, response) = http_call(
        &server,
        &token,
        "MCP-Protocol-Version: 2025-03-26\r\n",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    assert_eq!(status, 200);
    let discovery: Value = serde_json::from_str(&response).unwrap();
    let discovered = discovery["result"]["tools"].as_array().unwrap();
    assert_eq!(discovered.len(), 14);
    for name in ["send_text", "paste_text", "send_ctrl_c", "notify"] {
        let schema = &discovered.iter().find(|tool| tool["name"] == name).unwrap()["inputSchema"];
        assert!(schema["properties"].get("run_id").is_some());
        assert!(schema["properties"].get("connection_id").is_none());
        assert!(
            !schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("run_id"))
        );
    }
    assert_eq!(
        discovered
            .iter()
            .find(|tool| tool["name"] == "read_history")
            .unwrap()["inputSchema"]["properties"]["max_bytes"]["minimum"],
        1024
    );
    for (tool, args) in [
        (
            "send_text",
            json!({"session_id":"original","generation":"g","operation_id":"legacy-op","text":"hello","submit":false}),
        ),
        (
            "start_run",
            json!({"session_id":"original","generation":"g","run_id":"new-run","title":"Analyze"}),
        ),
    ] {
        let addr = server.addr;
        let token = server.auth.token_for_user();
        let expected = args.clone();
        let caller = std::thread::spawn(move || {
            http_call_addr(
                addr,
                &token,
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool,"arguments":args}}),
            )
        });
        let request = server
            .requests
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert_eq!(request.tool, tool);
        assert_eq!(request.args, expected);
        assert_eq!(request.principal.id, LEGACY_CONNECTION_ID);
        request.reply.send(Ok(json!({"status":"stored"}))).unwrap();
        let (status, body) = caller.join().unwrap();
        assert_eq!(status, 200);
        let result: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(result["result"]["isError"], false);
    }
}

#[test]
fn cloud_management_unknown_input_error_has_safe_retry_and_effect_fields() {
    let server = Server::start(0, "", || {}).unwrap();
    let addr = server.addr;
    let token = server.auth.token_for_user();
    let caller = std::thread::spawn(move || {
        http_call_addr(
            addr,
            &token,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"send_text","arguments":{}}}),
        )
    });
    let request = server
        .requests
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    request
        .reply
        .send(Err("outcome_unknown_do_not_retry_input".into()))
        .unwrap();
    let (status, body) = caller.join().unwrap();
    assert_eq!(status, 200);
    let envelope: Value = serde_json::from_str(&body).unwrap();
    let payload: Value =
        serde_json::from_str(envelope["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["retryable"], false);
    assert_eq!(payload["effect_state"], "unknown");
    assert_eq!(payload["error"], "outcome_unknown_do_not_retry_input");
}

#[test]
fn cloud_management_manual_identity_revocation_is_per_connection() {
    let time = now();
    let auth = Auth::new_at(time);
    let legacy = auth
        .authenticate_principal(&format!("Bearer {}", auth.token_for_user().as_str()), time)
        .unwrap();
    assert_eq!(legacy.id, LEGACY_CONNECTION_ID);
    let (a, token_a) = auth
        .mint_manual_connection("Connection A", true, time)
        .unwrap();
    let (b, token_b) = auth
        .mint_manual_connection("Connection B", false, time)
        .unwrap();
    assert_ne!(a.id, b.id);
    assert_ne!(a.id, legacy.id);
    assert_eq!(
        auth.authenticate_principal(&format!("Bearer {}", token_a.as_str()), time)
            .unwrap()
            .id,
        a.id
    );
    assert!(
        !auth
            .authenticate_principal(&format!("Bearer {}", token_b.as_str()), time)
            .unwrap()
            .input_scope
    );
    assert!(auth.rename_connection(&a.id, "Owner label"));
    assert_eq!(
        auth.authenticate_principal(&format!("Bearer {}", token_a.as_str()), time)
            .unwrap()
            .name,
        "Owner label"
    );
    assert!(!auth.rename_connection(&a.id, "spoof\nowner"));
    let epoch = auth
        .authenticate(&format!("Bearer {}", token_a.as_str()), time)
        .unwrap();
    let access_key = crate::oauth::hash(&token_a);
    assert!(auth.revoke_connection(&a.id));
    assert!(
        auth.authenticate_principal(&format!("Bearer {}", token_a.as_str()), time)
            .is_none()
    );
    assert!(
        auth.authenticate_principal(&format!("Bearer {}", token_b.as_str()), time)
            .is_some()
    );
    assert!(
        auth.authenticate_principal(&format!("Bearer {}", auth.token_for_user().as_str()), time)
            .is_some()
    );
    assert!(!auth.current_access(epoch, Some(&access_key), time));
    auth.admit_current_access(epoch, Some(&access_key), &mut || {
        panic!("revoked actor admitted queued effect")
    });
    let revoked = auth
        .connections_at(time)
        .into_iter()
        .find(|info| info.principal.id == a.id)
        .unwrap();
    assert_eq!(revoked.state, ConnectionState::Revoked);
    assert!(auth.revoke_connection(LEGACY_CONNECTION_ID));
    assert!(!auth.current_access(epoch, None, time));
    assert!(
        auth.authenticate_principal(&format!("Bearer {}", token_b.as_str()), time)
            .is_some()
    );
    let snapshot = serde_json::to_string(&auth.connections_at(time)).unwrap();
    assert!(!snapshot.contains(token_b.as_str()));
    assert_eq!(
        auth.connections_at(time + TOKEN_TTL)
            .into_iter()
            .find(|info| info.principal.id == b.id)
            .unwrap()
            .state,
        ConnectionState::Expired
    );
}

#[test]
fn cloud_management_poll_contention_does_not_block_another_connection() {
    let server = Server::start(0, "", || {}).unwrap();
    let time = now();
    let (_, token_a) = server
        .auth
        .mint_manual_connection("A", false, time)
        .unwrap();
    let (_, token_b) = server
        .auth
        .mint_manual_connection("B", false, time)
        .unwrap();
    let addr = server.addr;
    let a_copy = token_a.to_string();
    let a = std::thread::spawn(move || {
        http_call_addr(
            addr,
            &a_copy,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_runs","arguments":{}}}),
        )
    });
    let pending_a = server
        .requests
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    let (_, rejected) = http_call_addr(
        addr,
        &token_a,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_run","arguments":{}}}),
    );
    let envelope: Value = serde_json::from_str(&rejected).unwrap();
    let payload: Value =
        serde_json::from_str(envelope["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["error"], "poll_budget_no_effect");
    assert_eq!(payload["retryable"], true);
    assert_eq!(payload["effect_state"], "none");
    assert!(server.requests.try_recv().is_err());
    let b = std::thread::spawn(move || {
        http_call_addr(
            addr,
            &token_b,
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_sessions","arguments":{}}}),
        )
    });
    let pending_b = server
        .requests
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    assert_ne!(pending_a.principal.id, pending_b.principal.id);
    pending_b.reply.send(Ok(json!({"sessions":[]}))).unwrap();
    assert_eq!(b.join().unwrap().0, 200);
    pending_a.reply.send(Ok(json!({"runs":[]}))).unwrap();
    assert_eq!(a.join().unwrap().0, 200);
}

#[test]
fn cloud_management_private_diagnostics_do_not_leave_as_tool_errors() {
    let server = Server::start(0, "", || {}).unwrap();
    let addr = server.addr;
    let token = server.auth.token_for_user();
    let caller = std::thread::spawn(move || {
        http_call_addr(
            addr,
            &token,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_sessions","arguments":{}}}),
        )
    });
    let request = server
        .requests
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    request
        .reply
        .send(Err("secret=private-access-token /private/files".into()))
        .unwrap();
    let (_, response) = caller.join().unwrap();
    assert!(!response.contains("private-access-token"));
    assert!(!response.contains("/private/files"));
    assert!(response.contains("internal_error_outcome_unknown"));
    assert!(
        server
            .auth
            .connections()
            .iter()
            .all(|info| info.last_tool_success_at.is_none())
    );
}

#[test]
fn cloud_management_revoked_manual_credentials_do_not_exhaust_registry() {
    let time = now();
    let auth = Auth::new_at(time);
    for _ in 0..100 {
        let (principal, _) = auth
            .mint_manual_connection("Revocable connection", false, time)
            .expect("retired connections must leave room for new credentials");
        assert!(auth.revoke_connection(&principal.id));
        assert!(auth.connections_at(time).len() <= 65);
    }
}

#[test]
fn cloud_management_code_like_private_errors_and_structured_errors_are_safe() {
    let server = Server::start(0, "", || {}).unwrap();
    let cases = [
        Err("c0ffee0123456789".into()),
        Ok(
            json!({"status":"unknown","error":"secret=/private/answer","retryable":true,"effect_state":"none"}),
        ),
    ];
    for reply in cases {
        let addr = server.addr;
        let token = server.auth.token_for_user();
        let caller = std::thread::spawn(move || {
            http_call_addr(
                addr,
                &token,
                json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_sessions","arguments":{}}}),
            )
        });
        let request = server
            .requests
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        request.reply.send(reply).unwrap();
        let (_, response) = caller.join().unwrap();
        assert!(!response.contains("c0ffee0123456789"));
        assert!(!response.contains("/private/answer"));
        let envelope: Value = serde_json::from_str(&response).unwrap();
        let payload: Value =
            serde_json::from_str(envelope["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(payload["retryable"], false);
        assert_eq!(payload["effect_state"], "unknown");
    }
}

#[test]
fn cloud_management_http_activity_uses_credential_identity_only() {
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user();
    assert!(
        server
            .auth
            .connections()
            .iter()
            .all(|info| info.last_authenticated_at.is_none())
    );
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","clientInfo":{"name":"Owner impersonation","connection_id":"forged"}}});
    assert_eq!(http_call(&server, "invalid", "", init.clone()).0, 401);
    assert!(
        server
            .auth
            .connections()
            .iter()
            .all(|info| info.last_authenticated_at.is_none())
    );
    assert_eq!(http_call(&server, &token, "", init).0, 200);
    let info = server
        .auth
        .connections()
        .into_iter()
        .find(|info| info.principal.id == LEGACY_CONNECTION_ID)
        .unwrap();
    assert_ne!(info.principal.name, "Owner impersonation");
    assert!(info.last_authenticated_at.is_some());
    assert!(info.last_initialize_at.is_some());
    assert!(info.last_tool_success_at.is_none());
    let addr = server.addr;
    let caller = std::thread::spawn(move || {
        http_call_addr(
            addr,
            &token,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_sessions","arguments":{}}}),
        )
    });
    let request = server
        .requests
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    assert_eq!(request.principal.id, LEGACY_CONNECTION_ID);
    request.reply.send(Ok(json!({"sessions":[]}))).unwrap();
    assert_eq!(caller.join().unwrap().0, 200);
    assert!(
        server
            .auth
            .connections()
            .iter()
            .any(|info| info.last_tool_success_at.is_some())
    );
}

#[test]
fn queued_oauth_request_is_invalid_after_access_refresh() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    fn wire(
        addr: std::net::SocketAddr,
        method: &str,
        path: &str,
        kind: &str,
        body: &str,
        bearer: Option<&str>,
    ) -> (u16, String) {
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let headers=bearer.map(|token|format!("Authorization: Bearer {token}\r\nAccept: application/json, text/event-stream\r\n")).unwrap_or_default();
        write!(stream,"{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n{headers}\r\n{body}",body.len()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        (
            response.split_whitespace().nth(1).unwrap().parse().unwrap(),
            response,
        )
    }
    fn body(response: &str) -> Value {
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
    }
    let server = Server::start(0, "", || {}).unwrap();
    let addr = server.addr;
    let (_, r) = wire(
        addr,
        "POST",
        "/oauth/register",
        "application/json",
        r#"{"client_name":"Queue Bot","redirect_uris":["https://client.example/callback"]}"#,
        None,
    );
    let client = body(&r)["client_id"].as_str().unwrap().to_owned();
    let verifier = "v".repeat(43);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier));
    let resource = format!("http://{addr}/mcp");
    let q = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("client_id", client.as_str()),
            ("redirect_uri", "https://client.example/callback"),
            ("resource", resource.as_str()),
            ("response_type", "code"),
            ("code_challenge_method", "S256"),
            ("code_challenge", challenge.as_str()),
            ("scope", "deppy.read deppy.input"),
        ])
        .finish();
    assert_eq!(
        wire(addr, "GET", &format!("/oauth/authorize?{q}"), "", "", None).0,
        200
    );
    let approval = server.auth.approvals().pop().unwrap();
    assert_eq!(approval.client_name, "Queue Bot");
    assert!(server.auth.approve(&approval.id, true));
    let (_, r) = wire(
        addr,
        "GET",
        &format!("/oauth/authorize?request={}", approval.id),
        "",
        "",
        None,
    );
    let location = r
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
    let f = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("grant_type", "authorization_code"),
            ("client_id", client.as_str()),
            ("resource", resource.as_str()),
            ("redirect_uri", "https://client.example/callback"),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
        ])
        .finish();
    let (_, r) = wire(
        addr,
        "POST",
        "/oauth/token",
        "application/x-www-form-urlencoded",
        &f,
        None,
    );
    let tokens = body(&r);
    let old_access = tokens["access_token"].as_str().unwrap().to_owned();
    let token_for_request = old_access.clone();
    let http = std::thread::spawn(move || {
        wire(addr,"POST","/mcp","application/json",&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"send_text","arguments":{}}}).to_string(),Some(&token_for_request))
    });
    let pending = server
        .requests
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    assert!(pending.live(&server.auth));
    assert!(pending.input_scope);
    let connection_id = pending.principal.id.clone();
    assert_eq!(pending.principal.name, "Queue Bot");
    assert!(
        server
            .auth
            .rename_connection(&connection_id, "Owner queue label")
    );
    let access_expiry = now() + 3600;
    assert!(server.auth.current(pending.epoch, access_expiry));
    assert!(!server.auth.current_access(
        pending.epoch,
        pending.access_key.as_deref(),
        access_expiry
    ));
    let f = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("grant_type", "refresh_token"),
            ("client_id", client.as_str()),
            ("resource", resource.as_str()),
            ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
            ("scope", "deppy.read"),
        ])
        .finish();
    let refreshed = wire(
        addr,
        "POST",
        "/oauth/token",
        "application/x-www-form-urlencoded",
        &f,
        None,
    );
    assert_eq!(refreshed.0, 200);
    let refreshed_access = body(&refreshed.1)["access_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let refreshed_principal = server
        .auth
        .authenticate_principal(&format!("Bearer {refreshed_access}"), now())
        .unwrap();
    assert_eq!(refreshed_principal.id, connection_id);
    assert_eq!(refreshed_principal.name, "Owner queue label");
    assert_eq!(
        server
            .auth
            .connections()
            .iter()
            .find(|info| info.principal.id == connection_id)
            .unwrap()
            .principal
            .name,
        "Owner queue label"
    );
    assert!(!refreshed_principal.input_scope);
    assert!(
        server
            .auth
            .authenticate(&format!("Bearer {old_access}"), now())
            .is_none()
    );
    let live = pending.live(&server.auth);
    pending
        .reply
        .send(Err("expired_or_revoked_request_no_effect".into()))
        .unwrap();
    assert_eq!(http.join().unwrap().0, 200);
    assert!(
        !live,
        "a queued request must lose the replaced access token's authority"
    );
}

#[test]
fn persisted_answers_are_durable_and_exact_retries_return_receipt() {
    let path = std::env::temp_dir().join(format!("deppy-mcp-{}.db", uuid::Uuid::new_v4()));
    let args = serde_json::json!({"session_id":"a","message":"Grok 답변"});
    {
        let db = History::open(&path).unwrap();
        assert_eq!(
            db.claim("answer-1", "notify", &args, "workspace", "a")
                .unwrap(),
            Claim::New
        );
        db.finish(
            "answer-1",
            &serde_json::json!({"status":"stored"}),
            "Grok 답변",
        )
        .unwrap();
    }
    let db = History::open(&path).unwrap();
    assert_eq!(db.recent().unwrap()[0].message, "Grok 답변");
    assert_eq!(
        db.claim("answer-1", "notify", &args, "workspace", "a")
            .unwrap(),
        Claim::Existing(serde_json::json!({"status":"stored"}))
    );
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn notifications_and_responses_use_202_without_a_body() {
    let server = Server::start(0, "", || {}).unwrap();
    let token = server.auth.token_for_user();
    assert_eq!(
        http_call(
            &server,
            &token,
            "",
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/roots/list_changed"})
        ),
        (202, String::new())
    );
    assert_eq!(
        http_call(
            &server,
            &token,
            "",
            serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}})
        ),
        (202, String::new())
    );
}

#[test]
fn notified_answers_survive_input_audit_churn_and_expire_with_newer_answers() {
    let db = History::open_memory().unwrap();
    let args = serde_json::json!({});
    db.claim("answer-old", "notify", &args, "w", "s").unwrap();
    db.finish(
        "answer-old",
        &serde_json::json!({"status":"stored"}),
        "original answer",
    )
    .unwrap();
    for index in 0..500 {
        let id = format!("input-{index}");
        db.claim(&id, "send_text", &args, "w", "s").unwrap();
        db.finish(&id, &serde_json::json!({"status":"queued"}), "")
            .unwrap();
    }
    assert!(
        db.recent()
            .unwrap()
            .iter()
            .any(|r| r.id == "answer-old" && r.message == "original answer"),
        "input audit must not remove a notified answer"
    );
    for index in 0..100 {
        db.claim(&format!("unfinished-{index}"), "notify", &args, "w", "s")
            .unwrap();
    }
    assert!(
        db.recent()
            .unwrap()
            .iter()
            .any(|r| r.id == "answer-old" && r.message == "original answer"),
        "unfinished requests must not displace notified answers"
    );
    for index in 0..100 {
        let id = format!("answer-{index}");
        db.claim(&id, "notify", &args, "w", "s").unwrap();
        db.finish(&id, &serde_json::json!({"status":"stored"}), "new answer")
            .unwrap();
    }
    let records = db.recent().unwrap();
    assert!(!records.iter().any(|r| r.id == "answer-old"));
    assert_eq!(
        records
            .iter()
            .filter(|r| r.tool == "notify" && !r.message.is_empty())
            .count(),
        100
    );
    assert!(records.len() <= 600);
    assert!(matches!(
        db.claim("answer-old", "notify", &args, "w", "s").unwrap(),
        Claim::Existing(_)
    ));
}

#[cfg(unix)]
#[test]
fn history_database_symlinks_are_rejected() {
    let dir = std::env::temp_dir().join(format!("deppy-mcp-link-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("real.db");
    drop(History::open(&path).unwrap());
    let link = dir.join("link.db");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(History::open(&link).is_err());
    std::fs::remove_file(link).unwrap();
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

#[test]
fn answer_retention_uses_completion_order() {
    let db = History::open_memory().unwrap();
    let args = json!({});
    db.claim("late", "notify", &args, "w", "s").unwrap();
    for i in 0..100 {
        let id = format!("early-{i}");
        db.claim(&id, "notify", &args, "w", "s").unwrap();
        db.finish(&id, &json!({"status":"stored"}), "earlier")
            .unwrap();
    }
    db.finish("late", &json!({"status":"stored"}), "latest answer")
        .unwrap();
    let records = db.recent().unwrap();
    assert_eq!(records[0].id, "late");
    assert_eq!(
        records.iter().find(|r| r.id == "late").unwrap().message,
        "latest answer"
    );
    assert!(
        records
            .iter()
            .find(|r| r.id == "early-0")
            .unwrap()
            .message
            .is_empty()
    );
}

#[test]
fn oauth_http_discovery_and_challenge_are_available_without_a_token() {
    use std::io::{Read, Write};
    let s = Server::start(0, "", || {}).unwrap();
    let get = |path: &str| {
        let mut c = std::net::TcpStream::connect(s.addr).unwrap();
        write!(c, "GET {path} HTTP/1.1\r\nHost: {}\r\n\r\n", s.addr).unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        out
    };
    let metadata = get("/.well-known/oauth-protected-resource/mcp");
    assert!(metadata.starts_with("HTTP/1.1 200"), "{metadata}");
    let challenge = get("/mcp");
    assert!(challenge.starts_with("HTTP/1.1 401"));
    assert!(challenge.contains("WWW-Authenticate: Bearer resource_metadata="));
    let server_metadata = get("/.well-known/oauth-authorization-server");
    assert!(server_metadata.contains("code_challenge_methods_supported"));
}

#[test]
fn legacy_answer_database_migrates_and_prunes_on_open() {
    let path = std::env::temp_dir().join(format!("deppy-legacy-{}.db", uuid::Uuid::new_v4()));
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE operations(id TEXT PRIMARY KEY, fingerprint BLOB NOT NULL, tool TEXT NOT NULL, workspace TEXT NOT NULL, session TEXT NOT NULL, created INTEGER NOT NULL, outcome TEXT NOT NULL, message TEXT NOT NULL DEFAULT '');").unwrap();
        for i in 0..150 {
            db.execute(
                "INSERT INTO operations VALUES(?1,X'00','notify','w','s',0,'{}','legacy answer')",
                [format!("old-{i}")],
            )
            .unwrap();
        }
    }
    {
        let db = History::open(&path).unwrap();
        assert_eq!(
            db.recent()
                .unwrap()
                .iter()
                .filter(|r| !r.message.is_empty())
                .count(),
            100
        );
    }
    {
        let db = History::open(&path).unwrap();
        assert_eq!(
            db.recent()
                .unwrap()
                .iter()
                .filter(|r| !r.message.is_empty())
                .count(),
            100
        );
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn fragmented_http_request_waits_for_remaining_bytes() {
    use std::io::{Read, Write};
    let s = Server::start(0, "", || {}).unwrap();
    let mut c = std::net::TcpStream::connect(s.addr).unwrap();
    c.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    c.write_all(b"G").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let write = c.write_all(
        format!(
            "ET /.well-known/oauth-protected-resource/mcp HTTP/1.1\r\nHost: {}\r\n\r\n",
            s.addr
        )
        .as_bytes(),
    );
    let mut out = String::new();
    let read = c.read_to_string(&mut out);
    assert!(
        write.is_ok() && read.is_ok() && out.starts_with("HTTP/1.1 200"),
        "write={write:?}, read={read:?}, response={out}"
    );
}
