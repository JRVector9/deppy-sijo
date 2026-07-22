//! PR-16 Tool Permission & Audit (설계문서 11.7, 7장, PR-16).
//! permission policy / schema hash(sha256) / redacted audit log를 제공한다.
//! approval dialog UI와 앱 마이그레이션 통합은 crates/app 소관 —
//! 이 crate는 정책·해시·감사 로직 + dialog가 쓸 model 타입 + DDL 상수만 둔다.

mod crypto;
mod log;
mod policy;
#[cfg(feature = "test-support")]
mod test_support;

pub use crypto::{decrypt_input, encrypt_input};
pub use log::{
    AuditLifecycle, AuditOperation, AuditRecord, AuthorizationOutcome,
    MAX_AUTHORIZATION_INPUT_BYTES, MAX_SANITIZED_PREVIEW_CHARS, audit_lifecycle,
    complete_authorization_operation, mask_sensitive_keys, prepare_owned_authorization_preflight,
    record_audit, sanitized_input_preview, validate_tool_input,
};
pub use policy::{
    AUTHORIZATION_SESSION_ID_BYTES_MAX, AUTHORIZATION_WORKSPACE_ID_BYTES_MAX, ApprovalDecision,
    ApprovalReason, AuthorizationEvaluation, AuthorizationGrant, AuthorizationPlan,
    AuthorizationPreflight, AuthorizationSubject, AuthorizedCall, DeniedAuthorization,
    PendingAuthorization, PermissionFingerprint, PermissionPolicy, PermissionRule,
    PolicyEvaluation, ToolApprovalRequest, ToolDecision, evaluate_authorization,
    evaluate_authorization_with_fingerprint, evaluate_permission,
};
#[cfg(feature = "test-support")]
pub use test_support::{InMemoryAuthorizationLedger, TestAuthorizationCounters};

use sha2::{Digest, Sha256};

