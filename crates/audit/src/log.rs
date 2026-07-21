//! Redacted tool audit log (설계문서 11.7). input_json 평문 저장 금지 —
//! RedactionService를 통과한 텍스트만 input_redacted_json에 넣는다.
//! input_encrypted_blob은 선택 기능(7장: AEAD + keyring key + key id) —
//! record_audit에 encryptor(SecretStore)를 넘기면 전체 원본 입력이 암호화 저장되고,
//! None이면 NULL이다. 암호화 로직은 crate::crypto.

use anyhow::Context;
use rusqlite::Connection;
use secret::RedactionService;

use crate::ToolDecision;

/// 외부 tool call 한 건의 durable 상태. `Unknown`은 Prepared 상태에서 프로세스가
/// 종료되어 전송 여부를 증명할 수 없는 경우이며, 다시 Prepared로 되돌릴 수 없다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditLifecycle {
    Prepared,
    Succeeded,
    Failed,
    Unknown,
    Denied,
}

impl AuditLifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
            Self::Denied => "denied",
        }
    }

    fn from_persisted(value: &str) -> anyhow::Result<Self> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "unknown" => Ok(Self::Unknown),
            "denied" => Ok(Self::Denied),
            _ => anyhow::bail!("알 수 없는 audit lifecycle: {value}"),
        }
    }
}

/// preflight commit 결과. `audit_id`는 내부 row 식별자, `operation_id`는 policy/call/
/// outcome을 잇는 호출자 제공 correlation key다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditOperation {
    pub audit_id: String,
    pub operation_id: String,
    pub lifecycle: AuditLifecycle,
}

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
    // §7 대상 보강 (codex 리뷰): 접속 문자열/bearer 계열
    "bearer",
    "databaseurl", // database_url — 키 정규화(구분자 제거)와 짝
    "dburl",
    "dsn",
    "connectionstring",
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

/// 민감 key의 값 전체(중첩 포함)를 "[REDACTED]" 문자열로 치환한다.
/// audit 로그뿐 아니라 proxy 승인 미리보기 등에서도 미등록 secret 평문 저장을 막기 위해
/// 공개한다.
pub fn mask_sensitive_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            // 헤더류 {"name":"Authorization","value":"Bearer …"} 패턴 — name/key/header의
            // 값이 민감하면 짝인 "value"를 마스킹한다 (미등록 secret 평문 저장 방지, codex 리뷰)
            let paired_sensitive = map.iter().any(|(k, v)| {
                matches!(k.to_ascii_lowercase().as_str(), "name" | "key" | "header")
                    && v.as_str().is_some_and(is_sensitive_key)
            });
            for (key, val) in map.iter_mut() {
                if is_sensitive_key(key) || (paired_sensitive && key.eq_ignore_ascii_case("value"))
                {
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
    // Some이면 전체(원본) 입력을 AEAD 암호화해 input_encrypted_blob에 저장한다 (7장,
    // 선택 기능). None이면 blob은 NULL. 암호화 실패는 감사 저장을 막지 않는다(로그만).
    encryptor: Option<&dyn secret::SecretStore>,
) -> anyhow::Result<String> {
    let (input_redacted, encrypted_blob) =
        prepare_payload(redaction, record.input_json, encryptor, false)?;

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
            // encrypted blob은 선택(7장) — encryptor 있으면 암호화된 원본, 없으면 NULL
            encrypted_blob,
            record.decision.as_str(),
        ),
    )
    .with_context(|| format!("tool audit log 저장 실패: {}", record.tool_name))?;
    Ok(id)
}

