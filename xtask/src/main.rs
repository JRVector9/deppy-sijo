//! xtask — 워크스페이스 관리 명령 (v2.8 §11).
//!
//! `cargo run -p xtask -- check-deps`
//!   crate 그래프의 **금지 의존 edge**와 **순환**을 검사한다. v2.8 영속 계층 규칙
//!   (storage-core는 도메인을 모름, runtime crate는 store를 모름 등)을 코드로 강제해,
//!   `mcp → storage` 같은 순환 유발 edge가 무심코 추가되는 것을 막는다.
//!
//! `cargo run -p xtask -- check-boundary`
//!   app leaf UI가 secret/MCP/audit/storage side effect를 직접 갖지 않도록 검사한다.
//!   production leaf UI 경계 예외는 허용하지 않는다.
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
    for case in PERF_SMOKE_TESTS {
        run_exact_test(case)?;
    }
    println!(
        "perf-smoke OK — {} exact smoke tests",
        PERF_SMOKE_TESTS.len()
    );
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct FailureMatrixCase {
    point: &'static str,
    package: &'static str,
    exact_test: &'static str,
}

const PERF_SMOKE_TESTS: &[FailureMatrixCase] = &[
    FailureMatrixCase {
        point: "AppP95",
        package: "deppy-sijo",
        exact_test: "perf::tests::p95_계산",
    },
    FailureMatrixCase {
        point: "AppPercentile",
        package: "deppy-sijo",
        exact_test: "perf::tests::percentile_분위",
    },
    FailureMatrixCase {
        point: "AppHarnessShape",
        package: "deppy-sijo",
        exact_test: "perf::tests::하네스_명령_구성",
    },
    FailureMatrixCase {
        point: "RuntimeOutboundCoalescing",
        package: "runtime",
        exact_test: "remote::tests::outbound_queue는_viewport를_coalesce하고_status_lifecycle을_보존",
    },
    FailureMatrixCase {
        point: "RuntimeOutboundOverflow",
        package: "runtime",
        exact_test: "remote::tests::outbound_queue는_durable_overflow를_silent_drop하지_않는다",
    },
    FailureMatrixCase {
        point: "RuntimeReceiverCap",
        package: "runtime",
        exact_test: "remote::tests::receiver_drain은_durable_cap에서_멈춘다",
    },
    FailureMatrixCase {
        point: "RuntimeReceiverFilter",
        package: "runtime",
        exact_test: "remote::tests::receiver_drain은_additive_local_events를_wire에서_필터링한다",
    },
    FailureMatrixCase {
        point: "RuntimeHiddenStatus",
        package: "runtime",
        exact_test: "in_process::tests::status_화면_패턴_hidden에서_snapshot_없이_감지",
    },
    FailureMatrixCase {
        point: "RuntimeHiddenRemoteLease",
        package: "runtime",
        exact_test: "in_process::tests::원격_시청_lease는_hidden_세션_viewport를_흐르게_하고_해제시_멈춘다",
    },
];

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

struct BoundaryRule {
    pattern: &'static str,
    label: &'static str,
}

