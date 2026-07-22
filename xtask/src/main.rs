//! xtask — 워크스페이스 관리 명령 (v2.8 §11).
//!
//! `cargo run -p xtask -- check-deps`
//!   crate 그래프의 **금지 의존 edge**와 **순환**을 검사한다. v2.8 영속 계층 규칙
//!   (storage-core는 도메인을 모름, runtime crate는 store를 모름 등)을 코드로 강제해,
//!   `mcp → storage` 같은 순환 유발 edge가 무심코 추가되는 것을 막는다.
//!
//! `cargo run -p xtask -- check-boundary`
//!   app leaf UI가 secret/MCP/audit/storage side effect를 직접 갖지 않도록 검사한다.
//!   남아 있는 connector/agent/env DB 호출은 파일+snippet+개수 allowlist로 고정한다.
//!
//! `cargo run -p xtask -- smoke-db-migrations`
//!   storage migration smoke tests를 실행한다.
//!
//! `cargo run -p xtask -- security-scan`
//!   boundary/dependency gates와 secret/audit persistence tests를 실행한다.
//!
//! `cargo run -p xtask -- perf-smoke`
//!   현재 자동화 가능한 performance/backpressure smoke tests를 실행한다.
//!
//! `cargo run -p xtask -- od01-failure-matrix`
//!   Connector failure taxonomy 전체를 deterministic exact test에 1:1로 연결한다.
//!
//! `cargo run -p xtask -- i18n-check`
//!   필수 locale key completeness, fallback, CJK path, layout smoke tests를 실행한다.
//!
//! Cargo.toml의 `path = "../<dir>"` 로컬 의존만 본다(외부 crate는 무관). crate 식별은
//! 디렉터리명 기준(예: crates/core의 패키지명은 deppy-core지만 여기선 "core").

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

/// 금지 의존 edge (from → to, 디렉터리명 기준). 아직 존재하지 않는 crate가 규칙에
/// 있어도 된다 — 생기는 순간부터 검사된다 (v2.8 §3.3/§5.2를 코드화).
const FORBIDDEN_EDGES: &[(&str, &str)] = &[
    // storage-core는 DB infra만 — 어떤 도메인/조립 crate도 모른다
    ("storage-core", "storage"),
    ("storage-core", "mcp"),
    ("storage-core", "mcp-store"),
    ("storage-core", "audit"),
    ("storage-core", "persist"),
    ("storage-core", "mux"),
    ("storage-core", "session"),
    ("storage-core", "app"),
    ("storage-core", "runtime"),
    ("storage-core", "secret"),
    // runtime 성격 crate는 store/facade를 모른다 (v2.8: mcp-runtime → store 금지).
    // 이것이 원래 순환(mcp → storage → mcp)의 재발 방지 지점이다.
    ("mcp", "storage"),
    ("mcp", "storage-core"),
    ("mcp", "mcp-store"),
    ("mcp", "audit"),
    ("audit", "storage"),
    ("audit", "mcp"),
    ("audit", "mcp-store"),
    ("persist", "storage"),
    ("persist", "mcp"),
    ("persist", "audit"),
    ("mux", "storage"),
    ("mux", "storage-core"),
    ("mux", "persist"),
    ("session", "storage"),
    ("session", "persist"),
    ("session", "secret"),
    // env-store류가 생기면: secret 금지 (v2.8 §6.7)
    ("env-store", "secret"),
    // store/영속 crate가 상층(runtime/app/UI)을 아는 것 금지
    ("storage", "runtime"),
    ("storage", "app"),
    ("mcp-store", "runtime"),
    ("mcp-store", "app"),
    ("mcp-store", "mcp"),
    ("mcp-store", "audit"),
    ("mcp-store", "secret"),
    ("mcp-store", "storage"),
    ("mcp-store", "persist"),
    ("persist", "runtime"),
    ("persist", "app"),
];

/// Connector 계층은 deny-list만으로는 새 edge를 모두 막을 수 없으므로 direct dependency
/// 전체를 allow-list로 고정한다. dev/build/target dependency도 같은 규칙을 적용한다.
const STRICT_CRATE_DEPS: &[(&str, &[&str])] = &[
    ("connector-contract", &["serde"]),
    ("connector-ui", &["connector-contract", "egui", "i18n"]),
    (
        "connector-service",
        &[
            "audit",
            "auth",
            "connector-contract",
            "mcp",
            "secret",
            "tracing",
        ],
    ),
];

fn main() -> anyhow::Result<()> {
    let command = std::env::args().nth(1).unwrap_or_default();
    match command.as_str() {
        "check-boundary" => check_boundary(),
        "check-deps" => check_deps(),
        "smoke-db-migrations" => smoke_db_migrations(),
        "security-scan" => security_scan(),
        "perf-smoke" => perf_smoke(),
        "od01-failure-matrix" => od01_failure_matrix(),
        "i18n-check" => i18n_check(),
        other => bail!(
            "알 수 없는 명령 '{other}' — 사용법: cargo run -p xtask -- check-deps|check-boundary|smoke-db-migrations|security-scan|perf-smoke|od01-failure-matrix|i18n-check"
        ),
    }
}

