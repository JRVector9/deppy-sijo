//! DB 기반 PermissionHook — tools/call마다 (server_id, tool)의 현재 규칙을 DB에서
//! 새로 읽어 판단하고, 규칙이 Ask(미설정)면 pending_approvals로 GUI에 라이브 승인을
//! 요청한 뒤 결과를 폴링한다. 모든 결정은 audit 로그에 기록한다.
//!
//! fail-closed 원칙: 정책 조회 실패·승인 행 소실·대기 시간 초과는 전부 **거부**로 처리한다.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use audit::{AuditRecord, PermissionRule, ToolDecision};
use deppy_core::time::unix_secs_i64;
use mcp::{LocalMcpManager, PermissionHook, ProxyDecision};
use secret::RedactionService;
use serde_json::Value;
use storage::{ApprovalStatus, Db};

use crate::forwarder::BackendConfig;

/// 승인 미리보기 최대 길이 (문자 수). 긴 인자가 GUI/DB를 압박하지 않도록 자른다.
const PREVIEW_MAX_CHARS: usize = 500;

pub struct DbPermissionHook {
    db: Db,
    server_id: String,
    /// 프리뷰/감사 redaction — 시작 시 DB credential로 시드된다.
    redaction: RedactionService,
    poll_interval: Duration,
    approval_timeout: Duration,
    /// live 스키마 검증용 백엔드 spec + manager (DB 캐시가 아니라 실제 백엔드에서 해시 계산).
    /// kind별 config(stdio|http)는 forwarder와 동일한 BackendConfig로 분기한다 (H3).
    manager: LocalMcpManager,
    config: BackendConfig,
    /// 이 프록시가 붙은 pane_id (I2 — env DEPPY_SESSION_ID). 승인 등록 시 그대로 싣는다.
    /// **조회하지 않는다**: 등록은 fail-closed 경로라 새 실패 지점을 만들지 않기 위함.
    /// None이면 "세션 불명"으로 등록되고 승인 자체는 정상 진행된다.
    pane_id: Option<String>,
    /// live 스키마 해시 캐시 (tool_name → schema_hash). 프록시 세션당 최초 필요 시 한 번만
    /// 백엔드를 discover해 채운다(성공 시). None = 아직 성공 discover 못 함(다음 호출에서 재시도).
    schema_cache: Mutex<Option<HashMap<String, String>>>,
}

