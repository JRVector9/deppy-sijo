//! `deppy-mcp-proxy` — 에이전트가 자신의 stdio MCP 서버로 spawn하는 브리지 바이너리
//! (agent-proxy option 1.5). deppy의 권한 정책을 적용하고 Ask tool은 공유 DB로
//! GUI에 라이브 승인을 요청한 뒤, 허용된 호출만 실제 백엔드 MCP 서버로 포워딩한다.
//!
//! 배선(어떤 에이전트가 이 프록시를 쓰게 할지 config 주입)은 별도 후속 작업이다 —
//! 이 바이너리는 `--db`/`--server`만으로 단독 실행/테스트 가능하다.
//!
//! 로그는 반드시 **stderr**로만 나간다 — stdout은 JSON-RPC 전용이라 오염되면 안 된다.

mod approval_notify;
mod cli;
mod forwarder;
mod hook;
mod session;

use std::sync::{Arc, Mutex};

use deppy_core::time::unix_secs_i64;
use mcp::{LocalMcpManager, run_authorized_proxy};
#[cfg(test)]
use mcp_store::McpServerRow;
use secret::RedactionService;

use crate::approval_notify::ApprovalWakeNotifier;
use crate::cli::Cli;
use crate::hook::{authorization_subject_from_runtime_session_key, authorized_proxy_executor};
#[cfg(unix)]
use crate::session::IdleDeadlineReader;
use crate::session::{BackendClient, BackendConfig, BackendSession, DEFAULT_BACKEND_IDLE_TTL};

/// orphan 판정 컷오프(초): created_at이 (now - 이 값)보다 오래된 pending은 죽은 프록시가
/// 남긴 것으로 보고 시작 시 정리한다. **승인 대기 상한(MAX_APPROVAL_TIMEOUT_SECS)과 같게**
/// 둬서, 살아있는 다른 프록시의 pending(나이 < 자기 timeout ≤ 상한 = cutoff)은 절대 안
/// 쓸리게 한다 (codex — 짧은 고정 cutoff가 긴 timeout의 live pending을 오살하던 문제).
const ORPHAN_CUTOFF_SECS: i64 = cli::MAX_APPROVAL_TIMEOUT_SECS as i64;
const RESOLVED_APPROVAL_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Clone, Copy, PartialEq, Eq)]
struct ProxyRunFailure {
    phase: &'static str,
    error_code: &'static str,
}

impl ProxyRunFailure {
    const fn new(phase: &'static str, error_code: &'static str) -> Self {
        Self { phase, error_code }
    }

    fn from_error<E>(phase: &'static str, error_code: &'static str, _source: E) -> Self {
        Self::new(phase, error_code)
    }
}

impl std::fmt::Debug for ProxyRunFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProxyRunFailure")
            .field("phase", &self.phase)
            .field("error_code", &self.error_code)
            .finish()
    }
}

impl std::fmt::Display for ProxyRunFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "mcp proxy failure: phase={}, error_code={}",
            self.phase, self.error_code
        )
    }
}

impl std::error::Error for ProxyRunFailure {}

fn emit_proxy_run_failure(failure: ProxyRunFailure) {
    tracing::error!(
        kind = "mcp_proxy",
        phase = failure.phase,
        error_code = failure.error_code,
        "mcp proxy operation failed"
    );
}

fn main() {
    // stdout은 JSON-RPC 전용 → 로그는 stderr로만.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    if let Err(failure) = run() {
        emit_proxy_run_failure(failure);
        std::process::exit(1);
    }
}