fn smoke_db_migrations() -> anyhow::Result<()> {
    run_cargo(&["test", "-p", "storage", "마이그레이션"])?;
    run_cargo(&["test", "-p", "storage", "v8에서_v9"])?;
    run_cargo(&["test", "-p", "storage", "v9에서_v10"])?;
    run_cargo(&["test", "-p", "storage", "v10에서_v11"])?;
    run_cargo(&["test", "-p", "storage", "v11에서_v12"])?;
    run_cargo(&["test", "-p", "storage", "v20에서_v21"])?;
    println!("smoke-db-migrations OK");
    Ok(())
}

fn security_scan() -> anyhow::Result<()> {
    check_boundary()?;
    check_deps()?;
    run_cargo(&["test", "-p", "storage", "secret_like"])?;
    run_cargo(&["test", "-p", "storage", "db_파일에_secret_평문이_없다"])?;
    run_cargo(&["test", "-p", "mcp-store", "secret_like"])?;
    run_cargo(&["test", "-p", "audit", "-p", "mcp", "-p", "mcp-proxy"])?;
    println!("security-scan OK");
    Ok(())
}

fn perf_smoke() -> anyhow::Result<()> {
    run_cargo(&["test", "-p", "deppy-sijo", "perf"])?;
    run_cargo(&["test", "-p", "runtime", "backpressure"])?;
    run_cargo(&["test", "-p", "runtime", "hidden"])?;
    println!("perf-smoke OK");
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct FailureMatrixCase {
    point: &'static str,
    package: &'static str,
    exact_test: &'static str,
}

const OD01_FAILURE_MATRIX: &[FailureMatrixCase] = &[
    FailureMatrixCase {
        point: "RepositoryBeforeCommit",
        package: "storage",
        exact_test: "db::tests::server_delete_failure는_pending_permission_tool_server를_모두_rollback한다",
    },
    FailureMatrixCase {
        point: "RepositoryAfterPreparedAudit",
        package: "storage",
        exact_test: "db::tests::allow_always_permission과_audit은_같이_rollback된다",
    },
    FailureMatrixCase {
        point: "SecretWrite",
        package: "secret",
        exact_test: "bundle::tests::stage_failure_rolls_back_every_entry_written_in_new_slot",
    },
    FailureMatrixCase {
        point: "SecretPointerSwap",
        package: "storage",
        exact_test: "db::tests::oauth_secret_slot_publish_실패는_pointer와_metadata를_rollback한다",
    },
    FailureMatrixCase {
        point: "AuditPreflightCommit",
        package: "connector-service",
        exact_test: "coordinator::tests::approval_preflight_failure_and_double_resolve_never_call",
    },
    FailureMatrixCase {
        point: "McpBeforeSend",
        package: "mcp",
        exact_test: "manager::tests::call_tool_큰_payload는_stdio_write전에_거부",
    },
    FailureMatrixCase {
        point: "McpAfterSendUnknown",
        package: "mcp",
        exact_test: "transport::tests::tools_call_partial_write_or_eof_is_delivery_unknown",
    },
    FailureMatrixCase {
        point: "HttpTimeout",
        package: "mcp",
        exact_test: "http::tests::timed_out_http_senders_keep_permits_and_reaper_is_bounded",
    },
    FailureMatrixCase {
        point: "WorkerPanic",
        package: "connector-service",
        exact_test: "coordinator::tests::backend_panic_completes_and_releases_operation_capacity",
    },
    FailureMatrixCase {
        point: "ProcessCrash",
        package: "storage",
        exact_test: "db::tests::authorization_owner_graceful_close와_실패_fallback은_scope_run에_격리된다",
    },
];

fn od01_failure_matrix() -> anyhow::Result<()> {
    let root = workspace_root()?;
    let expected = failure_point_variants(&root)?;
    validate_failure_matrix(&expected, OD01_FAILURE_MATRIX)?;
    for case in OD01_FAILURE_MATRIX {
        run_exact_test(case)?;
    }
    println!(
        "od01-failure-matrix OK — {} failure points, exact deterministic tests",
        OD01_FAILURE_MATRIX.len()
    );
    Ok(())
}

fn failure_point_variants(root: &Path) -> anyhow::Result<Vec<String>> {
    let path = root.join("crates/connector-contract/src/lib.rs");
    let source =
        std::fs::read_to_string(&path).with_context(|| format!("{} 읽기 실패", path.display()))?;
    let body = source
        .split_once("pub enum FailurePoint {")
        .map(|(_, tail)| tail)
        .and_then(|tail| tail.split_once('}').map(|(body, _)| body))
        .context("connector-contract FailurePoint enum을 찾지 못했습니다")?;
    let variants = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .map(|line| line.trim_end_matches(',').to_owned())
        .collect::<Vec<_>>();
    anyhow::ensure!(!variants.is_empty(), "FailurePoint variant가 비어 있습니다");
    Ok(variants)
}

fn validate_failure_matrix(expected: &[String], cases: &[FailureMatrixCase]) -> anyhow::Result<()> {
    let mut counts = BTreeMap::<&str, usize>::new();
    for case in cases {
        *counts.entry(case.point).or_default() += 1;
    }
    let duplicates = counts
        .iter()
        .filter_map(|(point, count)| (*count > 1).then_some(*point))
        .collect::<Vec<_>>();
    anyhow::ensure!(duplicates.is_empty(), "failure matrix 중복: {duplicates:?}");

    let expected = expected.iter().map(String::as_str).collect::<Vec<_>>();
    let missing = expected
        .iter()
        .copied()
        .filter(|point| !counts.contains_key(point))
        .collect::<Vec<_>>();
    let unknown = counts
        .keys()
        .copied()
        .filter(|point| !expected.contains(point))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        missing.is_empty() && unknown.is_empty() && cases.len() == expected.len(),
        "failure matrix 불일치: missing={missing:?}, unknown={unknown:?}"
    );
    Ok(())
}

