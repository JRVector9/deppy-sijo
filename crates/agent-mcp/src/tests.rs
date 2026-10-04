use super::*;

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
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let body = body.to_string();
    write!(stream, "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
    // An oversized declared body is refused before body reading; do not create a TCP reset by sending it.
    if body.len() <= 64 * 1024 {
        stream.write_all(body.as_bytes()).unwrap();
    }
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    (
        response.split_whitespace().nth(1).unwrap().parse().unwrap(),
        response.split_once("\r\n\r\n").unwrap().1.to_owned(),
    )
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
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let body = body.to_string();
    write!(stream,"POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}\r\n{}",server.addr,token,body.len(),extra,body).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response.split_whitespace().nth(1).unwrap().parse().unwrap();
    (
        status,
        response.split_once("\r\n\r\n").unwrap().1.to_owned(),
    )
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
    assert_eq!(result["result"]["tools"].as_array().unwrap().len(), 6);
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
        r#"{"redirect_uris":["https://client.example/callback"]}"#,
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
    assert_eq!(
        wire(
            addr,
            "POST",
            "/oauth/token",
            "application/x-www-form-urlencoded",
            &f,
            None
        )
        .0,
        200
    );
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