fn run() -> Result<(), ProxyRunFailure> {
    // `deppy-mcp-proxy hooks --db <path> --event <needs-input|clear>` — claude/codex hook
    // 수신부. env DEPPY_SESSION_ID(=pane_id)로 needsInput을 DB에 set/clear하고 즉시 종료
    // (프록시 안 뜬다). stdin(hook payload)은 소비만 하고 안 씀 — 이벤트 타입으로 충분.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("hooks") {
        return run_hooks(&args[2..])
            .map_err(|error| ProxyRunFailure::from_error("hook", "hook_execution_failed", error));
    }
    // `deppy-mcp-proxy statusline --db <path>` — claude statusLine 오버레이가 렌더마다
    // 호출. stdin JSON에서 effort/model/남은 context%를 뽑아 (변경 시에만) DB에 기록하고,
    // 사용자 원래 statusLine을 체이닝 호출해 그 출력을 통과시킨다(사용자 바 보존).
    if args.get(1).map(String::as_str) == Some("statusline") {
        return run_statusline(&args[2..]).map_err(|error| {
            ProxyRunFailure::from_error("statusline", "statusline_execution_failed", error)
        });
    }

    let cli = Cli::from_env().map_err(|error| {
        ProxyRunFailure::from_error("startup", "cli_configuration_invalid", error)
    })?;
    let db = Arc::new(Mutex::new(storage::Db::open(&cli.db_path).map_err(
        |error| ProxyRunFailure::from_error("startup", "database_open_failed", error),
    )?));

    // 프론트할 백엔드 서버 spec을 DB에서 찾는다.
    let server = db
        .lock()
        .map_err(|error| ProxyRunFailure::from_error("startup", "database_unavailable", error))?
        .mcp_server(&cli.server_id)
        .map_err(|error| ProxyRunFailure::from_error("startup", "target_lookup_failed", error))?
        .ok_or_else(|| ProxyRunFailure::new("startup", "target_not_found"))?;
    // No credential/keyring access occurs at startup. Cold backend connect resolves only the
    // target's logical refs to current physical slots and holds a bounded redaction lease.
    let redaction = RedactionService::new();
    let config = BackendConfig::from_server_row(&server).map_err(|error| {
        ProxyRunFailure::from_error("startup", "target_configuration_invalid", error)
    })?;

    // 이전에 크래시한 프록시가 남긴 orphan pending 승인을 정리한다 — GUI가 죽은 팝업을
    // 띄우지 않게. best-effort(실패해도 서빙 계속). db를 hook으로 넘기기 전에 한다.
    // cutoff(now-ORPHAN_CUTOFF_SECS)보다 최근 행(다른 살아있는 프록시)은 건드리지 않고,
    // 이 프록시가 앞으로 넣을 행은 아직 없다.
    let now = unix_secs_i64();
    let db_guard = db
        .lock()
        .map_err(|error| ProxyRunFailure::from_error("startup", "database_unavailable", error))?;
    match db_guard.expire_pending_approvals(now - ORPHAN_CUTOFF_SECS, now) {
        Ok(n) if n > 0 => tracing::info!(
            kind = "approval",
            phase = "orphan_cleanup",
            error_code = "none",
            "mcp proxy maintenance completed"
        ),
        Ok(_) => {}
        Err(_) => tracing::warn!(
            kind = "approval",
            phase = "orphan_cleanup",
            error_code = "pending_expire_failed",
            "mcp proxy maintenance failed"
        ),
    }
    match db_guard.prune_resolved_approvals(now - RESOLVED_APPROVAL_RETENTION_SECS) {
        Ok(n) if n > 0 => tracing::info!(
            kind = "approval",
            phase = "resolved_cleanup",
            error_code = "none",
            "mcp proxy maintenance completed"
        ),
        Ok(_) => {}
        Err(_) => tracing::warn!(
            kind = "approval",
            phase = "resolved_cleanup",
            error_code = "approval_prune_failed",
            "mcp proxy maintenance failed"
        ),
    }
    drop(db_guard);

    // Stable proxy ownership is mandatory for scoped crash recovery. Missing pane identity must be
    // replaced explicitly; PID/random fallbacks would make every restart a new unrecoverable scope.
    let pane_id = std::env::var("DEPPY_SESSION_ID")
        .ok()
        .filter(|value| !value.trim().is_empty());
    // Validate and split the runtime-owned key before it can enter owner scope, approval rows, or
    // audit subject persistence. The executor derives the same subject again at construction so
    // non-main callers cannot pass a mismatched pane/subject pair.
    let _subject =
        authorization_subject_from_runtime_session_key(pane_id.as_deref()).map_err(|error| {
            ProxyRunFailure::from_error("startup", "session_identity_invalid", error)
        })?;
    let owner_id = pane_id.clone().or_else(|| {
        std::env::var("DEPPY_AUTHORIZATION_OWNER")
            .ok()
            .filter(|value| !value.trim().is_empty())
    });
    let owner_id =
        owner_id.ok_or_else(|| ProxyRunFailure::new("startup", "authorization_owner_missing"))?;
    let cleanup_session_key = pane_id.clone();
    let authorization_scope = format!("proxy:{owner_id}:{}", cli.server_id);
    let owner = db
        .lock()
        .map_err(|error| ProxyRunFailure::from_error("startup", "database_unavailable", error))?
        .acquire_authorization_owner(&authorization_scope)
        .map_err(|error| {
            ProxyRunFailure::from_error("startup", "authorization_owner_acquire_failed", error)
        })?;

    // hook live-schema와 forwarder call이 같은 lazy initialized connection을 공유한다.
    let manager = LocalMcpManager::new(redaction.clone());
    let backend_session = BackendSession::production(
        manager,
        Arc::clone(&db),
        redaction.clone(),
        Arc::new(secret::KeyringSecretStore),
        Some(Arc::new(secret::init_platform_store)),
        DEFAULT_BACKEND_IDLE_TTL,
    );
    let backend = Arc::new(BackendClient::managed(
        cli.server_id.clone(),
        config,
        Arc::clone(&backend_session),
        Arc::clone(&db),
    ));
    let executor = authorized_proxy_executor(
        Arc::clone(&db),
        owner,
        cli.server_id,
        redaction,
        cli.poll_interval,
        cli.approval_timeout,
        backend,
        pane_id,
        cli.approval_notifier
            .map(|notifier| Arc::new(notifier) as Arc<dyn ApprovalWakeNotifier>),
    )
    .map_err(|error| {
        ProxyRunFailure::from_error("startup", "authorization_executor_failed", error)
    })?;

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    #[cfg(unix)]
    let reader = IdleDeadlineReader::new(stdin.lock(), Arc::clone(&backend_session));
    #[cfg(not(unix))]
    let reader = stdin.lock();
    let result = run_authorized_proxy(reader, stdout.lock(), executor);
    backend_session.shutdown();
    let cleanup_result = deny_proxy_session_pending(&db, cleanup_session_key.as_deref());
    match (result, cleanup_result) {
        (Err(error), _) => Err(ProxyRunFailure::from_error(
            "protocol",
            "proxy_protocol_failed",
            error,
        )),
        (Ok(()), Err(error)) => Err(ProxyRunFailure::from_error(
            "shutdown",
            "session_cleanup_failed",
            error,
        )),
        (Ok(()), Ok(_)) => Ok(()),
    }
}