fn run_exact_test(case: &FailureMatrixCase) -> anyhow::Result<()> {
    let root = workspace_root()?;
    let output = std::process::Command::new("cargo")
        .args([
            "test",
            "-p",
            case.package,
            case.exact_test,
            "--",
            "--exact",
            "--test-threads=1",
        ])
        .current_dir(root)
        .output()
        .with_context(|| format!("{} failure test 실행 실패", case.point))?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    anyhow::ensure!(
        output.status.success(),
        "{} failure test 실패: {}",
        case.point,
        output.status
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    anyhow::ensure!(
        stdout.contains("test result: ok. 1 passed;"),
        "{} failure test가 정확히 1개 선택되지 않았습니다",
        case.point
    );
    Ok(())
}

fn i18n_check() -> anyhow::Result<()> {
    run_cargo(&["test", "-p", "i18n"])?;
    run_cargo(&["test", "-p", "deppy-sijo", "locale_설정"])?;
    run_cargo(&[
        "test",
        "-p",
        "deppy-sijo",
        "path_insert_paste_bytes_required_fixtures",
    ])?;
    run_cargo(&[
        "test",
        "-p",
        "deppy-sijo",
        "status_알림은_message_id를_저장한다",
    ])?;
    println!("i18n-check OK");
    Ok(())
}

fn run_cargo(args: &[&str]) -> anyhow::Result<()> {
    let root = workspace_root()?;
    let status = std::process::Command::new("cargo")
        .args(args)
        .current_dir(root)
        .status()
        .with_context(|| format!("cargo {} 실행 실패", args.join(" ")))?;
    if status.success() {
        Ok(())
    } else {
        bail!("cargo {} 실패: {status}", args.join(" "));
    }
}

#[derive(Clone, Copy)]
struct BoundaryAllow {
    path: &'static str,
    snippet: &'static str,
    count: usize,
    reason: &'static str,
}

struct BoundaryRule {
    pattern: &'static str,
    label: &'static str,
    allowed: &'static [BoundaryAllow],
}

const NO_ALLOW: &[BoundaryAllow] = &[];

const LOCAL_MCP_MANAGER_ALLOW: &[BoundaryAllow] = &[
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        // H5: McpAuthRequired(401 챌린지 downcast) 추가 후 rustfmt가 import를 줄바꿈 (2026-07-11)
        snippet: "LocalMcpManager, McpAuthRequired, McpHttpServerConfig",
        count: 1,
        reason: "PR-B00 deferred connector MCP runtime boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "let manager = LocalMcpManager::new(self.redaction.clone());",
        count: 1,
        reason: "PR-B00 deferred connector MCP discover/prepare/call execution",
    },
    // H5: 백그라운드 실행 컨텍스트(ExecContext)가 manager를 1회 생성해 소유
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "manager: LocalMcpManager::new(self.redaction.clone()),",
        count: 1,
        reason: "H5 connector ExecContext manager construction",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "manager: LocalMcpManager,",
        count: 1,
        reason: "H5 connector ExecContext manager field",
    },
    // H5: 401 사다리(run_oauth_ladder)가 manager를 인자로 받는 시그니처
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "manager: &LocalMcpManager,",
        count: 1,
        reason: "H5 connector OAuth ladder signature",
    },
];

const RECORD_TOOL_AUDIT_ALLOW: &[BoundaryAllow] = &[BoundaryAllow {
    path: "crates/app/src/ui/connectors.rs",
    snippet: "if let Err(e) = db.record_tool_audit(&record, &self.redaction, None) {",
    count: 1,
    reason: "PR-B00 deferred connector audit storage boundary",
}];

// H5: http Bearer/refresh 경로가 secret store를 leaf UI에서 직접 참조한다. auth의
// refresh_access_token(store: &dyn SecretStore)와 백그라운드 실행 모델이 커넥터가
// SecretStore를 보유하도록 강제한다 — deferred 경계로 명시. (더 특정한 secret_store:
// 스니펫을 store: 앞에 둬 substring 충돌을 피한다.)
const SECRET_STORE_ALLOW: &[BoundaryAllow] = &[
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "use secret::{RedactionService, SecretStore, SecretString};",
        count: 1,
        reason: "H5 connector Bearer/refresh secret store boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "secret_store: Arc<dyn SecretStore>,",
        count: 2,
        reason: "H5 connector secret store handle (UI + import dialog)",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "store: Arc<dyn SecretStore>,",
        count: 1,
        reason: "H5 connector ExecContext secret store handle",
    },
];

// H5: 저장된 access/DCR secret을 붙여 Bearer/refresh를 해석한다 (deferred 경계).
const GET_SECRET_ALLOW: &[BoundaryAllow] = &[
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: ".get_secret(&auth::dcr_secret_entry_id(&credential_id))",
        count: 1,
        reason: "H5 connector DCR client_secret read (stored-client)",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "match cx.store.get_secret(&auth_state.credential_id) {",
        count: 1,
        reason: "H5 connector single-flight refresh re-read",
    },
    // Bearer/DCR 해석은 run_http(백그라운드 실행 스레드)로 이동 — UI 스레드가
    // KEYRING_SERIAL을 잡지 않는다 (2026-07-16). 파일은 같아 예외로 남는다.
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "let access = cx.store.get_secret(&binding.credential_id).ok();",
        count: 1,
        reason: "H5 connector access token attach (run_http background)",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: ".get_secret(&auth::dcr_secret_entry_id(&binding.credential_id))",
        count: 1,
        reason: "H5 connector DCR client_secret read (run_http background)",
    },
];

