//! `deppy-mcp-proxy` — 에이전트가 자신의 stdio MCP 서버로 spawn하는 브리지 바이너리
//! (agent-proxy option 1.5). deppy의 권한 정책을 적용하고 Ask tool은 공유 DB로
//! GUI에 라이브 승인을 요청한 뒤, 허용된 호출만 실제 백엔드 MCP 서버로 포워딩한다.
//!
//! 배선(어떤 에이전트가 이 프록시를 쓰게 할지 config 주입)은 별도 후속 작업이다 —
//! 이 바이너리는 `--db`/`--server`만으로 단독 실행/테스트 가능하다.
//!
//! 로그는 반드시 **stderr**로만 나간다 — stdout은 JSON-RPC 전용이라 오염되면 안 된다.

mod cli;
mod forwarder;
mod hook;

use anyhow::Context;
use mcp::{LocalMcpManager, McpServerConfig, run_proxy};
use mcp_store::McpServerRow;
use secret::{KeyringSecretStore, RedactionService, SecretStore};

use crate::cli::Cli;
use crate::forwarder::ManagerToolForwarder;
use crate::hook::DbPermissionHook;

/// orphan 판정 컷오프(초): created_at이 (now - 이 값)보다 오래된 pending은 죽은 프록시가
/// 남긴 것으로 보고 시작 시 정리한다. **승인 대기 상한(MAX_APPROVAL_TIMEOUT_SECS)과 같게**
/// 둬서, 살아있는 다른 프록시의 pending(나이 < 자기 timeout ≤ 상한 = cutoff)은 절대 안
/// 쓸리게 한다 (codex — 짧은 고정 cutoff가 긴 timeout의 live pending을 오살하던 문제).
const ORPHAN_CUTOFF_SECS: i64 = cli::MAX_APPROVAL_TIMEOUT_SECS as i64;
const RESOLVED_APPROVAL_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

fn main() -> anyhow::Result<()> {
    // stdout은 JSON-RPC 전용 → 로그는 stderr로만.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    // `deppy-mcp-proxy hooks --db <path> --event <needs-input|clear>` — claude/codex hook
    // 수신부. env DEPPY_SESSION_ID(=pane_id)로 needsInput을 DB에 set/clear하고 즉시 종료
    // (프록시 안 뜬다). stdin(hook payload)은 소비만 하고 안 씀 — 이벤트 타입으로 충분.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("hooks") {
        return run_hooks(&args[2..]);
    }

    let cli = Cli::from_env()?;
    let db = storage::Db::open(&cli.db_path)?;

    // 프론트할 백엔드 서버 spec을 DB에서 찾는다.
    let server = db
        .list_mcp_servers()?
        .into_iter()
        .find(|s| s.id == cli.server_id)
        .with_context(|| format!("MCP 서버 '{}'를 DB에서 찾을 수 없음", cli.server_id))?;
    // keyring store 등록 (credential redaction 시드용).
    // 실패해도 프록시는 동작한다 — 시드만 비활성 (best-effort, insecure fallback 아님).
    let keyring_ok = match secret::init_platform_store() {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("keyring store 초기화 실패 — redaction 시드 생략: {e:#}");
            false
        }
    };

    // 로그/프리뷰 redaction: 저장된 credential secret을 시드한다 (app/runtime과 동일 관례).
    let redaction = RedactionService::new();
    let keyring_store = KeyringSecretStore;
    if keyring_ok {
        seed_redaction(&db, &keyring_store, &redaction);
    }
    let secret_store: Option<&dyn SecretStore> = if keyring_ok {
        Some(&keyring_store)
    } else {
        None
    };
    let config = server_config(&server, secret_store, &redaction)?;

    // 이전에 크래시한 프록시가 남긴 orphan pending 승인을 정리한다 — GUI가 죽은 팝업을
    // 띄우지 않게. best-effort(실패해도 서빙 계속). db를 hook으로 넘기기 전에 한다.
    // cutoff(now-ORPHAN_CUTOFF_SECS)보다 최근 행(다른 살아있는 프록시)은 건드리지 않고,
    // 이 프록시가 앞으로 넣을 행은 아직 없다.
    let now = unix_secs();
    match db.expire_pending_approvals(now - ORPHAN_CUTOFF_SECS, now) {
        Ok(n) if n > 0 => tracing::info!("orphan pending 승인 {n}건 정리(이전 크래시 잔여)"),
        Ok(_) => {}
        Err(e) => tracing::warn!("orphan pending 승인 정리 실패(무시하고 계속): {e:#}"),
    }
    match db.prune_resolved_approvals(now - RESOLVED_APPROVAL_RETENTION_SECS) {
        Ok(n) if n > 0 => tracing::info!("resolved approval {n}건 정리"),
        Ok(_) => {}
        Err(e) => tracing::warn!("resolved approval 정리 실패(무시하고 계속): {e:#}"),
    }

    // 백엔드 stderr redaction을 위해 manager도 같은 redaction을 공유한다.
    // hook도 live 스키마 검증용으로 자신의 manager+config를 갖는다 (forwarder와 별개 인스턴스).
    let manager = LocalMcpManager::new(redaction.clone());
    let hook_manager = LocalMcpManager::new(redaction.clone());
    let hook_config = config.clone();
    let forwarder = ManagerToolForwarder::new(manager, config);
    let hook = DbPermissionHook::new(
        db,
        cli.server_id,
        redaction,
        cli.poll_interval,
        cli.approval_timeout,
        hook_manager,
        hook_config,
    );

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    run_proxy(stdin.lock(), stdout.lock(), forwarder, hook)
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
    if let Ok(db) = storage::Db::open(&db_path) {
        // needsInput 이벤트만 대기 상태를 바꾼다. session-start 등은 바인딩만 기록.
        if event == "needs-input" || event == "clear" {
            let _ = db.set_agent_needs_input(&session_key, event == "needs-input");
        } else if event == "turn-done" {
            // Stop hook = 턴 완료 → 상태 레일 '완료(바이올렛)' 트랜지언트 트리거.
            let _ = db.set_agent_turn_done(&session_key);
        }
        // 어떤 이벤트든 payload에 (session_id, transcript_path)가 오면 최신 바인딩으로 갱신.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) {
            let sid = v.get("session_id").and_then(|x| x.as_str());
            let path = v.get("transcript_path").and_then(|x| x.as_str());
            if let (Some(sid), Some(path)) = (sid, path) {
                // kind는 transcript 경로로 판별(.codex=codex, 그 외=claude).
                let kind = if path.contains("/.codex/") {
                    "codex"
                } else {
                    "claude"
                };
                let _ = db.upsert_hook_session(&session_key, kind, sid, path);
            }
        }
    }
    Ok(())
}

