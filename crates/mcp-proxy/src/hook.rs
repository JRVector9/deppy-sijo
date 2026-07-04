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

    /// (server_id, tool)의 현재 규칙과 규칙에 바인딩된 승인 schema_hash를 DB에서 새로 읽는다.
    /// 행이 없거나 알 수 없는 문자열이면 기본값 (Ask, None). 조회 실패는 Err(fail-closed 거부).
    fn current_rule(&self, tool_name: &str) -> anyhow::Result<(PermissionRule, Option<String>)> {
        let rules = self.db.list_permission_rules()?;
        Ok(rules
            .iter()
            .find(|r| r.server_id == self.server_id && r.tool_name == tool_name)
            .map(|r| {
                (
                    PermissionRule::from_persisted(&r.rule).unwrap_or_default(),
                    r.approved_schema_hash.clone(),
                )
            })
            .unwrap_or((PermissionRule::default(), None)))
    }

    /// 해당 tool의 저장된 input schema에서 schema_hash를 계산한다.
    /// tool 행/스키마가 없으면 None (pending은 schema_hash 없이 등록된다).
    ///
    /// 한계(문서화): 여기서 쓰는 스키마는 GUI tool 탐색이 채우는 **mcp_tools 캐시**다.
    /// 백엔드가 캐시 갱신 없이 스키마를 바꾸면 이 해시가 낡아 SchemaChanged 재승인이
    /// 다음 GUI 재탐색까지 늦어질 수 있다. 캐시가 없으면(None) Allow 자동통과는 성립하지
    /// 않아(양쪽 Some 필요) fail-closed로 재승인한다. 매 호출 live 스키마 검증은 백엔드
    /// 서브프로세스 spawn 비용 때문에 별도 후속으로 둔다(프록시 시작 시 캐시 동기 갱신은
    /// initialize 블로킹·갱신 실패 시 fail-open 문제로 채택하지 않음, codex).
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
    /// 2단 방어: (1) 민감 key(api_key/token/password 등)의 값을 key 기반으로 마스킹해
    /// 미등록 secret이 pending_approvals.arguments_preview에 평문 저장되지 않게 하고(codex),
    /// (2) 시드된 credential secret은 RedactionService가 [REDACTED]로 치환한다. 마지막에 절단.
    fn redact_preview(&self, arguments: &Value) -> String {
        let mut masked = arguments.clone();
        audit::mask_sensitive_keys(&mut masked);
        let raw = serde_json::to_string(&masked).unwrap_or_else(|_| "{}".to_owned());
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
            // fail-closed도 결정이므로 감사에 남긴다 (best-effort, codex).
            self.record(tool_name, arguments, ToolDecision::DenyOnce);
            return ProxyDecision::Deny("승인 요청 등록 실패 — 안전을 위해 거부됨".to_owned());
        }

        let deadline = Instant::now() + self.approval_timeout;
        loop {
            // 항상 먼저 폴링해 '관측된 결정'(Allowed/Denied)을 존중한다 — sleep 중 도착한 승인을
            // 스케줄러 레이스로 버리지 않도록(codex). deadline은 결정 수락 컷오프가 아니라
            // Pending을 계속 기다리는 '대기 상한'이다: Pending인데 deadline을 넘겼을 때만 타임아웃.
            match self.db.poll_approval(&id) {
                // 행 소실(정리/삭제)·기형 status → fail-closed 거부
                Err(e) => {
                    tracing::warn!(tool = %tool_name, "승인 폴링 실패: {e:#}");
                    self.record(tool_name, arguments, ToolDecision::DenyOnce);
                    return ProxyDecision::Deny(
                        "승인 상태 조회 실패 — 안전을 위해 거부됨".to_owned(),
                    );
                }
                Ok(outcome) => match outcome.status {
                    ApprovalStatus::Pending => {
                        let now = Instant::now();
                        if now >= deadline {
                            // 타임아웃: 대기 행을 거부로 해소해 GUI가 만료 요청을 live 승인처럼
                            // 띄우지 않게 한다 (first-writer-wins라 이미 해소됐으면 no-op).
                            let _ = self.db.resolve_approval(&id, false, false, unix_secs());
                            self.record(tool_name, arguments, ToolDecision::DenyOnce);
                            return ProxyDecision::Deny(
                                "승인 대기 시간 초과 — 안전을 위해 거부됨".to_owned(),
                            );
                        }
                        // 남은 시간으로 clamp해 deadline을 크게 넘겨 자지 않는다
                        std::thread::sleep(
                            self.poll_interval
                                .min(deadline.saturating_duration_since(now)),
                        );
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
        let (rule, approved_hash) = match self.current_rule(tool_name) {
            Ok(rule) => rule,
            Err(e) => {
                // 정책을 읽지 못하면 판단 불가 — fail-closed 거부 (감사도 DB 의존이라 생략)
                tracing::warn!(tool = %tool_name, "정책 조회 실패: {e:#}");
                return ProxyDecision::Deny("정책 조회 실패 — 안전을 위해 거부됨".to_owned());
            }
        };
        match rule {
            PermissionRule::Allow => {
                // 저장된 Allow는 승인 당시 schema_hash에 바인딩돼 있다. 현재 tool 스키마의 해시가
                // 조회되고(Some) 그 값이 저장된 해시와 일치할 때만 자동 통과한다. 스키마를 못
                // 구하거나(None) 바뀌면(불일치) 재승인(Ask)한다 — None==None으로 무기한 통과하던
                // 문제 방지, SchemaChanged fail-closed 방어(codex).
                let current_hash = self.schema_hash_for(tool_name);
                if current_hash.is_some() && current_hash == approved_hash {
                    self.record(tool_name, arguments, ToolDecision::PolicyAllow);
                    ProxyDecision::Allow
                } else {
                    self.ask(tool_name, arguments)
                }
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

    /// tool 스키마 행을 seed하고 그 schema_hash를 반환한다 (Allow 자동통과 조건 충족용).
    fn seed_tool_schema(path: &Path, tool: &str, schema: &str) -> String {
        let mut db = Db::open(path).unwrap();
        // mcp_tools는 mcp_servers FK를 요구하므로 서버 행을 먼저 넣는다 (중복이면 무시)
        let _ = db.insert_mcp_server(&mcp::McpServerRow {
            id: "srv-1".to_owned(),
            name: "srv-1".to_owned(),
            kind: "stdio".to_owned(),
            command: Some("echo".to_owned()),
            args: Vec::new(),
            url: None,
            enabled: true,
        });
        db.replace_mcp_tools(
            "srv-1",
            &[mcp::McpToolRow {
                id: format!("id-{tool}"),
                server_id: "srv-1".to_owned(),
                name: tool.to_owned(),
                description: None,
                input_schema_json: Some(schema.to_owned()),
                trust_level: "unknown".to_owned(),
                schema_hash: None,
            }],
        )
        .unwrap();
        audit::schema_hash(schema)
    }

    #[test]
    fn allow_규칙은_allow이고_audit_기록() {
        let path = temp_db_path();
        let hook = hook_with(&path, Duration::from_millis(20), Duration::from_secs(5));
        // Allow 자동통과는 tool 스키마 존재 + 규칙이 그 schema_hash에 바인딩됐을 때만
        let hash = seed_tool_schema(&path, "read_file", r#"{"type":"object"}"#);
        hook.db
            .upsert_permission_rule("srv-1", "read_file", "allow", Some(&hash))
            .unwrap();

        let decision = hook.check("read_file", &serde_json::json!({"path": "/tmp/x"}));
        assert!(matches!(decision, ProxyDecision::Allow));
        assert_eq!(audit_count(&path), 1);
    }

    #[test]
    fn allow규칙_schema_hash_일치하면_통과_불일치면_재승인() {
        let path = temp_db_path();
        let hash = seed_tool_schema(&path, "read_file", r#"{"type":"object"}"#);
        // 저장된 해시가 현재 스키마와 일치 → 통과
        {
            let hook = hook_with(&path, Duration::from_millis(20), Duration::from_secs(5));
            hook.db
                .upsert_permission_rule("srv-1", "read_file", "allow", Some(&hash))
                .unwrap();
            assert!(matches!(
                hook.check("read_file", &serde_json::json!({})),
                ProxyDecision::Allow
            ));
        }
        // 저장된 해시가 옛것(불일치) → 자동통과 안 하고 재승인(짧은 timeout→Deny)
        {
            let hook = hook_with(&path, Duration::from_millis(20), Duration::from_millis(150));
            hook.db
                .upsert_permission_rule("srv-1", "read_file", "allow", Some("옛날해시"))
                .unwrap();
            assert!(matches!(
                hook.check("read_file", &serde_json::json!({})),
                ProxyDecision::Deny(_)
            ));
        }
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

    #[test]
    fn 프리뷰는_미등록_민감key값도_마스킹한다() {
        let path = temp_db_path();
        let hook = hook_with(&path, Duration::from_millis(20), Duration::from_secs(5));
        // 등록 안 된 secret이라도 key 이름(api_key)으로 마스킹돼야 한다
        let preview =
            hook.redact_preview(&serde_json::json!({"api_key": "unregistered-secret-xyz"}));
        assert!(!preview.contains("unregistered-secret-xyz"), "{preview}");
        assert!(preview.contains("[REDACTED]"), "{preview}");
    }

    #[test]
    fn allow규칙도_schema_hash_불일치면_자동통과_안한다() {
        let path = temp_db_path();
        // 승인 없이 짧게 타임아웃 → 재승인(ask) 경로로 빠지면 Deny로 관측된다
        let hook = hook_with(&path, Duration::from_millis(20), Duration::from_millis(150));
        // Allow 규칙이지만 옛 schema_hash에 바인딩. 현재 tool엔 스키마 행이 없어 hash=None →
        // 불일치이므로 자동 통과가 아니라 재승인해야 한다.
        hook.db
            .upsert_permission_rule("srv-1", "read_file", "allow", Some("옛날해시"))
            .unwrap();

        let decision = hook.check("read_file", &serde_json::json!({"path": "/tmp/x"}));
        assert!(
            matches!(decision, ProxyDecision::Deny(_)),
            "schema 불일치 Allow가 자동 통과됨"
        );
    }

    #[test]
    fn 타임아웃시_대기행이_거부로_해소된다() {
        let path = temp_db_path();
        let hook = hook_with(&path, Duration::from_millis(20), Duration::from_millis(150));

        let decision = hook.check("read_file", &serde_json::json!({"path": "/tmp/x"}));
        assert!(matches!(decision, ProxyDecision::Deny(_)));
        // 타임아웃 후 pending 행이 남아 GUI가 만료 요청을 띄우면 안 된다 → 해소됨
        assert!(
            hook.db.list_pending_approvals().unwrap().is_empty(),
            "타임아웃된 대기 행이 pending으로 남음"
        );
    }
}