const BOUNDARY_RULES: &[BoundaryRule] = &[
    BoundaryRule {
        pattern: "KeyringSecretStore",
        label: "leaf UI must not name concrete keyring secret store",
    },
    BoundaryRule {
        pattern: "SecretStore",
        label: "leaf UI must not import/use direct secret store trait",
    },
    BoundaryRule {
        pattern: "set_secret(",
        label: "leaf UI must not write secrets directly",
    },
    BoundaryRule {
        pattern: "get_secret(",
        label: "leaf UI must not read secrets directly",
    },
    BoundaryRule {
        pattern: "delete_secret(",
        label: "leaf UI must not delete secrets directly",
    },
    BoundaryRule {
        pattern: "auth::store_token",
        label: "leaf UI must not store OAuth tokens directly",
    },
    BoundaryRule {
        pattern: "LocalMcpManager",
        label: "leaf UI must not execute Connector MCP transports directly",
    },
    BoundaryRule {
        pattern: "record_tool_audit",
        label: "leaf UI must not write audit records directly",
    },
    BoundaryRule {
        pattern: "db.",
        label: "leaf UI must not call the database directly",
    },
    BoundaryRule {
        pattern: "alacritty_terminal",
        label: "app UI must not depend on terminal backend implementation",
    },
    BoundaryRule {
        pattern: "portable_pty",
        label: "app UI must not depend on PTY implementation",
    },
    BoundaryRule {
        pattern: "SessionManager",
        label: "app UI must not call session manager directly",
    },
    BoundaryRule {
        pattern: "TerminalBackend",
        label: "app UI must not name terminal backend trait directly",
    },
    BoundaryRule {
        pattern: "InProcessRuntimeClient",
        label: "leaf UI must not name concrete runtime client",
    },
];

fn check_boundary() -> anyhow::Result<()> {
    let root = workspace_root()?;
    let mut violations = Vec::new();

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
            for rule in BOUNDARY_RULES {
                if !line.contains(rule.pattern) {
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

    check_session_secret_boundary(&root, &mut violations)?;
    check_authorization_capability_boundary(&root, &mut violations)?;
    check_app_render_source_boundary(&root, &mut violations)?;

    if violations.is_empty() {
        println!("check-boundary OK — UI leaf boundary guard passed; zero allowlist capability");
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

fn check_app_render_source_boundary(
    root: &Path,
    violations: &mut Vec<String>,
) -> anyhow::Result<()> {
    let rel = "crates/app/src/app.rs";
    let source = std::fs::read_to_string(root.join(rel)).context("app.rs 읽기 실패")?;
    let (_, ui_tail) = source
        .split_once("fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {")
        .context("eframe::App::ui 시작을 찾지 못했습니다")?;
    let (ui_body, _) = ui_tail
        .split_once("\n}\n\n/// Instant →")
        .context("eframe::App::ui 끝을 찾지 못했습니다")?;
    const FORBIDDEN: &[(&str, &str)] = &[
        ("self.db.", "render must not call Db directly"),
        (
            "KeyringSecretStore",
            "render must not access concrete keyring",
        ),
        ("rfd::", "render must return native-dialog intents"),
        ("std::fs::", "render must not perform filesystem I/O"),
        (
            "std::process::Command",
            "render must not spawn subprocesses",
        ),
        (
            "self.refresh_workspaces(",
            "render must consume the bounded workspace projection",
        ),
        (
            "self.handle_configured_shortcut(",
            "render must not start config, runtime, or protocol shortcut effects",
        ),
        (
            "self.settings_snapshot_worker.",
            "render must stage settings jobs for logic-owned admission",
        ),
        (
            "send_command(",
            "render must stage runtime protocol commands for logic",
        ),
        (
            "self.switch_workspace(",
            "render must stage workspace lifecycle transitions for logic",
        ),
        (
            "self.close_workspace_sessions(",
            "render must not shut down or join workspace runtimes",
        ),
        (
            "self.reveal_active_workspace_for_new_session(",
            "render must not persist workspace lifecycle state",
        ),
        (
            "self.send_agent_resume(",
            "render must not scan transcripts or write runtime input",
        ),
        (
            "self.sync_dotenv_env(",
            "render must not admit dotenv filesystem/storage work",
        ),
        (
            "platform::notify(",
            "render must return native notification intents",
        ),
        (
            "crate::fonts::",
            "render must not read or install font files",
        ),
        (
            "request_repaint_after(",
            "render must not install polling repaint timers",
        ),
    ];
    for (pattern, reason) in FORBIDDEN {
        if ui_body.contains(pattern) {
            violations.push(format!(
                "{rel}: App::ui direct source violation: '{pattern}' ({reason})"
            ));
        }
    }
    Ok(())
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