const DB_CALL_ALLOW: &[BoundaryAllow] = &[
    BoundaryAllow {
        path: "crates/app/src/ui/agents.rs",
        snippet: "None => match db.list_agent_configs() {",
        count: 1,
        reason: "existing agent settings storage UI exception",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/agents.rs",
        snippet: "let profiles = db.list_env_profiles(workspace_id).unwrap_or_default();",
        count: 1,
        reason: "existing agent settings storage UI exception",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/agents.rs",
        snippet: "if let Err(e) = db.delete_agent_config(&id) {",
        count: 1,
        reason: "existing agent settings storage UI exception",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/agents.rs",
        snippet: "db.list_mcp_servers().unwrap_or_default()",
        count: 1,
        reason: "existing agent settings storage UI exception",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/agents.rs",
        snippet: "match db.insert_agent_config(",
        count: 1,
        reason: "existing agent settings storage UI exception",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/agents.rs",
        snippet: "for var in db.list_env_vars(profile_id)? {",
        count: 1,
        reason: "existing agent settings storage UI exception",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/agents.rs",
        snippet: "if !db.list_mcp_servers()?.iter().any(|s| &s.id == server_id) {",
        count: 1,
        reason: "existing agent settings storage UI exception",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "match db.list_permission_rules() {",
        count: 1,
        reason: "PR-B00 deferred connector storage boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "None => match db.list_mcp_servers() {",
        count: 1,
        reason: "PR-B00 deferred connector storage boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        // 가져오기(run_import)의 이름 중복 검사용 조회 (2026-07-11)
        snippet: "let mut existing: std::collections::HashSet<String> = match db.list_mcp_servers()",
        count: 1,
        reason: "PR-B00 deferred connector storage boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "if let Err(e) = db.insert_credential(&meta) {",
        count: 1,
        reason: "PR-B00 deferred connector OAuth metadata storage boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        // tools_cached 미스 시에만 조회 (2026-07-16 매 프레임 N+1 제거) +
        // HomeV1 Slack 상태 조회 slack_status/slack_status_for_server 2곳이
        // 동일 패턴을 재사용 (a5d1d1e, 2026-07-20)
        snippet: "None => match db.list_mcp_tools(&server.id) {",
        count: 3,
        reason: "PR-B00 deferred connector storage boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        // HomeV1 Slack 연결 상태 표시 (slack_status, a5d1d1e, 2026-07-20)
        snippet: "let Ok(servers) = db.list_mcp_servers() else {",
        count: 1,
        reason: "PR-B00 deferred connector storage boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        // HomeV1 Slack 커넥터 멱등 등록 (ensure_slack_server, a5d1d1e, 2026-07-20)
        snippet: "db.insert_mcp_server(&row)?;",
        count: 1,
        reason: "PR-B00 deferred connector storage boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "if let Err(e) = db.delete_permission_rule(&server.id, &tool.name) {",
        count: 1,
        reason: "PR-B00 deferred connector permission storage boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "if let Err(e) = db.upsert_permission_rule(",
        count: 1,
        reason: "PR-B00 deferred connector permission storage boundary",
    },
    // H3 url 편집 시점 훅(save_url_edit): Allow 규칙 초기화 + 도구 캐시 무효화 + url 갱신
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "let rules = db.list_permission_rules().context(\"권한 규칙 조회 실패\")?;",
        count: 1,
        reason: "H3 connector url-edit trust reset boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "db.delete_permission_rule(&rule.server_id, &rule.tool_name)",
        count: 1,
        reason: "H3 connector url-edit trust reset boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "db.replace_mcp_tools(server_id, &[])",
        count: 1,
        reason: "H3 connector url-edit tools cache invalidation boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "db.update_mcp_server_url(server_id, new_url)?;",
        count: 1,
        reason: "H3 connector url-edit storage boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "if let Err(e) = db.record_tool_audit(&record, &self.redaction, None) {",
        count: 1,
        reason: "PR-B00 deferred connector audit storage boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        // H5: OAuth 사다리 성공 시 tools 영속 (store_ladder_success — 기존 `match` 호출부는
        // drain_results로 이동해 &outcome.server_id로 갱신됨, 2026-07-11)
        snippet: "db.replace_mcp_tools(&server_id, &rows)",
        count: 1,
        reason: "PR-B00 deferred connector MCP tools storage boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "match db.replace_mcp_tools(&outcome.server_id, &rows) {",
        count: 1,
        reason: "H5 connector drain tools persistence boundary",
    },
    // H5: OAuth 바인딩 메타(oauth_json) 읽기/쓰기 — 비밀 아님, credentials.oauth_json
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "db.set_credential_oauth_json(&credential_id, &json)",
        count: 2,
        reason: "H5 connector OAuth binding metadata write (ladder + refresh)",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "db.list_credential_oauth_json()",
        count: 2,
        reason: "H5 connector OAuth binding lookup (bind + refresh persist)",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "let current = db.list_mcp_servers().ok().and_then(|rows| {",
        count: 1,
        reason: "H5 connector stale-URL discard boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        // 수동 추가 폼(add_server) + 가져오기 공용 등록(import_insert — stdio/http, H3) 두 경로
        snippet: "match db.insert_mcp_server(&row) {",
        count: 2,
        reason: "PR-B00 deferred connector MCP server storage boundary",
    },
    // env_profiles: dead 비-compact contents() 삭제(PR-ENV-D)로 compact 경로의 실제
    // 스니펫으로 재등록(2026-07-09). 예외 수는 삭제 전과 동일 범주(기존 storage UI 예외).
    // .env 일원화(E1, eb2bbe4)로 DB 전용 profile 생성/수정 경로가 사라져 insert/upsert
    // 예외를 제거하고, 대신 **레거시 이전 전용** 조회/삭제 경로를 등록한다(2026-07-14).
    // 레거시 이전이 끝나 해당 UI가 삭제되면 아래 3개(list/delete)도 함께 지운다.
    BoundaryAllow {
        path: "crates/app/src/ui/env_profiles.rs",
        snippet: "let p = db.list_env_profiles(workspace_id)?;",
        count: 1,
        reason: "existing env profile storage UI exception",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/env_profiles.rs",
        snippet: "let c = db.list_credentials_for_workspace(workspace_id)?;",
        count: 1,
        reason: "existing env profile storage UI exception",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/env_profiles.rs",
        snippet: "let v = db.list_env_vars(&profile_id)?;",
        count: 1,
        reason: "existing env profile storage UI exception",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/env_profiles.rs",
        snippet: "db.delete_env_var(&profile_id, &key)?;",
        count: 1,
        reason: "existing env profile storage UI exception",
    },
    // 아래 3개: 레거시 env profile 이전 UI (E1) 전용 — 이전 완료 후 UI와 함께 제거 대상.
    BoundaryAllow {
        path: "crates/app/src/ui/env_profiles.rs",
        snippet: "for var in db.list_env_vars(&profile.id)? {",
        count: 1,
        reason: "legacy env profile migration UI (E1) — remove with the migration UI",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/env_profiles.rs",
        snippet: "if db.list_env_vars(&profile_id)?.is_empty() {",
        count: 1,
        reason: "legacy env profile migration UI (E1) — remove with the migration UI",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/env_profiles.rs",
        snippet: "db.delete_env_profile(&profile_id)?;",
        count: 1,
        reason: "legacy env profile migration UI (E1) — remove with the migration UI",
    },
];

const BOUNDARY_RULES: &[BoundaryRule] = &[
    BoundaryRule {
        pattern: "KeyringSecretStore",
        label: "leaf UI must not name concrete keyring secret store",
        allowed: NO_ALLOW,
    },
    BoundaryRule {
        pattern: "SecretStore",
        label: "leaf UI must not import/use direct secret store trait",
        allowed: SECRET_STORE_ALLOW,
    },
    BoundaryRule {
        pattern: "set_secret(",
        label: "leaf UI must not write secrets directly",
        allowed: NO_ALLOW,
    },
    BoundaryRule {
        pattern: "get_secret(",
        label: "leaf UI must not read secrets directly",
        allowed: GET_SECRET_ALLOW,
    },
    BoundaryRule {
        pattern: "delete_secret(",
        label: "leaf UI must not delete secrets directly",
        allowed: NO_ALLOW,
    },
    BoundaryRule {
        pattern: "auth::store_token",
        label: "leaf UI must not store OAuth tokens directly",
        allowed: NO_ALLOW,
    },
    BoundaryRule {
        pattern: "LocalMcpManager",
        label: "connector-local MCP execution is a deferred boundary exception",
        allowed: LOCAL_MCP_MANAGER_ALLOW,
    },
    BoundaryRule {
        pattern: "record_tool_audit",
        label: "connector audit writes are a deferred boundary exception",
        allowed: RECORD_TOOL_AUDIT_ALLOW,
    },
    BoundaryRule {
        pattern: "db.",
        label: "leaf UI direct DB calls require explicit boundary exception",
        allowed: DB_CALL_ALLOW,
    },
    BoundaryRule {
        pattern: "alacritty_terminal",
        label: "app UI must not depend on terminal backend implementation",
        allowed: NO_ALLOW,
    },
    BoundaryRule {
        pattern: "portable_pty",
        label: "app UI must not depend on PTY implementation",
        allowed: NO_ALLOW,
    },
    BoundaryRule {
        pattern: "SessionManager",
        label: "app UI must not call session manager directly",
        allowed: NO_ALLOW,
    },
    BoundaryRule {
        pattern: "TerminalBackend",
        label: "app UI must not name terminal backend trait directly",
        allowed: NO_ALLOW,
    },
    BoundaryRule {
        pattern: "InProcessRuntimeClient",
        label: "leaf UI must not name concrete runtime client",
        allowed: NO_ALLOW,
    },
];

fn check_boundary() -> anyhow::Result<()> {
    let root = workspace_root()?;
    let mut violations = Vec::new();
    let mut allowed_seen: BTreeMap<(usize, &'static str, &'static str), usize> = BTreeMap::new();

    for path in rust_files_under(&root.join("crates/app/src/ui"))? {
        let rel = rel_path(&root, &path)?;
        let content = std::fs::read_to_string(&path).with_context(|| format!("{rel} 읽기 실패"))?;
        // 이 가드는 **프로덕션** leaf UI 경계만 governs한다. 유닛 테스트는 관례상
        // 파일 끝의 `#[cfg(test)]` 모듈에 모여 있고, 테스트 셋업은 DB/secret store/
        // manager를 직접 구성하는 것이 정상이므로 스캔에서 제외한다.
        // **마지막** 컬럼0 `#[cfg(test)]`부터를 test 영역으로 본다 — 첫 발생에서 끊으면
        // 중간에 `#[cfg(test)]` 헬퍼가 흩어진 파일(file_tree.rs: 2124/3015/3098/3230/
        // 3259)에서 그 뒤 프로덕션 코드가 통째로 스캔에서 빠진다 (H5 리뷰 P2 — 탐지
        // 통제 커버리지 구멍). 중간 헬퍼는 스캔되지만 test 코드라 경계 위반이 없다.
        let test_region_start = content
            .lines()
            .enumerate()
            .filter(|(_, line)| *line == "#[cfg(test)]")
            .map(|(idx, _)| idx)
            .last();
        for (line_idx, line) in content.lines().enumerate() {
            if test_region_start.is_some_and(|start| line_idx >= start) {
                break;
            }
            for (rule_idx, rule) in BOUNDARY_RULES.iter().enumerate() {
                if !line.contains(rule.pattern) {
                    continue;
                }
                if let Some(allow) = rule
                    .allowed
                    .iter()
                    .find(|allow| allow.path == rel && line.contains(allow.snippet))
                {
                    let key = (rule_idx, allow.path, allow.snippet);
                    let seen = allowed_seen.entry(key).or_insert(0);
                    *seen += 1;
                    if *seen > allow.count {
                        violations.push(format!(
                            "{rel}:{}: allowlist 초과: '{}' ({}) — {}",
                            line_idx + 1,
                            rule.pattern,
                            rule.label,
                            allow.reason
                        ));
                    }
                    continue;
                }
                violations.push(format!(
                    "{rel}:{}: boundary violation: '{}' ({})",
                    line_idx + 1,
                    rule.pattern,
                    rule.label
                ));
            }
        }
    }

    for (rule_idx, rule) in BOUNDARY_RULES.iter().enumerate() {
        for allow in rule.allowed {
            let seen = allowed_seen
                .get(&(rule_idx, allow.path, allow.snippet))
                .copied()
                .unwrap_or(0);
            if seen != allow.count {
                violations.push(format!(
                    "{}: allowlist drift for '{}' expected {} seen {} — {}",
                    allow.path, allow.snippet, allow.count, seen, allow.reason
                ));
            }
        }
    }

    check_session_secret_boundary(&root, &mut violations)?;
    check_authorization_capability_boundary(&root, &mut violations)?;

    if violations.is_empty() {
        let explicit_exceptions: usize = DB_CALL_ALLOW
            .iter()
            .chain(LOCAL_MCP_MANAGER_ALLOW)
            .chain(RECORD_TOOL_AUDIT_ALLOW)
            .chain(SECRET_STORE_ALLOW)
            .chain(GET_SECRET_ALLOW)
            .map(|allow| allow.count)
            .sum();
        println!(
            "check-boundary OK — UI leaf boundary guard passed; {explicit_exceptions} explicit UI DB/MCP/audit/secret exceptions remain"
        );
        Ok(())
    } else {
        violations.sort();
        violations.dedup();
        for violation in &violations {
            eprintln!("VIOLATION: {violation}");
        }
        bail!("check-boundary 실패: {}건", violations.len());
    }
}

fn check_authorization_capability_boundary(
    root: &Path,
    violations: &mut Vec<String>,
) -> anyhow::Result<()> {
    const STORAGE_ISSUER: &str = "crates/storage/src/db.rs";
    const EXACT_EVALUATORS: &[(&str, usize)] = &[
        ("crates/connector-service/src/coordinator.rs", 1),
        ("crates/mcp-proxy/src/hook.rs", 1),
    ];

    let mut issuer_count = 0usize;
    let mut evaluator_counts: BTreeMap<&str, usize> = EXACT_EVALUATORS
        .iter()
        .map(|(path, _)| (*path, 0))
        .collect();

    for path in rust_files_under(&root.join("crates"))? {
        let rel = rel_path(root, &path)?;
        if rel.starts_with("crates/audit/") {
            continue;
        }
        let content = std::fs::read_to_string(&path).with_context(|| format!("{rel} 읽기 실패"))?;

        // Capability identifiers are scanned across the complete source file. Unlike the leaf-UI
        // allowlist, dropping everything after a `#[cfg(test)]` marker would let a later production
        // item or an imported alias evade this security boundary.
        let issuers = identifier_occurrences(&content, "prepare_owned_authorization_preflight");
        if rel == STORAGE_ISSUER {
            issuer_count += issuers;
        } else if issuers != 0 {
            violations.push(format!(
                "{rel}: authorization grant issuer는 {STORAGE_ISSUER} transaction wrapper만 호출할 수 있습니다"
            ));
        }

        for sealed in [
            "prepare_authorization_operation",
            "AuthorizationPreflight::from_preflight",
        ] {
            if content.contains(sealed) {
                violations.push(format!(
                    "{rel}: sealed authorization proof API '{sealed}' production callsite 금지"
                ));
            }
        }

        if identifier_occurrences(&content, "evaluate_authorization") != 0 {
            violations.push(format!(
                "{rel}: compatibility authorization evaluator 금지 — exact permission fingerprint를 사용하세요"
            ));
        }

        let exact = content
            .matches("audit::evaluate_authorization_with_fingerprint(")
            .count();
        if let Some(count) = evaluator_counts.get_mut(rel.as_str()) {
            *count += exact;
        }
    }

    if issuer_count != 1 {
        violations.push(format!(
            "{STORAGE_ISSUER}: owner-scoped authorization issuer expected 1 seen {issuer_count}"
        ));
    }
    for (path, expected) in EXACT_EVALUATORS {
        let seen = evaluator_counts.get(path).copied().unwrap_or_default();
        if seen != *expected {
            violations.push(format!(
                "{path}: exact authorization evaluator expected {expected} seen {seen}"
            ));
        }
    }

    Ok(())
}

fn identifier_occurrences(source: &str, identifier: &str) -> usize {
    let bytes = source.as_bytes();
    source
        .match_indices(identifier)
        .filter(|(offset, _)| {
            let before = offset.checked_sub(1).and_then(|index| bytes.get(index));
            let after = bytes.get(offset + identifier.len());
            before.is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
                && after.is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
        })
        .count()
}

fn check_session_secret_boundary(root: &Path, violations: &mut Vec<String>) -> anyhow::Result<()> {
    const SESSION_SECRET_PATTERNS: &[(&str, &str)] = &[
        ("SecretStore", "session crate must not know secret store"),
        (
            "KeyringSecretStore",
            "session crate must not know concrete keyring store",
        ),
        ("secret::", "session crate must not depend on secret crate"),
    ];
    for path in rust_files_under(&root.join("crates/session/src"))? {
        let rel = rel_path(root, &path)?;
        let content = std::fs::read_to_string(&path).with_context(|| format!("{rel} 읽기 실패"))?;
        for (line_idx, line) in content.lines().enumerate() {
            for (pattern, label) in SESSION_SECRET_PATTERNS {
                if line.contains(pattern) {
                    violations.push(format!(
                        "{rel}:{}: boundary violation: '{}' ({label})",
                        line_idx + 1,
                        pattern
                    ));
                }
            }
        }
    }
    Ok(())
}

fn rust_files_under(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_rust_files(dir, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("{} 읽기 실패", dir.display()))?
    {
        let path = entry?.path();
        if path.is_dir() {
            collect_rust_files(&path, files)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            files.push(path);
        }
    }
    Ok(())
}

fn rel_path(root: &Path, path: &Path) -> anyhow::Result<String> {
    Ok(path
        .strip_prefix(root)
        .with_context(|| format!("{} is not under {}", path.display(), root.display()))?
        .to_string_lossy()
        .replace('\\', "/"))
}

fn check_deps() -> anyhow::Result<()> {
    let graph = local_dep_graph()?;
    let mut violations = Vec::new();

    // 1) 금지 edge 검사
    for (from, to) in FORBIDDEN_EDGES {
        if graph
            .get(*from)
            .is_some_and(|deps| deps.contains(&to.to_string()))
        {
            violations.push(format!("금지 edge: {from} → {to}"));
        }
    }

    // Connector contract/UI/service의 direct dependency는 역할별 정확한 집합만 허용한다.
    for (crate_name, allowed) in STRICT_CRATE_DEPS {
        let mut actual = direct_dependency_names(crate_name)?;
        let mut expected: Vec<String> = allowed.iter().map(|dep| (*dep).to_owned()).collect();
        actual.sort();
        actual.dedup();
        expected.sort();
        if actual != expected {
            violations.push(format!(
                "strict dependency drift: {crate_name} expected [{}], actual [{}]",
                expected.join(", "),
                actual.join(", ")
            ));
        }
    }

    // 2) 순환 검사 (로컬 그래프 DFS)
    for start in graph.keys() {
        let mut stack = vec![(start.clone(), vec![start.clone()])];
        while let Some((node, path)) = stack.pop() {
            for next in graph.get(&node).cloned().unwrap_or_default() {
                if next == *start {
                    violations.push(format!("순환: {} → {start}", path.join(" → ")));
                } else if !path.contains(&next) {
                    let mut p = path.clone();
                    p.push(next.clone());
                    stack.push((next, p));
                }
            }
        }
    }

    if violations.is_empty() {
        println!(
            "check-deps OK — crate {}개, 금지 edge/순환 없음",
            graph.len()
        );
        Ok(())
    } else {
        violations.sort();
        violations.dedup();
        for v in &violations {
            eprintln!("VIOLATION: {v}");
        }
        bail!("check-deps 실패: {}건", violations.len());
    }
}

fn direct_dependency_names(crate_name: &str) -> anyhow::Result<Vec<String>> {
    let root = workspace_root()?;
    let manifest_path = root.join("crates").join(crate_name).join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("{} 읽기 실패", manifest_path.display()))?;
    let mut in_deps_section = false;
    let mut dependencies = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_deps_section = trimmed.contains("dependencies");
            continue;
        }
        if !in_deps_section || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((name, _)) = trimmed.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if !name.is_empty() {
            dependencies.push(name.to_owned());
        }
    }
    Ok(dependencies)
}

/// crates/*/Cargo.toml + xtask에서 로컬 path 의존을 추출한다 (dev-dependencies 포함 —
/// dev 경유 순환도 금지). 반환: 디렉터리명 → 의존 디렉터리명 목록.
fn local_dep_graph() -> anyhow::Result<BTreeMap<String, Vec<String>>> {
    let root = workspace_root()?;
    let mut graph = BTreeMap::new();
    let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(root.join("crates"))
        .context("crates/ 디렉터리 읽기 실패")?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("Cargo.toml").is_file())
        .collect();
    dirs.push(root.join("xtask"));

    for dir in dirs {
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .context("crate 디렉터리명 없음")?
            .to_owned();
        let manifest = std::fs::read_to_string(dir.join("Cargo.toml"))
            .with_context(|| format!("{name}/Cargo.toml 읽기 실패"))?;
        // 의존 섹션([dependencies]/[dev-]/[build-]/target.*.dependencies) 안의,
        // `../`로 시작하는 path만 edge로 본다 — `[[bin]] path = "src/main.rs"` 같은
        // 비의존 라인 오탐 방지 (codex 리뷰).
        let mut in_deps_section = false;
        let mut deps = Vec::new();
        for line in manifest.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_deps_section = trimmed.contains("dependencies");
                continue;
            }
            if !in_deps_section {
                continue;
            }
            let Some((_, rest)) = trimmed.split_once("path") else {
                continue;
            };
            let Some(rest) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            let Some(rest) = rest.trim_start().strip_prefix('"') else {
                continue;
            };
            let Some(target) = rest.split('"').next() else {
                continue;
            };
            if !target.starts_with("../") {
                continue;
            }
            if let Some(dep_dir) = Path::new(target).file_name().and_then(|n| n.to_str()) {
                deps.push(dep_dir.to_owned());
            }
        }
        graph.insert(name, deps);
    }
    Ok(graph)
}

