//! Redacted tool audit log (설계문서 11.7). input_json 평문 저장 금지 —
//! RedactionService를 통과한 텍스트만 input_redacted_json에 넣는다.
//! input_encrypted_blob은 선택 기능(7장: AEAD + keyring key + key id) —
//! v0에서는 미구현으로 항상 NULL이고 컬럼 구조만 확보한다 (PR-16 완료 기준).

use anyhow::Context;
use rusqlite::Connection;
use secret::RedactionService;

use crate::ToolDecision;

/// 감사 로그 한 행의 입력 model. input_json은 평문으로 받되
/// record_audit 내부에서 redaction을 거친 뒤에만 DB에 닿는다.
#[derive(Clone)]
pub struct AuditRecord<'a> {
    pub workspace_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub server_id: Option<&'a str>,
    pub tool_name: &'a str,
    /// tool input JSON 평문 (redact 전)
    pub input_json: &'a str,
    pub decision: ToolDecision,
}

impl std::fmt::Debug for AuditRecord<'_> {
    /// input_json에 secret이 실릴 수 있으므로 Debug에서는 내용을 숨긴다 (7장 유출 방지)
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditRecord")
            .field("workspace_id", &self.workspace_id)
            .field("session_id", &self.session_id)
            .field("server_id", &self.server_id)
            .field("tool_name", &self.tool_name)
            .field("input_json", &"<elided>")
            .field("decision", &self.decision)
            .finish()
    }
}

/// RedactionService(등록 기반)만으로는 아직 등록되지 않은 secret이 남을 수 있어
/// key 이름 기반 마스킹을 이중 방어선으로 둔다 (7장 "MCP tool input secret", codex 리뷰 반영).
/// 매칭 전 key에서 `-`/`_` 구분자를 제거하고 소문자화하므로
/// x-api-key / apiKey / private_key 같은 표기 변형도 잡는다.
const SENSITIVE_KEY_PARTS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "apikey",
    "auth", // authorization / oauth 계열 포함. author 등 오탐은 유출보다 낫다
    "credential",
    "privatekey",
    "cookie",
    "session",
];

fn is_sensitive_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|c| *c != '-' && *c != '_')
        .collect::<String>()
        .to_lowercase();
    SENSITIVE_KEY_PARTS
        .iter()
        .any(|part| normalized.contains(part))
}

/// 민감 key의 값 전체(중첩 포함)를 "[REDACTED]" 문자열로 치환한다
fn mask_sensitive_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *val = serde_json::Value::String("[REDACTED]".into());
                } else {
                    mask_sensitive_keys(val);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                mask_sensitive_keys(item);
            }
        }
        _ => {}
    }
}

/// JSON으로 파싱되지 않는 input의 gap marker (7장 "redaction 불확실 시 보수적 폐기")
const INVALID_INPUT_MARKER: &str = "[INVALID_JSON_INPUT_OMITTED]";

