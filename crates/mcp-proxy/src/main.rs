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
use mcp::{LocalMcpManager, McpServerConfig, McpServerRow, run_proxy};
use secret::{KeyringSecretStore, RedactionService, SecretStore};

use crate::cli::Cli;
use crate::forwarder::ManagerToolForwarder;
use crate::hook::DbPermissionHook;

/// orphan 판정 컷오프(초): created_at이 (now - 이 값)보다 오래된 pending은 죽은 프록시가
/// 남긴 것으로 보고 시작 시 정리한다. **승인 대기 상한(MAX_APPROVAL_TIMEOUT_SECS)과 같게**
/// 둬서, 살아있는 다른 프록시의 pending(나이 < 자기 timeout ≤ 상한 = cutoff)은 절대 안
/// 쓸리게 한다 (codex — 짧은 고정 cutoff가 긴 timeout의 live pending을 오살하던 문제).
const ORPHAN_CUTOFF_SECS: i64 = cli::MAX_APPROVAL_TIMEOUT_SECS as i64;

fn main() -> anyhow::Result<()> {
    // stdout은 JSON-RPC 전용 → 로그는 stderr로만.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = Cli::from_env()?;
    let db = storage::Db::open(&cli.db_path)?;

    // 프론트할 백엔드 서버 spec을 DB에서 찾는다.
    let server = db
        .list_mcp_servers()?
        .into_iter()
        .find(|s| s.id == cli.server_id)
        .with_context(|| format!("MCP 서버 '{}'를 DB에서 찾을 수 없음", cli.server_id))?;
    let config = server_config(&server)?;

    // keyring store 등록 (credential redaction 시드 + audit blob 암호화에 필요).
    // 실패해도 프록시는 동작한다 — 시드/암호화만 비활성 (best-effort, insecure fallback 아님).
    let keyring_ok = match secret::init_platform_store() {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("keyring store 초기화 실패 — redaction 시드/audit 암호화 비활성: {e:#}");
            false
        }
    };

    // 로그/프리뷰 redaction: 저장된 credential secret을 시드한다 (app/runtime과 동일 관례).
    let redaction = RedactionService::new();
    if keyring_ok {
        seed_redaction(&db, &redaction);
    }

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
        keyring_ok,
        cli.poll_interval,
        cli.approval_timeout,
        hook_manager,
        hook_config,
    );

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    run_proxy(stdin.lock(), stdout.lock(), forwarder, hook)
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
fn server_config(row: &McpServerRow) -> anyhow::Result<McpServerConfig> {
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
    })
}

/// 저장된 credential secret을 redaction 대상으로 등록한다 (프리뷰/로그 누출 방지).
/// resolve는 이 단일 스레드에서만 일어난다 (secret 접근 직렬화, §1.4). best-effort —
/// 개별 실패는 경고만 남기고 계속한다.
fn seed_redaction(db: &storage::Db, redaction: &RedactionService) {
    let credentials = match db.list_credentials() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("credential 목록 조회 실패 (redaction 시드 생략): {e:#}");
            return;
        }
    };
    let store = KeyringSecretStore;
    for credential in credentials {
        register_secret(&store, redaction, &credential.id);
        // OAuth 토큰은 refresh entry가 별도 keyring 좌표에 있다 (app.rs와 동일).
        if credential.credential_kind == "oauth_token" {
            register_secret(&store, redaction, &auth::refresh_entry_id(&credential.id));
        }
    }
}

/// keyring에서 secret 하나를 읽어 원본 + (JSON이면) 내부 필드까지 redaction 등록한다.
fn register_secret(store: &KeyringSecretStore, redaction: &RedactionService, id: &str) {
    match store.get_secret(id) {
        Ok(value) => {
            redaction.register(&value);
            // OAuth 토큰 blob처럼 JSON이면 access/refresh 개별 필드도 등록 (PR-18).
            redaction.register_json_fields(&value);
        }
        Err(e) => tracing::warn!("redaction 시드 실패 (credential {id}): {e:#}"),
    }
}