/// tool_audit_logs DDL (설계문서 11.7/11.8). 오케스트레이터가 앱 마이그레이션에 붙인다.
/// input_json 평문 컬럼은 두지 않는다 — redacted / encrypted(선택)만 허용 (11.7).
pub const MIGRATION_SQL: &str = "
CREATE TABLE tool_audit_logs (
    id TEXT PRIMARY KEY,
    workspace_id TEXT,
    session_id TEXT,
    server_id TEXT,
    tool_name TEXT NOT NULL,
    input_redacted_json TEXT,
    input_encrypted_blob BLOB,
    decision TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_tool_audit_workspace_id ON tool_audit_logs(workspace_id);
";

/// Tool call의 durable 실행 수명주기. 기존 감사 행은 완료된 legacy 기록이므로
/// `succeeded`로 backfill한다. `operation_id`의 partial unique index가 Prepared/Unknown
/// operation을 같은 ID로 다시 준비하는 것을 막아 전송 여부 불명 호출의 자동 재시도를
/// 저장 계층에서도 차단한다.
pub const MIGRATION_AUDIT_LIFECYCLE: &str = "
ALTER TABLE tool_audit_logs ADD COLUMN operation_id TEXT;
ALTER TABLE tool_audit_logs ADD COLUMN lifecycle TEXT NOT NULL DEFAULT 'succeeded'
    CHECK (lifecycle IN ('prepared', 'succeeded', 'failed', 'unknown', 'denied'));
ALTER TABLE tool_audit_logs ADD COLUMN outcome_error_code TEXT;
ALTER TABLE tool_audit_logs ADD COLUMN completed_at TEXT;
CREATE UNIQUE INDEX idx_tool_audit_operation_id
    ON tool_audit_logs(operation_id) WHERE operation_id IS NOT NULL;
CREATE INDEX idx_tool_audit_lifecycle ON tool_audit_logs(lifecycle);
";

/// AU01 owner-scoped durable recovery. A run is safe only while its caller holds the matching
/// exclusive OS lifetime lock; storage owns that lock and run transaction.
pub const MIGRATION_AUTHORIZATION_OWNERS: &str = "
ALTER TABLE tool_audit_logs ADD COLUMN authorization_scope TEXT;
ALTER TABLE tool_audit_logs ADD COLUMN authorization_run_id TEXT;
CREATE INDEX idx_tool_audit_authorization_owner
    ON tool_audit_logs(authorization_scope, authorization_run_id, lifecycle);
";

pub fn authorization_scope_lock_key(scope: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        !scope.trim().is_empty(),
        "authorization scope가 비어 있습니다"
    );
    anyhow::ensure!(scope.len() <= 512, "authorization scope가 너무 깁니다");
    let digest = Sha256::digest(scope.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// mcp_tools.schema_hash (설계문서 11.0) — tool input schema의 sha256 hex(64자).
/// 도구 스키마 변경 감지 → 재승인 트리거(PR-22 "MCP tool 변경 시 재승인")에 쓴다.
/// JSON을 정규화(object 키 정렬·공백 제거·문자열 이스케이프 통일)한 뒤 해시하므로
/// 키 순서/공백/유니코드 이스케이프 표기가 달라도 같은 스키마면 같은 해시다.
/// JSON이 아닌 입력은 원문 그대로 해시한다 — 정규화 결과는 항상 valid JSON이므로
/// invalid 입력과 해시 입력이 겹치지 않는다 (결정성만 보장하면 충분).
///
/// 알려진 한계(codex 리뷰): 숫자 표현(1 vs 1.0)과 문자열 유니코드 NFC/NFD 차이는
/// 다른 해시가 된다 — "다른 해시 → 재승인" 방향이라 보안상 안전한 쪽.
/// 중복 키는 serde_json 파싱 규칙(마지막 값 승리)으로 붕괴되어
/// {"x":1,"x":2}와 {"x":2}가 같은 해시가 될 수 있다 — 스키마에 중복 키가 실리는 것
/// 자체가 비정상 입력이고, 소비자도 같은 파서를 쓰므로 실질 동작 차이는 없다.
pub fn schema_hash(input_schema_json: &str) -> String {
    let canonical = match serde_json::from_str::<serde_json::Value>(input_schema_json) {
        Ok(value) => {
            let mut out = String::with_capacity(input_schema_json.len());
            write_canonical(&value, &mut out);
            out
        }
        Err(_) => input_schema_json.to_owned(),
    };
    let digest = Sha256::digest(canonical.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Value를 정규 형태로 직렬화한다 — object 키 오름차순, 구분자 외 공백 없음.
/// serde_json의 preserve_order feature 활성 여부와 무관하게 결정적이도록
/// 키를 직접 정렬한다. 문자열/숫자는 serde_json 직렬화 규칙을 그대로 쓴다
/// (같은 파싱 결과 → 같은 출력).
fn write_canonical(value: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                // 키 이스케이프도 serde_json 직렬화로 통일
                out.push_str(&Value::String((*key).clone()).to_string());
                out.push(':');
                write_canonical(&map[key.as_str()], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        // Null / Bool / Number / String은 serde_json 직렬화가 이미 결정적이다
        other => out.push_str(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 같은_스키마는_키순서_공백_무관하게_같은_해시() {
        let a =
            r#"{"type":"object","properties":{"path":{"type":"string"},"n":{"type":"integer"}}}"#;
        let b = "{ \"properties\" : { \"n\" : { \"type\" : \"integer\" },\n  \"path\": {\"type\": \"string\"} }, \"type\": \"object\" }";
        assert_eq!(schema_hash(a), schema_hash(b));
    }

    #[test]
    fn 유니코드_이스케이프_표기_차이도_같은_해시() {
        // "café"를 raw UTF-8로 쓴 것과 \u 이스케이프로 쓴 것은 같은 스키마다
        let a = r#"{"description":"café"}"#;
        let b = "{\"description\":\"caf\\u00e9\"}";
        assert_eq!(schema_hash(a), schema_hash(b));
    }

    #[test]
    fn 다른_스키마는_다른_해시() {
        let a = r#"{"type":"object","properties":{"path":{"type":"string"}}}"#;
        let b = r#"{"type":"object","properties":{"path":{"type":"number"}}}"#;
        assert_ne!(schema_hash(a), schema_hash(b));
    }

    #[test]
    fn 해시는_sha256_hex_64자() {
        let hash = schema_hash(r#"{"type":"object"}"#);
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn json이_아니면_원문_해시로_결정성_유지() {
        assert_eq!(schema_hash("not-json"), schema_hash("not-json"));
        assert_ne!(schema_hash("not-json"), schema_hash("not-json-2"));
    }

    #[test]
    fn 중첩_배열과_null_bool_숫자_정규화() {
        let a = r#"{"enum":[null,true,1,1.5,"x"],"b":{"z":1,"a":2}}"#;
        let b = r#"{ "b": { "a": 2, "z": 1 }, "enum": [ null, true, 1, 1.5, "x" ] }"#;
        assert_eq!(schema_hash(a), schema_hash(b));
        // 배열 순서는 의미가 있으므로 다르면 다른 해시
        let c = r#"{"enum":[true,null,1,1.5,"x"],"b":{"z":1,"a":2}}"#;
        assert_ne!(schema_hash(a), schema_hash(c));
    }
}