/// 감사 로그 기록. 생성된 행 id를 돌려준다.
/// input은 두 단계 redaction을 거친다 — 1차 key 이름 기반 마스킹,
/// 2차 등록된 secret 패턴 stream redaction. secret이 [REDACTED]로 치환되면
/// valid JSON이 아닐 수 있으나 그대로 텍스트로 저장한다 (평문 보존보다 유출 차단 우선).
/// JSON으로 파싱되지 않는 input은 1차 마스킹이 불가능하므로 본문을 저장하지 않고
/// gap marker만 남긴다 — MCP tool input은 프로토콜상 항상 JSON이라 정상 경로가 아니다.
pub fn record_audit(
    conn: &Connection,
    redaction: &RedactionService,
    record: &AuditRecord<'_>,
) -> anyhow::Result<String> {
    // 1차: key 이름 기반 마스킹 — RedactionService에 아직 등록되지 않은 secret 대비
    let keyed = match serde_json::from_str::<serde_json::Value>(record.input_json) {
        Ok(mut value) => {
            mask_sensitive_keys(&mut value);
            value.to_string()
        }
        // 파싱 불가면 redaction 확실성이 없다 — 본문 폐기, marker만 (codex 리뷰 반영)
        Err(_) => INVALID_INPUT_MARKER.to_owned(),
    };
    // 2차: 등록된 secret 패턴(원본 + base64/URL/JSON-escape 변형) redaction.
    //      한 번에 들어온 입력이므로 redact_chunk + flush로 전체를 처리한다.
    let mut redactor = redaction.stream_redactor();
    let mut redacted = redactor.redact_chunk(keyed.as_bytes());
    redacted.extend(redactor.flush());
    let input_redacted = String::from_utf8_lossy(&redacted).into_owned();

    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO tool_audit_logs
           (id, workspace_id, session_id, server_id, tool_name,
            input_redacted_json, input_encrypted_blob, decision, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
            strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
        (
            &id,
            record.workspace_id,
            record.session_id,
            record.server_id,
            record.tool_name,
            &input_redacted,
            // encrypted blob은 선택(7장) — v0 미구현, 항상 NULL
            None::<Vec<u8>>,
            record.decision.as_str(),
        ),
    )
    .with_context(|| format!("tool audit log 저장 실패: {}", record.tool_name))?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use secret::SecretString;

    use super::*;
    use crate::MIGRATION_SQL;

    const SECRET: &str = "sk-abcdef123456";

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATION_SQL).unwrap();
        conn
    }

    fn service_with_secret() -> RedactionService {
        let service = RedactionService::new();
        service.register(&SecretString::new(SECRET.into()));
        service
    }

    fn record<'a>(input_json: &'a str) -> AuditRecord<'a> {
        AuditRecord {
            workspace_id: Some("ws-1"),
            session_id: Some("sess-1"),
            server_id: Some("srv-1"),
            tool_name: "http_request",
            input_json,
            decision: ToolDecision::AllowOnce,
        }
    }

    #[test]
    fn 평문_secret은_감사_로그에_남지_않는다() {
        let conn = test_conn();
        // key 이름이 민감하지 않아도(1차 마스킹 미적용) 등록된 secret은 2차에서 치환된다
        let input = format!(r#"{{"url":"https://api.example.com","note":"use {SECRET} here"}}"#);
        let id = record_audit(&conn, &service_with_secret(), &record(&input)).unwrap();

        let stored: String = conn
            .query_row(
                "SELECT input_redacted_json FROM tool_audit_logs WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!stored.contains(SECRET), "{stored}");
        assert!(stored.contains("[REDACTED]"), "{stored}");
        // secret 외 내용은 보존된다
        assert!(stored.contains("api.example.com"), "{stored}");
    }

    #[test]
    fn encrypted_blob은_v0에서_항상_null() {
        let conn = test_conn();
        let id = record_audit(
            &conn,
            &service_with_secret(),
            &record(r#"{"path":"/tmp/x"}"#),
        )
        .unwrap();
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT input_encrypted_blob FROM tool_audit_logs WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blob, None);
    }

    #[test]
    fn 컬럼_저장_roundtrip() {
        let conn = test_conn();
        let id = record_audit(
            &conn,
            &RedactionService::new(),
            &AuditRecord {
                workspace_id: Some("ws-1"),
                session_id: None,
                server_id: Some("srv-1"),
                tool_name: "read_file",
                input_json: r#"{"path":"/tmp/x"}"#,
                decision: ToolDecision::PolicyAllow,
            },
        )
        .unwrap();

        let (workspace_id, session_id, server_id, tool_name, decision, created_at): (
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT workspace_id, session_id, server_id, tool_name, decision, created_at
                 FROM tool_audit_logs WHERE id = ?1",
                [&id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(workspace_id.as_deref(), Some("ws-1"));
        assert_eq!(session_id, None);
        assert_eq!(server_id.as_deref(), Some("srv-1"));
        assert_eq!(tool_name, "read_file");
        assert_eq!(decision, "policy_allow"); // ToolDecision::as_str와 일치
        assert!(!created_at.is_empty());
    }

    #[test]
    fn json_escape된_secret_변형도_치환된다() {
        // RedactionService가 등록하는 변형 중 JSON-escape가 tool input에 실리는 경우
        let conn = test_conn();
        let service = RedactionService::new();
        let secret_with_quote = r#"pa"ss-word-123456"#;
        service.register(&SecretString::new(secret_with_quote.into()));
        // serde_json 직렬화를 거친 input — secret은 JSON-escape된 형태로 실린다.
        // key는 민감하지 않게 두어 2차(등록 패턴 변형) 경로를 검증한다.
        let input =
            serde_json::to_string(&serde_json::json!({ "note": secret_with_quote })).unwrap();
        let id = record_audit(&conn, &service, &record(&input)).unwrap();
        let stored: String = conn
            .query_row(
                "SELECT input_redacted_json FROM tool_audit_logs WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!stored.contains("ss-word-123456"), "{stored}");
        assert!(stored.contains("[REDACTED]"), "{stored}");
    }

    #[test]
    fn 등록되지_않은_secret도_민감_key면_마스킹된다() {
        // codex 리뷰 반영: RedactionService에 등록되지 않은 값도 key 이름 기반 1차 방어
        let conn = test_conn();
        let input = r#"{
            "url": "https://api.example.com",
            "Api_Key": "unregistered-key-xyz",
            "nested": { "refresh_token": "unregistered-token-abc" },
            "list": [ { "password": "unregistered-pw-999" } ]
        }"#;
        let id = record_audit(&conn, &RedactionService::new(), &record(input)).unwrap();
        let stored: String = conn
            .query_row(
                "SELECT input_redacted_json FROM tool_audit_logs WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!stored.contains("unregistered-key-xyz"), "{stored}");
        assert!(!stored.contains("unregistered-token-abc"), "{stored}");
        assert!(!stored.contains("unregistered-pw-999"), "{stored}");
        assert!(stored.contains("[REDACTED]"), "{stored}");
        // 민감하지 않은 필드는 보존 + 결과는 여전히 valid JSON
        assert!(stored.contains("api.example.com"), "{stored}");
        assert!(serde_json::from_str::<serde_json::Value>(&stored).is_ok());
    }

    #[test]
    fn 민감_key_표기_변형도_마스킹된다() {
        // codex 리뷰 반영: 구분자/대소문자 변형(x-api-key, privateKey, Cookie 등)
        let conn = test_conn();
        let input = r#"{
            "x-api-key": "variant-key-111",
            "privateKey": "variant-key-222",
            "Cookie": "variant-key-333",
            "session_id": "variant-key-444",
            "OAuth-Token": "variant-key-555"
        }"#;
        let id = record_audit(&conn, &RedactionService::new(), &record(input)).unwrap();
        let stored: String = conn
            .query_row(
                "SELECT input_redacted_json FROM tool_audit_logs WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        for leaked in [
            "variant-key-111",
            "variant-key-222",
            "variant-key-333",
            "variant-key-444",
            "variant-key-555",
        ] {
            assert!(!stored.contains(leaked), "{stored}");
        }
    }

    #[test]
    fn json이_아닌_input은_본문_대신_gap_marker만_저장() {
        // 7장 "redaction 불확실 시 보수적 폐기" — key 마스킹 불가 경로는 본문을 남기지 않는다
        let conn = test_conn();
        let id = record_audit(
            &conn,
            &RedactionService::new(),
            &record("password=unregistered-pw-777"),
        )
        .unwrap();
        let stored: String = conn
            .query_row(
                "SELECT input_redacted_json FROM tool_audit_logs WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, "[INVALID_JSON_INPUT_OMITTED]");
    }

    #[test]
    fn debug_출력에_input_평문이_없다() {
        let input = format!(r#"{{"token":"{SECRET}"}}"#);
        let debug = format!("{:?}", record(&input));
        assert!(!debug.contains(SECRET), "{debug}");
    }
}