/// permission 검사가 끝난 tool call을 durable preflight로 기록한다. 허용 결정은
/// `Prepared`, 거부 결정은 `Denied`로 바로 종결한다. Invalid JSON은 행을 만들지 않으며,
/// 같은 operation id는 lifecycle과 무관하게 다시 준비할 수 없다.
pub fn prepare_audit_operation(
    conn: &Connection,
    operation_id: &str,
    redaction: &RedactionService,
    record: &AuditRecord<'_>,
    encryptor: Option<&dyn secret::SecretStore>,
) -> anyhow::Result<AuditOperation> {
    validate_operation_id(operation_id)?;
    let (input_redacted, encrypted_blob) =
        prepare_payload(redaction, record.input_json, encryptor, true)?;
    let lifecycle = if record.decision.is_allowed() {
        AuditLifecycle::Prepared
    } else {
        AuditLifecycle::Denied
    };
    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO tool_audit_logs
           (id, operation_id, workspace_id, session_id, server_id, tool_name,
            input_redacted_json, input_encrypted_blob, decision, lifecycle, created_at,
            completed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
            strftime('%Y-%m-%dT%H:%M:%fZ','now'),
            CASE WHEN ?10 = 'denied' THEN strftime('%Y-%m-%dT%H:%M:%fZ','now') END)",
        (
            &id,
            operation_id,
            record.workspace_id,
            record.session_id,
            record.server_id,
            record.tool_name,
            &input_redacted,
            encrypted_blob,
            record.decision.as_str(),
            lifecycle.as_str(),
        ),
    )
    .with_context(|| format!("tool audit preflight 저장 실패: {}", record.tool_name))?;
    Ok(AuditOperation {
        audit_id: id,
        operation_id: operation_id.to_owned(),
        lifecycle,
    })
}

/// Prepared call을 알려진 최종 상태로 한 번만 전이한다. Unknown/Denied/이미 완료된 행은
/// 갱신하지 않고 오류를 반환하므로 호출자가 무심코 같은 call 결과를 덮어쓰지 못한다.
pub fn complete_audit_operation(
    conn: &Connection,
    operation_id: &str,
    outcome: AuditLifecycle,
    error_code: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(outcome, AuditLifecycle::Succeeded | AuditLifecycle::Failed),
        "audit 완료 상태는 succeeded 또는 failed만 허용됩니다"
    );
    validate_error_code(outcome, error_code)?;
    let affected = conn
        .execute(
            "UPDATE tool_audit_logs
             SET lifecycle = ?2, outcome_error_code = ?3,
                 completed_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE operation_id = ?1 AND lifecycle = 'prepared'",
            (operation_id, outcome.as_str(), error_code),
        )
        .with_context(|| format!("tool audit outcome 저장 실패: {operation_id}"))?;
    anyhow::ensure!(
        affected == 1,
        "완료 가능한 prepared audit가 없음: {operation_id}"
    );
    Ok(())
}

/// 시작 시 남아 있는 Prepared는 전송 여부를 증명할 수 없으므로 Unknown으로 종결한다.
/// 이 상태는 `prepare_audit_operation`의 unique operation id guard 때문에 자동 retry되지 않는다.
pub fn reconcile_prepared_audits(conn: &Connection) -> anyhow::Result<usize> {
    let affected = conn
        .execute(
            "UPDATE tool_audit_logs
             SET lifecycle = 'unknown', outcome_error_code = 'process_interrupted',
                 completed_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE lifecycle = 'prepared'",
            [],
        )
        .context("prepared audit crash reconciliation 실패")?;
    Ok(affected)
}

pub fn audit_lifecycle(
    conn: &Connection,
    operation_id: &str,
) -> anyhow::Result<Option<AuditLifecycle>> {
    use rusqlite::OptionalExtension as _;

    let value: Option<String> = conn
        .query_row(
            "SELECT lifecycle FROM tool_audit_logs WHERE operation_id = ?1",
            [operation_id],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| AuditLifecycle::from_persisted(&value))
        .transpose()
}

fn prepare_payload(
    redaction: &RedactionService,
    input_json: &str,
    encryptor: Option<&dyn secret::SecretStore>,
    reject_invalid_json: bool,
) -> anyhow::Result<(String, Option<Vec<u8>>)> {
    let keyed = match serde_json::from_str::<serde_json::Value>(input_json) {
        Ok(mut value) => {
            mask_sensitive_keys(&mut value);
            value.to_string()
        }
        Err(error) if reject_invalid_json => {
            return Err(error).context("tool input JSON 검증 실패");
        }
        Err(_) => INVALID_INPUT_MARKER.to_owned(),
    };
    let mut redactor = redaction.stream_redactor();
    let mut redacted = redactor.redact_chunk(keyed.as_bytes());
    redacted.extend(redactor.flush());
    let input_redacted = String::from_utf8_lossy(&redacted).into_owned();
    let encrypted_blob =
        encryptor.and_then(|store| match crate::encrypt_input(store, input_json) {
            Ok(blob) => Some(blob),
            Err(error) => {
                tracing::warn!("audit 입력 암호화 실패 (blob NULL로 저장): {error:#}");
                None
            }
        });
    Ok((input_redacted, encrypted_blob))
}

