//! DB 기반 PermissionHook — tools/call마다 (server_id, tool)의 현재 규칙을 DB에서
//! 새로 읽어 판단하고, 규칙이 Ask(미설정)면 pending_approvals로 GUI에 라이브 승인을
//! 요청한 뒤 결과를 폴링한다. 모든 결정은 audit 로그에 기록한다.
//!
//! fail-closed 원칙: 정책 조회 실패·승인 행 소실·대기 시간 초과는 전부 **거부**로 처리한다.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use audit::{AuditRecord, PermissionRule, ToolDecision};
use mcp::{PermissionHook, ProxyDecision};
use secret::{KeyringSecretStore, RedactionService, SecretStore};
use serde_json::Value;
use storage::{ApprovalStatus, Db};

/// 승인 미리보기 최대 길이 (문자 수). 긴 인자가 GUI/DB를 압박하지 않도록 자른다.
const PREVIEW_MAX_CHARS: usize = 500;

pub struct DbPermissionHook {
    db: Db,
    server_id: String,
    /// 프리뷰/감사 redaction — 시작 시 DB credential로 시드된다.
    redaction: RedactionService,
    /// keyring store가 초기화됐는지 — false면 audit encryptor를 넘기지 않는다 (blob NULL).
    keyring_ok: bool,
    store: KeyringSecretStore,
    poll_interval: Duration,
    approval_timeout: Duration,
}

impl DbPermissionHook {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Db,
        server_id: String,
        redaction: RedactionService,
        keyring_ok: bool,
        poll_interval: Duration,
        approval_timeout: Duration,
    ) -> Self {
        Self {
            db,
            server_id,
            redaction,
            keyring_ok,
            store: KeyringSecretStore,
            poll_interval,
            approval_timeout,
        }
    }

    /// (server_id, tool)의 현재 규칙을 DB에서 새로 읽는다. 행이 없거나 알 수 없는 문자열이면
    /// 기본값 Ask. 조회 실패는 Err(호출측이 fail-closed 거부).
    fn current_rule(&self, tool_name: &str) -> anyhow::Result<PermissionRule> {
        let rules = self.db.list_permission_rules()?;
        Ok(rules
            .iter()
            .find(|r| r.server_id == self.server_id && r.tool_name == tool_name)
            .and_then(|r| PermissionRule::from_persisted(&r.rule))
            .unwrap_or_default())
    }

    /// 해당 tool의 저장된 input schema에서 schema_hash를 계산한다.
    /// tool 행/스키마가 없으면 None (pending은 schema_hash 없이 등록된다).
    fn schema_hash_for(&self, tool_name: &str) -> Option<String> {
        let tools = self.db.list_mcp_tools(&self.server_id).ok()?;
        let schema = tools
            .into_iter()
            .find(|t| t.name == tool_name)?
            .input_schema_json?;
        Some(audit::schema_hash(&schema))
    }

    /// 감사 로그 한 건 기록. input_json 원문은 record_audit이 내부에서 redact/암호화한다.
    fn record(&self, tool_name: &str, arguments: &Value, decision: ToolDecision) {
        let input_json = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_owned());
        let encryptor: Option<&dyn SecretStore> = if self.keyring_ok {
            Some(&self.store)
        } else {
            None
        };
        let record = AuditRecord {
            workspace_id: None,
            session_id: None,
            server_id: Some(&self.server_id),
            tool_name,
            input_json: &input_json,
            decision,
        };
        if let Err(e) = self
            .db
            .record_tool_audit(&record, &self.redaction, encryptor)
        {
            tracing::warn!(tool = %tool_name, "audit 기록 실패: {e:#}");
        }
    }

    /// 인자 JSON을 redact + 길이 제한해 GUI 표시용 미리보기 문자열을 만든다.
    /// 시드된 credential secret은 [REDACTED]로 치환된다. (한계: 미등록 secret은
    /// RedactionService가 잡지 못하므로 절단에만 의존한다 — 원문 secret 노출 방지는
    /// 등록된 값 기준. 자세한 한계는 크레이트 보고 참조.)
    fn redact_preview(&self, arguments: &Value) -> String {
        let raw = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_owned());
        let mut redactor = self.redaction.stream_redactor();
        let mut bytes = redactor.redact_chunk(raw.as_bytes());
        bytes.extend(redactor.flush());
        let text = String::from_utf8_lossy(&bytes);
        truncate_chars(&text, PREVIEW_MAX_CHARS)
    }

    /// Ask 규칙: pending 등록 → 폴링 → Allowed/Denied/타임아웃. remember면 규칙 영속.
    fn ask(&self, tool_name: &str, arguments: &Value) -> ProxyDecision {
        let schema_hash = self.schema_hash_for(tool_name);
        let preview = self.redact_preview(arguments);
        let id = uuid::Uuid::new_v4().to_string();
        let now = unix_secs();

        if let Err(e) = self.db.insert_pending_approval(
            &id,
            &self.server_id,
            tool_name,
            &preview,
            schema_hash.as_deref(),
            now,
        ) {
            tracing::warn!(tool = %tool_name, "승인 요청 등록 실패: {e:#}");
            return ProxyDecision::Deny("승인 요청 등록 실패 — 안전을 위해 거부됨".to_owned());
        }

        let deadline = Instant::now() + self.approval_timeout;
        loop {
            std::thread::sleep(self.poll_interval);
            match self.db.poll_approval(&id) {
                // 행 소실(정리/삭제)·기형 status → fail-closed 거부
                Err(e) => {
                    tracing::warn!(tool = %tool_name, "승인 폴링 실패: {e:#}");
                    return ProxyDecision::Deny(
                        "승인 상태 조회 실패 — 안전을 위해 거부됨".to_owned(),
                    );
                }
                Ok(outcome) => match outcome.status {
                    ApprovalStatus::Pending => {
                        if Instant::now() >= deadline {
                            self.record(tool_name, arguments, ToolDecision::DenyOnce);
                            return ProxyDecision::Deny(
                                "승인 대기 시간 초과 — 안전을 위해 거부됨".to_owned(),
                            );
                        }
                    }
                    ApprovalStatus::Allowed => {
                        let decision = if outcome.remember {
                            // "항상 허용": 규칙을 Allow로 영속(현재 schema_hash 바인딩)
                            if let Err(e) = self.db.upsert_permission_rule(
                                &self.server_id,
                                tool_name,
                                PermissionRule::Allow.as_str(),
                                schema_hash.as_deref(),
                            ) {
                                tracing::warn!(tool = %tool_name, "Allow 규칙 저장 실패: {e:#}");
                            }
                            ToolDecision::AllowAlways
                        } else {
                            ToolDecision::AllowOnce
                        };
                        self.record(tool_name, arguments, decision);
                        return ProxyDecision::Allow;
                    }
                    ApprovalStatus::Denied => {
                        let decision = if outcome.remember {
                            if let Err(e) = self.db.upsert_permission_rule(
                                &self.server_id,
                                tool_name,
                                PermissionRule::Deny.as_str(),
                                None,
                            ) {
                                tracing::warn!(tool = %tool_name, "Deny 규칙 저장 실패: {e:#}");
                            }
                            ToolDecision::DenyAlways
                        } else {
                            ToolDecision::DenyOnce
                        };
                        self.record(tool_name, arguments, decision);
                        return ProxyDecision::Deny("승인 거부됨".to_owned());
                    }
                },
            }
        }
    }
}