fn workspace_root() -> anyhow::Result<std::path::PathBuf> {
    // xtask는 항상 워크스페이스 안에서 실행된다 — CARGO_MANIFEST_DIR/..
    let manifest = std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR 없음")?;
    Ok(Path::new(&manifest)
        .parent()
        .context("워크스페이스 루트 없음")?
        .to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 현재_그래프는_금지edge와_순환이_없다() {
        check_deps().unwrap();
    }

    #[test]
    fn 현재_boundary는_허용된_예외만_남는다() {
        check_boundary().unwrap();
    }

    #[test]
    fn 로컬_의존_그래프가_기대_edge를_담는다() {
        let graph = local_dep_graph().unwrap();
        // 실재하는 대표 edge 몇 개로 파서가 동작함을 고정
        assert!(graph["storage"].contains(&"mcp".to_owned()) || !graph["storage"].is_empty());
        assert!(graph["runtime"].contains(&"mux".to_owned()));
        assert!(graph.contains_key("xtask"));
    }

    #[test]
    fn connector_crate_direct_dependency는_역할별_allowlist와_일치한다() {
        for (crate_name, allowed) in STRICT_CRATE_DEPS {
            let mut actual = direct_dependency_names(crate_name).unwrap();
            let mut expected: Vec<String> = allowed.iter().map(|dep| (*dep).to_owned()).collect();
            actual.sort();
            actual.dedup();
            expected.sort();
            assert_eq!(actual, expected, "{crate_name}");
        }
    }

    #[test]
    fn od01_failure_matrix는_contract_variant와_exact_one_to_one이다() {
        let root = workspace_root().unwrap();
        let variants = failure_point_variants(&root).unwrap();
        validate_failure_matrix(&variants, OD01_FAILURE_MATRIX).unwrap();
        assert_eq!(variants.len(), 10);
    }

    #[test]
    fn od01_failure_matrix는_missing_duplicate_unknown을_거부한다() {
        const DUPLICATE: &[FailureMatrixCase] = &[
            FailureMatrixCase {
                point: "Only",
                package: "fixture",
                exact_test: "fixture::one",
            },
            FailureMatrixCase {
                point: "Only",
                package: "fixture",
                exact_test: "fixture::two",
            },
        ];
        const UNKNOWN: &[FailureMatrixCase] = &[FailureMatrixCase {
            point: "Unexpected",
            package: "fixture",
            exact_test: "fixture::one",
        }];
        let expected = vec!["Only".to_owned(), "Missing".to_owned()];
        assert!(validate_failure_matrix(&expected, DUPLICATE).is_err());
        assert!(validate_failure_matrix(&expected, UNKNOWN).is_err());
        assert!(validate_failure_matrix(&expected, &OD01_FAILURE_MATRIX[..1]).is_err());
    }

    #[test]
    fn authorization_capability는_단일_transaction_경로만_사용한다() {
        let root = workspace_root().unwrap();
        let mut violations = Vec::new();
        check_authorization_capability_boundary(&root, &mut violations).unwrap();
        assert!(violations.is_empty(), "{violations:#?}");
    }

    #[test]
    fn authorization_identifier_scan은_midfile_cfg와_import_alias를_놓치지_않는다() {
        let source = r#"
#[cfg(test)]
mod tests {}

use audit::prepare_owned_authorization_preflight as mint;
use audit::evaluate_authorization as compatibility_evaluator;

fn production_item() {
    mint();
    compatibility_evaluator();
    audit::evaluate_authorization_with_fingerprint();
}
"#;
        assert_eq!(
            identifier_occurrences(source, "prepare_owned_authorization_preflight"),
            1
        );
        assert_eq!(identifier_occurrences(source, "evaluate_authorization"), 1);
        assert_eq!(
            identifier_occurrences(source, "evaluate_authorization_with_fingerprint"),
            1
        );
    }
}