fn deny_proxy_session_pending(
    db: &Arc<Mutex<storage::Db>>,
    session_key: Option<&str>,
) -> anyhow::Result<usize> {
    let Some(session_key) = session_key else {
        return Ok(0);
    };
    db.lock()
        .map_err(|_| anyhow::anyhow!("proxy DB unavailable"))?
        .deny_pending_approvals_for_session(session_key, unix_secs_i64())
        .map_err(|_| anyhow::anyhow!("proxy session cleanup failed"))
}

/// claude/codex hook 수신: `--db <path> --event <needs-input|clear>`, 세션은 env
/// DEPPY_SESSION_ID(=pane_id). needsInput을 DB에 set/clear하고 즉시 종료한다.
/// **hook은 절대 에이전트를 막으면 안 되므로** 뭐가 없거나 실패해도 조용히 성공 반환한다.
fn run_hooks(args: &[String]) -> anyhow::Result<()> {
    let mut db_path: Option<std::path::PathBuf> = None;
    let mut event: Option<String> = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--db" => db_path = it.next().map(std::path::PathBuf::from),
            "--event" => event = it.next().cloned(),
            _ => {}
        }
    }
    // hook payload(claude/codex가 stdin으로 보냄)를 끝까지 읽는다 — 안 읽고 종료하면
    // 에이전트의 write가 broken pipe로 막힐 수 있고(codex 지적), payload에 세션 바인딩
    // (session_id/transcript_path)이 들어 있어 결정적 바인딩 소스로 기록한다.
    let mut payload = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut payload);
    // codex는 hook stdout이 유효 JSON이길 기대 — 무슨 일이 있어도 '{}' 출력.
    println!("{{}}");
    let Some(session_key) = std::env::var("DEPPY_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return Ok(());
    };
    let (Some(db_path), Some(event)) = (db_path, event) else {
        return Ok(());
    };
    // payload는 아래 바인딩 기록에도 쓰이므로 한 번만 파싱한다.
    let parsed = serde_json::from_str::<serde_json::Value>(&payload).ok();
    if let Ok(db) = storage::Db::open(&db_path) {
        // needsInput 이벤트만 대기 상태를 바꾼다. session-start 등은 바인딩만 기록.
        if event == "needs-input" || event == "clear" {
            // claude Notification hook은 payload.message에 대기 사유를 싣는다
            // ("Claude needs your permission to use Bash"). 벨 인박스가 이 문구를
            // 헤드라인으로 쓴다 — 로그 tail은 TUI 재그리기라 상태줄이 섞인다(2026-07-17).
            // 문구를 안 싣는 에이전트는 None → 인박스가 tail로 폴백한다.
            let message = parsed
                .as_ref()
                .and_then(|v| v.get("message"))
                .and_then(|m| m.as_str())
                .map(str::trim)
                .filter(|m| !m.is_empty());
            let _ = db.set_agent_needs_input(&session_key, event == "needs-input", message);
        } else if event == "turn-done" {
            // Stop hook = 턴 완료 → 상태 레일 '완료(바이올렛)' 트랜지언트 트리거.
            let _ = db.set_agent_turn_done(&session_key);
        }
        // 어떤 이벤트든 payload에 (session_id, transcript_path)가 오면 최신 바인딩으로 갱신.
        if let Some(v) = parsed.as_ref() {
            let sid = v.get("session_id").and_then(|x| x.as_str());
            let path = v.get("transcript_path").and_then(|x| x.as_str());
            // transcript_path를 **안 싣는** 에이전트가 있다. Kimi(0.34.0)가 그렇다 —
            // 페이로드는 hook_event_name/session_id/cwd뿐이다(바이너리의 triggerInner가
            // camelCase로 만들고 toHookInputData가 snake_case로 바꿔 보낸다). 그래서
            // 아래 (sid, path) 쌍이 성립하지 않아 바인딩이 하나도 기록되지 않았다.
            // 경로가 없을 때만 세션 id로 인덱스를 뒤진다 — 인덱스가 풀어주지 못하면
            // 바인딩하지 않는다(추측 경로로 묶으면 엉뚱한 세션 상태를 보여준다).
            if path.is_none()
                && let Some(sid) = sid
                && let Some(resolved) = kimi_transcript_path(sid)
            {
                let _ = db.upsert_hook_session(&session_key, "kimi", sid, &resolved);
            }
            if let (Some(sid), Some(path)) = (sid, path) {
                // kind는 transcript 경로로 판별한다. 폴백이 claude라 새 에이전트를
                // 안 넣으면 **전부 claude로 기록**돼 카드가 거짓말을 한다.
                let kind = if path.contains("/.codex/") {
                    "codex"
                } else if path.contains("/.kimi-code/") {
                    "kimi"
                } else {
                    "claude"
                };
                let _ = db.upsert_hook_session(&session_key, kind, sid, path);
            }
        }
    }
    Ok(())
}