impl PermissionHook for DbPermissionHook {
    fn check(&self, tool_name: &str, arguments: &Value) -> ProxyDecision {
        let rule = match self.current_rule(tool_name) {
            Ok(rule) => rule,
            Err(e) => {
                // 정책을 읽지 못하면 판단 불가 — fail-closed 거부 (감사도 DB 의존이라 생략)
                tracing::warn!(tool = %tool_name, "정책 조회 실패: {e:#}");
                return ProxyDecision::Deny("정책 조회 실패 — 안전을 위해 거부됨".to_owned());
            }
        };
        match rule {
            PermissionRule::Allow => {
                self.record(tool_name, arguments, ToolDecision::PolicyAllow);
                ProxyDecision::Allow
            }
            PermissionRule::Deny => {
                self.record(tool_name, arguments, ToolDecision::PolicyDeny);
                ProxyDecision::Deny("정책상 거부됨".to_owned())
            }
            PermissionRule::Ask => self.ask(tool_name, arguments),
        }
    }
}

/// 현재 unix epoch seconds (SystemTime). 정상 시스템 시계에서 UNIX_EPOCH 이후이므로 0으로 폴백.
fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 문자(char) 기준으로 자르고, 잘렸으면 말줄임표를 붙인다 (바이트 경계 안전).
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// 파일 기반 임시 Db (open_in_memory는 storage 비공개라 파일로 연다).
    /// WAL이라 별도 프로세스/스레드가 같은 파일을 동시에 열 수 있다.
    fn temp_db_path() -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("deppy-proxy-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("metadata.sqlite3")
    }

    fn hook_with(path: &Path, poll: Duration, timeout: Duration) -> DbPermissionHook {
        DbPermissionHook::new(
            Db::open(path).unwrap(),
            "srv-1".to_owned(),
            RedactionService::new(),
            false, // keyring 없음 — encryptor None
            poll,
            timeout,
        )
    }

    /// 같은 DB 파일에서 tool_audit_logs 행 수를 raw 연결로 센다 (storage 조회 API 부재).
    fn audit_count(path: &Path) -> i64 {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row("SELECT count(*) FROM tool_audit_logs", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn allow_규칙은_allow이고_audit_기록() {
        let path = temp_db_path();
        let hook = hook_with(&path, Duration::from_millis(20), Duration::from_secs(5));
        hook.db
            .upsert_permission_rule("srv-1", "read_file", "allow", None)
            .unwrap();

        let decision = hook.check("read_file", &serde_json::json!({"path": "/tmp/x"}));
        assert!(matches!(decision, ProxyDecision::Allow));
        assert_eq!(audit_count(&path), 1);
    }

    #[test]
    fn deny_규칙은_deny이고_audit_기록() {
        let path = temp_db_path();
        let hook = hook_with(&path, Duration::from_millis(20), Duration::from_secs(5));
        hook.db
            .upsert_permission_rule("srv-1", "delete_file", "deny", None)
            .unwrap();

        let decision = hook.check("delete_file", &serde_json::json!({}));
        assert!(matches!(decision, ProxyDecision::Deny(_)));
        assert_eq!(audit_count(&path), 1);
    }

    #[test]
    fn ask_승인_remember는_allow하고_규칙_영속() {
        let path = temp_db_path();
        let hook = hook_with(&path, Duration::from_millis(20), Duration::from_secs(5));

        // 백그라운드에서 GUI 역할: pending이 뜨면 allow+remember로 해소한다.
        let bg_path = path.clone();
        let resolver = std::thread::spawn(move || {
            let db = Db::open(&bg_path).unwrap();
            for _ in 0..500 {
                if let Some(p) = db.list_pending_approvals().unwrap().first() {
                    db.resolve_approval(&p.id, true, true, unix_secs()).unwrap();
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("pending 승인 요청이 등장하지 않음");
        });

        let decision = hook.check("write_file", &serde_json::json!({"path": "/tmp/y"}));
        resolver.join().unwrap();

        assert!(matches!(decision, ProxyDecision::Allow));
        // remember=true → Allow 규칙이 영속됐다
        let rules = hook.db.list_permission_rules().unwrap();
        let rule = rules
            .iter()
            .find(|r| r.tool_name == "write_file")
            .expect("영속된 규칙 없음");
        assert_eq!(rule.rule, "allow");
        // 승인 결과가 audit에도 남는다
        assert_eq!(audit_count(&path), 1);
    }

    #[test]
    fn ask_타임아웃은_deny() {
        let path = temp_db_path();
        // 아무도 해소하지 않는다 → 짧은 timeout 후 거부
        let hook = hook_with(&path, Duration::from_millis(20), Duration::from_millis(150));

        let started = Instant::now();
        let decision = hook.check("read_file", &serde_json::json!({"path": "/tmp/x"}));
        assert!(matches!(decision, ProxyDecision::Deny(_)));
        assert!(started.elapsed() < Duration::from_secs(5)); // 무한 대기 금지
        // 타임아웃도 거부 감사로 기록된다
        assert_eq!(audit_count(&path), 1);
    }

    #[test]
    fn ask_거부_remember는_deny_규칙_영속() {
        let path = temp_db_path();
        let hook = hook_with(&path, Duration::from_millis(20), Duration::from_secs(5));

        let bg_path = path.clone();
        let resolver = std::thread::spawn(move || {
            let db = Db::open(&bg_path).unwrap();
            for _ in 0..500 {
                if let Some(p) = db.list_pending_approvals().unwrap().first() {
                    db.resolve_approval(&p.id, false, true, unix_secs())
                        .unwrap();
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("pending 승인 요청이 등장하지 않음");
        });

        let decision = hook.check("delete_file", &serde_json::json!({}));
        resolver.join().unwrap();

        assert!(matches!(decision, ProxyDecision::Deny(_)));
        let rules = hook.db.list_permission_rules().unwrap();
        let rule = rules.iter().find(|r| r.tool_name == "delete_file").unwrap();
        assert_eq!(rule.rule, "deny");
    }

    #[test]
    fn 프리뷰는_등록된_secret을_redact하고_절단한다() {
        let path = temp_db_path();
        let hook = hook_with(&path, Duration::from_millis(20), Duration::from_secs(5));
        // 시드된 credential처럼 secret을 등록
        hook.redaction.register(&secret::SecretString::new(
            "sk-super-secret-123456".to_owned(),
        ));

        let preview = hook.redact_preview(&serde_json::json!({"token": "sk-super-secret-123456"}));
        assert!(!preview.contains("sk-super-secret-123456"), "{preview}");
        assert!(preview.contains("[REDACTED]"), "{preview}");

        // 절단: 긴 값은 PREVIEW_MAX_CHARS + 말줄임표로 제한된다
        let long = "a".repeat(PREVIEW_MAX_CHARS + 100);
        let preview = hook.redact_preview(&serde_json::json!({ "v": long }));
        assert!(preview.chars().count() <= PREVIEW_MAX_CHARS + 1);
        assert!(preview.ends_with('…'));
    }
}