/// 현재 unix epoch seconds. 정상 시스템 시계에서 UNIX_EPOCH 이후이므로 0으로 폴백.
fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// McpServerRow → LocalMcpManager가 spawn할 McpServerConfig.
/// v0는 stdio kind + command 필수 (§1.5).
fn server_config(
    row: &McpServerRow,
    secret_store: Option<&dyn SecretStore>,
    redaction: &RedactionService,
) -> anyhow::Result<McpServerConfig> {
    anyhow::ensure!(
        row.kind == "stdio",
        "서버 '{}'의 kind가 '{}' — v0 프록시는 stdio만 지원",
        row.id,
        row.kind
    );
    let command = row
        .command
        .clone()
        .with_context(|| format!("stdio 서버 '{}'에 command가 없음", row.id))?;
    Ok(McpServerConfig {
        name: row.name.clone(),
        command,
        args: row.args.clone(),
        env: resolve_server_env(row, secret_store, redaction)?,
        inherit_env: row.inherit_env,
    })
}

fn resolve_server_env(
    row: &McpServerRow,
    secret_store: Option<&dyn SecretStore>,
    redaction: &RedactionService,
) -> anyhow::Result<Vec<(String, String)>> {
    let mut env = row.env_plain.clone();
    if row.env_secrets.is_empty() {
        return Ok(env);
    }
    let store = secret_store.with_context(|| {
        format!(
            "MCP 서버 '{}' scoped secret env를 해석할 keyring이 없음",
            row.id
        )
    })?;
    for (key, credential_id) in &row.env_secrets {
        let secret = store
            .get_secret(credential_id)
            .with_context(|| format!("MCP 서버 '{}' env '{}' credential 조회 실패", row.id, key))?;
        redaction.register(&secret);
        redaction.register_json_fields(&secret);
        env.push((key.clone(), secret.expose().to_owned()));
    }
    Ok(env)
}

/// 저장된 credential secret을 redaction 대상으로 등록한다 (프리뷰/로그 누출 방지).
/// resolve는 이 단일 스레드에서만 일어난다 (secret 접근 직렬화, §1.4). best-effort —
/// 개별 실패는 경고만 남기고 계속한다.
fn seed_redaction(db: &storage::Db, store: &dyn SecretStore, redaction: &RedactionService) {
    let credentials = match db.list_credentials() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("credential 목록 조회 실패 (redaction 시드 생략): {e:#}");
            return;
        }
    };
    for credential in credentials {
        register_secret(store, redaction, &credential.id);
        // OAuth 토큰은 refresh entry가 별도 keyring 좌표에 있다 (app.rs와 동일).
        if credential.credential_kind == "oauth_token" {
            register_secret(store, redaction, &auth::refresh_entry_id(&credential.id));
        }
    }
}

/// keyring에서 secret 하나를 읽어 원본 + (JSON이면) 내부 필드까지 redaction 등록한다.
fn register_secret(store: &dyn SecretStore, redaction: &RedactionService, id: &str) {
    match store.get_secret(id) {
        Ok(value) => {
            redaction.register(&value);
            // OAuth 토큰 blob처럼 JSON이면 access/refresh 개별 필드도 등록 (PR-18).
            redaction.register_json_fields(&value);
        }
        Err(e) => tracing::warn!("redaction 시드 실패 (credential {id}): {e:#}"),
    }
}