/// Kimi 세션 id → transcript 경로. `~/.kimi-code/session_index.jsonl`이
/// `sessionId → sessionDir`을 들고 있고, transcript는 그 아래 고정 위치다.
///
/// Kimi hook payload에는 transcript 경로가 없어서(2026-08-09 실측) 여기서 풀어야 한다.
/// 인덱스가 없거나 항목이 없으면 바인딩을 만들지 않는다 — 추측 경로로 바인딩하면
/// 엉뚱한 세션의 상태를 보여준다.
fn kimi_transcript_path(session_id: &str) -> Option<String> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let index = home.join(".kimi-code/session_index.jsonl");
    let text = std::fs::read_to_string(index).ok()?;
    for line in text.lines() {
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if row.get("sessionId").and_then(|v| v.as_str()) != Some(session_id) {
            continue;
        }
        let dir = row.get("sessionDir").and_then(|v| v.as_str())?;
        let path = std::path::Path::new(dir).join("agents/main/wire.jsonl");
        return path.is_file().then(|| path.display().to_string());
    }
    None
}

/// claude statusLine 수신: `--db <path>`, 세션은 env DEPPY_SESSION_ID(=pane_id).
/// stdin JSON에서 effort/model/남은 context%를 뽑아 **값이 바뀔 때만** DB에 기록하고,
/// 사용자 원래 statusLine을 체이닝 호출해 그 출력을 stdout으로 통과시킨다.
/// statusLine은 절대 claude를 막으면 안 되므로 뭐가 실패해도 조용히 진행한다.
fn run_statusline(args: &[String]) -> anyhow::Result<()> {
    let mut db_path: Option<std::path::PathBuf> = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        if flag == "--db" {
            db_path = it.next().map(std::path::PathBuf::from);
        }
    }
    let mut payload = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut payload);
    let json: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();

    // 사용자 원래 statusLine을 먼저 실행해 출력 통과(우리 처리 실패와 무관하게 바 유지).
    if let Some(out) = chain_user_statusline(&json, &payload) {
        print!("{out}");
    }

    // effort/model/context% 추출 → 변경 시에만 DB. session_key 없으면 스킵.
    let session_key = std::env::var("DEPPY_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty());
    let (Some(session_key), Some(db_path)) = (session_key, db_path) else {
        return Ok(());
    };
    let effort = json.pointer("/effort/level").and_then(|v| v.as_str());
    let model = json.pointer("/model/display_name").and_then(|v| v.as_str());
    let ctx = json
        .pointer("/context_window/remaining_percentage")
        .and_then(serde_json::Value::as_i64);
    let usage = claude_statusline_usage(&json);
    let sig = format!(
        "{}|{}|{}|{}|{}",
        effort.unwrap_or(""),
        model.unwrap_or(""),
        ctx.map(|c| c.to_string()).unwrap_or_default(),
        usage
            .and_then(|(five_hour, _)| five_hour)
            .map(|value| value.to_string())
            .unwrap_or_default(),
        usage
            .and_then(|(_, weekly)| weekly)
            .map(|value| value.to_string())
            .unwrap_or_default()
    );
    // sig 파일로 렌더마다의 DB write를 막는다 — 값이 바뀐 렌더에서만 DB에 쓴다.
    let sig_path = statusline_sig_path(&session_key);
    if std::fs::read_to_string(&sig_path).ok().as_deref() == Some(sig.as_str()) {
        return Ok(()); // 변화 없음
    }
    // **DB write 성공 시에만** sig를 커밋한다 — 실패했는데 sig를 쓰면 값이 바뀔 때까지
    // 영영 재시도를 못 해 DB가 stale로 남는다(codex Medium).
    let wrote = storage::Db::open(&db_path)
        .and_then(|db| db.upsert_statusline(&session_key, effort, model, ctx))
        .is_ok();
    if wrote {
        if let Some(dir) = sig_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&sig_path, sig);
        if let Some((five_hour, weekly)) = usage {
            write_claude_usage_snapshot(five_hour, weekly);
        }
    }
    Ok(())
}