fn validate_operation_id(operation_id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        operation_id == operation_id.trim(),
        "operation id 앞뒤 공백은 허용되지 않습니다"
    );
    anyhow::ensure!(!operation_id.is_empty(), "operation id가 비어 있습니다");
    anyhow::ensure!(operation_id.len() <= 128, "operation id가 너무 깁니다");
    anyhow::ensure!(
        operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')),
        "operation id 형식이 잘못됐습니다"
    );
    Ok(())
}

fn validate_error_code(outcome: AuditLifecycle, error_code: Option<&str>) -> anyhow::Result<()> {
    anyhow::ensure!(
        outcome != AuditLifecycle::Succeeded || error_code.is_none(),
        "succeeded audit에는 error code를 저장할 수 없습니다"
    );
    if let Some(code) = error_code {
        anyhow::ensure!(
            !code.is_empty() && code.len() <= 64,
            "audit error code 길이가 잘못됐습니다"
        );
        anyhow::ensure!(
            code.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            }),
            "audit error code 형식이 잘못됐습니다"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use secret::{SecretStore, SecretString};
    use std::sync::Mutex;

    /// 테스트용 인메모리 SecretStore (keyring 불필요).
    #[derive(Default)]
    struct MemStore(Mutex<std::collections::HashMap<String, String>>);

    impl SecretStore for MemStore {
        fn set_secret(&self, id: &str, secret: &SecretString) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(id.to_owned(), secret.expose().to_owned());
            Ok(())
        }
        fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
            self.0
                .lock()
                .unwrap()
                .get(id)
                .map(|v| SecretString::new(v.clone()))
                .ok_or_else(|| anyhow::anyhow!("없음: {id}"))
        }
        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }
        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            Ok(self.0.lock().unwrap().contains_key(id))
        }
    }

    use super::*;
    use crate::{MIGRATION_AUDIT_LIFECYCLE, MIGRATION_SQL};

    const SECRET: &str = "sk-abcdef123456";

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATION_SQL).unwrap();
        conn.execute_batch(MIGRATION_AUDIT_LIFECYCLE).unwrap();
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
    fn 헤더류_name_value_쌍의_secret이_마스킹된다() {
        let conn = test_conn();
        // {"name":"Authorization","value":"Bearer 미등록토큰"} — value 키는 민감어가 아니고
        // 값도 미등록이지만, 짝인 name이 민감하므로 value가 마스킹돼야 한다
        let input = r#"{"headers":[{"name":"Authorization","value":"Bearer unregistered-xyz"}]}"#;
        let id = record_audit(&conn, &RedactionService::new(), &record(input), None).unwrap();
        let stored: String = conn
            .query_row(
                "SELECT input_redacted_json FROM tool_audit_logs WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!stored.contains("unregistered-xyz"), "{stored}");
        assert!(stored.contains("[REDACTED]"), "{stored}");
    }

    #[test]
    fn 평문_secret은_감사_로그에_남지_않는다() {
        let conn = test_conn();
        // key 이름이 민감하지 않아도(1차 마스킹 미적용) 등록된 secret은 2차에서 치환된다
        let input = format!(r#"{{"url":"https://api.example.com","note":"use {SECRET} here"}}"#);
        let id = record_audit(&conn, &service_with_secret(), &record(&input), None).unwrap();

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
    fn encryptor_없으면_blob_null() {
        let conn = test_conn();
        let id = record_audit(
            &conn,
            &service_with_secret(),
            &record(r#"{"path":"/tmp/x"}"#),
            None,
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
    fn encryptor_있으면_원본이_암호화_저장되고_복호된다() {
        let conn = test_conn();
        let store = MemStore::default();
        let plain = r#"{"path":"/tmp/x","token":"sk-secret-xyz"}"#;
        let id = record_audit(
            &conn,
            &RedactionService::new(),
            &record(plain),
            Some(&store),
        )
        .unwrap();
        let blob: Vec<u8> = conn
            .query_row(
                "SELECT input_encrypted_blob FROM tool_audit_logs WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        // blob에 평문 secret이 없어야 하고, 복호 시 원본이 그대로 나와야 한다
        assert!(!String::from_utf8_lossy(&blob).contains("sk-secret-xyz"));
        assert_eq!(crate::decrypt_input(&store, &blob).unwrap(), plain);
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
            None,
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
        let id = record_audit(&conn, &service, &record(&input), None).unwrap();
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
        let id = record_audit(&conn, &RedactionService::new(), &record(input), None).unwrap();
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
        let id = record_audit(&conn, &RedactionService::new(), &record(input), None).unwrap();
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
            None,
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

    #[test]
    fn audit_operation은_prepared에서_한번만_완료된다() {
        let conn = test_conn();
        let operation = prepare_audit_operation(
            &conn,
            "op-allow-1",
            &RedactionService::new(),
            &record(r#"{"path":"/tmp/x"}"#),
            None,
        )
        .unwrap();
        assert_eq!(operation.lifecycle, AuditLifecycle::Prepared);
        assert_eq!(
            audit_lifecycle(&conn, "op-allow-1").unwrap(),
            Some(AuditLifecycle::Prepared)
        );

        complete_audit_operation(&conn, "op-allow-1", AuditLifecycle::Succeeded, None).unwrap();
        assert_eq!(
            audit_lifecycle(&conn, "op-allow-1").unwrap(),
            Some(AuditLifecycle::Succeeded)
        );
        assert!(
            complete_audit_operation(&conn, "op-allow-1", AuditLifecycle::Failed, Some("late"))
                .is_err()
        );
    }

    #[test]
    fn denied는_종결되고_invalid_json은_행을_만들지_않는다() {
        let conn = test_conn();
        let denied = AuditRecord {
            decision: ToolDecision::DenyOnce,
            ..record(r#"{"path":"/tmp/x"}"#)
        };
        let operation = prepare_audit_operation(
            &conn,
            "op-denied-1",
            &RedactionService::new(),
            &denied,
            None,
        )
        .unwrap();
        assert_eq!(operation.lifecycle, AuditLifecycle::Denied);

        assert!(
            prepare_audit_operation(
                &conn,
                "op-invalid-1",
                &RedactionService::new(),
                &record("not-json"),
                None,
            )
            .is_err()
        );
        assert_eq!(audit_lifecycle(&conn, "op-invalid-1").unwrap(), None);
    }

    #[test]
    fn crash_recovery는_prepared를_unknown으로_바꾸고_같은_id_retry를_막는다() {
        let conn = test_conn();
        prepare_audit_operation(
            &conn,
            "op-unknown-1",
            &RedactionService::new(),
            &record(r#"{"path":"/tmp/x"}"#),
            None,
        )
        .unwrap();

        assert_eq!(reconcile_prepared_audits(&conn).unwrap(), 1);
        assert_eq!(
            audit_lifecycle(&conn, "op-unknown-1").unwrap(),
            Some(AuditLifecycle::Unknown)
        );
        assert!(
            prepare_audit_operation(
                &conn,
                "op-unknown-1",
                &RedactionService::new(),
                &record(r#"{"path":"/tmp/x"}"#),
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn 실패_outcome에는_low_cardinality_error_code만_허용한다() {
        let conn = test_conn();
        prepare_audit_operation(
            &conn,
            "op-failed-1",
            &RedactionService::new(),
            &record(r#"{"path":"/tmp/x"}"#),
            None,
        )
        .unwrap();
        assert!(
            complete_audit_operation(
                &conn,
                "op-failed-1",
                AuditLifecycle::Failed,
                Some("raw error: token=secret"),
            )
            .is_err()
        );
        complete_audit_operation(
            &conn,
            "op-failed-1",
            AuditLifecycle::Failed,
            Some("transport_timeout"),
        )
        .unwrap();
    }

    #[test]
    fn lifecycle_migration은_legacy_audit을_succeeded로_보존한다() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATION_SQL).unwrap();
        let legacy_id = record_audit(
            &conn,
            &RedactionService::new(),
            &record(r#"{"path":"/tmp/legacy"}"#),
            None,
        )
        .unwrap();

        conn.execute_batch(MIGRATION_AUDIT_LIFECYCLE).unwrap();
        let (lifecycle, operation_id): (String, Option<String>) = conn
            .query_row(
                "SELECT lifecycle, operation_id FROM tool_audit_logs WHERE id = ?1",
                [&legacy_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(lifecycle, "succeeded");
        assert_eq!(operation_id, None);
    }
}