impl DbPermissionHook {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Db,
        server_id: String,
        redaction: RedactionService,
        poll_interval: Duration,
        approval_timeout: Duration,
        manager: LocalMcpManager,
        config: BackendConfig,
        pane_id: Option<String>,
    ) -> Self {
        Self {
            db,
            server_id,
            redaction,
            poll_interval,
            approval_timeout,
            manager,
            config,
            pane_id,
            schema_cache: Mutex::new(None),
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

    /// 해당 tool의 **live 백엔드 스키마**에서 schema_hash를 계산한다 (DB 캐시가 아니라
    /// 실제 백엔드를 discover). 백엔드는 이 프록시 세션의 고정된 subprocess spec이므로,
    /// 최초 필요 시 한 번만 discover해 `HashMap<tool_name, schema_hash>`로 캐시하고
    /// 이후 호출은 캐시를 재사용한다 — 호출마다 subprocess를 spawn하는 비용을 피하면서도,
    /// 낡을 수 있는 DB 캐시(오래된 GUI 탐색 결과)에 의존하지 않는다.
    ///
    /// discover 실패 시 캐시하지 않고 None을 돌려준다(다음 호출에서 재시도). None이면
    /// Allow 자동통과 조건(양쪽 Some 필요)이 성립하지 않아 fail-closed로 재승인(Ask)한다 —
    /// 낡은 DB 캐시로 폴백하지 않는다.
    ///
    /// 잔여 한계: 세션 도중 백엔드 spec이 바뀌어 스키마가 달라지면(mid-session swap) 캐시가
    /// 갱신되지 않아 재검출되지 않는다. 프록시는 per-session 프로세스라 허용 가능하다 —
    /// 백엔드가 바뀌면 새 프록시 세션이 다시 discover한다.
    fn schema_hash_for(&self, tool_name: &str) -> Option<String> {
        let mut cache = self.schema_cache.lock().unwrap();
        if cache.is_none() {
            match self.config.discover_tools(&self.manager) {
                Ok(tools) => {
                    let map = tools
                        .into_iter()
                        .map(|t| (t.name, audit::schema_hash(&t.input_schema_json)))
                        .collect();
                    *cache = Some(map);
                }
                Err(e) => {
                    tracing::warn!(tool = %tool_name, "live 스키마 discover 실패 — 재승인 유도: {e:#}");
                    return None; // 캐시하지 않음 → 다음 호출에서 재시도
                }
            }
        }
        cache.as_ref().and_then(|m| m.get(tool_name).cloned())
    }

    /// 감사 로그 한 건 기록. 기본 경로는 redacted JSON만 저장하고 encrypted raw blob은 NULL이다.
    fn record(&self, tool_name: &str, arguments: &Value, decision: ToolDecision) {
        let input_json = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_owned());
        let record = AuditRecord {
            workspace_id: None,
            session_id: None,
            server_id: Some(&self.server_id),
            tool_name,
            input_json: &input_json,
            decision,
        };
        if let Err(e) = self.db.record_tool_audit(&record, &self.redaction, None) {
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
        let now = unix_secs_i64();

        if let Err(e) = self.db.insert_pending_approval(
            &id,
            &self.server_id,
            tool_name,
            &preview,
            schema_hash.as_deref(),
            now,
            self.pane_id.as_deref(),
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
                            let _ = self.db.resolve_approval(&id, false, false, unix_secs_i64());
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
        if !arguments.is_object() {
            tracing::warn!(tool = %tool_name, "tools/call arguments가 object가 아님 — audit/approval 전에 거부");
            return ProxyDecision::Deny("tools/call arguments는 JSON object여야 합니다".to_owned());
        }
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

    /// 존재하지 않는 command를 가리키는 config — live discover가 항상 실패한다
    /// (schema_hash_for → None). live 백엔드가 필요 없는 테스트의 기본값.
    fn bad_config() -> BackendConfig {
        BackendConfig::Stdio(mcp::McpServerConfig::stdio(
            "no-backend".to_owned(),
            "/nonexistent/deppy-proxy-test-cmd".to_owned(),
            Vec::new(),
            Vec::new(),
            true,
        ))
    }

    fn hook_with(path: &Path, poll: Duration, timeout: Duration) -> DbPermissionHook {
        hook_with_config(path, poll, timeout, bad_config())
    }

    fn hook_with_config(
        path: &Path,
        poll: Duration,
        timeout: Duration,
        config: BackendConfig,
    ) -> DbPermissionHook {
        DbPermissionHook::new(
            Db::open(path).unwrap(),
            "srv-1".to_owned(),
            RedactionService::new(),
            poll,
            timeout,
            LocalMcpManager::new(RedactionService::new()),
            config,
            Some("pane-test".to_owned()),
        )
    }

    /// 같은 DB 파일에서 tool_audit_logs 행 수를 raw 연결로 센다 (storage 조회 API 부재).
    fn audit_count(path: &Path) -> i64 {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row("SELECT count(*) FROM tool_audit_logs", [], |row| row.get(0))
            .unwrap()
    }

    fn audit_redacted_and_blob(path: &Path) -> (String, Option<Vec<u8>>) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row(
            "SELECT input_redacted_json, input_encrypted_blob
             FROM tool_audit_logs ORDER BY created_at, id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    }

    /// initialize → tools/list로 tool 하나(주어진 name/inputSchema)를 내놓는 mock stdio
    /// 백엔드 스크립트. manager.rs 목 서버와 동일한 핸드셰이크(2025-11-25 개정판).
    #[cfg(unix)]
    fn mock_backend_script(tool: &str, schema: &str) -> String {
        format!(
            "read -r _init\n\
             printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{{\"tools\":{{}}}},\"serverInfo\":{{\"name\":\"mock\",\"version\":\"0.1\"}}}}}}'\n\
             read -r _initialized\n\
             read -r _list\n\
             printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"tools\":[{{\"name\":\"{tool}\",\"inputSchema\":{schema}}}]}}}}'\n"
        )
    }

    /// 주어진 script를 `/bin/sh -c`로 실행하는 config.
    #[cfg(unix)]
    fn sh_config(script: String) -> BackendConfig {
        BackendConfig::Stdio(mcp::McpServerConfig::stdio(
            "mock".to_owned(),
            "/bin/sh".to_owned(),
            vec!["-c".to_owned(), script],
            Vec::new(),
            true,
        ))
    }

    #[cfg(unix)]
    #[test]
    fn allow_규칙은_live_hash_일치시_allow이고_audit_기록() {
        let path = temp_db_path();
        // live 백엔드가 read_file(inputSchema={"type":"object"})을 내놓는다
        let schema = r#"{"type":"object"}"#;
        let config = sh_config(mock_backend_script("read_file", schema));
        let hook = hook_with_config(
            &path,
            Duration::from_millis(20),
            Duration::from_secs(5),
            config,
        );
        // Allow 규칙을 live 스키마 해시에 바인딩 → 자동 통과 조건 충족
        let hash = audit::schema_hash(schema);
        hook.db
            .upsert_permission_rule("srv-1", "read_file", "allow", Some(&hash))
            .unwrap();

        let decision = hook.check("read_file", &serde_json::json!({"path": "/tmp/x"}));
        assert!(matches!(decision, ProxyDecision::Allow));
        assert_eq!(audit_count(&path), 1);
    }

    #[cfg(unix)]
    #[test]
    fn allow규칙_live_hash_일치하면_통과_불일치면_재승인() {
        let schema = r#"{"type":"object"}"#;
        let hash = audit::schema_hash(schema);
        // live 백엔드 해시가 저장 해시와 일치 → 통과
        {
            let path = temp_db_path();
            let config = sh_config(mock_backend_script("read_file", schema));
            let hook = hook_with_config(
                &path,
                Duration::from_millis(20),
                Duration::from_secs(5),
                config,
            );
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
            let path = temp_db_path();
            let config = sh_config(mock_backend_script("read_file", schema));
            let hook = hook_with_config(
                &path,
                Duration::from_millis(20),
                Duration::from_millis(150),
                config,
            );
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
    fn 기본_proxy_audit는_redacted_only_blob_null() {
        let path = temp_db_path();
        let hook = hook_with(&path, Duration::from_millis(20), Duration::from_secs(5));
        hook.db
            .upsert_permission_rule("srv-1", "delete_file", "deny", None)
            .unwrap();

        let decision = hook.check(
            "delete_file",
            &serde_json::json!({"token": "unregistered-secret-xyz"}),
        );

        assert!(matches!(decision, ProxyDecision::Deny(_)));
        let (redacted, blob) = audit_redacted_and_blob(&path);
        assert!(!redacted.contains("unregistered-secret-xyz"), "{redacted}");
        assert!(redacted.contains("[REDACTED]"), "{redacted}");
        assert!(blob.is_none(), "encrypted blob must be default-off");
    }

    #[test]
    fn hook_non_object_arguments는_audit_없이_거부() {
        let path = temp_db_path();
        let hook = hook_with(&path, Duration::from_millis(20), Duration::from_secs(5));

        let decision = hook.check("read_file", &serde_json::json!([1, 2]));

        assert!(matches!(decision, ProxyDecision::Deny(_)));
        assert_eq!(audit_count(&path), 0);
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
                    db.resolve_approval(&p.id, true, true, unix_secs_i64())
                        .unwrap();
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
                    db.resolve_approval(&p.id, false, true, unix_secs_i64())
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
    fn allow규칙도_live_discovery_실패면_자동통과_안하고_재승인() {
        let path = temp_db_path();
        // 승인 없이 짧게 타임아웃 → 재승인(ask) 경로로 빠지면 Deny로 관측된다.
        // bad_config라 live discover가 실패 → schema_hash_for=None → 자동통과 조건 불성립.
        let hook = hook_with(&path, Duration::from_millis(20), Duration::from_millis(150));
        // Allow 규칙이지만 live 스키마를 못 구하므로(None) 자동 통과가 아니라 재승인해야 한다
        // (낡은 DB 캐시로 폴백하지 않고 fail-closed).
        hook.db
            .upsert_permission_rule("srv-1", "read_file", "allow", Some("아무해시"))
            .unwrap();

        let decision = hook.check("read_file", &serde_json::json!({"path": "/tmp/x"}));
        assert!(
            matches!(decision, ProxyDecision::Deny(_)),
            "live discovery 실패한 Allow가 자동 통과됨"
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

    /// live 스키마는 프록시 세션당 한 번만 discover된다(백엔드 subprocess spawn 1회).
    /// 백엔드가 spawn될 때마다 카운터 파일에 한 줄을 남기고, Ask 경로를 2번 통과시킨 뒤
    /// (각 check가 schema_hash_for를 호출) spawn이 정확히 1회임을 확인한다.
    #[cfg(unix)]
    #[test]
    fn live_스키마는_세션당_한_번만_discover된다() {
        let path = temp_db_path();
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let counter =
            std::env::temp_dir().join(format!("deppy-proxy-spawn-{}-{n}", std::process::id()));
        let _ = std::fs::remove_file(&counter);
        // 백엔드 spawn마다 카운터에 한 줄 append 후 정상 핸드셰이크
        let script = format!(
            "echo x >> '{}'\n{}",
            counter.display(),
            mock_backend_script("read_file", r#"{"type":"object"}"#)
        );
        let hook = hook_with_config(
            &path,
            Duration::from_millis(20),
            Duration::from_secs(5),
            sh_config(script),
        );

        // 백그라운드 GUI: 뜨는 pending을 allow로 해소한다(최대 2건).
        let bg_path = path.clone();
        let resolver = std::thread::spawn(move || {
            let db = Db::open(&bg_path).unwrap();
            let mut resolved = 0;
            for _ in 0..2000 {
                if let Some(p) = db.list_pending_approvals().unwrap().first() {
                    db.resolve_approval(&p.id, true, false, unix_secs_i64())
                        .unwrap();
                    resolved += 1;
                    if resolved == 2 {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("2건의 pending을 해소하지 못함");
        });

        // 규칙 없음 → 두 번 다 Ask 경로(schema_hash_for 호출) → allow로 관측
        assert!(matches!(
            hook.check("read_file", &serde_json::json!({})),
            ProxyDecision::Allow
        ));
        assert!(matches!(
            hook.check("read_file", &serde_json::json!({})),
            ProxyDecision::Allow
        ));
        resolver.join().unwrap();

        // 두 번 check했지만 백엔드는 한 번만 spawn됐다(메모이제이션).
        let spawns = std::fs::read_to_string(&counter).unwrap();
        assert_eq!(
            spawns.lines().count(),
            1,
            "백엔드가 여러 번 spawn됨(메모이제이션 실패): {spawns:?}"
        );
    }
}