fn claude_statusline_usage(json: &serde_json::Value) -> Option<(Option<u8>, Option<u8>)> {
    let percent = |path: &str| {
        let window = json.pointer(path)?;
        window
            .get("used_percentage")
            .or_else(|| window.get("utilization"))?
            .as_f64()
            .filter(|value| value.is_finite())
            .map(|value| value.clamp(0.0, 100.0).round() as u8)
    };
    let five_hour = percent("/rate_limits/five_hour");
    let weekly = percent("/rate_limits/seven_day");
    (five_hour.is_some() || weekly.is_some()).then_some((five_hour, weekly))
}

fn write_claude_usage_snapshot(five_hour: Option<u8>, weekly: Option<u8>) {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return;
    };
    let dir = home.join(".deppy-sijo");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("claude-usage.json");
    let temporary = dir.join(format!("claude-usage.{}.tmp", std::process::id()));
    let updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default();
    let payload = serde_json::json!({
        "five_hour": five_hour,
        "seven_day": weekly,
        "updated_at": updated_at,
    });
    let Ok(bytes) = serde_json::to_vec(&payload) else {
        return;
    };
    if std::fs::write(&temporary, bytes).is_err() {
        return;
    }
    if std::fs::rename(&temporary, &path).is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
}

/// 세션별 statusLine 시그니처 캐시 경로(`~/.deppy-sijo/statusline/<sanitized-key>.sig`).
fn statusline_sig_path(session_key: &str) -> std::path::PathBuf {
    let safe: String = session_key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let base = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(".deppy-sijo")
        .join("statusline")
        .join(format!("{safe}.sig"))
}

