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
use deppy_core::time::unix_secs_i64;
use mcp::{LocalMcpManager, McpHttpServerConfig, McpServerConfig, run_proxy, validate_mcp_url};
use mcp_store::McpServerRow;
use secret::{KeyringSecretStore, RedactionService, SecretStore};

use crate::cli::Cli;
use crate::forwarder::{BackendConfig, ManagerToolForwarder};
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
    // `deppy-mcp-proxy statusline --db <path>` — claude statusLine 오버레이가 렌더마다
    // 호출. stdin JSON에서 effort/model/남은 context%를 뽑아 (변경 시에만) DB에 기록하고,
    // 사용자 원래 statusLine을 체이닝 호출해 그 출력을 통과시킨다(사용자 바 보존).
    if args.get(1).map(String::as_str) == Some("statusline") {
        return run_statusline(&args[2..]);
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
    let now = unix_secs_i64();
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
    // pane_id = env DEPPY_SESSION_ID — 승인이 어느 세션에서 났는지 표시/딥링크용 (I2).
    let pane_id = std::env::var("DEPPY_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty());
    let hook = DbPermissionHook::new(
        db,
        cli.server_id,
        redaction,
        cli.poll_interval,
        cli.approval_timeout,
        hook_manager,
        hook_config,
        pane_id,
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
    let sig = format!(
        "{}|{}|{}",
        effort.unwrap_or(""),
        model.unwrap_or(""),
        ctx.map(|c| c.to_string()).unwrap_or_default()
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
    }
    Ok(())
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

/// McpServerRow → kind별 BackendConfig (H3): stdio는 command 필수 + scoped env 해석,
/// http는 url 필수 + 시작 시점 URL 정책 검증(https/localhost — 연결 때도 재검증된다).
/// http bearer는 H3에서 항상 None — credential 연동(401 사다리)은 H5 소관.
fn server_config(
    row: &McpServerRow,
    secret_store: Option<&dyn SecretStore>,
    redaction: &RedactionService,
) -> anyhow::Result<BackendConfig> {
    match row.kind.as_str() {
        "stdio" => {
            let command = row
                .command
                .clone()
                .with_context(|| format!("stdio 서버 '{}'에 command가 없음", row.id))?;
            Ok(BackendConfig::Stdio(McpServerConfig::stdio(
                row.name.clone(),
                command,
                row.args.clone(),
                resolve_server_env(row, secret_store, redaction)?,
                row.inherit_env,
            )))
        }
        "http" => {
            let url = row
                .url
                .clone()
                .with_context(|| format!("http 서버 '{}'에 url이 없음", row.id))?;
            validate_mcp_url(&url)
                .with_context(|| format!("http 서버 '{}' url 정책 위반", row.id))?;
            Ok(BackendConfig::Http(McpHttpServerConfig {
                name: row.name.clone(),
                url,
                bearer: None,
            }))
        }
        other => anyhow::bail!(
            "서버 '{}'의 kind가 '{}' — 프록시는 stdio|http만 지원",
            row.id,
            other
        ),
    }
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
