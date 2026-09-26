use super::*;

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
    assert_eq!(result["result"]["tools"].as_array().unwrap().len(), 5);
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