/// 사용자 원래 statusLine command를 settings에서 찾아 payload를 stdin으로 넘겨 실행하고
/// stdout을 돌려준다. 없거나 실패하면 None(=우리 오버레이는 아무것도 안 그림 → 사용자가
/// 원래 statusLine이 없던 상태와 동일). 우리 오버레이 파일은 읽지 않으므로 무한루프 없음.
fn chain_user_statusline(json: &serde_json::Value, payload: &str) -> Option<String> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let project = json
        .pointer("/workspace/project_dir")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from);
    // 우선순위: 프로젝트 local > 프로젝트 settings > user local > user.
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(p) = &project {
        candidates.push(p.join(".claude/settings.local.json"));
        candidates.push(p.join(".claude/settings.json"));
    }
    candidates.push(home.join(".claude/settings.local.json"));
    candidates.push(home.join(".claude/settings.json"));
    let command = candidates.iter().find_map(|p| {
        let text = std::fs::read_to_string(p).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        v.pointer("/statusLine/command")
            .and_then(|c| c.as_str())
            .map(str::to_owned)
    })?;
    // payload를 stdin으로 넘겨 사용자 스크립트 실행(그들이 기대하는 입력 형식 그대로).
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    // **새 프로세스 그룹**으로 띄운다(setpgid) — 타임아웃 시 sh뿐 아니라 그 손자(파이프로
    // stdout을 붙든 서브프로세스)까지 그룹째 kill해 좀비/블록을 막는다(codex High/Med).
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    cmd.process_group(0); // pgid = 자식 pid
    let mut child = cmd.spawn().ok()?;
    let pid = child.id() as i32;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes());
        // drop(stdin)으로 EOF 신호 — take한 값이 블록 끝에서 drop됨.
    }
    // stdout을 별도 스레드로 읽어 파이프-full 데드락을 피하고, 최대 2초만 기다린다.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(2)).ok();
    // 성공/타임아웃 무관하게 **그룹째 kill 후 reap** — stdout EOF 뒤에도 살아있는 프로세스나
    // 손자를 남기지 않는다. 이미 죽었으면 killpg는 no-op(ESRCH). 그 뒤 wait는 즉시 반환.
    #[cfg(unix)]
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
    let _ = child.wait();
    result.map(|buf| String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    struct TraceWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for TraceWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn concrete_keyring_store는_proxy_main_composition_root에만_존재한다() {
        fn production(source: &str) -> &str {
            source
                .split_once("\n#[cfg(test)]\nmod tests")
                .map_or(source, |(production, _)| production)
        }

        for (module, source) in [
            ("approval_notify.rs", include_str!("approval_notify.rs")),
            ("cli.rs", include_str!("cli.rs")),
            ("forwarder.rs", include_str!("forwarder.rs")),
            ("hook.rs", include_str!("hook.rs")),
            ("session.rs", include_str!("session.rs")),
        ] {
            assert!(
                !production(source).contains("KeyringSecretStore"),
                "concrete keyring escaped proxy main: {module}"
            );
        }
        assert_eq!(
            production(include_str!("main.rs"))
                .matches("KeyringSecretStore")
                .count(),
            1,
            "proxy main must remain the single concrete keyring composition seam"
        );
    }

    #[test]
    fn process_failure_debug_display_trace는_source_marker를_보존하지_않는다() {
        const MARKER: &str = "HOSTILE_PROXY_SOURCE_MARKER";
        let failure = ProxyRunFailure::from_error(
            "startup",
            "target_configuration_invalid",
            anyhow::anyhow!(MARKER),
        );
        assert!(std::error::Error::source(&failure).is_none());
        assert!(!format!("{failure:?}").contains(MARKER));
        assert!(!format!("{failure}").contains(MARKER));

        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer_buffer = Arc::clone(&captured);
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_writer(move || TraceWriter(Arc::clone(&writer_buffer)))
            .finish();
        tracing::subscriber::with_default(subscriber, || emit_proxy_run_failure(failure));

        let output = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        assert!(output.contains("kind=\"mcp_proxy\""));
        assert!(output.contains("phase=\"startup\""));
        assert!(output.contains("error_code=\"target_configuration_invalid\""));
        assert!(!output.contains(MARKER));
    }

    #[test]
    fn backend_target은_secret대신_logical_refs만_보관한다() {
        let row = McpServerRow {
            id: "server".to_owned(),
            name: "server".to_owned(),
            kind: "stdio".to_owned(),
            command: Some("command".to_owned()),
            args: Vec::new(),
            env_plain: vec![("SAFE".to_owned(), "value".to_owned())],
            env_secrets: vec![("TOKEN".to_owned(), "credential-logical".to_owned())],
            inherit_env: false,
            url: None,
            enabled: true,
        };
        let BackendConfig::Stdio(config) = BackendConfig::from_server_row(&row).unwrap() else {
            panic!("stdio target expected")
        };
        assert_eq!(
            config.env_plain,
            vec![("SAFE".to_owned(), "value".to_owned())]
        );
        assert_eq!(
            config.env_credentials,
            vec![("TOKEN".to_owned(), "credential-logical".to_owned())]
        );
    }

    #[test]
    fn normal_proxy_exit_cleanup은_exact_session_pending만_deny한다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-proxy-exit-cleanup-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        let db = Arc::new(Mutex::new(storage::Db::open(&path).unwrap()));
        let target = "315f68b6-333f-409f-a2c5-922b9eacfd7e:1";
        let other = "315f68b6-333f-409f-a2c5-922b9eacfd7e:2";
        for (id, session) in [("target", target), ("other", other)] {
            db.lock()
                .unwrap()
                .insert_pending_approval(id, "server", "tool", "{}", None, 1, Some(session))
                .unwrap();
        }

        assert_eq!(deny_proxy_session_pending(&db, Some(target)).unwrap(), 1);
        assert_eq!(
            db.lock().unwrap().poll_approval("target").unwrap().status,
            storage::ApprovalStatus::Denied
        );
        assert_eq!(
            db.lock().unwrap().poll_approval("other").unwrap().status,
            storage::ApprovalStatus::Pending
        );
        assert_eq!(deny_proxy_session_pending(&db, Some(target)).unwrap(), 0);
        assert_eq!(deny_proxy_session_pending(&db, None).unwrap(), 0);

        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
